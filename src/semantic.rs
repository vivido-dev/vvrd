#[cfg(not(unix))]
use std::io;

use serde::Serialize;

use crate::renderer::{LinkInfo, TocEntry};

#[derive(Debug, Clone, Default, Serialize)]
pub struct SemanticSnapshot {
    pub revision: u64,
    pub document_revision: u64,
    pub title: String,
    pub page: usize,
    pub search_term: Option<String>,
    pub text: String,
    pub links: Vec<LinkInfo>,
    pub outline: Vec<TocEntry>,
}

#[cfg(unix)]
mod platform {
    use std::{
        fs,
        io::{self, Read, Write},
        os::unix::{fs::MetadataExt, fs::PermissionsExt, net::UnixListener},
        path::{Path, PathBuf},
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, JoinHandle},
        time::{Duration, SystemTime},
    };

    use serde_json::{Value, json};

    use super::SemanticSnapshot;

    const MAX_REQUEST_BYTES: usize = 4096;
    const MAX_TEXT_BYTES: usize = 1024 * 1024;

    pub struct SemanticControl {
        locator: String,
        path: PathBuf,
        state: Arc<RwLock<SemanticSnapshot>>,
        shutdown: Arc<AtomicBool>,
        join: Option<JoinHandle<()>>,
    }

    impl SemanticControl {
        pub fn start(title: String, page: usize) -> io::Result<Self> {
            let directory = secure_runtime_directory()?;
            let nonce = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            // Unix-domain socket paths are short on some platforms (104 bytes on macOS).
            // Keep the basename compact because the system temporary directory may be long.
            let path = directory.join(format!("s-{}-{nonce:016x}", std::process::id()));
            let listener = UnixListener::bind(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;
            let state = Arc::new(RwLock::new(SemanticSnapshot {
                title,
                page,
                revision: 1,
                document_revision: 1,
                ..SemanticSnapshot::default()
            }));
            let shutdown = Arc::new(AtomicBool::new(false));
            let thread_state = state.clone();
            let thread_shutdown = shutdown.clone();
            let join = thread::Builder::new()
                .name("vvrd-semantic-control".into())
                .spawn(move || serve(listener, thread_state, thread_shutdown))?;
            Ok(Self {
                locator: format!("vvrd+unix://{}", path.display()),
                path,
                state,
                shutdown,
                join: Some(join),
            })
        }

        pub fn locator(&self) -> &str {
            &self.locator
        }

        pub fn update(&self, update: impl FnOnce(&mut SemanticSnapshot)) {
            let mut state = self
                .state
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            update(&mut state);
            if state.text.len() > MAX_TEXT_BYTES {
                state.text.truncate(MAX_TEXT_BYTES);
            }
        }
    }

    impl Drop for SemanticControl {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Release);
            let _ = std::os::unix::net::UnixStream::connect(&self.path);
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
            let _ = fs::remove_file(&self.path);
        }
    }

    fn serve(
        listener: UnixListener,
        state: Arc<RwLock<SemanticSnapshot>>,
        shutdown: Arc<AtomicBool>,
    ) {
        while !shutdown.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut request = Vec::new();
                    let _ = Read::by_ref(&mut stream)
                        .take((MAX_REQUEST_BYTES + 1) as u64)
                        .read_to_end(&mut request);
                    let response = if request.len() > MAX_REQUEST_BYTES {
                        json!({"error": "request_too_large"})
                    } else {
                        respond(&request, &state)
                    };
                    let _ = serde_json::to_writer(&mut stream, &response);
                    let _ = stream.write_all(b"\n");
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    }

    fn respond(request: &[u8], state: &RwLock<SemanticSnapshot>) -> Value {
        let Ok(request) = serde_json::from_slice::<Value>(request) else {
            return json!({"error": "bad_request"});
        };
        let Some(method) = request.get("method").and_then(Value::as_str) else {
            return json!({"error": "missing_method"});
        };
        let snapshot = state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match method {
            "describe" => serde_json::to_value(&*snapshot)
                .unwrap_or_else(|_| json!({"error": "serialization_failed"})),
            "text" => json!({
                "revision": snapshot.revision,
                "page": snapshot.page,
                "text": snapshot.text,
            }),
            "links" => json!({
                "revision": snapshot.revision,
                "page": snapshot.page,
                "links": snapshot.links,
            }),
            "outline" => json!({
                "revision": snapshot.revision,
                "outline": snapshot.outline,
            }),
            _ => json!({"error": "unknown_method"}),
        }
    }

    fn secure_runtime_directory() -> io::Result<PathBuf> {
        let uid = unsafe { libc::geteuid() };
        if let Some(path) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)
            && secure_directory(&path, uid)
        {
            return Ok(path);
        }
        let path = std::env::temp_dir().join(format!("vvrd-{uid}"));
        match fs::create_dir(&path) {
            Ok(()) => fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        if !secure_directory(&path, uid) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Vvrd semantic runtime directory is not owner-only",
            ));
        }
        Ok(path)
    }

    fn secure_directory(path: &Path, uid: u32) -> bool {
        let Ok(metadata) = fs::symlink_metadata(path) else {
            return false;
        };
        metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0
    }

    #[cfg(test)]
    mod tests {
        use std::os::unix::net::UnixStream;

        use super::*;

        #[test]
        fn semantic_control_is_owner_only_and_reports_bounded_state() {
            let control = SemanticControl::start("guide.pdf".into(), 2).unwrap();
            assert_eq!(
                fs::metadata(&control.path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            control.update(|state| {
                state.revision = 3;
                state.text = "page text".into();
            });
            let mut stream = UnixStream::connect(&control.path).unwrap();
            stream.write_all(br#"{"method":"text"}"#).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            let response: Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response["revision"], 3);
            assert_eq!(response["page"], 2);
            assert_eq!(response["text"], "page text");
        }
    }
}

#[cfg(unix)]
pub use platform::SemanticControl;

#[cfg(not(unix))]
pub struct SemanticControl {
    state: std::sync::RwLock<SemanticSnapshot>,
}

#[cfg(not(unix))]
impl SemanticControl {
    pub fn start(title: String, page: usize) -> io::Result<Self> {
        Ok(Self {
            state: std::sync::RwLock::new(SemanticSnapshot {
                title,
                page,
                revision: 1,
                document_revision: 1,
                ..SemanticSnapshot::default()
            }),
        })
    }

    pub fn locator(&self) -> &str {
        ""
    }

    pub fn update(&self, update: impl FnOnce(&mut SemanticSnapshot)) {
        update(
            &mut self
                .state
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
    }
}
