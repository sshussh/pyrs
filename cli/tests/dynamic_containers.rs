//! Reading a container that is only known dynamically.
//!
//! `object` (`Any`) is a `{ i32 print_tag; i64 payload }` box, and a container
//! inside one carries the tag of its *own* element type: a `list[int]` is tag
//! 4, a `list[object]` is 68. Nothing could read either without first naming
//! which, so a library function taking `object` — the shape every serialiser
//! has — could not walk what it was handed.
//!
//! Narrowing `object` to a fixed container type was tried in 0.139 and
//! reverted: `isinstance(v, list)` is true for both encodings, so peeling to
//! one of them breaks the other at every read. The operations here take the
//! other route and read the value *through* its own tag, which needs no peel,
//! no copy, and so does not break aliasing.
//!
//! `len`, `v[i]`, `v[k]`, `v.keys()` and iteration all work this way. A tuple
//! reads too: it carries a tag per slot, so its elements come back exactly
//! typed even though a tuple has no single element type.

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
            .join(format!("pyrs-dyn-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    (dir, src)
}

/// Differential at every optimization level, plus GC stress: every read of a
/// dynamic element allocates a box, so a mis-rooted one is a use-after-free.
fn matches_python(tag: &str, source: &str) {
    let (_dir, src) = write_prog(tag, source);
    let expected = Command::new("python3").arg(&src).output().unwrap();
    assert!(
        expected.status.success(),
        "CPython failed for {tag}: {}",
        String::from_utf8_lossy(&expected.stderr)
    );
    let want = String::from_utf8_lossy(&expected.stdout);

    for (opt, stress) in [("0", false), ("2", false), ("3", false), ("2", true)] {
        let mut cmd = Command::new(PYRS);
        cmd.args(["run", "--no-cache", "-O", opt, "-i"]).arg(&src);
        if stress {
            cmd.env("PYRS_GC_STRESS", "1");
        }
        let actual = cmd.output().unwrap();
        let label = if stress { "gc-stress" } else { "default" };
        assert!(
            actual.status.success(),
            "PyRs {tag} at -O{opt} ({label}) failed:\n{}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&actual.stdout),
            want,
            "{tag} differs from CPython at -O{opt} ({label})"
        );
    }
}

/// Run under PyRs alone and compare to recorded output. For the cases where
/// the value has no CPython spelling, or where the message is ours.
fn outputs(tag: &str, source: &str, want: &str) {
    let (_dir, src) = write_prog(tag, source);
    let actual = Command::new(PYRS)
        .args(["run", "--no-cache", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(
        actual.status.success(),
        "PyRs {tag} failed:\n{}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&actual.stdout), want, "{tag}");
}

/// `len` reads the count out of the first i64 of every sized object, which is
/// why `cplen` leads `PyrsStr`. All four shapes, in both encodings.
#[test]
fn len_reads_any_sized_value() {
    matches_python(
        "len",
        r#"
xs: list[int] = [10, 20, 30]
a: object = xs
print(len(a))

mixed: list[object] = [1, "b", True]
b: object = mixed
print(len(b))

d: dict[str, int] = {"x": 7, "y": 8}
c: object = d
print(len(c))

e: object = "abcd"
print(len(e))

t: tuple[int, str] = (1, "a")
f: object = t
print(len(f))
"#,
    );
}

/// Indexing. The concrete `list[int]` is the case that matters: its elements
/// are raw i64 slots, so the read has to box each one on the way out, using
/// the element tag encoded in the list's own container tag.
#[test]
fn indexing_reads_through_the_values_own_tag() {
    matches_python(
        "index",
        r#"
xs: list[int] = [10, 20, 30]
a: object = xs
print(a[0], a[2], a[-1])

mixed: list[object] = [1, "b", True]
b: object = mixed
print(b[0], b[1], b[2])

names: list[str] = ["p", "q"]
c: object = names
print(c[0], c[1])

d: dict[str, int] = {"x": 7, "y": 8}
e: object = d
print(e["x"], e["y"])
"#,
    );
}

/// A tuple's slots are individually tagged, so a heterogeneous tuple reads
/// back exactly — which a list, with one tag for the whole container, could
/// only do by boxing every element.
#[test]
fn a_tuple_reads_element_by_element() {
    matches_python(
        "tuple",
        r#"
t: tuple[int, str, bool, float] = (1, "a", True, 2.5)
v: object = t
print(v[0], v[1], v[2], v[3])
print(v[-1], v[-4])
for item in v:
    print(item)
"#,
    );
}

/// Iterating a dynamic value yields what Python's `for` yields for the thing
/// it holds: a list's elements, a dict's keys, a str's characters. This is a
/// different operation from `v[i]` — `d[0]` on a dict looks up the key `0`.
#[test]
fn iteration_yields_what_the_value_holds() {
    matches_python(
        "iter",
        r#"
xs: list[int] = [10, 20, 30]
a: object = xs
for x in a:
    print(x)

d: dict[str, int] = {"x": 7, "y": 8}
b: object = d
for k in b:
    print(k)

c: object = "abc"
for ch in c:
    print(ch)

mixed: list[object] = [1, "b", True]
e: object = mixed
for m in e:
    print(m)
"#,
    );
}

/// A dynamic key carries its own tag, so it hashes and compares as whatever
/// it holds — which is what lets a dict be walked without knowing its key
/// type in advance.
#[test]
fn a_dynamic_key_indexes_a_dynamic_dict() {
    matches_python(
        "dyn_key",
        r#"
d: dict[str, int] = {"x": 7, "y": 8}
a: object = d
for k in a:
    print(k, a[k])

counts: dict[int, str] = {1: "one", 2: "two"}
b: object = counts
for n in b:
    print(n, b[n])
"#,
    );
}

/// `.keys()` yields a `list[str]`, so a dict keyed by anything else has to be
/// refused rather than read an int slot back as a pointer. Iterating the dict
/// is the spelling that works for those, and the message says so.
#[test]
fn keys_is_for_str_keyed_dicts_and_says_so() {
    matches_python(
        "keys",
        r#"
d: dict[str, int] = {"x": 7, "y": 8}
a: object = d
for k in a.keys():
    print(k, a[k])
"#,
    );
    outputs(
        "keys_refused",
        r#"
counts: dict[int, str] = {1: "one"}
b: object = counts
try:
    print(b.keys())
except TypeError as exc:
    print("TypeError:", exc)
"#,
        "TypeError: .keys() on a dynamic dict needs str keys\n",
    );
}

/// The reads compose: a recursive walk over a value whose shape is only known
/// at run time, which is the whole point of the operations.
#[test]
fn a_recursive_walk_handles_every_shape() {
    matches_python(
        "walk",
        r#"
def size(value: object) -> int:
    if isinstance(value, str):
        return 1
    if isinstance(value, (list, tuple)):
        total: int = 1
        for item in value:
            total += size(item)
        return total
    if isinstance(value, dict):
        subtotal: int = 1
        for key in value:
            subtotal += size(value[key])
        return subtotal
    return 1


ints: list[int] = [1, 2, 3]
print(size(ints))
print(size({"a": ints, "b": ints}))
print(size([[1, 2], [3]]))
print(size((1, "x", [1, 2])))
print(size("hello"))
print(size(4))
"#,
    );
}

/// Out of range and the wrong shape raise, rather than reading past the end
/// or reinterpreting a slot.
#[test]
fn a_bad_read_raises() {
    outputs(
        "bad_read",
        r#"
xs: list[int] = [1, 2]
a: object = xs
try:
    print(a[5])
except IndexError as exc:
    print("IndexError:", exc)

n: object = 42
try:
    print(len(n))
except TypeError as exc:
    print("TypeError:", exc)

try:
    print(n[0])
except TypeError as exc:
    print("TypeError:", exc)
"#,
        "IndexError: list index out of range\n\
         TypeError: object of this dynamic type has no len()\n\
         TypeError: dynamic value is not a list\n",
    );
}
