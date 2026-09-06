//! `key=` may return a tuple (or list), for multi-criteria sorting.
//!
//! `sorted(items, key=lambda p: (-p[1], p[0]))` is *the* way to sort by more
//! than one criterion in Python, and it was rejected: the key's return type
//! had to be a bare scalar. Tuples were already orderable everywhere else —
//! `(1, 2) < (1, 3)`, `sorted(list_of_tuples)`, `min` and `max` of tuples all
//! worked — so the restriction sat only on the key path.
//!
//! Found by running small realistic programs against CPython rather than by
//! probing constructs: a word-frequency script needed exactly this.

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
            .join(format!("pyrs-tuplekey-{tag}-{}", std::process::id())),
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
// The idiom
// ---------------------------------------------------------------------------

#[test]
fn sorted_accepts_a_tuple_key() {
    matches_python(
        "sorted",
        r#"
xs = ["bb", "a", "cc", "b"]
print(sorted(xs, key=lambda s: (len(s), s)))
print(sorted(xs, key=lambda s: (len(s), s), reverse=True))
print(sorted(xs, key=lambda s: (len(s), s, len(s))))
"#,
    );
}

#[test]
fn descending_then_ascending_is_the_common_shape() {
    // Sort by count descending, then by name ascending — what a frequency
    // table needs, and what a scalar key cannot express.
    matches_python(
        "multi-criteria",
        r#"
d = [("a", 2), ("b", 1), ("c", 2)]
print(sorted(d, key=lambda p: (-p[1], p[0])))
print(sorted(d, key=lambda p: (p[1], p[0])))
"#,
    );
}

#[test]
fn list_sort_accepts_a_tuple_key() {
    matches_python(
        "list-sort",
        r#"
xs = ["bb", "a", "cc", "b"]
xs.sort(key=lambda s: (len(s), s))
print(xs)
xs.sort(key=lambda s: (len(s), s), reverse=True)
print(xs)
"#,
    );
}

#[test]
fn min_and_max_accept_a_tuple_key() {
    matches_python(
        "min-max",
        r#"
d = [("a", 2), ("b", 1), ("c", 2)]
print(min(d, key=lambda p: (p[1], p[0])))
print(max(d, key=lambda p: (p[1], p[0])))
print(min(("z", 1), ("a", 2), key=lambda p: (p[1], p[0])))
print(max(("z", 1), ("a", 2), key=lambda p: (p[1], p[0])))
"#,
    );
}

#[test]
fn a_named_function_may_return_the_tuple_key() {
    matches_python(
        "named-fn",
        r#"
def by_len(s: str) -> tuple[int, str]:
    return (len(s), s)

xs = ["bb", "a", "cc"]
print(sorted(xs, key=by_len))
print(min(xs, key=by_len), max(xs, key=by_len))
"#,
    );
}

#[test]
fn tuple_keys_work_over_every_element_type() {
    matches_python(
        "elem-types",
        r#"
print(sorted([1.5, 1.0, 2.0], key=lambda v: (v, v)))
print(sorted([3, 1, 2], key=lambda v: (-v, v)))
print(sorted(["b", "a"], key=lambda s: (s, s)))
print(sorted([True, False], key=lambda b: (b, b)))
"#,
    );
}

#[test]
fn a_list_key_works_too() {
    // `is_orderable_ty` already covered lists, and CPython compares them
    // lexicographically the same way.
    matches_python(
        "list-key",
        r#"
xs = [3, 1, 2]
print(sorted(xs, key=lambda v: [v]))
print(sorted(xs, key=lambda v: [-v, v]))
"#,
    );
}

// ---------------------------------------------------------------------------
// The realistic case that found this
// ---------------------------------------------------------------------------

#[test]
fn a_word_frequency_table_sorts_correctly() {
    matches_python(
        "wordcount",
        r#"
text = "the quick brown fox jumps over the lazy dog the fox"
counts: dict[str, int] = {}
for w in text.split():
    counts[w] = counts.get(w, 0) + 1
pairs = sorted(counts.items(), key=lambda p: (-p[1], p[0]))
for word, n in pairs[:3]:
    print("%-8s %d" % (word, n))
"#,
    );
}

// ---------------------------------------------------------------------------
// Still rejected
// ---------------------------------------------------------------------------

#[test]
fn a_key_type_with_no_ordering_is_still_rejected() {
    let err = rejects(
        "unorderable",
        "def bad(x: int) -> dict[str, int]:\n    return {\"k\": x}\nprint(sorted([1, 2], key=bad))\n",
    );
    assert!(err.contains("must be sortable"), "{err}");
    // The message has to name what does work, since tuples and lists now do.
    assert!(err.contains("tuple or list"), "{err}");
}

// ---------------------------------------------------------------------------
// Nothing regressed
// ---------------------------------------------------------------------------

#[test]
fn scalar_keys_and_keyless_sorting_are_unchanged() {
    matches_python(
        "scalars",
        r#"
xs = ["bb", "a", "cc"]
print(sorted(xs), sorted(xs, key=len), sorted(xs, reverse=True))
ns = [3, 1, 2]
print(sorted(ns), sorted(ns, key=lambda v: -v), min(ns), max(ns))
print(sorted([(2, "b"), (1, "a")]))
print(min([(2, "b"), (1, "a")]), max([(2, "b"), (1, "a")]))
"#,
    );
}
