//! Lambda parameter types, inferred rather than annotated.
//!
//! A lambda cannot carry annotations — the first `:` starts the body — so
//! requiring them made lambdas unusable, and `sorted(xs, key=lambda v: -v)`,
//! the idiom they exist for, was a compile error. Named functions already
//! worked as `key=`, so the whole gap was parameter typing.
//!
//! Two sources of type information, in that order: what the consumer knows
//! (a `key=` argument is called with one element, so it knows the parameter
//! type even when the body reveals nothing — `lambda s: len(s)` says nothing
//! about `s`), then the same body-usage inference nested `def`s already had.

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
            .join(format!("pyrs-lambda-{tag}-{}", std::process::id())),
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
// key= — the idiom lambdas exist for
// ---------------------------------------------------------------------------

#[test]
fn a_key_lambda_takes_its_type_from_the_element() {
    matches_python(
        "key-basic",
        r#"
xs = [3, 1, 2]
print(sorted(xs, key=lambda v: -v))
print(sorted(xs, key=lambda v: v, reverse=True))
xs.sort(key=lambda v: -v)
print(xs)
"#,
    );
}

#[test]
fn a_key_lambda_works_when_the_body_reveals_nothing() {
    // `len(s)` constrains nothing about `s`; only the consumer knows it is a
    // string. This is the case body-usage inference alone cannot reach.
    matches_python(
        "key-opaque",
        r#"
ss = ["aa", "b", "ccc"]
print(sorted(ss, key=lambda s: len(s)))
print(max(ss, key=lambda s: len(s)))
print(min(ss, key=lambda s: len(s)))
"#,
    );
}

#[test]
fn key_lambdas_may_return_any_sortable_type() {
    matches_python(
        "key-returns",
        r#"
ss = ["b", "aa", "C"]
print(sorted(ss, key=lambda s: s.upper()))
xs = [3, 1, 2]
print(sorted(xs, key=lambda v: v * 1.0))
print(sorted(xs, key=lambda v: v > 1))
"#,
    );
}

#[test]
fn a_key_lambda_can_index_a_tuple_element() {
    matches_python(
        "key-tuple",
        r#"
ps = [(1, "b"), (2, "a"), (3, "c")]
print(min(ps, key=lambda p: p[1]))
print(max(ps, key=lambda p: p[0]))
print(sorted(ps, key=lambda p: p[1]))
"#,
    );
}

#[test]
fn a_written_annotation_still_wins_where_one_is_possible() {
    // A `def` used as `key=` keeps its declared type; the hint only fills a
    // parameter that has none.
    matches_python(
        "key-named-fn",
        r#"
def neg(v: int) -> int:
    return -v

xs = [3, 1, 2]
print(sorted(xs, key=neg))
print(sorted(xs, key=lambda v: -v))
"#,
    );
}

// ---------------------------------------------------------------------------
// Body-usage inference
// ---------------------------------------------------------------------------

#[test]
fn a_lambda_parameter_is_inferred_from_the_body() {
    matches_python(
        "body-infer",
        r#"
f = lambda a: a + 1
print(f(1))
g = lambda a, b: a + b
print(g(1, 2))
h = lambda a: a * 2 + 1
print(h(3))
"#,
    );
}

#[test]
fn defaults_and_captures_still_work() {
    matches_python(
        "defaults-captures",
        r#"
f = lambda a=2: a + 1
print(f(), f(10))

def outer() -> int:
    n = 5
    add = lambda a: a + n
    return add(1)

print(outer())
"#,
    );
}

#[test]
fn a_lambda_can_be_passed_and_returned() {
    matches_python(
        "first-class",
        r#"
def apply(f: int, v: int) -> int:
    return v

def make(n: int):
    return lambda a: a + n

add3 = make(3)
print(add3(4))
"#,
    );
}

// ---------------------------------------------------------------------------
// Rejections
// ---------------------------------------------------------------------------

#[test]
fn a_body_that_constrains_nothing_is_still_rejected() {
    // No consumer to ask and nothing in the body to go on. A lambda cannot
    // carry an annotation, so the fix is a `def`.
    let err = rejects("no-info", "f = lambda x: len(x)\nprint(f(\"ab\"))\n");
    assert!(
        err.contains("missing a type annotation") || err.contains("could not infer"),
        "{err}"
    );
}

#[test]
fn a_key_hint_does_not_leak_onto_an_unrelated_parameter() {
    // The hint is keyed by the parameter's own name, so it has to be scoped:
    // `s` here must still infer int from its body, not str from the lambda.
    matches_python(
        "no-leak",
        r#"
ss = ["aa", "b"]
print(sorted(ss, key=lambda s: len(s)))

def f(s):
    return s + 1

print(f(2))
"#,
    );
}
