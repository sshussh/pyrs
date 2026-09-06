//! Bare `raise` — re-raise the exception the enclosing handler caught.
//!
//! `except E: log(); raise` is the standard way to observe an error without
//! swallowing it, and it had no workaround here: re-raising a *new* exception
//! loses the original type and message, which is the whole point of the idiom.
//!
//! The mechanism worth pinning is that the handler prologue calls
//! `pyrs_exc_clear()` before running its body, so the pending exception is
//! gone by the time the body executes. The exception object is captured just
//! before that clear, and a bare `raise` re-raises it.

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

fn write_prog(tag: &str, source: &str) -> (TempDir, PathBuf) {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-reraise-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    (dir, src)
}

/// Differential check at every optimization level, comparing stdout and exit
/// status against the CPython oracle.
fn matches_python(tag: &str, source: &str) {
    let (_dir, src) = write_prog(tag, source);
    let expected = Command::new("python3")
        .arg(&src)
        .output()
        .expect("failed to spawn CPython");
    assert!(
        expected.status.success(),
        "CPython failed for {tag}: {}",
        String::from_utf8_lossy(&expected.stderr)
    );
    for opt in ["0", "2", "3"] {
        let actual = Command::new(PYRS)
            .args(["run", "-O", opt, "-i"])
            .arg(&src)
            .output()
            .expect("failed to spawn PyRs");
        assert!(
            actual.status.success(),
            "PyRs {tag} at -O{opt} failed:\n{}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&actual.stdout),
            String::from_utf8_lossy(&expected.stdout),
            "stdout differs for {tag} at -O{opt}"
        );
    }
}

/// An uncaught exception must fail with CPython's final traceback line.
fn uncaught_matches_python(tag: &str, source: &str) {
    let (_dir, src) = write_prog(tag, source);
    let expected = Command::new("python3").arg(&src).output().unwrap();
    assert!(!expected.status.success(), "CPython unexpectedly succeeded");
    let want = String::from_utf8_lossy(&expected.stderr);
    let want_last = want.trim_end().lines().last().unwrap_or("").to_string();
    for opt in ["0", "2", "3"] {
        let actual = Command::new(PYRS)
            .args(["run", "-O", opt, "-i"])
            .arg(&src)
            .output()
            .unwrap();
        assert!(
            !actual.status.success(),
            "{tag} at -O{opt} should have failed"
        );
        let got = String::from_utf8_lossy(&actual.stderr);
        assert_eq!(
            got.trim_end().lines().last().unwrap_or(""),
            want_last,
            "uncaught text differs for {tag} at -O{opt}"
        );
        assert_eq!(
            String::from_utf8_lossy(&actual.stdout),
            String::from_utf8_lossy(&expected.stdout),
            "stdout before the raise differs for {tag}"
        );
    }
}

/// Returns the diagnostic for a program that must be rejected.
fn rejects(tag: &str, source: &str) -> String {
    let (_dir, src) = write_prog(tag, source);
    let out = Command::new(PYRS)
        .args(["check", "-i"])
        .arg(&src)
        .output()
        .expect("failed to spawn PyRs");
    assert!(
        !out.status.success(),
        "{tag} was accepted, expected an error"
    );
    String::from_utf8_lossy(&out.stderr).to_string()
}

// ---------------------------------------------------------------------------
// The idiom
// ---------------------------------------------------------------------------

#[test]
fn a_bare_raise_preserves_type_and_message() {
    matches_python(
        "preserve",
        r#"
try:
    try:
        raise ValueError("v")
    except ValueError:
        print("logging")
        raise
except ValueError as e:
    print("re-raised:", e)
"#,
    );
}

#[test]
fn a_bare_raise_works_for_user_exception_classes() {
    matches_python(
        "user-class",
        r#"
class AppError(Exception):
    pass
class NotFound(AppError):
    pass

try:
    try:
        raise NotFound("missing")
    except AppError as inner:
        print("saw", inner)
        raise
except NotFound as outer:
    print("still NotFound:", outer)
"#,
    );
}

#[test]
fn a_bare_raise_propagates_out_of_a_function() {
    matches_python(
        "from-function",
        r#"
def check(n: int) -> int:
    try:
        if n < 0:
            raise ValueError("negative")
        return n
    except ValueError:
        print("rethrowing")
        raise

print(check(3))
try:
    check(-1)
except ValueError as e:
    print("caught:", e)
"#,
    );
}

#[test]
fn a_bare_raise_runs_finally_on_the_way_out() {
    matches_python(
        "finally",
        r#"
try:
    try:
        raise ValueError("fin")
    except ValueError:
        raise
    finally:
        print("finally ran")
except ValueError as e:
    print("after finally:", e)
"#,
    );
}

#[test]
fn a_bare_raise_picks_the_innermost_handler() {
    matches_python(
        "nesting",
        r#"
try:
    try:
        try:
            raise ValueError("inner")
        except ValueError:
            raise
    except ValueError as mid:
        print("mid:", mid)
        raise RuntimeError("outer")
except RuntimeError as e:
    print("outer:", e)
"#,
    );
}

#[test]
fn a_bare_raise_works_after_the_handler_did_other_work() {
    // The handler prologue clears the pending exception, so anything the body
    // does before the `raise` must not disturb what gets re-raised.
    matches_python(
        "after-work",
        r#"
def noisy() -> int:
    print("side effect")
    return 1

try:
    try:
        raise ValueError("kept")
    except ValueError:
        noisy()
        try:
            raise RuntimeError("swallowed")
        except RuntimeError as r:
            print("nested handled:", r)
        raise
except ValueError as e:
    print("original survived:", e)
"#,
    );
}

#[test]
fn a_bare_raise_inside_a_generator_propagates() {
    matches_python(
        "generator",
        r#"
def gen():
    try:
        yield 1
        raise ValueError("g")
    except ValueError:
        print("in gen")
        raise

try:
    for v in gen():
        print(v)
except ValueError as e:
    print("caught:", e)
"#,
    );
}

#[test]
fn an_unhandled_bare_raise_reports_the_original_exception() {
    uncaught_matches_python(
        "uncaught",
        r#"
try:
    raise ValueError("boom")
except ValueError:
    print("logging")
    raise
"#,
    );
}

// ---------------------------------------------------------------------------
// Rejection
// ---------------------------------------------------------------------------

#[test]
fn a_bare_raise_outside_a_handler_is_rejected() {
    // CPython raises RuntimeError: No active exception to re-raise. There is
    // nothing to re-raise here either, and the compiler can say so first.
    let err = rejects("no-handler", "raise\n");
    assert!(
        err.contains("only valid inside an 'except' handler"),
        "{err}"
    );
}

#[test]
fn a_bare_raise_in_a_try_body_is_rejected() {
    let err = rejects(
        "try-body",
        "try:\n    raise\nexcept ValueError:\n    pass\n",
    );
    assert!(
        err.contains("only valid inside an 'except' handler"),
        "{err}"
    );
}

#[test]
fn a_bare_raise_in_a_finally_is_rejected() {
    let err = rejects("finally-body", "try:\n    pass\nfinally:\n    raise\n");
    assert!(
        err.contains("only valid inside an 'except' handler"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// Nothing regressed
// ---------------------------------------------------------------------------

#[test]
fn raise_with_an_exception_still_works() {
    matches_python(
        "explicit",
        r#"
for which in [0, 1, 2]:
    try:
        if which == 0:
            raise ValueError("a")
        elif which == 1:
            raise ValueError
        else:
            raise ValueError()
    except ValueError as e:
        print(which, len(str(e)))
"#,
    );
}
