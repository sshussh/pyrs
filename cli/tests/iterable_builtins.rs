//! The eager builtins accept any iterable, not just a list.
//!
//! `sorted`, `sum`, `max`, `min`, `set`, `list` and `str.join` took a list
//! (and, since 0.95, a generator) and rejected everything else — so
//! `sorted(some_set)`, `sorted(some_dict)`, `sum(range(n))` and
//! `list(range(n))` were all compile errors, despite `for x in` accepting
//! every one of them.
//!
//! `range` is the interesting case: it is not a first-class value here, so it
//! cannot be lowered and then converted. It is materialized through the same
//! comprehension path `[x for x in range(n)]` already used.
//!
//! `any` and `all` are deliberately not part of this: they short-circuit, and
//! materializing would change what a generator prints and would build a huge
//! list to answer `all(range(10**9))`, which CPython answers instantly.

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
            .join(format!("pyrs-iterbuiltin-{tag}-{}", std::process::id())),
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
// sorted
// ---------------------------------------------------------------------------

#[test]
fn sorted_accepts_every_iterable() {
    matches_python(
        "sorted-all",
        r#"
print(sorted([3, 1, 2]))
print(sorted((3, 1, 2)))
print(sorted({3, 1, 2}))
print(sorted({"b": 1, "a": 2}))
print(sorted("cab"))
print(sorted(range(3, 0, -1)))
def g():
    yield 3
    yield 1
print(sorted(g()))
"#,
    );
}

#[test]
fn sorted_keeps_reverse_and_key_over_other_iterables() {
    matches_python(
        "sorted-kwargs",
        r#"
print(sorted({3, 1, 2}, reverse=True))
print(sorted("cab", reverse=True))
print(sorted({"bb": 1, "a": 2}, key=lambda s: len(s)))
print(sorted((3, 1, 2), key=lambda v: -v))
"#,
    );
}

// ---------------------------------------------------------------------------
// Numeric folds
// ---------------------------------------------------------------------------

#[test]
fn sum_max_min_accept_every_iterable() {
    matches_python(
        "folds",
        r#"
print(sum([1, 2, 3]), sum((1, 2, 3)), sum({1, 2, 3}), sum(range(5)))
print(max([1, 3, 2]), max((1, 3, 2)), max({1, 3, 2}), max(range(5)))
print(min([1, 3, 2]), min((1, 3, 2)), min({1, 3, 2}), min(range(1, 5)))
print(max("abc"), min("abc"))
print(max({"b": 1, "a": 2}), min({"b": 1, "a": 2}))
"#,
    );
}

// ---------------------------------------------------------------------------
// Constructors
// ---------------------------------------------------------------------------

#[test]
fn list_and_set_accept_every_iterable() {
    matches_python(
        "ctors",
        r#"
print(list((1, 2, 3)))
print(list({"b": 1, "a": 2}))
print(list("abc"))
print(list(range(4)))
print(sorted(list({3, 1, 2})))
print(len(set((1, 2, 2))), len(set(range(3))), len(set("aab")))
print(sorted(set({"b": 1, "a": 2})))
"#,
    );
}

#[test]
fn join_accepts_every_iterable_of_strings() {
    matches_python(
        "join",
        r#"
print(",".join(["a", "b"]))
print(",".join(("a", "b")))
print(",".join("abc"))
print(",".join(sorted({"b", "a"})))
print("-".join({"b": 1, "a": 2}) in ("a-b", "b-a"))
"#,
    );
}

// ---------------------------------------------------------------------------
// range specifically
// ---------------------------------------------------------------------------

#[test]
fn range_works_wherever_a_list_would() {
    matches_python(
        "range",
        r#"
print(list(range(4)))
print(list(range(1, 4)))
print(list(range(4, 0, -1)))
print(sum(range(5)), max(range(5)), min(range(2, 5)))
print(sorted(range(3, 0, -1)))
print(len(set(range(3))))
print(list(range(0)), sum(range(0)))
"#,
    );
}

#[test]
fn range_is_still_not_a_value_and_says_so() {
    let err = rejects("range-value", "r = range(3)\nprint(r)\n");
    assert!(err.contains("not a value here"), "{err}");
    // The message must name what does work, since a lot now does.
    assert!(err.contains("sorted"), "{err}");
}

// ---------------------------------------------------------------------------
// any / all keep short-circuiting
// ---------------------------------------------------------------------------

#[test]
fn any_and_all_gained_dict_without_losing_short_circuit() {
    matches_python(
        "any-all",
        r#"
d = {"a": 1}
e: dict[str, int] = {}
print(any(d), all(d))
print(any(e), all(e))
print(any([0, 1]), all((1, 2)), any({0}), all("ab"))

def loud():
    for i in range(4):
        print("visit", i)
        yield i

print(any(loud()))
"#,
    );
}

#[test]
fn any_over_a_range_is_rejected_rather_than_materialized() {
    // Materializing would answer `all(range(10**9))` by building a billion
    // elements, where CPython returns False on the first one.
    let err = rejects("any-range", "print(all(range(3)))\n");
    assert!(err.contains("not a value here"), "{err}");
}

// ---------------------------------------------------------------------------
// Nothing regressed
// ---------------------------------------------------------------------------

#[test]
fn lists_still_pass_through_untouched() {
    matches_python(
        "lists",
        r#"
xs = [3, 1, 2]
print(sorted(xs), xs)
ys = list(xs)
ys.append(9)
print(xs, ys)
print(sum(xs), max(xs), min(xs))
print(",".join(["a", "b"]))
"#,
    );
}

#[test]
fn tuple_of_a_tuple_is_unchanged() {
    // tuple() is deliberately not materialized: tuples are fixed-arity here,
    // so it needs the tuple itself, and a list is exactly what it cannot take.
    matches_python(
        "tuple-ctor",
        r#"
t = (1, 2)
print(tuple(t))
print(len(tuple(t)))
"#,
    );
}
