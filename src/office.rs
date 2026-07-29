//! Office document input: DOCX, PPTX, ODT, and ODP are converted to PDF before
//! MuPDF ever sees them.
//!
//! MuPDF cannot read any of these formats, so the reader resolves the CLI path
//! once at startup into a *render path*. Native inputs (PDF, EPUB, anything
//! else MuPDF sniffs) pass straight through untouched; office inputs are
//! converted into a self-deleting temporary directory and the resulting PDF
//! becomes the render path. Everything downstream — the render thread, the page
//! cache, the compositor, the raster-delta planner — keeps working on an
//! ordinary fixed-layout PDF and needs no format knowledge at all.
//!
//! Two conversion backends exist. A real LibreOffice install (`soffice`) is
//! preferred because it is the only one with full fidelity. Without it the
//! pure-Rust `lo_writer`/`lo_impress` importers are used; they are faithful to
//! the text but drop embedded images and ignore page geometry, so that
//! degradation is reported to the user rather than passed off as the document.

use std::{
    ffi::{OsStr, OsString},
    fmt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, anyhow, bail};
use tempfile::TempDir;

/// Backend selection override, mainly so both paths are exercisable on one
/// machine. Accepts `soffice`, `pure`, or `auto`; anything else falls back to
/// `auto` with a warning.
const BACKEND_ENV: &str = "VVRD_OFFICE_BACKEND";

/// Upper bound on a single `soffice` conversion. LibreOffice occasionally wedges
/// on a malformed document, and a reader that never reaches its first frame is
/// indistinguishable from a hang.
const SOFFICE_TIMEOUT: Duration = Duration::from_secs(60);
const SOFFICE_POLL: Duration = Duration::from_millis(25);

/// Guard against loading an entire disk image because it happened to be named
/// `.docx`. Real office documents are far below this.
const MAX_OFFICE_BYTES: u64 = 512 * 1024 * 1024;

pub const PURE_BACKEND_NOTICE: &str = "approximate pure-Rust conversion: embedded images omitted; install LibreOffice for full fidelity";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfficeFormat {
    Docx,
    Pptx,
    Odt,
    Odp,
}

impl OfficeFormat {
    /// Office format implied by the file extension, or `None` for anything
    /// MuPDF should open directly.
    pub fn from_path(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        match extension.as_str() {
            "docx" => Some(Self::Docx),
            "pptx" => Some(Self::Pptx),
            "odt" => Some(Self::Odt),
            "odp" => Some(Self::Odp),
            _ => None,
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Docx => "docx",
            Self::Pptx => "pptx",
            Self::Odt => "odt",
            Self::Odp => "odp",
        }
    }
}

impl fmt::Display for OfficeFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.hint())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Soffice,
    Pure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendPreference {
    Auto,
    Soffice,
    Pure,
}

impl BackendPreference {
    fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            None | Some("") | Some("auto") => Self::Auto,
            Some("soffice") | Some("libreoffice") => Self::Soffice,
            Some("pure") => Self::Pure,
            Some(other) => {
                log::warn!("ignoring unknown {BACKEND_ENV}={other:?}; using auto");
                Self::Auto
            }
        }
    }

    fn from_env() -> Self {
        Self::parse(std::env::var(BACKEND_ENV).ok().as_deref())
    }
}

struct Conversion {
    backend: Backend,
    /// Deleting this removes the converted PDF. It must outlive every reader
    /// thread, so `DocumentInput` is held for the whole process lifetime.
    _temp: TempDir,
}

/// A resolved document: what the user asked for, and what MuPDF should open.
pub struct DocumentInput {
    origin: PathBuf,
    render_path: PathBuf,
    conversion: Option<Conversion>,
}

impl DocumentInput {
    /// The path the user named. Identity for saved state, the source title,
    /// capture policy, and export filenames — never a temporary path.
    pub fn origin(&self) -> &Path {
        &self.origin
    }

    /// The path MuPDF opens. Equal to [`Self::origin`] unless the document was
    /// converted.
    pub fn render_path(&self) -> &Path {
        &self.render_path
    }

    /// The backend that produced [`Self::render_path`], or `None` when the
    /// document was opened natively.
    pub fn backend(&self) -> Option<Backend> {
        self.conversion
            .as_ref()
            .map(|conversion| conversion.backend)
    }
}

/// Resolve `path` into a document MuPDF can open, converting office formats.
pub fn resolve(path: &Path) -> anyhow::Result<DocumentInput> {
    let Some(format) = OfficeFormat::from_path(path) else {
        return Ok(DocumentInput {
            origin: path.to_path_buf(),
            render_path: path.to_path_buf(),
            conversion: None,
        });
    };

    let bytes = read_office_bytes(path)?;
    verify_container(&bytes, format)
        .with_context(|| format!("{} is not a usable {format} document", path.display()))?;

    let temp = tempfile::Builder::new()
        .prefix("vvrd-")
        .tempdir()
        .context("cannot create a temporary directory for document conversion")?;

    let (backend, pdf_path) = convert(path, &bytes, format, temp.path())?;
    Ok(DocumentInput {
        origin: path.to_path_buf(),
        render_path: pdf_path,
        conversion: Some(Conversion {
            backend,
            _temp: temp,
        }),
    })
}

fn read_office_bytes(path: &Path) -> anyhow::Result<Vec<u8>> {
    let length = std::fs::metadata(path)
        .with_context(|| format!("cannot read {}", path.display()))?
        .len();
    if length > MAX_OFFICE_BYTES {
        bail!(
            "{} is {length} bytes, larger than the {MAX_OFFICE_BYTES} byte conversion limit",
            path.display()
        );
    }
    std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))
}

/// Confirm the bytes really are the container the extension claims, so a
/// mislabelled file fails with a clear message instead of an XML parse error
/// from deep inside an importer.
fn verify_container(bytes: &[u8], format: OfficeFormat) -> anyhow::Result<()> {
    if !bytes.starts_with(b"PK\x03\x04") {
        bail!("not a ZIP container");
    }
    let archive = lo_zip::ZipArchive::new(bytes).map_err(|error| anyhow!("{error}"))?;
    let actual = sniff_container(&archive).ok_or_else(|| anyhow!("unrecognised ZIP container"))?;
    if actual != format {
        bail!("contents are {actual}, not {format}");
    }
    Ok(())
}

fn sniff_container(archive: &lo_zip::ZipArchive) -> Option<OfficeFormat> {
    if archive.contains("[Content_Types].xml") {
        let types = archive.read_string("[Content_Types].xml").ok()?;
        let types = types.to_ascii_lowercase();
        if types.contains("wordprocessingml") {
            return Some(OfficeFormat::Docx);
        }
        if types.contains("presentationml") {
            return Some(OfficeFormat::Pptx);
        }
        return None;
    }
    if archive.contains("mimetype") {
        let mimetype = archive.read_string("mimetype").ok()?.to_ascii_lowercase();
        if mimetype.contains("opendocument.text") {
            return Some(OfficeFormat::Odt);
        }
        if mimetype.contains("opendocument.presentation") {
            return Some(OfficeFormat::Odp);
        }
    }
    None
}

/// Run the selected backend, falling back from `soffice` to the pure-Rust path
/// so a broken LibreOffice install degrades instead of blocking the reader.
fn convert(
    source: &Path,
    bytes: &[u8],
    format: OfficeFormat,
    dir: &Path,
) -> anyhow::Result<(Backend, PathBuf)> {
    let preference = BackendPreference::from_env();
    let soffice = match preference {
        BackendPreference::Pure => None,
        BackendPreference::Auto | BackendPreference::Soffice => find_soffice(),
    };

    if let Some(program) = soffice {
        eprintln!(
            "vvrd: converting {} with LibreOffice...",
            display_name(source)
        );
        match convert_with_soffice(&program, source, dir) {
            Ok(pdf) => return Ok((Backend::Soffice, pdf)),
            Err(error) if preference == BackendPreference::Soffice => {
                return Err(error.context("LibreOffice conversion failed"));
            }
            Err(error) => {
                log::warn!("LibreOffice conversion failed, falling back to pure Rust: {error:#}");
                eprintln!("vvrd: LibreOffice conversion failed, using the pure-Rust converter");
            }
        }
    } else if preference == BackendPreference::Soffice {
        bail!("{BACKEND_ENV}=soffice was requested but no LibreOffice binary is on PATH");
    }

    let pdf = convert_with_pure(bytes, format, dir)?;
    Ok((Backend::Pure, pdf))
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn find_soffice() -> Option<PathBuf> {
    const CANDIDATES: &[&str] = &["soffice", "libreoffice"];
    for candidate in CANDIDATES {
        if let Some(program) = which(candidate) {
            return Some(program);
        }
    }
    #[cfg(target_os = "macos")]
    {
        let bundled = Path::new("/Applications/LibreOffice.app/Contents/MacOS/soffice");
        if bundled.is_file() {
            return Some(bundled.to_path_buf());
        }
    }
    None
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

/// Arguments for a headless single-document PDF conversion into `dir`.
///
/// The `-env:UserInstallation` override is not optional: without a private
/// profile, `soffice` silently no-ops or blocks whenever the user already has
/// LibreOffice running against the default profile.
fn soffice_args(source: &Path, dir: &Path) -> Vec<OsString> {
    let mut profile = OsString::from("-env:UserInstallation=file://");
    profile.push(dir.join("profile"));
    vec![
        OsString::from("--headless"),
        OsString::from("--norestore"),
        OsString::from("--invisible"),
        profile,
        OsString::from("--convert-to"),
        OsString::from("pdf"),
        OsString::from("--outdir"),
        OsString::from(dir),
        OsString::from(source),
    ]
}

fn convert_with_soffice(program: &Path, source: &Path, dir: &Path) -> anyhow::Result<PathBuf> {
    // The child inherits this process's environment; strip the Vivid token so a
    // credential never reaches an unrelated program. Nothing secret is ever
    // passed as an argument.
    let mut child = Command::new(program)
        .args(soffice_args(source, dir))
        .env_remove("VIVID_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("cannot start {}", program.display()))?;

    let deadline = Instant::now() + SOFFICE_TIMEOUT;
    let status = loop {
        match child.try_wait().context("cannot wait for LibreOffice")? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "LibreOffice did not finish within {} seconds",
                    SOFFICE_TIMEOUT.as_secs()
                );
            }
            None => thread::sleep(SOFFICE_POLL),
        }
    };
    if !status.success() {
        bail!("LibreOffice exited with {status}");
    }
    converted_pdf_in_dir(dir)
}

/// Find the single PDF `soffice` wrote. Scanning beats deriving the name from
/// the input stem because LibreOffice rewrites some characters.
fn converted_pdf_in_dir(dir: &Path) -> anyhow::Result<PathBuf> {
    let mut found: Option<PathBuf> = None;
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("cannot list {}", dir.display()))?;
    for entry in entries {
        let path = entry
            .with_context(|| format!("cannot list {}", dir.display()))?
            .path();
        let is_pdf = path
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"));
        if !is_pdf {
            continue;
        }
        if found.is_some() {
            bail!("conversion produced more than one PDF");
        }
        found = Some(path);
    }
    found.ok_or_else(|| anyhow!("conversion produced no PDF"))
}

fn convert_with_pure(bytes: &[u8], format: OfficeFormat, dir: &Path) -> anyhow::Result<PathBuf> {
    let hint = format.hint();
    let pdf = match format {
        OfficeFormat::Docx | OfficeFormat::Odt => lo_writer::load_bytes("", bytes, hint)
            .and_then(|document| lo_writer::save_as(&document, "pdf")),
        OfficeFormat::Pptx | OfficeFormat::Odp => lo_impress::load_bytes("", bytes, hint)
            .and_then(|deck| lo_impress::save_as(&deck, "pdf")),
    }
    .map_err(|error| anyhow!("pure-Rust {format} conversion failed: {error}"))?;

    let path = dir.join("document.pdf");
    std::fs::write(&path, pdf)
        .with_context(|| format!("cannot write the converted PDF to {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn office_extensions_map_to_formats() {
        for (name, expected) in [
            ("a.docx", OfficeFormat::Docx),
            ("a.DOCX", OfficeFormat::Docx),
            ("a.pptx", OfficeFormat::Pptx),
            ("a.Odt", OfficeFormat::Odt),
            ("a.odp", OfficeFormat::Odp),
        ] {
            assert_eq!(OfficeFormat::from_path(Path::new(name)), Some(expected));
        }
    }

    #[test]
    fn native_extensions_are_never_converted() {
        for name in ["a.pdf", "a.epub", "a.PDF", "a.txt", "a.doc", "a.xlsx", "a"] {
            assert_eq!(OfficeFormat::from_path(Path::new(name)), None);
        }
    }

    #[test]
    fn resolve_passes_native_documents_through_untouched() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("book.pdf");
        std::fs::write(&path, b"%PDF-1.7\n").expect("write");
        let input = resolve(&path).expect("resolve");
        assert_eq!(input.origin(), path);
        assert_eq!(input.render_path(), path);
        assert_eq!(input.backend(), None);
    }

    #[test]
    fn non_zip_office_input_is_rejected_clearly() {
        let error = verify_container(b"not a zip at all", OfficeFormat::Docx)
            .expect_err("must reject non-ZIP bytes");
        assert!(error.to_string().contains("ZIP"), "{error}");
    }

    #[test]
    fn empty_office_input_is_rejected_without_panicking() {
        assert!(verify_container(b"", OfficeFormat::Pptx).is_err());
        assert!(verify_container(b"PK\x03\x04", OfficeFormat::Pptx).is_err());
    }

    #[test]
    fn backend_preference_parses_every_accepted_value() {
        assert_eq!(BackendPreference::parse(None), BackendPreference::Auto);
        assert_eq!(
            BackendPreference::parse(Some("auto")),
            BackendPreference::Auto
        );
        assert_eq!(
            BackendPreference::parse(Some(" soffice ")),
            BackendPreference::Soffice
        );
        assert_eq!(
            BackendPreference::parse(Some("pure")),
            BackendPreference::Pure
        );
        assert_eq!(
            BackendPreference::parse(Some("nonsense")),
            BackendPreference::Auto
        );
    }

    #[test]
    fn soffice_args_use_a_private_profile_and_order_the_input_last() {
        let args = soffice_args(Path::new("/docs/report.docx"), Path::new("/tmp/vvrd-x"));
        let rendered: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            rendered
                .iter()
                .any(|arg| arg == "-env:UserInstallation=file:///tmp/vvrd-x/profile"),
            "{rendered:?}"
        );
        let outdir = rendered.iter().position(|arg| arg == "--outdir").unwrap();
        assert_eq!(rendered[outdir + 1], "/tmp/vvrd-x");
        assert_eq!(rendered.last().unwrap(), "/docs/report.docx");
        assert!(rendered.iter().any(|arg| arg == "--headless"));
    }

    #[test]
    fn converted_pdf_lookup_requires_exactly_one_pdf() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(converted_pdf_in_dir(dir.path()).is_err());

        std::fs::write(dir.path().join("out.pdf"), b"%PDF-1.7\n").expect("write");
        std::fs::create_dir(dir.path().join("profile")).expect("mkdir");
        assert_eq!(
            converted_pdf_in_dir(dir.path()).expect("one pdf"),
            dir.path().join("out.pdf")
        );

        std::fs::write(dir.path().join("other.pdf"), b"%PDF-1.7\n").expect("write");
        assert!(converted_pdf_in_dir(dir.path()).is_err());
    }

    #[test]
    fn dropping_the_input_removes_the_converted_pdf() {
        let temp = tempfile::Builder::new()
            .prefix("vvrd-")
            .tempdir()
            .expect("temp dir");
        let directory = temp.path().to_path_buf();
        let render_path = directory.join("document.pdf");
        std::fs::write(&render_path, b"%PDF-1.7\n").expect("write");

        let input = DocumentInput {
            origin: PathBuf::from("/docs/report.docx"),
            render_path: render_path.clone(),
            conversion: Some(Conversion {
                backend: Backend::Pure,
                _temp: temp,
            }),
        };
        assert_eq!(input.backend(), Some(Backend::Pure));
        assert!(render_path.exists());

        drop(input);
        assert!(!directory.exists(), "the temporary directory must be gone");
    }

    /// The pure backend must produce a PDF MuPDF can page through, for every
    /// format the reader accepts. This is the only end-to-end guarantee that the
    /// converted document behaves like an ordinary fixed-layout PDF.
    #[test]
    fn pure_backend_produces_a_pdf_mupdf_can_open() {
        for (fixture, format) in [
            (
                &include_bytes!("../tests/fixtures/sample.docx")[..],
                OfficeFormat::Docx,
            ),
            (
                &include_bytes!("../tests/fixtures/sample.pptx")[..],
                OfficeFormat::Pptx,
            ),
            (
                &include_bytes!("../tests/fixtures/sample.odt")[..],
                OfficeFormat::Odt,
            ),
            (
                &include_bytes!("../tests/fixtures/sample.odp")[..],
                OfficeFormat::Odp,
            ),
        ] {
            verify_container(fixture, format).unwrap_or_else(|error| {
                panic!("{format} fixture must sniff as {format}: {error}");
            });

            let dir = tempfile::tempdir().expect("temp dir");
            let pdf_path = convert_with_pure(fixture, format, dir.path())
                .unwrap_or_else(|error| panic!("{format} conversion: {error:#}"));
            let pdf = std::fs::read(&pdf_path).expect("read pdf");
            assert!(pdf.starts_with(b"%PDF"), "{format} output is not a PDF");

            let document = crate::mupdf::Document::from_bytes(&pdf, "application/pdf")
                .unwrap_or_else(|error| panic!("{format} PDF must open in MuPDF: {error}"));
            assert!(
                document.page_count().expect("page count") >= 1,
                "{format} PDF must have at least one page"
            );
            assert!(
                !document.is_reflowable().unwrap_or(false),
                "{format} PDF must be fixed layout so zoom stays enabled"
            );
        }
    }
}
