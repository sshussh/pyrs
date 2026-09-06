//! Tuple dict and set keys, and the `d[i, j]` subscript they unblock.
//!
//! Keys were restricted to `int` and `str`: the runtime could hash only those
//! two tags, so a composite key had to be flattened into a string by hand.
//! That is the shape a transition table, a sparse grid or a two-argument memo
//! wants, so the workaround showed up constantly.
//!
//! Equality was never the missing piece — `slot_eq` already compared tuples
//! structurally. Only `hash_key` lacked a `TAG_TUPLE` arm; it now folds the
//! element hashes, recursing for nested tuples. The hash is internal and never
//! observed, so any mix that agrees with the existing equality works.
//!
//! `bool` stays rejected on purpose: CPython's `True == 1` would make a bool
//! key collide with an int one, and this subset does not model that.

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
            .join(format!("pyrs-tuplekeys-{tag}-{}", std::process::id())),
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
// Dict keys
// ---------------------------------------------------------------------------

#[test]
fn a_tuple_key_round_trips_through_a_dict_literal() {
    matches_python(
        "literal",
        r#"
d = {(1, "a"): 10, (2, "b"): 20, (3, "c"): 30}
print(len(d))
print(d[(1, "a")], d[(2, "b")], d[(3, "c")])
print((2, "b") in d, (2, "c") in d)
"#,
    );
}

#[test]
fn tuple_keys_are_compared_by_value_not_identity() {
    matches_python(
        "by-value",
        r#"
d: dict[tuple[str, int], str] = {}
a = "he" + "llo"
d[(a, 1 + 1)] = "stored"
print(("hello", 2) in d, d[("hello", 2)])
print(("hello", 3) in d, ("world", 2) in d)
"#,
    );
}

#[test]
fn a_repeated_tuple_key_overwrites_rather_than_duplicating() {
    matches_python(
        "overwrite",
        r#"
d: dict[tuple[int, int], int] = {}
d[(1, 2)] = 1
d[(1, 2)] = 2
d[(2, 1)] = 3
print(len(d), d[(1, 2)], d[(2, 1)])
"#,
    );
}

#[test]
fn tuple_key_order_matters() {
    matches_python(
        "order",
        r#"
d = {(1, 2): "forward", (2, 1): "reverse"}
print(len(d), d[(1, 2)], d[(2, 1)])
"#,
    );
}

#[test]
fn tuple_keys_of_every_supported_arity_work() {
    matches_python(
        "arity",
        r#"
one: dict[tuple[int], str] = {}
one[(5,)] = "one"
three: dict[tuple[int, str, int], str] = {}
three[(1, "x", 2)] = "three"
print(one[(5,)], three[(1, "x", 2)])
print((5,) in one, (6,) in one, (1, "y", 2) in three)
"#,
    );
}

#[test]
fn the_empty_tuple_is_a_valid_key() {
    matches_python(
        "empty-tuple",
        r#"
d: dict[tuple[()], int] = {}
d[()] = 1
print(len(d), () in d, d[()])
"#,
    );
}

#[test]
fn nested_tuple_keys_hash_recursively() {
    matches_python(
        "nested",
        r#"
d: dict[tuple[str, tuple[int, int]], int] = {}
d[("grid", (1, 2))] = 12
d[("grid", (2, 1))] = 21
print(len(d), d[("grid", (1, 2))], d[("grid", (2, 1))])
print(("grid", (1, 3)) in d)
"#,
    );
}

#[test]
fn get_and_pop_take_tuple_keys() {
    matches_python(
        "get-pop",
        r#"
d = {(1, "a"): 1, (2, "b"): 2}
print(d.get((1, "a"), -1), d.get((9, "z"), -1))
print(d.pop((1, "a"), -1), d.pop((9, "z"), -1))
print(len(d))
"#,
    );
}

#[test]
fn a_tuple_key_can_be_deleted() {
    matches_python(
        "delete",
        r#"
d = {(1, "a"): 1, (2, "b"): 2}
del d[(1, "a")]
print(len(d), (1, "a") in d, (2, "b") in d)
"#,
    );
}

#[test]
fn iterating_a_tuple_keyed_dict_yields_the_tuples() {
    matches_python(
        "iterate",
        r#"
d = {(1, "a"): 10, (2, "b"): 20}
for k in sorted(d.keys()):
    print(k, k[0], k[1], d[k])
for k, v in sorted(d.items()):
    print(k, v)
"#,
    );
}

#[test]
fn a_tuple_key_is_printed_as_a_tuple() {
    // `str(k)` is a separate gap -- `str()` and f-strings cannot stringify any
    // container yet, only `print` can. Unrelated to keys.
    matches_python(
        "print",
        r#"
d = {(1, "a"): 10, (2, "b"): 20}
for k in sorted(d):
    print(k, d[k])
"#,
    );
}

// ---------------------------------------------------------------------------
// Set elements
// ---------------------------------------------------------------------------

#[test]
fn a_set_deduplicates_equal_tuples() {
    matches_python(
        "set-dedup",
        r#"
s: set[tuple[str, int]] = set()
s.add(("a", 1))
s.add(("a", 1))
s.add(("b", 2))
print(len(s), ("a", 1) in s, ("z", 0) in s)
print(sorted(s))
"#,
    );
}

#[test]
fn a_tuple_element_can_be_removed_from_a_set() {
    matches_python(
        "set-remove",
        r#"
s = {(1, 2), (3, 4)}
s.discard((1, 2))
s.discard((9, 9))
print(len(s), sorted(s))
"#,
    );
}

// ---------------------------------------------------------------------------
// Comprehensions
// ---------------------------------------------------------------------------

#[test]
fn a_dict_comprehension_can_build_tuple_keys() {
    matches_python(
        "dict-comp",
        r#"
d = {(i, i + 1): i * i for i in range(4)}
print(len(d), sorted(d.items()))
print(d[(2, 3)])
"#,
    );
}

#[test]
fn a_set_comprehension_can_build_tuple_elements() {
    matches_python(
        "set-comp",
        r#"
words = ["x", "yy", "x", "zzz"]
s = {(w, len(w)) for w in words}
print(len(s), sorted(s))
"#,
    );
}

// ---------------------------------------------------------------------------
// `d[i, j]` subscripts
// ---------------------------------------------------------------------------

#[test]
fn a_bare_tuple_subscript_indexes_a_dict() {
    matches_python(
        "bare-subscript",
        r#"
grid: dict[tuple[int, int], int] = {}
for i in range(3):
    for j in range(3):
        grid[i, j] = i * 3 + j
print(grid[1, 2], grid[0, 0], grid[2, 2])
print((1, 1) in grid, len(grid))
"#,
    );
}

#[test]
fn a_bare_tuple_subscript_accepts_arbitrary_expressions() {
    matches_python(
        "subscript-exprs",
        r#"
d: dict[tuple[int, str], int] = {}
n = 2
d[n * 2, "a" + "b"] = 7
print(d[4, "ab"], d[(4, "ab")])
"#,
    );
}

#[test]
fn a_trailing_comma_makes_a_one_tuple_subscript() {
    matches_python(
        "subscript-one-tuple",
        r#"
d: dict[tuple[int], str] = {}
d[3,] = "three"
print(d[3,], d[(3,)])
"#,
    );
}

// ---------------------------------------------------------------------------
// Programs
// ---------------------------------------------------------------------------

#[test]
fn a_transition_table_is_keyed_by_state_and_event() {
    matches_python(
        "transitions",
        r#"
TRANSITIONS = {
    ("idle", "go"): "running",
    ("running", "stop"): "idle",
    ("running", "pause"): "paused",
    ("paused", "go"): "running",
}


def run(events: list[str]) -> list[str]:
    state = "idle"
    trace = [state]
    for e in events:
        key = (state, e)
        if key in TRANSITIONS:
            state = TRANSITIONS[key]
        trace.append(state)
    return trace


print(run(["go", "pause", "go", "stop"]))
print(run(["stop", "go", "bogus"]))
"#,
    );
}

#[test]
fn a_two_argument_memo_is_keyed_by_a_tuple() {
    matches_python(
        "memo",
        r#"
memo: dict[tuple[int, int], int] = {}


def paths(r: int, c: int) -> int:
    if r == 0 or c == 0:
        return 1
    k = (r, c)
    if k in memo:
        return memo[k]
    v = paths(r - 1, c) + paths(r, c - 1)
    memo[k] = v
    return v


print(paths(8, 8), len(memo))
"#,
    );
}

#[test]
fn tuple_keyed_counting_survives_collection() {
    matches_python(
        "gc-stress",
        r#"
counts: dict[tuple[str, int], int] = {}
for i in range(2000):
    key = ("k" + str(i % 500), i % 7)
    counts[key] = counts.get(key, 0) + 1
    junk = [str(j) for j in range(50)]
    if len(junk) == 0:
        print("unreachable")
total = 0
for v in counts.values():
    total += v
print(len(counts), total, counts[("k0", 0)])
"#,
    );
}

// ---------------------------------------------------------------------------
// Rejections
// ---------------------------------------------------------------------------

#[test]
fn an_unhashable_key_type_is_still_rejected() {
    let msg = rejects(
        "unhashable-key",
        r#"
d: dict[float, int] = {}
d[1.5] = 1
print(len(d))
"#,
    );
    assert!(
        msg.contains("int, str, or a tuple of those"),
        "unexpected diagnostic: {msg}"
    );
}

#[test]
fn a_bool_inside_a_key_tuple_is_rejected() {
    // Not an oversight: CPython's `True == 1` would make `(True, 1)` and
    // `(1, 1)` the same key, which the tag-checking tuple equality does not do.
    let msg = rejects(
        "bool-in-tuple",
        r#"
d: dict[tuple[bool, int], int] = {}
d[(True, 1)] = 1
print(len(d))
"#,
    );
    assert!(
        msg.contains("tuple[bool, int]"),
        "unexpected diagnostic: {msg}"
    );
}

#[test]
fn a_tuple_of_unhashable_elements_is_rejected() {
    let msg = rejects(
        "unhashable-tuple",
        r#"
d: dict[tuple[int, float], int] = {}
d[(1, 1.5)] = 1
print(len(d))
"#,
    );
    assert!(
        msg.contains("tuple[int, float]"),
        "unexpected diagnostic: {msg}"
    );
}
