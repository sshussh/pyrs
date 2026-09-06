//! `zip` over any number of iterables, and `enumerate`'s `start`.
//!
//! `zip` accepted exactly two arguments and `enumerate` only a keyword
//! `start=`, so `zip(a, b, c)` and `enumerate(xs, 1)` — both ordinary Python —
//! were compile errors. Found by running realistic programs against CPython.
//!
//! Both now materialize their arguments the way the other eager builtins do
//! (0.98), so `zip(range(3), "ab")` and `enumerate(range(3), 10)` work too.

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
            .join(format!("pyrs-zipenum-{tag}-{}", std::process::id())),
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
// zip
// ---------------------------------------------------------------------------

#[test]
fn zip_takes_any_number_of_iterables() {
    matches_python(
        "zip-arity",
        r#"
print(list(zip([1, 2])))
print(list(zip([1, 2], ["a", "b"])))
print(list(zip([1], [2], [3])))
print(list(zip([1, 2], ["a", "b"], [True, False], [1.5, 2.5])))
"#,
    );
}

#[test]
fn zip_truncates_to_the_shortest() {
    matches_python(
        "zip-shortest",
        r#"
print(list(zip([1, 2, 3], [4, 5])))
print(list(zip([1], [4, 5, 6], [7, 8])))
empty: list[int] = []
print(list(zip([1, 2], empty)))
"#,
    );
}

#[test]
fn zip_accepts_every_iterable() {
    matches_python(
        "zip-iterables",
        r#"
def g():
    yield 10
    yield 20

print(list(zip(range(3), "ab")))
print(list(zip([1, 2], (3, 4))))
print(list(zip(g(), [1, 2])))
print(sorted(zip(sorted({3, 1}), ["x", "y"])))
"#,
    );
}

#[test]
fn zip_results_unpack_in_a_loop() {
    matches_python(
        "zip-loop",
        r#"
names = ["a", "b", "c"]
vals = [1, 2, 3]
for n, v in zip(names, vals):
    print(n, v)
print(dict(zip(names, vals)))
for n, v, f in zip(names, vals, [True, False, True]):
    print(n, v, f)
"#,
    );
}

// ---------------------------------------------------------------------------
// enumerate
// ---------------------------------------------------------------------------

#[test]
fn enumerate_takes_start_positionally_and_by_keyword() {
    matches_python(
        "enum-start",
        r#"
for i, s in enumerate(["a", "b"], 1):
    print(i, s)
for j, t in enumerate(["a", "b"], start=5):
    print(j, t)
for k, u in enumerate(["a", "b"]):
    print(k, u)
print(list(enumerate([1, 2], 1)))
"#,
    );
}

#[test]
fn enumerate_accepts_every_iterable() {
    matches_python(
        "enum-iterables",
        r#"
def g():
    yield 7
    yield 8

for i, c in enumerate("ab"):
    print(i, c)
for j, n in enumerate(range(3), 10):
    print(j, n)
for k, v in enumerate(g(), 1):
    print(k, v)
print(list(enumerate((1, 2))))
"#,
    );
}

#[test]
fn zip_and_enumerate_compose() {
    matches_python(
        "compose",
        r#"
names = ["a", "b", "c"]
vals = [1, 2, 3]
for i, (n, v) in enumerate(zip(names, vals)):
    print(i, n, v)
for j, (n2, v2) in enumerate(zip(names, vals), 1):
    print(j, n2, v2)
"#,
    );
}

// ---------------------------------------------------------------------------
// Rejections
// ---------------------------------------------------------------------------

#[test]
fn a_duplicate_start_is_rejected() {
    let err = rejects("dup-start", "print(list(enumerate([1], 1, start=2)))\n");
    assert!(
        err.contains("multiple values for argument 'start'"),
        "{err}"
    );
}

#[test]
fn too_many_enumerate_arguments_are_rejected() {
    let err = rejects("enum-arity", "print(list(enumerate([1], 1, 2)))\n");
    assert!(err.contains("1 or 2 positional arguments"), "{err}");
}

#[test]
fn zip_with_no_arguments_is_empty() {
    // `list(zip())` is `[]` in CPython. This was rejected, which contradicted
    // the "any number of iterables" contract for the one arity that needs no
    // iteration at all.
    matches_python(
        "zip-none",
        r#"
print(list(zip()))
print(len(list(zip())))
for t in zip():
    print("unreachable")
print("done")
"#,
    );
}
