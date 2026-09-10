//! `pyrs profile` records type tags; `compile --profile` consumes them.
//!
//! A missing or stale profile must degrade to today's behaviour. `--profile`
//! does not change observable stdout or exit status.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

const PYRS: &str = env!("CARGO_BIN_EXE_pyrs");

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("retaining failure artifacts in {}", self.0.display());
            return;
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn temp_source(tag: &str, source: &str) -> TempDir {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-profile-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    fs::write(dir.0.join("prog.py"), source).unwrap();
    dir
}

const PROG: &str = "a: object = 7\nb: object = 2\nprint(a + b)\n";

#[test]
fn a_profile_round_trips() {
    let dir = temp_source("roundtrip", PROG);
    let prof = dir.0.join("prog.prof");
    let out = Command::new(PYRS)
        .args(["profile", "-i", "prog.py", "-o"])
        .arg(&prof)
        .current_dir(&dir.0)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = fs::read_to_string(&prof).unwrap();
    assert!(text.starts_with("pyrs-profile 1\n"), "{text}");
    assert!(
        text.contains(&format!("compiler {}", env!("CARGO_PKG_VERSION"))),
        "{text}"
    );
    assert!(text.contains("source "), "{text}");
    assert!(text.contains("site "), "{text}");
    // int tag 0 must appear for `a + b`
    assert!(text.contains("\n  0 "), "{text}");

    let compiled = Command::new(PYRS)
        .args(["compile", "--no-cache", "--profile"])
        .arg(&prof)
        .args(["-i", "prog.py", "-o", "prog"])
        .current_dir(&dir.0)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&compiled.stderr).contains("stale"),
        "a fresh profile must be trusted: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
}

#[test]
fn a_stale_profile_is_ignored_with_a_warning() {
    let dir = temp_source("stale", PROG);
    let prof = dir.0.join("prog.prof");
    let collected = Command::new(PYRS)
        .args(["profile", "-i", "prog.py", "-o"])
        .arg(&prof)
        .current_dir(&dir.0)
        .output()
        .unwrap();
    assert!(collected.status.success());
    let mut text = fs::read_to_string(&prof).unwrap();
    text = text.replace(
        &format!("compiler {}", env!("CARGO_PKG_VERSION")),
        "compiler 0.0.0",
    );
    fs::write(&prof, text).unwrap();
    let compiled = Command::new(PYRS)
        .args(["compile", "--no-cache", "--profile"])
        .arg(&prof)
        .args(["-i", "prog.py", "-o", "prog"])
        .current_dir(&dir.0)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let stderr = String::from_utf8_lossy(&compiled.stderr);
    assert!(stderr.contains("stale"), "{stderr}");
}

#[test]
fn a_missing_profile_is_ignored_with_a_warning() {
    let dir = temp_source("missing", PROG);
    let compiled = Command::new(PYRS)
        .args([
            "compile",
            "--no-cache",
            "--profile",
            "no-such.prof",
            "-i",
            "prog.py",
            "-o",
            "prog",
        ])
        .current_dir(&dir.0)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let stderr = String::from_utf8_lossy(&compiled.stderr);
    assert!(stderr.contains("missing"), "{stderr}");
}

#[test]
fn profile_does_not_change_observable_behaviour() {
    let dir = temp_source("obs", PROG);
    let expected = Command::new("python3")
        .arg("prog.py")
        .current_dir(&dir.0)
        .output()
        .unwrap();
    assert!(expected.status.success());
    let prof = dir.0.join("prog.prof");
    let collected = Command::new(PYRS)
        .args(["profile", "-i", "prog.py", "-o"])
        .arg(&prof)
        .current_dir(&dir.0)
        .output()
        .unwrap();
    assert!(
        collected.status.success(),
        "{}",
        String::from_utf8_lossy(&collected.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&collected.stdout),
        String::from_utf8_lossy(&expected.stdout)
    );
    for opt in ["0", "2", "3"] {
        let actual = Command::new(PYRS)
            .args(["run", "--no-cache", "-O", opt, "-i", "prog.py"])
            .current_dir(&dir.0)
            .output()
            .unwrap();
        assert!(actual.status.success());
        assert_eq!(
            String::from_utf8_lossy(&actual.stdout),
            String::from_utf8_lossy(&expected.stdout),
            "stdout differs at -O{opt}"
        );
        let with = Command::new(PYRS)
            .args(["compile", "--no-cache", "--profile"])
            .arg(&prof)
            .args(["-O", opt, "-i", "prog.py", "-o", "with"])
            .current_dir(&dir.0)
            .output()
            .unwrap();
        assert!(with.status.success());
        let ran = Command::new(dir.0.join("with")).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&ran.stdout),
            String::from_utf8_lossy(&expected.stdout),
            "profiled binary stdout differs at -O{opt}"
        );
        assert_eq!(ran.status.code(), expected.status.code());
    }
}
