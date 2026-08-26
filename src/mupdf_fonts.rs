//! Cached system-font resolution for MuPDF.
//!
//! MuPDF asks for a substitute font every time it meets a character that no already-loaded face
//! covers, and it only remembers the answer when one is found. A book written in a script with no
//! installed font therefore repeats the same failing lookup once per character. The `mupdf` crate's
//! built-in `SystemFontLoader` answers each of those with a fresh `font-kit` `SystemSource`, so
//! every repeat enumerates the whole system font collection over a synchronous CoreText/fontconfig
//! round trip. Laying out one chapter of a Chinese EPUB issued ~40k such lookups and took seconds;
//! counting a whole book took ten minutes, nearly all of it in font enumeration.
//!
//! This loader replaces that path. It builds one [`fontdb`] database, resolves through it, and
//! memoizes the outcome — including misses, which are what the pathological case repeats. The
//! `mupdf` dependency drops the `system-fonts` feature so this is the only loader in the chain and
//! a remembered miss really does end the lookup.

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use fontdb::{Database, Family, Query, Style, Weight};
use mupdf::{CjkFontOrdering, Font, FontHints, FontLoader};

/// Script selectors MuPDF passes to [`FontLoader::load_fallback_font`].
/// Values come from `mupdf/include/mupdf/ucdn.h`.
const UCDN_SCRIPT_HEBREW: u32 = 5;
const UCDN_SCRIPT_ARABIC: u32 = 6;
const UCDN_SCRIPT_DEVANAGARI: u32 = 9;
const UCDN_SCRIPT_THAI: u32 = 19;
const UCDN_SCRIPT_HANGUL: u32 = 24;
const UCDN_SCRIPT_HIRAGANA: u32 = 32;
const UCDN_SCRIPT_KATAKANA: u32 = 33;
const UCDN_SCRIPT_BOPOMOFO: u32 = 34;
const UCDN_SCRIPT_HAN: u32 = 35;

/// A document controls how many distinct names it asks for, so the memo is bounded. Past the cap
/// lookups still resolve correctly, they just stop being remembered.
const MAX_CACHE_ENTRIES: usize = 1024;
/// Longest font name worth a database query.
const MAX_FONT_NAME_BYTES: usize = 256;

/// Families to try for a script, most preferred first.
struct Candidates {
    serif: &'static [&'static str],
    sans: &'static [&'static str],
}

impl Candidates {
    /// The preferred list first, then the other one: a serif request is a preference, not a
    /// requirement, and a missing glyph is worse than a mismatched stroke style.
    fn ordered(&self, serif: bool) -> impl Iterator<Item = &'static str> {
        let (first, second) = if serif {
            (self.serif, self.sans)
        } else {
            (self.sans, self.serif)
        };
        first.iter().chain(second.iter()).copied()
    }
}

const HAN_SC: Candidates = Candidates {
    serif: &[
        "Songti SC",
        "STSong",
        "Noto Serif CJK SC",
        "Source Han Serif SC",
        "SimSun",
    ],
    sans: &[
        "PingFang SC",
        "Heiti SC",
        "Hiragino Sans GB",
        "Noto Sans CJK SC",
        "Source Han Sans SC",
        "Microsoft YaHei",
        "SimHei",
        "WenQuanYi Micro Hei",
        "Droid Sans Fallback",
        "Arial Unicode MS",
    ],
};

const HAN_TC: Candidates = Candidates {
    serif: &[
        "Songti TC",
        "STSong",
        "Noto Serif CJK TC",
        "Source Han Serif TC",
        "PMingLiU",
    ],
    sans: &[
        "PingFang TC",
        "Heiti TC",
        "Noto Sans CJK TC",
        "Source Han Sans TC",
        "Microsoft JhengHei",
        "Droid Sans Fallback",
        "Arial Unicode MS",
    ],
};

const HAN_JP: Candidates = Candidates {
    serif: &[
        "Hiragino Mincho ProN",
        "YuMincho",
        "Noto Serif CJK JP",
        "Source Han Serif JP",
        "MS Mincho",
    ],
    sans: &[
        "Hiragino Sans",
        "Hiragino Kaku Gothic ProN",
        "Yu Gothic",
        "Noto Sans CJK JP",
        "Source Han Sans JP",
        "MS Gothic",
        "Droid Sans Fallback",
        "Arial Unicode MS",
    ],
};

const HAN_KR: Candidates = Candidates {
    serif: &["AppleMyungjo", "Noto Serif CJK KR", "Batang"],
    sans: &[
        "Apple SD Gothic Neo",
        "AppleGothic",
        "Noto Sans CJK KR",
        "Source Han Sans KR",
        "Malgun Gothic",
        "Gulim",
        "Arial Unicode MS",
    ],
};

const ARABIC: Candidates = Candidates {
    serif: &["Noto Naskh Arabic", "Times New Roman"],
    sans: &["Geeza Pro", "Noto Sans Arabic", "Arial", "Tahoma"],
};

const HEBREW: Candidates = Candidates {
    serif: &["Noto Serif Hebrew", "Times New Roman"],
    sans: &["Arial Hebrew", "Noto Sans Hebrew", "Arial"],
};

const THAI: Candidates = Candidates {
    serif: &["Noto Serif Thai"],
    sans: &["Thonburi", "Noto Sans Thai", "Ayuthaya"],
};

const DEVANAGARI: Candidates = Candidates {
    serif: &["Noto Serif Devanagari"],
    sans: &[
        "Devanagari Sangam MN",
        "Noto Sans Devanagari",
        "Kohinoor Devanagari",
    ],
};

/// Last resort for a script with no table of its own: faces that cover unusually large parts of
/// Unicode. Answering with one of these still beats answering with nothing.
const WIDE_COVERAGE: Candidates = Candidates {
    serif: &["Noto Serif", "Times New Roman"],
    sans: &[
        "Arial Unicode MS",
        "Noto Sans",
        "Droid Sans Fallback",
        "Arial",
    ],
};

/// Install the cached loader. Call once, before opening any document.
pub fn install() {
    mupdf::set_font_loader(CachedSystemFonts);
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Named {
        name: String,
        bold: bool,
        italic: bool,
        needs_exact_metrics: bool,
    },
    Cjk {
        name: String,
        ordering: i32,
        serif: bool,
    },
    Fallback {
        script: u32,
        language: u32,
        serif: bool,
    },
}

/// A resolved face, held as a database handle rather than as font bytes: MuPDF keeps its own copy
/// of every face it accepts, and a CJK collection runs to tens of megabytes.
#[derive(Debug, Clone)]
struct Resolved {
    id: fontdb::ID,
    family: String,
    index: i32,
}

fn database() -> &'static Database {
    static DATABASE: OnceLock<Database> = OnceLock::new();
    DATABASE.get_or_init(|| {
        let mut database = Database::new();
        database.load_system_fonts();
        database
    })
}

fn cache() -> &'static Mutex<HashMap<Key, Option<Resolved>>> {
    static CACHE: OnceLock<Mutex<HashMap<Key, Option<Resolved>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

struct CachedSystemFonts;

impl CachedSystemFonts {
    /// Resolve `key` through `resolve`, remembering the outcome — a miss included.
    fn cached(
        &self,
        key: Key,
        resolve: impl FnOnce(&Database) -> Option<Resolved>,
    ) -> Option<Font> {
        let guard = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(hit) = guard.get(&key) {
            let hit = hit.clone();
            drop(guard);
            return hit.and_then(build_font);
        }
        drop(guard);

        let resolved = resolve(database());

        let mut guard = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.len() < MAX_CACHE_ENTRIES {
            guard.insert(key, resolved.clone());
        }
        drop(guard);
        resolved.and_then(build_font)
    }
}

impl FontLoader for CachedSystemFonts {
    fn load_font(&self, name: &str, hints: FontHints) -> Option<Font> {
        if name.is_empty() || name.len() > MAX_FONT_NAME_BYTES {
            return None;
        }
        let key = Key::Named {
            name: name.to_owned(),
            bold: hints.bold,
            italic: hints.italic,
            needs_exact_metrics: hints.needs_exact_metrics,
        };
        self.cached(key, |database| resolve_named(database, name, hints))
    }

    fn load_cjk_font(&self, name: &str, ordering: CjkFontOrdering, serif: bool) -> Option<Font> {
        if name.len() > MAX_FONT_NAME_BYTES {
            return None;
        }
        let key = Key::Cjk {
            name: name.to_owned(),
            ordering: ordering as i32,
            serif,
        };
        self.cached(key, |database| {
            let candidates = match ordering {
                CjkFontOrdering::AdobeGb => &HAN_SC,
                CjkFontOrdering::AdobeCns => &HAN_TC,
                CjkFontOrdering::AdobeJapan => &HAN_JP,
                CjkFontOrdering::AdobeKorea => &HAN_KR,
            };
            resolve_families(database, candidates.ordered(serif), FontHints::default())
        })
    }

    fn load_fallback_font(&self, script: u32, language: u32, hints: FontHints) -> Option<Font> {
        let key = Key::Fallback {
            script,
            language,
            serif: hints.serif,
        };
        self.cached(key, |database| {
            let candidates = fallback_candidates(script, language);
            resolve_families(database, candidates.ordered(hints.serif), hints)
        })
    }
}

/// Pick the family table for a script, using the text language to choose a Han variant.
fn fallback_candidates(script: u32, language: u32) -> &'static Candidates {
    match script {
        UCDN_SCRIPT_HAN => match language_tag(language).as_str() {
            // "zhs"/"zht" are how `FZ_LANG_zh_Hans`/`FZ_LANG_zh_Hant` pack down.
            "zht" => &HAN_TC,
            "ja" => &HAN_JP,
            "ko" => &HAN_KR,
            // Simplified Chinese also answers an unset or non-CJK language. MuPDF probes the Han
            // variants in turn, so covering the first probe avoids three more failing lookups.
            _ => &HAN_SC,
        },
        UCDN_SCRIPT_HIRAGANA | UCDN_SCRIPT_KATAKANA => &HAN_JP,
        UCDN_SCRIPT_HANGUL => &HAN_KR,
        UCDN_SCRIPT_BOPOMOFO => &HAN_TC,
        UCDN_SCRIPT_ARABIC => &ARABIC,
        UCDN_SCRIPT_HEBREW => &HEBREW,
        UCDN_SCRIPT_THAI => &THAI,
        UCDN_SCRIPT_DEVANAGARI => &DEVANAGARI,
        _ => &WIDE_COVERAGE,
    }
}

/// Undo `FZ_LANG_TAG2`/`FZ_LANG_TAG3` (`mupdf/include/mupdf/fitz/text.h`), which pack up to three
/// lowercase letters into base-27 digits.
fn language_tag(language: u32) -> String {
    let mut remaining = language;
    let mut tag = String::new();
    while remaining != 0 && tag.len() < 3 {
        let letter = (remaining % 27) as u8;
        remaining /= 27;
        if letter == 0 {
            break;
        }
        tag.push(char::from(b'a' + letter - 1));
    }
    tag
}

fn resolve_named(database: &Database, name: &str, hints: FontHints) -> Option<Resolved> {
    // A document usually names a face the way the PDF font dictionary does, so try PostScript
    // names before family names.
    if let Some(face) = database
        .faces()
        .find(|face| face.post_script_name.eq_ignore_ascii_case(name))
    {
        return Some(resolved_from(face));
    }

    // MuPDF passes through PDF-flavoured suffixes that no installed family carries.
    let mut trimmed = name;
    for suffix in ["MT", "PS", "IdentityH"] {
        trimmed = trimmed.strip_suffix(suffix).unwrap_or(trimmed);
    }

    let family = generic_family(trimmed).unwrap_or(Family::Name(trimmed));
    resolve_families_inner(database, std::iter::once(family), hints)
}

/// Map the CSS generic families MuPDF's HTML engine asks for by name.
fn generic_family(name: &str) -> Option<Family<'static>> {
    match name.to_ascii_lowercase().as_str() {
        "serif" => Some(Family::Serif),
        "sans-serif" | "sans serif" => Some(Family::SansSerif),
        "monospace" => Some(Family::Monospace),
        "cursive" => Some(Family::Cursive),
        "fantasy" => Some(Family::Fantasy),
        _ => None,
    }
}

fn resolve_families<'a>(
    database: &Database,
    names: impl Iterator<Item = &'a str>,
    hints: FontHints,
) -> Option<Resolved> {
    names.into_iter().find_map(|name| {
        resolve_families_inner(database, std::iter::once(Family::Name(name)), hints)
    })
}

fn resolve_families_inner<'a>(
    database: &Database,
    families: impl Iterator<Item = Family<'a>>,
    hints: FontHints,
) -> Option<Resolved> {
    let families: Vec<Family<'a>> = families.collect();
    let id = database.query(&Query {
        families: &families,
        weight: if hints.bold {
            Weight::BOLD
        } else {
            Weight::NORMAL
        },
        style: if hints.italic {
            Style::Italic
        } else {
            Style::Normal
        },
        ..Query::default()
    })?;
    let face = database.face(id)?;

    // `needs_exact_metrics` means MuPDF cannot synthesise the style itself, so a face that only
    // approximates the request is worse than no answer at all.
    if hints.needs_exact_metrics {
        let is_bold = face.weight >= Weight::BOLD;
        let is_italic = !matches!(face.style, Style::Normal);
        if (hints.bold && !is_bold) || (hints.italic && !is_italic) {
            return None;
        }
    }
    Some(resolved_from(face))
}

fn resolved_from(face: &fontdb::FaceInfo) -> Resolved {
    Resolved {
        id: face.id,
        family: face
            .families
            .first()
            .map(|(family, _)| family.clone())
            .unwrap_or_else(|| face.post_script_name.clone()),
        index: i32::try_from(face.index).unwrap_or(0),
    }
}

fn build_font(resolved: Resolved) -> Option<Font> {
    database().with_face_data(resolved.id, |data, index| {
        Font::from_bytes_with_index(
            &resolved.family,
            i32::try_from(index).unwrap_or(resolved.index),
            data,
        )
        .ok()
    })?
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn language_tag_decodes_mupdf_packing() {
        // The values MuPDF actually passed while laying out a Chinese EPUB.
        assert_eq!(language_tag(37), "ja");
        assert_eq!(language_tag(416), "ko");
        assert_eq!(language_tag(14093), "zhs");
        assert_eq!(language_tag(14822), "zht");
        assert_eq!(language_tag(383), "en");
        assert_eq!(language_tag(0), "");
    }

    #[test]
    fn han_fallback_follows_the_text_language() {
        let families = |script, language| {
            fallback_candidates(script, language)
                .ordered(false)
                .collect::<Vec<_>>()
        };
        assert!(families(UCDN_SCRIPT_HAN, 14093).contains(&"PingFang SC"));
        assert!(families(UCDN_SCRIPT_HAN, 14822).contains(&"PingFang TC"));
        assert!(families(UCDN_SCRIPT_HAN, 37).contains(&"Hiragino Sans"));
        assert!(families(UCDN_SCRIPT_HAN, 416).contains(&"Apple SD Gothic Neo"));
        // An unset or non-CJK language still has to answer, or MuPDF asks again per character.
        assert!(families(UCDN_SCRIPT_HAN, 0).contains(&"PingFang SC"));
        assert!(families(UCDN_SCRIPT_HANGUL, 0).contains(&"Apple SD Gothic Neo"));
        assert!(families(UCDN_SCRIPT_HIRAGANA, 0).contains(&"Hiragino Sans"));
    }

    #[test]
    fn a_serif_request_still_falls_through_to_sans() {
        // Preference, not requirement: a missing glyph is worse than a mismatched stroke style.
        let serif_first = HAN_SC.ordered(true).collect::<Vec<_>>();
        assert_eq!(serif_first.first(), Some(&"Songti SC"));
        assert!(serif_first.contains(&"PingFang SC"));
    }

    #[test]
    fn generic_css_families_are_recognised() {
        assert_eq!(generic_family("serif"), Some(Family::Serif));
        assert_eq!(generic_family("Sans-Serif"), Some(Family::SansSerif));
        assert_eq!(generic_family("monospace"), Some(Family::Monospace));
        assert_eq!(generic_family("Charis SIL"), None);
    }

    /// The regression this module exists for: MuPDF repeats a failing lookup once per character,
    /// so a miss that is not remembered is a system-wide font enumeration per character.
    #[test]
    fn a_missing_font_is_only_resolved_once() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let key = Key::Named {
            name: "vvrd-test-font-that-does-not-exist".to_owned(),
            bold: false,
            italic: false,
            needs_exact_metrics: false,
        };
        for _ in 0..32 {
            let font = CachedSystemFonts.cached(key.clone(), |_| {
                CALLS.fetch_add(1, Ordering::Relaxed);
                None
            });
            assert!(font.is_none());
        }
        assert_eq!(CALLS.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn an_over_long_font_name_is_rejected_before_it_reaches_the_database() {
        let name = "x".repeat(MAX_FONT_NAME_BYTES + 1);
        assert!(
            CachedSystemFonts
                .load_font(&name, FontHints::default())
                .is_none()
        );
        assert!(
            CachedSystemFonts
                .load_font("", FontHints::default())
                .is_none()
        );
        let guard = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!guard.keys().any(|key| matches!(
            key,
            Key::Named { name: cached, .. } if cached.len() > MAX_FONT_NAME_BYTES
        )));
    }
}
