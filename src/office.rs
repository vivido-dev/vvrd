//! LibreOffice-backed conversion of office documents (PPTX, DOCX, ODP, ODT) into cached PDFs
//! that the MuPDF backend renders like any other fixed-layout document.

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use md5::{Digest, Md5};

use crate::error::RenderError;

/// Hard ceiling for one LibreOffice conversion, including first-run profile creation.
const CONVERT_TIMEOUT: Duration = Duration::from_secs(120);
/// Longest soffice stderr excerpt embedded in an error message.
const MAX_STDERR_EXCERPT_BYTES: usize = 2048;
/// Poll interval while waiting for the conversion subprocess.
const WAIT_POLL: Duration = Duration::from_millis(50);

/// Locates the LibreOffice launcher: `VVRD_SOFFICE` when set (authoritatively — a broken
/// override means "unavailable"), then `soffice` on `PATH`, then platform defaults.
pub fn find_soffice() -> Option<PathBuf> {
    if let Some(candidate) = std::env::var_os("VVRD_SOFFICE") {
        let candidate = PathBuf::from(candidate);
        return candidate.is_file().then_some(candidate);
    }
    find_on_path("soffice").or_else(platform_soffice)
}

fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        #[cfg(windows)]
        {
            [dir.join(format!("{program}.exe")), dir.join(program)]
                .into_iter()
                .find(|candidate| candidate.is_file())
        }
        #[cfg(not(windows))]
        {
            let candidate = dir.join(program);
            candidate.is_file().then_some(candidate)
        }
    })
}

#[cfg(windows)]
fn platform_soffice() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(root) = std::env::var_os("ProgramFiles") {
        candidates.push(PathBuf::from(root).join(r"LibreOffice\program\soffice.exe"));
    }
    if let Some(root) = std::env::var_os("ProgramFiles(x86)") {
        candidates.push(PathBuf::from(root).join(r"LibreOffice\program\soffice.exe"));
    }
    if let Some(root) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(PathBuf::from(root).join(r"Programs\LibreOffice\program\soffice.exe"));
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}

#[cfg(target_os = "macos")]
fn platform_soffice() -> Option<PathBuf> {
    PathBuf::from("/Applications/LibreOffice.app/Contents/MacOS/soffice")
        .is_file()
        .then(|| PathBuf::from("/Applications/LibreOffice.app/Contents/MacOS/soffice"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_soffice() -> Option<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/usr/bin/soffice"),
        PathBuf::from("/usr/local/bin/soffice"),
    ];
    // Distribution installs under /opt (for example /opt/libreoffice*/program/soffice).
    if let Ok(entries) = std::fs::read_dir("/opt") {
        let mut extra: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path().join("program").join("soffice"))
            .filter(|candidate| candidate.is_file())
            .collect();
        extra.sort();
        candidates.extend(extra);
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}

#[cfg(not(any(unix, windows)))]
fn platform_soffice() -> Option<PathBuf> {
    None
}

/// Directory holding converted PDFs (`<content-hash>.pdf`), override with `VVRD_OFFICE_CACHE`.
fn office_cache_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("VVRD_OFFICE_CACHE") {
        return Some(PathBuf::from(dir));
    }
    directories::ProjectDirs::from("", "", "vvrd").map(|dirs| dirs.cache_dir().join("office"))
}

/// Converts `source` to PDF once per distinct content, returning the cached PDF path.
pub fn ensure_pdf(source: &Path) -> Result<PathBuf, RenderError> {
    let soffice = find_soffice().ok_or_else(|| {
        RenderError::Converting(
            "LibreOffice (soffice) not found; PPTX/DOCX/ODP/ODT viewing requires LibreOffice"
                .to_owned(),
        )
    })?;
    let cache_dir = office_cache_dir().ok_or_else(|| {
        RenderError::Converting("cannot determine the office conversion cache directory".to_owned())
    })?;
    convert_with(&soffice, &cache_dir, source, CONVERT_TIMEOUT)
}

/// Runs `soffice --headless --convert-to pdf` into a staging directory, then atomically renames
/// the result to `<md5-of-source>.pdf`. A per-conversion LibreOffice profile under the staging
/// directory keeps vvrd isolated from the interactive profile and immune to the "another
/// instance owns this profile" failure when several vvrd processes convert at once.
fn convert_with(
    soffice: &Path,
    cache_dir: &Path,
    source: &Path,
    timeout: Duration,
) -> Result<PathBuf, RenderError> {
    let digest = source_digest(source)?;
    let target = cache_dir.join(format!("{digest}.pdf"));
    if target.is_file() {
        return Ok(target);
    }
    create_dir_all(cache_dir)?;
    let staging = cache_dir.join(format!("tmp-{}-{digest}", std::process::id()));
    create_dir_all(&staging)?;
    if let Err(error) = run_soffice(soffice, source, &staging, timeout) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    // soffice names the export after the input stem inside --outdir.
    let stem = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| {
            RenderError::Converting(format!(
                "document path has no readable file name: {}",
                source.display()
            ))
        })?;
    let produced = staging.join(format!("{stem}.pdf"));
    if !produced.is_file() {
        let excerpt = stderr_excerpt(&staging.join("soffice-stderr.log"));
        let _ = std::fs::remove_dir_all(&staging);
        return Err(RenderError::Converting(format!(
            "LibreOffice conversion of {} produced no PDF{}",
            source.display(),
            excerpt
        )));
    }
    let renamed = std::fs::rename(&produced, &target);
    let _ = std::fs::remove_dir_all(&staging);
    match renamed {
        Ok(()) => Ok(target),
        // A concurrent vvrd instance may have installed the same content first; that is a hit.
        Err(_) if target.is_file() => Ok(target),
        Err(error) => Err(RenderError::Converting(format!(
            "cannot publish converted PDF for {}: {error}",
            source.display()
        ))),
    }
}

fn create_dir_all(dir: &Path) -> Result<(), RenderError> {
    std::fs::create_dir_all(dir).map_err(|error| {
        RenderError::Converting(format!("cannot create {}: {error}", dir.display()))
    })
}

fn run_soffice(
    soffice: &Path,
    source: &Path,
    staging: &Path,
    timeout: Duration,
) -> Result<(), RenderError> {
    let profile = staging.join("lo-profile");
    create_dir_all(&profile)?;
    let stderr_log = staging.join("soffice-stderr.log");
    let stderr = std::fs::File::create(&stderr_log).map_err(|error| {
        RenderError::Converting(format!("cannot create {}: {error}", stderr_log.display()))
    })?;
    let mut child = Command::new(soffice)
        .arg("--headless")
        .arg("--norestore")
        .arg("--nolockcheck")
        .arg("--nodefault")
        .arg(format!("-env:UserInstallation={}", file_url(&profile)))
        .arg("--convert-to")
        .arg("pdf")
        .arg("--outdir")
        .arg(staging)
        .arg(source)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|error| {
            RenderError::Converting(format!(
                "cannot start {} for {}: {error}",
                soffice.display(),
                source.display()
            ))
        })?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(RenderError::Converting(format!(
                        "LibreOffice conversion of {} timed out after {} seconds",
                        source.display(),
                        timeout.as_secs()
                    )));
                }
                thread::sleep(WAIT_POLL);
            }
            Err(error) => {
                return Err(RenderError::Converting(format!(
                    "cannot wait for LibreOffice converting {}: {error}",
                    source.display()
                )));
            }
        }
    };
    if !status.success() {
        return Err(RenderError::Converting(format!(
            "LibreOffice conversion of {} failed with {status}{}",
            source.display(),
            stderr_excerpt(&stderr_log)
        )));
    }
    Ok(())
}

fn source_digest(source: &Path) -> Result<String, RenderError> {
    let bytes = std::fs::read(source).map_err(|error| {
        RenderError::Converting(format!("cannot read {}: {error}", source.display()))
    })?;
    let mut digest = String::with_capacity(32);
    for byte in Md5::digest(&bytes) {
        write!(&mut digest, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(digest)
}

fn stderr_excerpt(log: &Path) -> String {
    let Ok(bytes) = std::fs::read(log) else {
        return String::new();
    };
    let start = bytes.len().saturating_sub(MAX_STDERR_EXCERPT_BYTES);
    let mut excerpt = String::from_utf8_lossy(&bytes[start..]).into_owned();
    excerpt = excerpt.replace(['\n', '\r'], " ");
    format!(": {excerpt}")
}

/// Percent-encodes a path into the `file://` URL LibreOffice expects for
/// `-env:UserInstallation`, so cache paths containing spaces still work.
fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    let text = path.to_string_lossy().replace('\\', "/");
    // Windows drive letters need the third slash: file:///C:/Users/....
    if !text.starts_with('/') {
        url.push('/');
    }
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                url.push(*byte as char);
            }
            _ => write!(&mut url, "%{byte:02X}").expect("writing to String cannot fail"),
        }
    }
    url
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::Mutex,
        sync::atomic::{AtomicU64, Ordering as AtomicOrdering},
    };

    use super::*;

    /// Serializes the one test that mutates process-global environment variables.
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    static TEMP_ID: AtomicU64 = AtomicU64::new(1);

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "vvrd-office-test-{}-{}-{name}",
            std::process::id(),
            TEMP_ID.fetch_add(1, AtomicOrdering::Relaxed)
        ))
    }

    #[test]
    fn file_url_percent_encodes_unreserved_bytes() {
        assert_eq!(
            file_url(Path::new("/tmp/a b/c-d_office")),
            "file:///tmp/a%20b/c-d_office"
        );
    }

    #[test]
    fn missing_soffice_binary_is_reported() {
        let dir = temp_dir("missing");
        fs::create_dir_all(&dir).unwrap();
        let cache = dir.join("cache");
        let source = dir.join("deck.pptx");
        fs::write(&source, b"fake deck").unwrap();
        let error = convert_with(
            &dir.join("no-such-soffice"),
            &cache,
            &source,
            CONVERT_TIMEOUT,
        )
        .unwrap_err();
        assert!(error.to_string().contains("cannot start"));
        fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    fn write_fake_soffice(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let script = dir.join("fake-soffice");
        fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[cfg(unix)]
    #[test]
    fn conversion_runs_soffice_once_and_caches_by_content() {
        let dir = temp_dir("convert");
        let cache = dir.join("cache");
        fs::create_dir_all(&cache).unwrap();
        let marker = dir.join("marker.pdf");
        fs::write(&marker, b"%PDF-1.4 fake").unwrap();
        let log = dir.join("calls.log");
        let script = write_fake_soffice(
            &dir,
            &format!(
                "printf '%s\\n' \"$*\" >> {log:?}
prev=
outdir=
for arg in \"$@\"; do
  if [ \"$prev\" = \"--outdir\" ]; then outdir=\"$arg\"; fi
  prev=$arg
done
cp {marker:?} \"$outdir/$(basename \"${{prev%.*}}\").pdf\"",
                log = log,
                marker = marker,
            ),
        );
        let source = dir.join("deck.pptx");
        fs::write(&source, b"fake deck").unwrap();

        let pdf = convert_with(&script, &cache, &source, CONVERT_TIMEOUT).unwrap();
        assert_eq!(
            pdf,
            cache.join(format!("{}.pdf", source_digest(&source).unwrap()))
        );
        assert_eq!(fs::read(&pdf).unwrap(), b"%PDF-1.4 fake");
        let calls = fs::read_to_string(&log).unwrap();
        assert!(calls.contains("--headless"));
        assert!(calls.contains("--convert-to"));
        assert!(calls.contains("pdf"));
        assert!(calls.contains("UserInstallation=file://"));

        // Same content converts once; the second call is a cache hit that spawns nothing.
        let again = convert_with(&script, &cache, &source, CONVERT_TIMEOUT).unwrap();
        assert_eq!(again, pdf);
        assert_eq!(fs::read_to_string(&log).unwrap(), calls);

        // Different content gets its own cache entry.
        fs::write(&source, b"edited deck").unwrap();
        let changed = convert_with(&script, &cache, &source, CONVERT_TIMEOUT).unwrap();
        assert_ne!(changed, pdf);
        fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn conversion_kills_a_hung_soffice() {
        let dir = temp_dir("hung");
        let cache = dir.join("cache");
        fs::create_dir_all(&cache).unwrap();
        let script = write_fake_soffice(&dir, "sleep 30");
        let source = dir.join("deck.odt");
        fs::write(&source, b"fake doc").unwrap();

        let started = Instant::now();
        let error = convert_with(&script, &cache, &source, Duration::from_millis(200)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(10));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn env_overrides_control_soffice_lookup_and_cache_dir() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = temp_dir("env");
        fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("soffice");
        fs::write(&fake, b"").unwrap();
        let prior_soffice = std::env::var_os("VVRD_SOFFICE");
        let prior_cache = std::env::var_os("VVRD_OFFICE_CACHE");
        // SAFETY: ENV_LOCK serializes this test; the previous values are restored below.
        unsafe {
            std::env::set_var("VVRD_SOFFICE", &fake);
        }
        assert_eq!(find_soffice(), Some(fake.clone()));
        unsafe {
            std::env::set_var("VVRD_SOFFICE", dir.join("missing"));
        }
        assert_eq!(find_soffice(), None);
        let cache = dir.join("office");
        unsafe {
            std::env::set_var("VVRD_OFFICE_CACHE", &cache);
        }
        assert_eq!(office_cache_dir(), Some(cache));
        // SAFETY: restoring the values this test replaced.
        unsafe {
            match prior_soffice {
                Some(value) => std::env::set_var("VVRD_SOFFICE", value),
                None => std::env::remove_var("VVRD_SOFFICE"),
            }
            match prior_cache {
                Some(value) => std::env::set_var("VVRD_OFFICE_CACHE", value),
                None => std::env::remove_var("VVRD_OFFICE_CACHE"),
            }
        }
        fs::remove_dir_all(&dir).ok();
    }
}
