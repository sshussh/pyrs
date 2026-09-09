//! A container literal keeps each element's own type.
//!
//! `[1, "a"]` was `list elements must share one type; found int and str`,
//! while `xs: list[int | str] = [1, "a"]` compiled and ran correctly. The
//! representation was never the obstacle — 0.89 built exactly this rule for
//! mixed *numeric* literals (`[1, 2.5]` is `list[int | float]`, keeping each
//! element's own type so printing and `==` match CPython) and scoped the rest
//! out. The inference now covers any pair that can be a tagged slot.
//!
//! Two boundaries are real and tested here rather than assumed:
//!
//! - **`File` declines**, because a union member is stored as a tagged slot
//!   and `elem_tag` has no tag for a file handle. A representation limit, not
//!   a policy choice.
//! - **An empty `[]` still yields.** It is a *provisional* `list[Any]`, so
//!   `{"x": [1], "y": []}` must stay a `dict[str, list[int]]` rather than
//!   becoming a dict of two different list types.

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
            .join(format!("pyrs-het-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    (dir, src)
}

/// Differential at every optimization level, plus GC stress: each element of
/// a union-typed container is a heap box, so a mis-rooted one is a
/// use-after-free rather than a wrong value.
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

fn rejects(tag: &str, source: &str) -> String {
    let (_dir, src) = write_prog(tag, source);
    let out = Command::new(PYRS)
        .args(["check", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "{tag} was expected to be rejected but compiled"
    );
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// The shape that prompted this: a literal with no target to take a hint
/// from, which an annotation cannot reach.
#[test]
fn a_bare_literal_in_a_for_iterable() {
    matches_python(
        "for-iterable",
        r#"
for v in [1, "ab", 2.5, True, None]:
    print(v)
for pair in [(1, "a"), (2, "b")]:
    print(pair)
total = 0
for n in [1, 2, "skip", 4]:
    if isinstance(n, int):
        total += n
print(total)
"#,
    );
}

/// Every element operation the annotated spelling already supported.
#[test]
fn a_mixed_list_prints_indexes_and_iterates() {
    matches_python(
        "operations",
        r#"
xs = [1, "a", 2.5, True, None]
print(xs)
print(len(xs), xs[0], xs[1], xs[-1])
print(xs[1:3])
for v in xs:
    print(v)
print(str(xs), repr(xs))
print(f"{xs}")
"#,
    );
}

/// Each element keeps its own type rather than promoting, which is the whole
/// reason this is a union and not a widened single type.
#[test]
fn elements_keep_their_own_types() {
    matches_python(
        "fidelity",
        r#"
xs = [1, 2.5, True, "1"]
print(xs)
for v in xs:
    print(v)
# `v == 1` on the union itself is a separate, pre-existing gap -- it fails
# for an annotated `list[int | str]` too -- so the working idiom is to narrow
# first. Two members here, not four: `isinstance(v, int)` over a union that
# also contains `bool` is a multi-member peel, which keeps storage by design.
pair = [1, "a"]
for item in pair:
    if isinstance(item, int):
        print("int", item + 1)
    else:
        print("str", item)
nums = [1, 2.5]
print(nums, nums[0], nums[1])
"#,
    );
}

/// Nested containers, classes and callables as elements — anything with a
/// print tag can be a union member.
#[test]
fn nested_and_object_elements() {
    matches_python(
        "nested",
        r#"
class Dog:
    def __repr__(self) -> str:
        return "Dog"

class Cat:
    def __repr__(self) -> str:
        return "Cat"

print([[1], "s"])
print([{"a": 1}, [2], (3,)])
print([Dog(), Cat()])
print([[1, "a"], [2.5, None]])
pets = [Dog(), Cat(), Dog()]
print(pets, len(pets))
"#,
    );
}

/// Dict *values* join the same way; dict *keys* do not, because a key has to
/// be hashable and a union is not a supported key type.
#[test]
fn dict_values_join_but_keys_stay_restricted() {
    matches_python(
        "dict-values",
        r#"
d = {"a": 1, "b": "two", "c": 3.5}
print(d)
for k in sorted(d.keys()):
    print(k, d[k])
"#,
    );
    let msg = rejects("dict-keys", "d = {1: \"a\", \"b\": 2}\nprint(d)\n");
    assert!(
        msg.contains("dict keys") && msg.contains("int | str"),
        "a mixed key type should be named and refused: {msg}"
    );
    let msg = rejects("set-elems", "s = {1, \"a\"}\nprint(s)\n");
    assert!(
        msg.contains("set keys/elements"),
        "a mixed set element type should be refused: {msg}"
    );
}

/// The one pair the union fallback declines, and the reason it must.
#[test]
fn a_file_handle_has_no_slot_tag() {
    let msg = rejects(
        "file-elem",
        "f = open(\"/tmp/_pyrs_het.txt\", \"w\")\nxs = [f, 1]\nprint(len(xs))\n",
    );
    assert!(
        msg.contains("share one type") && msg.contains("file"),
        "a file element should still be refused by name: {msg}"
    );
}

/// An empty `[]` is provisional and must yield to a concrete list type rather
/// than union with it — the regression this change first introduced.
#[test]
fn an_empty_list_still_yields_to_a_concrete_one() {
    matches_python(
        "provisional",
        r#"
d = {"x": [1, 2], "y": []}
print(d)
print(len(d["x"]), len(d["y"]))
d["y"].append(3)
print(d)

rows: list[list[int]] = [[1], []]
rows[1].append(9)
print(rows)
"#,
    );
}

/// Storage is still fixed by the literal, unchanged: a later `append` of a
/// type the literal did not contain is refused, exactly as it was for a
/// homogeneous literal before this change. `xs = []` plus appends still
/// grows, and an annotation still widens.
#[test]
fn the_literal_still_fixes_the_storage_type() {
    let msg = rejects(
        "fixed-storage",
        "xs = [1, \"a\"]\nxs.append(2.5)\nprint(xs)\n",
    );
    assert!(
        msg.contains("expected int | str") && msg.contains("found float"),
        "should name the fixed element type: {msg}"
    );
    matches_python(
        "grown-and-annotated",
        r#"
grown = []
grown.append(1)
grown.append("a")
print(grown)

widened: list[int | str | float] = [1, "a"]
widened.append(2.5)
print(widened)
"#,
    );
}
