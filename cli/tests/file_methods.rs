//! `f.writelines(lines)`.
//!
//! Writing a file line by line meant a `for` loop around `f.write`, which is
//! not how the code being ported is written. CPython's `writelines` takes any
//! iterable of `str` and adds no separator despite the name; here the argument
//! is typed `list[str]`, so the `TypeError` CPython raises part-way through a
//! write is a compile error instead, and nothing is written.

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

/// A workspace holding `prog.py`. Both engines run with this as the working
/// directory, so the program can name its output file relatively.
fn temp_source(tag: &str, source: &str) -> TempDir {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-filemethods-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    fs::write(dir.0.join("prog.py"), source).unwrap();
    dir
}

/// Differential check against the CPython oracle at every optimization level,
/// and once more under GC stress.
fn matches_python(tag: &str, source: &str) {
    let dir = temp_source(tag, source);
    let expected = Command::new("python3")
        .arg("prog.py")
        .current_dir(&dir.0)
        .output()
        .expect("failed to spawn CPython");
    assert!(
        expected.status.success(),
        "CPython failed for {tag}: {}",
        String::from_utf8_lossy(&expected.stderr)
    );
    for opt in ["0", "2", "3"] {
        for stress in ["0", "1"] {
            let actual = Command::new(PYRS)
                .args(["run", "-O", opt, "-i", "prog.py"])
                .env("PYRS_GC_STRESS", stress)
                .current_dir(&dir.0)
                .output()
                .expect("failed to spawn PyRs");
            assert_eq!(
                String::from_utf8_lossy(&actual.stdout),
                String::from_utf8_lossy(&expected.stdout),
                "{tag}: stdout differs at -O{opt} (GC stress {stress})\nstderr: {}",
                String::from_utf8_lossy(&actual.stderr)
            );
            assert_eq!(
                actual.status.code(),
                expected.status.code(),
                "{tag}: exit status differs at -O{opt} (GC stress {stress})"
            );
        }
    }
}

/// Both engines must fail, with the same final stderr line. Only the last line
/// is compared: PyRs prints no traceback, which is a recorded divergence.
fn fails_like_python(tag: &str, source: &str) {
    let dir = temp_source(tag, source);
    let expected = Command::new("python3")
        .arg("prog.py")
        .current_dir(&dir.0)
        .output()
        .expect("failed to spawn CPython");
    assert!(
        !expected.status.success(),
        "{tag}: CPython was expected to fail"
    );
    let want = last_line(&expected.stderr);
    for opt in ["0", "2", "3"] {
        let actual = Command::new(PYRS)
            .args(["run", "-O", opt, "-i", "prog.py"])
            .current_dir(&dir.0)
            .output()
            .expect("failed to spawn PyRs");
        assert!(
            !actual.status.success(),
            "{tag}: PyRs succeeded at -O{opt} but CPython failed"
        );
        assert_eq!(
            last_line(&actual.stderr),
            want,
            "{tag}: error differs at -O{opt}"
        );
        assert_eq!(
            actual.status.code(),
            expected.status.code(),
            "{tag}: exit status differs at -O{opt}"
        );
    }
}

fn last_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .unwrap_or_default()
        .to_string()
}

/// The program must be rejected, with `needle` in the diagnostic, and nothing
/// may run.
fn rejected_with(tag: &str, source: &str, needle: &str) {
    let dir = temp_source(tag, source);
    let out = Command::new(PYRS)
        .args(["run", "-i", "prog.py"])
        .current_dir(&dir.0)
        .output()
        .expect("failed to spawn PyRs");
    assert!(
        !out.status.success(),
        "{tag} compiled but should have been rejected"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(needle),
        "{tag}: expected {needle:?} in diagnostic, got:\n{stderr}"
    );
    assert!(
        out.stdout.is_empty(),
        "{tag}: produced output before rejecting: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

// ---------------------------------------------------------------------------
// writelines
// ---------------------------------------------------------------------------

#[test]
fn writelines_joins_nothing_between_the_items() {
    // The name says "lines" but CPython adds no separator, so only the items
    // that already end in a newline produce one.
    matches_python(
        "no-separator",
        "f = open(\"out.txt\", \"w\")\n\
         f.writelines([\"a\\n\", \"b\", \"c\\n\"])\n\
         f.close()\n\
         print(repr(open(\"out.txt\").read()))\n",
    );
}

#[test]
fn writelines_accepts_an_empty_list() {
    // A bare `[]` is provisionally `list[Any]`; it has to take the parameter's
    // element type rather than be rejected.
    matches_python(
        "empty",
        "f = open(\"out.txt\", \"w\")\n\
         f.writelines([])\n\
         f.close()\n\
         print(repr(open(\"out.txt\").read()))\n",
    );
}

#[test]
fn writelines_writes_bytes_for_non_ascii_items() {
    matches_python(
        "non-ascii",
        "f = open(\"out.txt\", \"w\")\n\
         f.writelines([\"héllo \", \"wörld\", \"\\n\"])\n\
         f.close()\n\
         print(repr(open(\"out.txt\").read()))\n",
    );
}

#[test]
fn writelines_appends_and_composes_with_write() {
    matches_python(
        "append",
        "f = open(\"out.txt\", \"w\")\n\
         f.write(\"first\\n\")\n\
         f.writelines([\"second\\n\", \"third\\n\"])\n\
         f.close()\n\
         a = open(\"out.txt\", \"a\")\n\
         a.writelines([\"fourth\\n\"])\n\
         a.close()\n\
         print(open(\"out.txt\").readlines())\n",
    );
}

#[test]
fn writelines_takes_a_list_built_at_runtime() {
    matches_python(
        "runtime-list",
        "lines: list[str] = []\n\
         for i in range(4):\n\
         \x20   lines.append(str(i) + \"\\n\")\n\
         f = open(\"out.txt\", \"w\")\n\
         f.writelines(lines)\n\
         f.close()\n\
         print(open(\"out.txt\").readlines())\n",
    );
}

#[test]
fn writelines_on_a_closed_file_raises() {
    fails_like_python(
        "closed",
        "f = open(\"out.txt\", \"w\")\n\
         f.close()\n\
         f.writelines([\"x\"])\n",
    );
}

#[test]
fn writelines_on_a_read_only_file_raises() {
    fails_like_python(
        "not-writable",
        "w = open(\"out.txt\", \"w\")\n\
         w.close()\n\
         f = open(\"out.txt\")\n\
         f.writelines([\"x\"])\n",
    );
}

#[test]
fn writelines_needs_a_list_of_str() {
    // CPython accepts any iterable and raises TypeError part-way through the
    // write; the static type turns both cases into a compile error instead.
    rejected_with(
        "wrong-element",
        "f = open(\"out.txt\", \"w\")\nf.writelines([1, 2])\n",
        "writelines() expects a list[str] argument, found list[int]",
    );
    // A str is an iterable of str in CPython, which writes it character by
    // character. Rejecting it is a deliberate divergence, not an oversight.
    rejected_with(
        "str-argument",
        "f = open(\"out.txt\", \"w\")\nf.writelines(\"abc\")\n",
        "writelines() expects a list[str] argument, found str",
    );
}

#[test]
fn writelines_takes_exactly_one_argument() {
    rejected_with(
        "no-argument",
        "f = open(\"out.txt\", \"w\")\nf.writelines()\n",
        "writelines() takes exactly 1 argument(s) (0 given)",
    );
}

#[test]
fn an_unsupported_file_method_names_the_supported_ones() {
    rejected_with(
        "unsupported",
        "f = open(\"out.txt\", \"w\")\nf.truncate()\n",
        "read, readline, readlines, write, writelines, close, flush",
    );
}
