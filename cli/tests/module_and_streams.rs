//! `__name__`, `sys.exit` and `print(file=...)`.
//!
//! `if __name__ == "__main__":` is the most common idiom in Python and was
//! impossible to write. `sys.exit` and `sys.stderr` are how a program reports
//! failure, and `sys` was `argv` and nothing else.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

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

fn workspace(tag: &str) -> TempDir {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-modstream-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    dir
}

fn write(path: &std::path::Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

/// Run one entry point under both engines and compare stdout, stderr and exit
/// status separately — the point of these features is *which stream* and
/// *which status*, so a combined comparison would not test them.
fn parity(tag: &str, dir: &TempDir, entry: &str) {
    let expected = Command::new("python3")
        .arg(entry)
        .current_dir(&dir.0)
        .output()
        .expect("failed to spawn CPython");
    for opt in ["0", "2", "3"] {
        let actual: Output = Command::new(PYRS)
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

// ---------------------------------------------------------------------------
// __name__
// ---------------------------------------------------------------------------

#[test]
fn the_main_guard_runs_in_the_entry_module() {
    let dir = workspace("main-guard");
    write(
        &dir.0.join("prog.py"),
        "def main() -> None:\n    print(\"main ran\")\n\n\n\
         print(\"name is\", __name__)\n\
         if __name__ == \"__main__\":\n    main()\n",
    );
    parity("main-guard", &dir, "prog.py");
}

#[test]
fn an_imported_module_sees_its_own_dotted_name() {
    // The guard is only useful if it is *false* somewhere, which needs the
    // imported module to report its import name rather than `__main__`.
    let dir = workspace("imported-name");
    write(&dir.0.join("pkg/__init__.py"), "");
    write(
        &dir.0.join("pkg/util.py"),
        "def who() -> str:\n    return __name__\n\n\n\
         print(\"module says\", __name__)\n\
         if __name__ == \"__main__\":\n    print(\"should not run\")\n",
    );
    write(
        &dir.0.join("prog.py"),
        "from pkg import util\n\n\
         print(\"entry says\", __name__)\n\
         print(\"util says\", util.who())\n",
    );
    parity("imported-name", &dir, "prog.py");
}

#[test]
fn a_local_binding_shadows_the_module_name() {
    // Resolution happens after locals and globals, so a user binding wins.
    let dir = workspace("shadow-name");
    write(
        &dir.0.join("prog.py"),
        "def f() -> str:\n    __name__ = \"local\"\n    return __name__\n\n\n\
         print(f(), __name__)\n",
    );
    parity("shadow-name", &dir, "prog.py");
}

// ---------------------------------------------------------------------------
// sys.exit
// ---------------------------------------------------------------------------

#[test]
fn sys_exit_stops_the_program_with_its_status() {
    let dir = workspace("sys-exit");
    write(
        &dir.0.join("prog.py"),
        "import sys\n\n\
         print(\"before\")\n\
         sys.exit(3)\n\
         print(\"never\")\n",
    );
    parity("sys-exit", &dir, "prog.py");
}

#[test]
fn sys_exit_with_no_argument_is_success() {
    let dir = workspace("sys-exit-bare");
    write(
        &dir.0.join("prog.py"),
        "import sys\n\nprint(\"done\")\nsys.exit()\n",
    );
    parity("sys-exit-bare", &dir, "prog.py");
}

#[test]
fn sys_exit_works_from_inside_a_function_and_a_loop() {
    let dir = workspace("sys-exit-nested");
    write(
        &dir.0.join("prog.py"),
        "import sys\n\n\n\
         def bail(n: int) -> None:\n    \
         print(\"bailing\", n)\n    \
         sys.exit(n)\n\n\n\
         for i in range(5):\n    \
         print(i)\n    \
         if i == 2:\n        \
         bail(i)\n",
    );
    parity("sys-exit-nested", &dir, "prog.py");
}

#[test]
fn buffered_output_is_flushed_before_exiting() {
    // The failure this guards: stdout is block-buffered when redirected, so
    // exiting without a flush loses everything printed.
    let dir = workspace("exit-flush");
    write(
        &dir.0.join("prog.py"),
        "import sys\n\n\
         for i in range(200):\n    print(\"line\", i)\n\
         sys.exit(1)\n",
    );
    parity("exit-flush", &dir, "prog.py");
}

// ---------------------------------------------------------------------------
// print(file=...)
// ---------------------------------------------------------------------------

#[test]
fn print_writes_to_the_stream_it_is_given() {
    let dir = workspace("print-file");
    write(
        &dir.0.join("prog.py"),
        "import sys\n\n\
         print(\"out one\")\n\
         print(\"err one\", file=sys.stderr)\n\
         print(\"out two\", file=sys.stdout)\n\
         print(\"err two\", \"and more\", sep=\"|\", file=sys.stderr)\n",
    );
    parity("print-file", &dir, "prog.py");
}

#[test]
fn the_destination_does_not_leak_into_the_next_print() {
    // The flag is set around one call; a leak would send everything after it
    // to stderr.
    let dir = workspace("no-leak");
    write(
        &dir.0.join("prog.py"),
        "import sys\n\n\
         print(\"a\", file=sys.stderr)\n\
         print(\"b\")\n\
         print(\"c\", file=sys.stderr)\n\
         print(\"d\")\n",
    );
    parity("no-leak", &dir, "prog.py");
}

#[test]
fn str_of_a_value_is_unaffected_by_the_destination() {
    // `str()` captures the same writer the print routines use, so the flag
    // must not reach it.
    let dir = workspace("capture");
    write(
        &dir.0.join("prog.py"),
        "import sys\n\n\
         xs: list[int] = [1, 2]\n\
         print(str(xs), file=sys.stderr)\n\
         print(str(xs))\n\
         print(f\"{xs}\", file=sys.stderr)\n",
    );
    parity("capture", &dir, "prog.py");
}

/// `print(..., file=f)` reaches any open file, not just the two standard
/// streams. The destination is set for the duration of the one statement, so
/// a `str()` of a value inside it still captures rather than escaping to the
/// file.
#[test]
fn print_writes_to_an_opened_file() {
    let dir = workspace("print-to-open-file");
    write(
        &dir.0.join("prog.py"),
        "f = open(\"out.txt\", \"w\")\n\
         print(\"a\", 1, [2, 3], sep=\"|\", file=f)\n\
         print(\"second\", file=f)\n\
         f.close()\n\
         print(open(\"out.txt\").read(), end=\"\")\n\
         print(\"back on stdout\")\n",
    );
    parity("print-to-open-file", &dir, "prog.py");
}

/// A destination that is not a file is still refused, and the message says
/// what one looks like.
#[test]
fn a_print_destination_must_be_a_file() {
    let dir = workspace("bad-file");
    write(&dir.0.join("prog.py"), "print(1, file=\"out.txt\")\n");
    let out = Command::new(PYRS)
        .args(["check", "-i", "prog.py"])
        .current_dir(&dir.0)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let message = String::from_utf8_lossy(&out.stderr);
    assert!(message.contains("needs a file"), "{message}");
    assert!(message.contains("open()"), "{message}");
}
