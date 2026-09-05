//! Test the actual shared-library boundary in a separate target interpreter.
#[test]
#[cfg(target_os = "linux")]
fn native_cpython_extension_contracts() {
    let python = std::env::var_os("PYRS_TEST_PYTHON").unwrap_or_else(|| "python3".into());
    let suite =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../compatibility/test_extension.py");
    let output = std::process::Command::new(python)
        .arg(suite)
        .args(["--pyrs", env!("CARGO_BIN_EXE_pyrs")])
        .output()
        .expect("cannot start CPython extension tests");
    assert!(
        output.status.success(),
        "extension tests failed ({}):\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
