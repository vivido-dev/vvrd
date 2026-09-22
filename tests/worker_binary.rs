// The production worker must be built alongside unit tests that launch it.
#[test]
fn worker_binary_is_available() {
    assert!(std::path::Path::new(env!("CARGO_BIN_EXE_vvrd")).is_file());
}
