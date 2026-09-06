//! Driver contracts: execution modes must preserve the user's program boundary.
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

const PYRS: &str = env!("CARGO_BIN_EXE_pyrs");
static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct WorkDir(PathBuf);

impl WorkDir {
    fn new() -> Self {
        let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "pyrs-invocation-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        // Keep the inputs when a test fails. CI uploads `target/tmp`, so a
        // directory deleted on the way out makes a CI-only failure
        // impossible to reproduce from the artifact.
        if std::thread::panicking() {
            eprintln!("retaining failure artifacts in {}", self.0.display());
            return;
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn parity(got: Output, want: Output) {
    assert_eq!(
        got.status.code(),
        want.status.code(),
        "{}",
        String::from_utf8_lossy(&got.stderr)
    );
    assert_eq!(got.stdout, want.stdout);
    assert_eq!(got.stderr, want.stderr);
}

fn piped(mut command: Command, input: &str) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn direct_native_script_preserves_argv_and_script_arguments() {
    let dir = WorkDir::new();
    fs::write(
        dir.0.join("script name.py"),
        "import sys\nprint(sys.argv)\n",
    )
    .unwrap();
    let args = ["script name.py", "--compat", "-O", "--help", "two words"];
    parity(
        Command::new(PYRS)
            .args(args)
            .current_dir(&dir.0)
            .output()
            .unwrap(),
        Command::new("python3")
            .args(args)
            .current_dir(&dir.0)
            .output()
            .unwrap(),
    );
}

#[test]
fn inline_native_code_imports_from_cwd_and_preserves_argv() {
    let dir = WorkDir::new();
    fs::write(dir.0.join("helper.py"), "def result():\n    return 42\n").unwrap();
    let code = "import sys\nfrom helper import result\nprint(sys.argv)\nprint(result())\n";
    let args = ["-c", code, "--help", "--", "two words"];
    parity(
        Command::new(PYRS)
            .args(args)
            .current_dir(&dir.0)
            .output()
            .unwrap(),
        Command::new("python3")
            .args(args)
            .current_dir(&dir.0)
            .output()
            .unwrap(),
    );
}

#[test]
fn stdin_native_source_matches_python() {
    let source = "import sys\nprint(sys.argv)\nprint(6 * 7)\n";
    let mut pyrs = Command::new(PYRS);
    pyrs.args(["-", "arg"]);
    let mut python = Command::new("python3");
    python.args(["-", "arg"]);
    parity(piped(pyrs, source), piped(python, source));
}

#[test]
fn compatibility_preserves_streams_exit_and_inline_arguments() {
    let source = "import sys\nprint(sys.argv)\nprint(input())\nprint('diagnostic', file=sys.stderr)\nsys.exit(7)";
    let mut pyrs = Command::new(PYRS);
    pyrs.args(["--compat", "-c", source, "--help", "--", "arg"]);
    let mut python = Command::new("python3");
    python.args(["-c", source, "--help", "--", "arg"]);
    parity(piped(pyrs, "payload\n"), piped(python, "payload\n"));
}

#[test]
fn compatibility_script_runs_once_and_native_never_falls_back() {
    let dir = WorkDir::new();
    let source = "from pathlib import Path\nwith Path('effect').open('a') as f:\n    f.write('once\\n')\nraise ValueError('failure')\n";
    fs::write(dir.0.join("script.py"), source).unwrap();
    let native = Command::new(PYRS)
        .arg("script.py")
        .current_dir(&dir.0)
        .output()
        .unwrap();
    assert!(!native.status.success());
    assert!(!dir.0.join("effect").exists());
    let compat = Command::new(PYRS)
        .args(["--compat", "script.py"])
        .current_dir(&dir.0)
        .output()
        .unwrap();
    assert_eq!(compat.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&compat.stderr).contains("ValueError: failure"));
    assert_eq!(fs::read_to_string(dir.0.join("effect")).unwrap(), "once\n");
}

#[test]
fn compatibility_runs_package_main_with_relative_imports() {
    let dir = WorkDir::new();
    fs::create_dir(dir.0.join("package")).unwrap();
    fs::write(dir.0.join("package/__init__.py"), "value = 42\n").unwrap();
    fs::write(
        dir.0.join("package/__main__.py"),
        "from . import value\nimport sys\nprint(value, sys.argv)\n",
    )
    .unwrap();
    parity(
        Command::new(PYRS)
            .args(["--compat", "-m", "package", "--arg"])
            .current_dir(&dir.0)
            .output()
            .unwrap(),
        Command::new("python3")
            .args(["-m", "package", "--arg"])
            .current_dir(&dir.0)
            .output()
            .unwrap(),
    );
}

#[test]
fn compatibility_stdin_and_interpreter_selection() {
    let code = "import sys\nprint(sys.argv)\n";
    let mut pyrs = Command::new(PYRS);
    pyrs.args(["run", "--compat", "--python", "python3", "-", "x"])
        .env("PYRS_PYTHON", "/missing/python");
    let mut python = Command::new("python3");
    python.args(["-", "x"]);
    parity(piped(pyrs, code), piped(python, code));
    let missing = Command::new(PYRS)
        .args(["--compat", "-c", "print(42)"])
        .env("PYRS_PYTHON", "/missing/python")
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(missing.stdout.is_empty());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("/missing/python"));
}

#[test]
fn native_check_does_not_link_or_execute() {
    let dir = WorkDir::new();
    fs::write(
        dir.0.join("script.py"),
        "with open('effect', 'w') as f:\n    f.write('executed')\n",
    )
    .unwrap();
    let checked = Command::new(PYRS)
        .args(["check", "-i", "script.py"])
        .current_dir(&dir.0)
        .env("CC", "/missing/cc")
        .output()
        .unwrap();
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    assert!(!dir.0.join("effect").exists());
    let failed = Command::new(PYRS)
        .arg("script.py")
        .current_dir(&dir.0)
        .env("CC", "/missing/cc")
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(!dir.0.join("effect").exists());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("/missing/cc"));
}

#[test]
fn rejects_invalid_execution_options() {
    for args in [
        vec!["compile", "-i", "missing.py", "-O", "4"],
        vec!["-O", "255", "missing.py"],
        vec!["-m", "json"],
    ] {
        let output = Command::new(PYRS).args(&args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn python_without_compat_warns_rather_than_refusing() {
    // `--python` stopped requiring `--compat` once [tool.pyrs] could select
    // compatibility mode, so this is no longer a usage error -- but a flag
    // that cannot take effect should still say so.
    let output = Command::new(PYRS)
        .args(["--python", "python3", "missing.py"])
        .output()
        .unwrap();
    assert_ne!(output.status.code(), Some(2), "should not be a usage error");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--python has no effect"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn compatibility_preserves_signal_termination() {
    use std::os::unix::process::ExitStatusExt;
    let output = Command::new(PYRS)
        .args([
            "--compat",
            "-c",
            "import os, signal; os.kill(os.getpid(), signal.SIGTERM)",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.signal(), Some(15));
}
