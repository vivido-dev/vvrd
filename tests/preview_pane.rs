//! `vvrd --preview` end to end: the real binary against the SDK's in-process presenter.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use vivid_protocol::cbor::Value;
use vivid_protocol::messages::{CREATE_NODE, DELETE_NODE, UPDATE_NODE};
use vivid_sdk::testing::{Observed, ROOT_SECRET_HEX, TestPresenter};

/// Generous: the first page waits on document preflight and a render worker.
const PAGE_TIMEOUT: Duration = Duration::from_secs(60);

fn temp_markdown() -> PathBuf {
    let path = std::env::temp_dir().join(format!("vvrd-preview-{}.md", std::process::id()));
    let mut text = String::from("# Preview\n\n");
    for line in 0..200 {
        text.push_str(&format!(
            "Paragraph {line} of a document long enough for two pages.\n\n"
        ));
    }
    std::fs::write(&path, text).unwrap();
    path
}

/// The (column, row, visible) a node record places its node at.
fn placement(request: &Observed) -> (Option<u64>, Option<u64>, Option<bool>) {
    let field = |key| {
        request
            .payload
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    };
    let geometry = match field(4) {
        Some(Value::Map(fields)) => fields.clone(),
        _ => Vec::new(),
    };
    let cell = |key| {
        geometry
            .iter()
            .find(|(k, _)| *k == key)
            .and_then(|(_, v)| v.as_u64())
            .map(|value| value >> 32)
    };
    let visible = request
        .payload
        .iter()
        .rev()
        .find_map(|(_, value)| value.as_bool());
    (cell(1), cell(2), visible)
}

fn wait_for(presenter: &TestPresenter, what: &str, done: impl Fn(&[Observed]) -> bool) {
    let deadline = Instant::now() + PAGE_TIMEOUT;
    while !done(&presenter.observed()) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn preview_stays_in_its_pane_hides_on_request_and_cleans_up_when_stdin_closes() {
    let document = temp_markdown();
    let presenter = TestPresenter::start(100, 30).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_vvrd"))
        .args(["--preview"])
        .arg(&document)
        .args(["48", "27", "52", "2"])
        .env("VIVID_ENDPOINT_CONTROL", presenter.endpoint())
        .env("VIVID_ENDPOINT_BULK", presenter.endpoint())
        .env("VIVID_ROOT_SECRET", ROOT_SECRET_HEX)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let updates = |observed: &[Observed]| {
        observed
            .iter()
            .filter(|request| request.record_type == UPDATE_NODE)
            .map(placement)
            .collect::<Vec<_>>()
    };

    // The node moves into the pane while hidden, then shows with the first page.
    wait_for(&presenter, "the first page", |observed| {
        updates(observed)
            .iter()
            .any(|update| update.2 == Some(true))
    });
    let shown = updates(&presenter.observed());
    assert!(
        shown
            .iter()
            .all(|update| (update.0, update.1) == (Some(52), Some(2))),
        "{shown:?}"
    );
    assert_eq!(shown[0].2, Some(false), "placed before it is shown");

    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"next\nhide\n").unwrap();
    wait_for(&presenter, "the hide", |observed| {
        updates(observed)
            .last()
            .is_some_and(|update| update.2 == Some(false))
    });

    drop(stdin);
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "vvrd outlived its stdin");
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = child.wait_with_output().unwrap();
    assert!(
        status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "a preview never writes to the terminal"
    );
    let observed = presenter.observed();
    assert_eq!(
        observed
            .iter()
            .filter(|r| r.record_type == CREATE_NODE)
            .count(),
        1
    );
    assert!(
        observed
            .iter()
            .any(|request| request.record_type == DELETE_NODE)
    );
    let _ = std::fs::remove_file(document);
}
