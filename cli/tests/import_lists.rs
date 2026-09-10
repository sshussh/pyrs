//! Parenthesized import lists -- `from m import (a, b as c,)`.
//!
//! Ordinary Python, and the form a long import list is written in. The
//! parser accepted only a bare comma-separated list. The lexer already
//! suppresses newlines inside brackets, so the multi-line form comes free
//! once the parentheses themselves are consumed.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

const PYRS: &str = env!("CARGO_BIN_EXE_pyrs");

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        // Keep the inputs when a test fails: CI uploads `target/tmp`, so a
        // directory deleted on the way out makes a CI-only failure
        // impossible to reproduce from the artifact.
        if std::thread::panicking() {
            eprintln!("retaining failure artifacts in {}", self.0.display());
            return;
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn workspace(tag: &str) -> TempDir {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-imports-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    dir
}

fn write(dir: &TempDir, name: &str, text: &str) {
    fs::write(dir.0.join(name), text).unwrap();
}

/// Run `entry` under CPython and under PyRs at every optimization level and
/// require identical stdout, stderr and exit status.
fn parity(tag: &str, dir: &TempDir, entry: &str) {
    let expected = Command::new("python3")
        .arg(entry)
        .current_dir(&dir.0)
        .output()
        .expect("failed to spawn CPython");
    for opt in ["0", "2", "3"] {
        let actual = Command::new(PYRS)
            .args(["run", "-O", opt, "-i", entry])
            .current_dir(&dir.0)
            .output()
            .expect("failed to spawn PyRs");
        assert_eq!(
            String::from_utf8_lossy(&actual.stdout),
            String::from_utf8_lossy(&expected.stdout),
            "{tag}: stdout differs at -O{opt}"
        );
        assert_eq!(
            String::from_utf8_lossy(&actual.stderr),
            String::from_utf8_lossy(&expected.stderr),
            "{tag}: stderr differs at -O{opt}"
        );
        assert_eq!(
            actual.status.code(),
            expected.status.code(),
            "{tag}: exit status differs at -O{opt}"
        );
    }
}

const HELPER: &str = "\
def double(n: int) -> int:
    return n * 2


def label() -> str:
    return \"helper\"


GREETING: str = \"hi\"
";

#[test]
fn a_parenthesized_import_list_binds_every_name() {
    let dir = workspace("paren-list");
    write(&dir, "helper.py", HELPER);
    write(
        &dir,
        "main.py",
        "from helper import (double, label, GREETING)\n\
         print(double(21), label(), GREETING)\n",
    );
    parity("paren-list", &dir, "main.py");
}

#[test]
fn a_wrapped_import_list_may_span_lines_and_end_with_a_comma() {
    let dir = workspace("wrapped");
    write(&dir, "helper.py", HELPER);
    write(
        &dir,
        "main.py",
        "from helper import (\n    double,\n    label as name,\n    GREETING,\n)\n\
         print(double(3), name(), GREETING)\n",
    );
    parity("wrapped", &dir, "main.py");
}

#[test]
fn an_unparenthesized_import_list_still_works() {
    let dir = workspace("bare-list");
    write(&dir, "helper.py", HELPER);
    write(
        &dir,
        "main.py",
        "from helper import double, label as name\nprint(double(5), name())\n",
    );
    parity("bare-list", &dir, "main.py");
}
