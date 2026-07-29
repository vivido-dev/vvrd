use std::{
    env,
    ffi::OsStr,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context as _, bail, ensure};
use command_group::{CommandGroup as _, GroupChild};
use tempfile::TempDir;
use url::Url;

pub const DEFAULT_OFFICE_TIMEOUT_SECS: u64 = 120;
pub const MAX_OFFICE_TIMEOUT_SECS: u64 = 3_600;

const MAX_OFFICE_INPUT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_CONVERTED_PDF_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 16 * 1024;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OfficeKind {
    Docx,
    Pptx,
}

impl OfficeKind {
    fn input_name(self) -> &'static str {
        match self {
            Self::Docx => "input.docx",
            Self::Pptx => "input.pptx",
        }
    }

    fn pdf_filter(self) -> &'static str {
        match self {
            Self::Docx => "pdf:writer_pdf_Export",
            Self::Pptx => "pdf:impress_pdf_Export",
        }
    }
}

#[derive(Debug, Clone)]
pub struct OfficeOptions {
    pub soffice: Option<PathBuf>,
    pub timeout: Duration,
}

pub struct PreparedDocument {
    original_path: PathBuf,
    render_path: PathBuf,
    _workspace: Option<TempDir>,
}

impl PreparedDocument {
    pub fn original_path(&self) -> &Path {
        &self.original_path
    }

    pub fn render_path(&self) -> &Path {
        &self.render_path
    }
}

pub fn prepare_document(path: &Path, options: &OfficeOptions) -> anyhow::Result<PreparedDocument> {
    let Some(kind) = office_kind(path)? else {
        return Ok(PreparedDocument {
            original_path: path.to_owned(),
            render_path: path.to_owned(),
            _workspace: None,
        });
    };
    ensure!(
        !options.timeout.is_zero(),
        "Office conversion timeout must be greater than zero"
    );
    ensure!(
        options.timeout <= Duration::from_secs(MAX_OFFICE_TIMEOUT_SECS),
        "Office conversion timeout may not exceed {MAX_OFFICE_TIMEOUT_SECS} seconds"
    );

    let metadata = fs::metadata(path)
        .with_context(|| format!("cannot inspect Office document {}", path.display()))?;
    ensure!(metadata.is_file(), "Office document is not a regular file");
    ensure!(
        metadata.len() <= MAX_OFFICE_INPUT_BYTES,
        "Office document is too large ({} bytes; maximum is {MAX_OFFICE_INPUT_BYTES})",
        metadata.len()
    );

    let soffice = find_soffice(options.soffice.as_deref())?;
    let workspace = tempfile::Builder::new()
        .prefix("vvrd-office-")
        .tempdir()
        .context("cannot create private Office conversion directory")?;
    let output_dir = workspace.path().join("output");
    let profile_dir = workspace.path().join("profile");
    fs::create_dir(&output_dir).context("cannot create Office conversion output directory")?;
    fs::create_dir(&profile_dir).context("cannot create isolated LibreOffice profile")?;

    let input_path = workspace.path().join(kind.input_name());
    stage_input(path, &input_path)?;

    let profile_url = Url::from_directory_path(&profile_dir)
        .map_err(|()| anyhow::anyhow!("cannot encode isolated LibreOffice profile path"))?;
    let output_path = output_dir.join("input.pdf");
    let command = build_soffice_command(
        &soffice,
        kind,
        &input_path,
        &output_dir,
        &profile_url,
        workspace.path(),
    );
    run_soffice(command, &output_path, options.timeout, workspace.path())?;
    validate_pdf(&output_path)?;
    set_owner_only_file(&output_path)?;

    Ok(PreparedDocument {
        original_path: path.to_owned(),
        render_path: output_path,
        _workspace: Some(workspace),
    })
}

fn stage_input(source_path: &Path, destination_path: &Path) -> anyhow::Result<()> {
    let source = File::open(source_path)
        .with_context(|| format!("cannot open Office document {}", source_path.display()))?;
    let mut source = source.take(MAX_OFFICE_INPUT_BYTES + 1);
    let mut destination =
        File::create(destination_path).context("cannot create staged Office document")?;
    let copied = io::copy(&mut source, &mut destination).context("cannot stage Office document")?;
    ensure!(
        copied <= MAX_OFFICE_INPUT_BYTES,
        "Office document changed while being staged and exceeded the size limit"
    );
    set_owner_only_file(destination_path)
}

fn office_kind(path: &Path) -> anyhow::Result<Option<OfficeKind>> {
    let Some(extension) = path.extension().and_then(OsStr::to_str) else {
        return Ok(None);
    };
    match extension.to_ascii_lowercase().as_str() {
        "docx" => Ok(Some(OfficeKind::Docx)),
        "pptx" => Ok(Some(OfficeKind::Pptx)),
        "docm" | "pptm" => {
            bail!("macro-enabled Office documents are not supported; use DOCX or PPTX")
        }
        _ => Ok(None),
    }
}

fn find_soffice(override_path: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(path) = override_path {
        return resolve_executable(path).with_context(|| {
            format!(
                "LibreOffice executable configured by --soffice was not found: {}",
                path.display()
            )
        });
    }

    for name in executable_names() {
        if let Some(path) = find_on_path(OsStr::new(name)) {
            return Ok(path);
        }
    }
    for path in platform_installation_paths() {
        if is_executable_file(&path) {
            return Ok(path);
        }
    }
    bail!(
        "DOCX/PPTX viewing requires LibreOffice; install it or pass --soffice PATH \
         (or set VVRD_SOFFICE)"
    )
}

fn resolve_executable(path: &Path) -> io::Result<PathBuf> {
    if path.components().count() > 1 || path.is_absolute() {
        return is_executable_file(path)
            .then(|| path.to_owned())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "executable does not exist"));
    }
    find_on_path(path.as_os_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "executable is not on PATH"))
}

#[cfg(windows)]
fn executable_names() -> &'static [&'static str] {
    &["soffice.com", "soffice.exe", "libreoffice.exe"]
}

#[cfg(not(windows))]
fn executable_names() -> &'static [&'static str] {
    &["soffice", "libreoffice"]
}

fn find_on_path(name: &OsStr) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    for directory in env::split_paths(&paths) {
        let candidate = directory.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        if candidate.extension().is_none() {
            for extension in windows_executable_extensions() {
                let mut with_extension = candidate.as_os_str().to_owned();
                with_extension.push(extension);
                let with_extension = PathBuf::from(with_extension);
                if is_executable_file(&with_extension) {
                    return Some(with_extension);
                }
            }
        }
    }
    None
}

#[cfg(windows)]
fn windows_executable_extensions() -> Vec<std::ffi::OsString> {
    env::var_os("PATHEXT")
        .map(|value| {
            value
                .to_string_lossy()
                .split(';')
                .filter(|value| !value.is_empty())
                .map(std::ffi::OsString::from)
                .collect()
        })
        .unwrap_or_else(|| {
            vec![
                std::ffi::OsString::from(".COM"),
                std::ffi::OsString::from(".EXE"),
            ]
        })
}

#[cfg(not(windows))]
fn platform_installation_paths() -> Vec<PathBuf> {
    let paths = vec![
        PathBuf::from("/usr/lib/libreoffice/program/soffice"),
        PathBuf::from("/opt/libreoffice/program/soffice"),
    ];
    #[cfg(target_os = "macos")]
    let paths = {
        let mut paths = paths;
        paths.insert(
            0,
            PathBuf::from("/Applications/LibreOffice.app/Contents/MacOS/soffice"),
        );
        paths
    };
    paths
}

#[cfg(windows)]
fn platform_installation_paths() -> Vec<PathBuf> {
    ["ProgramFiles", "ProgramFiles(x86)"]
        .into_iter()
        .filter_map(env::var_os)
        .flat_map(|root| {
            let program = PathBuf::from(root).join("LibreOffice").join("program");
            [program.join("soffice.com"), program.join("soffice.exe")]
        })
        .collect()
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn build_soffice_command(
    executable: &Path,
    kind: OfficeKind,
    input_path: &Path,
    output_dir: &Path,
    profile_url: &Url,
    working_directory: &Path,
) -> Command {
    let mut command = Command::new(executable);
    command
        .arg("--headless")
        .arg("--nologo")
        .arg("--nodefault")
        .arg("--nofirststartwizard")
        .arg("--norestore")
        .arg(format!("-env:UserInstallation={profile_url}"))
        .arg("--convert-to")
        .arg(kind.pdf_filter())
        .arg("--outdir")
        .arg(output_dir)
        .arg(input_path)
        .current_dir(working_directory)
        .env_remove("VIVID_TOKEN")
        .env_remove("VIVID_ENDPOINT")
        .env_remove("VIVID_ENDPOINT_BULK")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn run_soffice(
    mut command: Command,
    output_path: &Path,
    timeout: Duration,
    workspace: &Path,
) -> anyhow::Result<()> {
    ensure!(
        !timeout.is_zero(),
        "Office conversion timeout must be greater than zero"
    );
    let child = command
        .group_spawn()
        .context("cannot start LibreOffice converter")?;
    let mut child = ChildGroupGuard::new(child);
    let stdout = child.take_stdout();
    let stderr = child.take_stderr();
    let stdout_reader = stdout.map(spawn_diagnostic_reader);
    let stderr_reader = stderr.map(spawn_diagnostic_reader);
    let started = Instant::now();

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                child.kill_and_wait();
                let diagnostics = collect_diagnostics(stdout_reader, stderr_reader, workspace);
                return Err(anyhow::anyhow!(
                    "cannot monitor LibreOffice converter: {error}{diagnostics}"
                ));
            }
        }
        if output_path
            .metadata()
            .is_ok_and(|metadata| metadata.len() > MAX_CONVERTED_PDF_BYTES)
        {
            child.kill_and_wait();
            let _ = collect_diagnostics(stdout_reader, stderr_reader, workspace);
            bail!(
                "LibreOffice conversion exceeded the {MAX_CONVERTED_PDF_BYTES}-byte output limit"
            );
        }
        if started.elapsed() >= timeout {
            child.kill_and_wait();
            let diagnostics = collect_diagnostics(stdout_reader, stderr_reader, workspace);
            bail!(
                "LibreOffice conversion timed out after {} seconds{diagnostics}",
                timeout.as_secs()
            );
        }
        thread::sleep(PROCESS_POLL_INTERVAL);
    };
    child.disarm();
    let diagnostics = collect_diagnostics(stdout_reader, stderr_reader, workspace);
    if !status.success() {
        bail!("LibreOffice conversion failed with {status}{diagnostics}");
    }
    Ok(())
}

struct ChildGroupGuard {
    child: Option<GroupChild>,
}

impl ChildGroupGuard {
    fn new(child: GroupChild) -> Self {
        Self { child: Some(child) }
    }

    fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.as_mut()?.inner().stdout.take()
    }

    fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.child.as_mut()?.inner().stderr.take()
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child
            .as_mut()
            .expect("child group guard is armed")
            .try_wait()
    }

    fn kill_and_wait(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn disarm(&mut self) {
        self.child = None;
    }
}

impl Drop for ChildGroupGuard {
    fn drop(&mut self) {
        self.kill_and_wait();
    }
}

fn spawn_diagnostic_reader<R>(mut reader: R) -> JoinHandle<io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let stream_limit = MAX_DIAGNOSTIC_BYTES / 2;
        let mut retained = Vec::with_capacity(stream_limit);
        let mut chunk = [0_u8; 4096];
        loop {
            let count = reader.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            let available = stream_limit.saturating_sub(retained.len());
            retained.extend_from_slice(&chunk[..count.min(available)]);
        }
        Ok(retained)
    })
}

fn collect_diagnostics(
    stdout: Option<JoinHandle<io::Result<Vec<u8>>>>,
    stderr: Option<JoinHandle<io::Result<Vec<u8>>>>,
    workspace: &Path,
) -> String {
    let mut messages = Vec::new();
    let workspace = workspace.to_string_lossy();
    for handle in [stdout, stderr].into_iter().flatten() {
        if let Ok(Ok(bytes)) = handle.join() {
            let message = sanitize_diagnostic(&bytes, workspace.as_ref());
            if !message.is_empty() {
                messages.push(message);
            }
        }
    }
    if messages.is_empty() {
        String::new()
    } else {
        format!(": {}", messages.join("\n"))
    }
}

fn sanitize_diagnostic(bytes: &[u8], workspace: &str) -> String {
    let message = String::from_utf8_lossy(bytes)
        .replace(workspace, "<temporary directory>")
        .replace('\r', "\n");
    message
        .trim()
        .chars()
        .map(|character| match character {
            '\n' | '\t' => character,
            character if character.is_control() => '�',
            character => character,
        })
        .collect()
}

fn validate_pdf(path: &Path) -> anyhow::Result<()> {
    let metadata = fs::metadata(path).context("LibreOffice did not produce input.pdf")?;
    ensure!(
        metadata.len() > 5,
        "LibreOffice produced an empty or truncated PDF"
    );
    ensure!(
        metadata.len() <= MAX_CONVERTED_PDF_BYTES,
        "converted PDF is too large ({} bytes; maximum is {MAX_CONVERTED_PDF_BYTES})",
        metadata.len()
    );
    let mut header = [0_u8; 5];
    File::open(path)
        .context("cannot open converted PDF")?
        .read_exact(&mut header)
        .context("cannot read converted PDF header")?;
    ensure!(
        &header == b"%PDF-",
        "LibreOffice output is not a valid PDF stream"
    );
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_file(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("cannot secure temporary file {}", path.display()))
}

#[cfg(not(unix))]
fn set_owner_only_file(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsStr, fs::OpenOptions, io::Read as _};

    use super::*;
    use crate::{geometry::WindowSize, office_test_fixtures, renderer};

    #[test]
    fn office_extensions_are_case_insensitive_and_macro_formats_are_rejected() {
        assert_eq!(
            office_kind(Path::new("report.DoCx")).unwrap(),
            Some(OfficeKind::Docx)
        );
        assert_eq!(
            office_kind(Path::new("slides.PPTX")).unwrap(),
            Some(OfficeKind::Pptx)
        );
        assert_eq!(office_kind(Path::new("paper.pdf")).unwrap(), None);
        assert!(office_kind(Path::new("unsafe.docm")).is_err());
        assert!(office_kind(Path::new("unsafe.pptm")).is_err());
    }

    #[test]
    fn native_documents_pass_through_without_looking_for_libreoffice() {
        let path = Path::new("paper.pdf");
        let prepared = prepare_document(
            path,
            &OfficeOptions {
                soffice: Some(PathBuf::from("/definitely/missing/soffice")),
                timeout: Duration::from_secs(1),
            },
        )
        .unwrap();
        assert_eq!(prepared.original_path(), path);
        assert_eq!(prepared.render_path(), path);
    }

    #[test]
    fn command_uses_the_expected_filter_profile_and_secret_scrubbing() {
        let profile = Url::parse("file:///tmp/profile").unwrap();
        let command = build_soffice_command(
            Path::new("/usr/bin/soffice"),
            OfficeKind::Pptx,
            Path::new("/tmp/input.pptx"),
            Path::new("/tmp/output"),
            &profile,
            Path::new("/tmp"),
        );
        let args: Vec<_> = command.get_args().collect();
        assert!(args.contains(&OsStr::new("pdf:impress_pdf_Export")));
        assert!(args.contains(&OsStr::new("-env:UserInstallation=file:///tmp/profile")));
        assert_eq!(args.last().copied(), Some(OsStr::new("/tmp/input.pptx")));

        let removed: Vec<_> = command
            .get_envs()
            .filter_map(|(key, value)| value.is_none().then_some(key))
            .collect();
        assert!(removed.contains(&OsStr::new("VIVID_TOKEN")));
        assert!(removed.contains(&OsStr::new("VIVID_ENDPOINT")));
        assert!(removed.contains(&OsStr::new("VIVID_ENDPOINT_BULK")));
    }

    #[test]
    fn sparse_oversize_office_input_is_rejected_before_converter_discovery() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large.docx");
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_OFFICE_INPUT_BYTES + 1)
            .unwrap();
        let error = prepare_document(
            &path,
            &OfficeOptions {
                soffice: None,
                timeout: Duration::from_secs(1),
            },
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("too large"));
    }

    #[test]
    fn converted_output_must_be_a_bounded_pdf() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input.pdf");
        fs::write(&path, b"not a pdf").unwrap();
        assert!(validate_pdf(&path).is_err());
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_CONVERTED_PDF_BYTES + 1)
            .unwrap();
        assert!(validate_pdf(&path).is_err());
        fs::write(&path, b"%PDF-1.7\n").unwrap();
        validate_pdf(&path).unwrap();
    }

    #[test]
    fn converter_diagnostics_hide_the_workspace_and_control_sequences() {
        let message = sanitize_diagnostic(
            b"\x1b[31merror in /tmp/vvrd-office-secret/input.docx\rnext",
            "/tmp/vvrd-office-secret",
        );
        assert_eq!(
            message,
            "�[31merror in <temporary directory>/input.docx\nnext"
        );
    }

    #[test]
    fn embedded_image_ooxml_fixtures_are_well_formed_packages() {
        let directory = tempfile::tempdir().unwrap();
        for (extension, create, image_path) in [
            (
                "docx",
                office_test_fixtures::create_embedded_image_docx as fn(&Path) -> anyhow::Result<()>,
                "word/media/image1.png",
            ),
            (
                "pptx",
                office_test_fixtures::create_embedded_image_pptx,
                "ppt/media/image1.png",
            ),
        ] {
            let path = directory.path().join(format!("fixture.{extension}"));
            create(&path).unwrap();
            let mut archive = zip::ZipArchive::new(File::open(path).unwrap()).unwrap();
            assert!(archive.by_name("[Content_Types].xml").is_ok());
            let mut image = Vec::new();
            archive
                .by_name(image_path)
                .unwrap()
                .read_to_end(&mut image)
                .unwrap();
            let decoded = image::load_from_memory(&image).unwrap().into_rgb8();
            assert_eq!(decoded.dimensions(), (160, 80));
        }
    }

    #[cfg(unix)]
    #[test]
    fn converter_uses_private_staging_and_cleans_it_up() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let converter = directory.path().join("fake-soffice");
        fs::write(
            &converter,
            r#"#!/bin/sh
out=
previous=
for argument in "$@"; do
    if [ "$previous" = "--outdir" ]; then
        out=$argument
    fi
    previous=$argument
done
printf '%s\n' '%PDF-1.7' > "$out/input.pdf"
"#,
        )
        .unwrap();
        fs::set_permissions(&converter, fs::Permissions::from_mode(0o700)).unwrap();
        let input = directory.path().join("private report.docx");
        fs::write(&input, b"fixture").unwrap();

        let prepared = prepare_document(
            &input,
            &OfficeOptions {
                soffice: Some(converter),
                timeout: Duration::from_secs(2),
            },
        )
        .unwrap();
        assert_eq!(prepared.original_path(), input);
        assert_ne!(prepared.render_path(), input);
        assert_eq!(fs::read(prepared.render_path()).unwrap(), b"%PDF-1.7\n");
        let render_path = prepared.render_path().to_owned();
        drop(prepared);
        assert!(!render_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn converter_timeout_terminates_the_process_group() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let converter = directory.path().join("slow-soffice");
        fs::write(&converter, "#!/bin/sh\nsleep 30\n").unwrap();
        fs::set_permissions(&converter, fs::Permissions::from_mode(0o700)).unwrap();
        let input = directory.path().join("slow.docx");
        fs::write(&input, b"fixture").unwrap();

        let error = prepare_document(
            &input,
            &OfficeOptions {
                soffice: Some(converter),
                timeout: Duration::from_millis(100),
            },
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    #[ignore = "requires VVRD_TEST_SOFFICE to point to a real LibreOffice executable"]
    fn libreoffice_docx_preserves_embedded_image() {
        run_embedded_image_integration("docx", office_test_fixtures::create_embedded_image_docx);
    }

    #[test]
    #[ignore = "requires VVRD_TEST_SOFFICE to point to a real LibreOffice executable"]
    fn libreoffice_pptx_preserves_embedded_image() {
        run_embedded_image_integration("pptx", office_test_fixtures::create_embedded_image_pptx);
    }

    fn run_embedded_image_integration(extension: &str, create: fn(&Path) -> anyhow::Result<()>) {
        let soffice = env::var_os("VVRD_TEST_SOFFICE")
            .map(PathBuf::from)
            .expect("set VVRD_TEST_SOFFICE to a real LibreOffice executable");
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join(format!("embedded-image.{extension}"));
        create(&input).unwrap();
        let prepared = prepare_document(
            &input,
            &OfficeOptions {
                soffice: Some(soffice),
                timeout: Duration::from_secs(DEFAULT_OFFICE_TIMEOUT_SECS),
            },
        )
        .unwrap();
        let rendered = renderer::render_page(
            prepared.render_path(),
            0,
            WindowSize::from_cells(120, 41, 10, 20),
        )
        .unwrap();
        let image = rendered.page.into_rgb().unwrap();
        let red_pixels = image
            .pixels()
            .filter(|pixel| pixel[0] > 180 && pixel[1] < 100 && pixel[2] < 100)
            .count();
        let blue_pixels = image
            .pixels()
            .filter(|pixel| pixel[0] < 100 && pixel[1] < 120 && pixel[2] > 180)
            .count();
        assert!(
            red_pixels > 1_000 && blue_pixels > 1_000,
            "embedded image was not rendered (red={red_pixels}, blue={blue_pixels})"
        );
    }
}
