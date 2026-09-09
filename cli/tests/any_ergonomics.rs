//! Working with `Any`: getting a value in, and getting it back out.
//!
//! `Any` already existed as a scalar dynamic box with a runtime-checked
//! extraction, but two gaps made it impractical for the shape that wants it —
//! a table with columns of different types.
//!
//! **Getting in.** `xs: list[Any] = ["a", "b"]` worked, because an assignment
//! to a *name* propagates the expected element type into the literal and each
//! element boxes at construction. The same literal at an index or attribute
//! target did not: the hint was dropped and the literal inferred `list[str]`,
//! which is a different runtime encoding (a `list[Any]` slot holds a box
//! wrapping the string, not the string). The hint now reaches those targets.
//!
//! It is a hint on the *literal*, not a conversion of a value. Assigning an
//! already-typed `list[str]` into a `list[Any]` slot is still refused, and
//! deliberately: it would be an O(n) re-box into a fresh list, which breaks
//! aliasing — after `f.cols["k"] = xs`, an `xs.append(...)` would no longer be
//! visible through the frame.
//!
//! **Getting out.** `isinstance(x, int)` compiled to a runtime tag check but
//! did not narrow, so `x` stayed `Any` inside the guard and every use needed
//! an explicit `y: int = x`. It now peels, for a single non-container pattern.

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
            .join(format!("pyrs-any-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    (dir, src)
}

/// Differential at every optimization level, plus GC stress: boxing allocates
/// one `PyrsUnionBox` per element, so a mis-rooted box is a use-after-free.
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

// ------------------------------------------------------ the hint reaching

/// The case this milestone exists for: a table with columns of different
/// types, built by assigning literals into a `dict[str, list[Any]]`.
#[test]
fn a_literal_reaches_a_dict_value_slot() {
    matches_python(
        "frame",
        r#"
from typing import Any

class Frame:
    def __init__(self) -> None:
        self.cols: dict[str, list[Any]] = {}

f = Frame()
f.cols["name"] = ["a", "b"]
f.cols["age"] = [30, 40]
f.cols["score"] = [1.5, 2.5]
f.cols["mixed"] = [1, "two", 3.0]
for k in sorted(f.cols.keys()):
    print(k, f.cols[k])
print(len(f.cols))
"#,
    );
}

/// The same hint at an attribute target, and through a plain local dict, so
/// the probe is exercised on each shape of base it walks.
#[test]
fn the_hint_reaches_attribute_and_local_targets() {
    matches_python(
        "targets",
        r#"
from typing import Any

class Bag:
    def __init__(self) -> None:
        self.items: list[Any] = []
        self.index: dict[str, Any] = {}

b = Bag()
b.items = ["x", 1, 2.5]
b.index["k"] = "v"
b.index["n"] = 7
print(b.items, b.index["k"], b.index["n"])

d: dict[str, list[Any]] = {}
d["row"] = [1, "two"]
print(d["row"])

nested: list[list[Any]] = []
nested.append(["a", 1])
nested.insert(0, [2.5, "b"])
print(nested)
"#,
    );
}

/// Homogeneous inference is unchanged where no `Any` is involved: the hint is
/// exact rather than a join guess, so it must not loosen an ordinary list.
#[test]
fn a_concrete_slot_type_still_checks_elements() {
    matches_python(
        "concrete",
        r#"
class Holder:
    def __init__(self) -> None:
        self.nums: list[int] = []
        self.table: dict[str, list[int]] = {}

h = Holder()
h.nums = [1, 2, 3]
h.table["a"] = [4, 5]
print(h.nums, h.table["a"])
"#,
    );
    let msg = rejects(
        "concrete-bad",
        "class H:\n    def __init__(self) -> None:\n        self.nums: list[int] = []\n\
         h = H()\nh.nums = [1, \"two\"]\n",
    );
    assert!(
        msg.contains("int") && msg.contains("str"),
        "an int list must still reject a str element: {msg}"
    );
}

/// A value that already has a narrower list type is *not* converted. The
/// re-box would be O(n) and would break aliasing, so it stays an error.
#[test]
fn an_already_typed_list_is_not_silently_reboxed() {
    let msg = rejects(
        "no-rebox",
        "from typing import Any\n\
         class F:\n    def __init__(self) -> None:\n        self.cols: dict[str, list[Any]] = {}\n\
         xs: list[str] = [\"a\"]\n\
         f = F()\n\
         f.cols[\"k\"] = xs\n",
    );
    assert!(
        msg.contains("list[Any]") && msg.contains("list[str]"),
        "should name both list types: {msg}"
    );
}

// -------------------------------------------------------- getting it out

/// `isinstance` narrows an `Any`, so the guarded body can use the value
/// directly instead of restating its type.
#[test]
fn isinstance_narrows_an_any() {
    matches_python(
        "narrow",
        r#"
from typing import Any

def describe(x: Any) -> str:
    if isinstance(x, int):
        return "int:" + str(x + 1)
    if isinstance(x, str):
        return "str:" + x.upper()
    if isinstance(x, float):
        return "float:" + str(x * 2.0)
    return "other"

vals: list[Any] = [1, "ab", 2.5, True, None]
for v in vals:
    print(describe(v))
"#,
    );
}

/// The reduction a typed column wants: walk `list[Any]`, narrow per element.
#[test]
fn narrowing_drives_a_column_reduction() {
    matches_python(
        "column",
        r#"
from typing import Any

def numeric_total(col: list[Any]) -> float:
    acc = 0.0
    for cell in col:
        if isinstance(cell, int):
            acc += float(cell)
        elif isinstance(cell, float):
            acc += cell
    return acc

def count_strings(col: list[Any]) -> int:
    n = 0
    for cell in col:
        if isinstance(cell, str):
            n += len(cell)
    return n

col: list[Any] = [1, 2.5, "skip", 3, "ab"]
print(numeric_total(col), count_strings(col))
"#,
    );
}

/// The narrowing composes with the other refinement positions: a conditional
/// expression, an `and` chain, and an `else` arm that keeps `Any`.
#[test]
fn any_narrowing_composes_with_other_refinements() {
    matches_python(
        "compose",
        r#"
from typing import Any

def bump(x: Any) -> int:
    return x + 1 if isinstance(x, int) else 0

def long_string(x: Any) -> bool:
    return isinstance(x, str) and len(x) > 2

def either(x: Any) -> str:
    if isinstance(x, int):
        return "n" + str(x * 2)
    else:
        return "?"

vals: list[Any] = [41, "abc", "ab", 2.5]
for v in vals:
    print(bump(v), long_string(v), either(v))
"#,
    );
}

/// `bool` is a subclass of `int` in CPython, and the tag check has to agree:
/// `isinstance(True, int)` is True and the narrowed value arithmetics as 1.
#[test]
fn bool_narrows_as_an_int() {
    matches_python(
        "bool-subtype",
        r#"
from typing import Any

def as_int(x: Any) -> int:
    if isinstance(x, int):
        return x + 10
    return -1

vals: list[Any] = [True, False, 5]
for v in vals:
    print(as_int(v))
"#,
    );
}

/// Declined shapes stay declined rather than guessing: a container pattern
/// has no element type to peel to, and a multi-pattern tuple would need a
/// union whose member indices do not exist in the box's tag space. Both leave
/// `Any` in place, so the explicit restatement still works.
#[test]
fn container_and_multi_patterns_leave_any_alone() {
    matches_python(
        "declined",
        r#"
from typing import Any

def kind(x: Any) -> str:
    if isinstance(x, (int, float)):
        return "number"
    if isinstance(x, str):
        return "text"
    return "other"

vals: list[Any] = [1, 2.5, "s"]
for v in vals:
    print(kind(v))

def unwrap(x: Any) -> int:
    if isinstance(x, int):
        y: int = x
        return y
    return 0

print(unwrap(7), unwrap("no"))
"#,
    );
}
