//! macOS Quick Look previews for PowerPoint and Word documents.
//!
//! Quick Look renders Office files with the system's own Office generator, which is far more
//! faithful than anything available in-process. It is a *preview* technology, though, and that
//! shapes what this module can offer: `qlmanage -t` produces exactly one image per document, so
//! `vvrd` shows the first slide or page and nothing more.
//!
//! The paginated-looking alternative does not hold up. `qlmanage -p` emits a `.qlpreview` bundle
//! whose `Preview.html` does contain every slide, but as WebKit-specific markup — unitless CSS
//! lengths (`width:960`, `font-size:18`) and `<img src="Attachment1.pdf">` — that no other engine
//! renders correctly. Word previews are worse: their HTML carries no page structure at all.
//!
//! The conversion result is cached under the same cache directory `state.rs` uses, keyed by the
//! source path, length, and modification time, so the `--probe-document` preflight re-exec and the
//! render thread that follows it pay for only one `qlmanage` run.

use std::path::Path;

/// The kind of Office document being previewed. Drives the metadata row and the open notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfficeKind {
    Presentation,
    Document,
}

impl OfficeKind {
    /// Product name for the metadata overlay.
    pub fn label(self) -> &'static str {
        match self {
            Self::Presentation => "PowerPoint",
            Self::Document => "Word",
        }
    }

    /// What the single Quick Look preview actually shows, for the status notice.
    pub fn previewed_unit(self) -> &'static str {
        match self {
            Self::Presentation => "first slide",
            Self::Document => "first page",
        }
    }
}

/// Classify an already-lowercased extension against the formats Quick Look's Office generator
/// claims. Verified against `/System/Library/QuickLook/Office.qlgenerator`, which registers the
/// PresentationML, WordprocessingML, and legacy PowerPoint/Word UTIs together.
pub fn office_kind_for_extension(extension: &str) -> Option<OfficeKind> {
    match extension {
        "pptx" | "pptm" | "ppsx" | "ppsm" | "potx" | "potm" | "ppt" | "pps" | "pot" => {
            Some(OfficeKind::Presentation)
        }
        "docx" | "docm" | "dotx" | "dotm" | "doc" | "dot" => Some(OfficeKind::Document),
        _ => None,
    }
}

/// Classify a path by extension, lowercasing first so `DECK.PPTX` is recognised.
pub fn office_kind(path: &Path) -> Option<OfficeKind> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    office_kind_for_extension(&extension)
}

#[cfg(target_os = "macos")]
pub use macos::convert;

#[cfg(target_os = "macos")]
mod macos {
    use std::{
        fmt::Write as _,
        fs,
        path::{Path, PathBuf},
        process::{Command, Stdio},
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, Instant, UNIX_EPOCH},
    };

    use md5::{Digest, Md5};

    use crate::error::RenderError;

    const QLMANAGE: &str = "/usr/bin/qlmanage";

    /// Longest edge requested from Quick Look. A 16:9 slide comes back near 2048x1155, which is
    /// sharp enough for zoom mode without dwarfing the render cache's per-page budget.
    const PREVIEW_PIXELS: u32 = 2_048;

    /// `qlmanage` does not bound itself: handed a file with an Office extension but malformed
    /// contents it blocks forever rather than reporting failure (measured past two minutes), so
    /// this timeout is the only thing that ends the conversion. Real documents are far quicker —
    /// a three-slide deck converts in well under a second — and `wait_for_document` in `main.rs`
    /// gives the first `Opened` event only 30 seconds, with the MuPDF open still to come.
    const CONVERT_TIMEOUT: Duration = Duration::from_secs(15);

    const POLL_INTERVAL: Duration = Duration::from_millis(25);

    /// Validate before allocate: a preview far larger than this is a malfunction, not a document.
    const MAX_PREVIEW_BYTES: u64 = 64 * 1024 * 1024;

    static WORKDIR_ID: AtomicU64 = AtomicU64::new(1);

    /// Render `source` with Quick Look, returning a cached PNG that MuPDF can open.
    ///
    /// The result is a single page: Quick Look cannot paginate.
    pub fn convert(source: &Path) -> Result<PathBuf, RenderError> {
        let cached = cache_path(source)?;
        if cached.is_file() {
            return Ok(cached);
        }
        let cache_dir = cached
            .parent()
            .ok_or_else(|| converting("the Quick Look cache path has no parent directory"))?;
        let workdir = WorkDir::create(cache_dir)?;
        run_qlmanage(source, workdir.path())?;
        let produced = produced_preview(workdir.path(), source)?;
        // Rename inside the cache directory so a concurrent vvrd never sees a partial file.
        fs::rename(&produced, &cached).map_err(|error| {
            converting(format!(
                "cannot store the Quick Look preview of {}: {error}",
                source.display()
            ))
        })?;
        Ok(cached)
    }

    /// Cache key: absolute path, length, and modification time. Any edit to the source produces a
    /// different name, so `R`/F5 re-converts instead of serving a stale preview.
    fn cache_path(source: &Path) -> Result<PathBuf, RenderError> {
        let metadata = fs::metadata(source)
            .map_err(|error| converting(format!("cannot read {}: {error}", source.display())))?;
        if !metadata.is_file() {
            return Err(converting(format!(
                "{} is not a regular file",
                source.display()
            )));
        }
        let absolute = std::path::absolute(source).unwrap_or_else(|_| source.to_path_buf());

        let mut hasher = Md5::new();
        hasher.update(absolute.to_string_lossy().as_bytes());
        hasher.update(metadata.len().to_le_bytes());
        if let Ok(modified) = metadata.modified()
            && let Ok(since_epoch) = modified.duration_since(UNIX_EPOCH)
        {
            hasher.update(since_epoch.as_nanos().to_le_bytes());
        }

        let mut name = String::with_capacity(32 + ".png".len());
        for byte in hasher.finalize().as_slice() {
            let _ = write!(&mut name, "{byte:02x}");
        }
        name.push_str(".png");

        let directory = directories::ProjectDirs::from("", "", "vvrd")
            .ok_or_else(|| converting("no cache directory is available for Quick Look previews"))?
            .cache_dir()
            .join("quicklook");
        fs::create_dir_all(&directory).map_err(|error| {
            converting(format!("cannot create {}: {error}", directory.display()))
        })?;
        Ok(directory.join(name))
    }

    fn run_qlmanage(source: &Path, workdir: &Path) -> Result<(), RenderError> {
        let mut child = Command::new(QLMANAGE)
            .arg("-t")
            .arg("-s")
            .arg(PREVIEW_PIXELS.to_string())
            .arg("-o")
            .arg(workdir)
            .arg(source)
            // Quick Look never talks Vivid, so it inherits no session material. This mirrors the
            // document preflight in `main.rs`; VIVID_TOKEN is the retired 1.1 name and is scrubbed
            // too, so a stale variable cannot leak.
            .env_remove("VIVID_ROOT_SECRET")
            .env_remove("VIVID_TOKEN")
            .env_remove("VIVID_ENDPOINT_CONTROL")
            .env_remove("VIVID_ENDPOINT_INTERACTIVE")
            .env_remove("VIVID_ENDPOINT_REALTIME")
            .env_remove("VIVID_ENDPOINT_BULK")
            // vvrd owns the terminal: a child inheriting stdout would corrupt the live session.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| converting(format!("cannot start {QLMANAGE}: {error}")))?;

        let deadline = Instant::now() + CONVERT_TIMEOUT;
        loop {
            match child.try_wait() {
                // qlmanage reports success even when it declines to produce a thumbnail, so the
                // exit status is not load-bearing; `produced_preview` decides.
                Ok(Some(_)) => return Ok(()),
                Ok(None) => {}
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(converting(format!("cannot wait for {QLMANAGE}: {error}")));
                }
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(converting(format!(
                    "Quick Look did not preview {} within {} seconds; the document may be \
                     malformed, or this macOS session may have no Quick Look service",
                    source.display(),
                    CONVERT_TIMEOUT.as_secs()
                )));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Find the PNG Quick Look wrote. It names the file after the source, but the exact name is not
    /// contractual, so take the one PNG in a directory we created empty.
    fn produced_preview(workdir: &Path, source: &Path) -> Result<PathBuf, RenderError> {
        let entries = fs::read_dir(workdir)
            .map_err(|error| converting(format!("cannot read {}: {error}", workdir.display())))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
            {
                continue;
            }
            let length = fs::metadata(&path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            if length == 0 {
                continue;
            }
            if length > MAX_PREVIEW_BYTES {
                return Err(converting(format!(
                    "the Quick Look preview of {} exceeds {MAX_PREVIEW_BYTES} bytes",
                    source.display()
                )));
            }
            return Ok(path);
        }
        Err(converting(format!(
            "Quick Look produced no preview for {}; it needs a logged-in macOS session",
            source.display()
        )))
    }

    /// A scratch directory inside the cache directory, so publishing is a same-filesystem rename.
    struct WorkDir(PathBuf);

    impl WorkDir {
        fn create(cache_dir: &Path) -> Result<Self, RenderError> {
            let path = cache_dir.join(format!(
                "pending-{}-{}",
                std::process::id(),
                WORKDIR_ID.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).map_err(|error| {
                converting(format!("cannot create {}: {error}", path.display()))
            })?;
            Ok(Self(path))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for WorkDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn converting(message: impl Into<String>) -> RenderError {
        RenderError::Converting(message.into())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A conversion that fails partway must not leave scratch files behind in the cache.
        #[test]
        fn the_scratch_directory_is_removed_even_when_a_conversion_fails() {
            let root = std::env::temp_dir().join(format!(
                "vvrd-quicklook-workdir-test-{}-{}",
                std::process::id(),
                WORKDIR_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).expect("create the test cache directory");

            let path = {
                let workdir = WorkDir::create(&root).expect("create the scratch directory");
                let path = workdir.path().to_path_buf();
                assert!(path.is_dir(), "the scratch directory exists while in scope");
                fs::write(path.join("partial.png"), b"partial").expect("write a partial artifact");
                path
            };

            assert!(!path.exists(), "the scratch directory is removed on drop");
            let _ = fs::remove_dir_all(&root);
        }

        /// Two conversions running at once must not share a scratch directory.
        #[test]
        fn concurrent_scratch_directories_do_not_collide() {
            let root = std::env::temp_dir().join(format!(
                "vvrd-quicklook-workdir-unique-{}-{}",
                std::process::id(),
                WORKDIR_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).expect("create the test cache directory");

            let first = WorkDir::create(&root).expect("first scratch directory");
            let second = WorkDir::create(&root).expect("second scratch directory");
            assert_ne!(first.path(), second.path());

            let _ = fs::remove_dir_all(&root);
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn convert(source: &Path) -> Result<std::path::PathBuf, crate::error::RenderError> {
    Err(crate::error::RenderError::Converting(format!(
        "cannot display {}: PowerPoint and Word rendering needs macOS Quick Look",
        source.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn office_extensions_map_to_their_product_and_others_do_not() {
        assert_eq!(
            office_kind(Path::new("/decks/launch.pptx")),
            Some(OfficeKind::Presentation)
        );
        assert_eq!(
            office_kind(Path::new("/decks/legacy.ppt")),
            Some(OfficeKind::Presentation)
        );
        assert_eq!(
            office_kind(Path::new("/papers/report.docx")),
            Some(OfficeKind::Document)
        );
        assert_eq!(
            office_kind(Path::new("/papers/legacy.doc")),
            Some(OfficeKind::Document)
        );
        assert_eq!(office_kind(Path::new("/papers/paper.pdf")), None);
        assert_eq!(office_kind(Path::new("/books/book.epub")), None);
        assert_eq!(office_kind(Path::new("/notes/notes.md")), None);
        // Spreadsheets share the generator but vvrd does not claim them.
        assert_eq!(office_kind(Path::new("/sheets/budget.xlsx")), None);
        assert_eq!(office_kind(Path::new("/decks/no-extension")), None);
    }

    #[test]
    fn office_extension_matching_ignores_case() {
        assert_eq!(
            office_kind(Path::new("/decks/LAUNCH.PPTX")),
            Some(OfficeKind::Presentation)
        );
        assert_eq!(
            office_kind(Path::new("/papers/Report.DocX")),
            Some(OfficeKind::Document)
        );
    }

    #[test]
    fn office_kinds_describe_what_the_preview_shows() {
        assert_eq!(OfficeKind::Presentation.label(), "PowerPoint");
        assert_eq!(OfficeKind::Presentation.previewed_unit(), "first slide");
        assert_eq!(OfficeKind::Document.label(), "Word");
        assert_eq!(OfficeKind::Document.previewed_unit(), "first page");
    }

    /// Quick Look claims PNG, so a file we can write in-process exercises the whole conversion
    /// path — spawn, timeout, artifact discovery, and cache publish — without a binary fixture.
    #[cfg(target_os = "macos")]
    #[test]
    fn quicklook_converts_and_then_serves_the_cached_preview() {
        use std::{path::PathBuf, sync::atomic::AtomicU64};

        static TEMP_ID: AtomicU64 = AtomicU64::new(1);

        if !Path::new("/usr/bin/qlmanage").is_file() {
            return;
        }
        let source: PathBuf = std::env::temp_dir().join(format!(
            "vvrd-quicklook-test-{}-{}.png",
            std::process::id(),
            TEMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        image::RgbaImage::from_pixel(64, 48, image::Rgba([32, 96, 200, 255]))
            .save(&source)
            .expect("write the source image");

        let first = convert(&source).expect("Quick Look converts a PNG");
        assert!(first.is_file(), "the preview is published into the cache");
        let second = convert(&source).expect("the second conversion is served from the cache");
        assert_eq!(
            first, second,
            "the cache key is stable for an unchanged file"
        );

        let _ = std::fs::remove_file(&source);
        let _ = std::fs::remove_file(&first);
    }

    /// Editing the source must produce a different cache entry, or `R`/F5 would serve a stale
    /// preview forever.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_changed_source_does_not_reuse_the_previous_preview() {
        use std::{path::PathBuf, sync::atomic::AtomicU64};

        static TEMP_ID: AtomicU64 = AtomicU64::new(1);

        if !Path::new("/usr/bin/qlmanage").is_file() {
            return;
        }
        let source: PathBuf = std::env::temp_dir().join(format!(
            "vvrd-quicklook-reload-test-{}-{}.png",
            std::process::id(),
            TEMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        image::RgbaImage::from_pixel(64, 48, image::Rgba([32, 96, 200, 255]))
            .save(&source)
            .expect("write the source image");
        let first = convert(&source).expect("first conversion");

        // A different size guarantees a different length as well as a different mtime.
        image::RgbaImage::from_pixel(96, 72, image::Rgba([200, 32, 96, 255]))
            .save(&source)
            .expect("rewrite the source image");
        let second = convert(&source).expect("second conversion");

        assert_ne!(first, second, "the cache key follows the source content");

        let _ = std::fs::remove_file(&source);
        let _ = std::fs::remove_file(&first);
        let _ = std::fs::remove_file(&second);
    }
}
