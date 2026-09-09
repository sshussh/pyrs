//! `str()` / `repr()` of containers, and the `repr` / `ascii` builtins.
//!
//! `print([1, 2])` wrote `[1, 2]`, but `str([1, 2])` and `f"{xs}"` were
//! rejected outright -- the formatting logic existed, it just could not be
//! reached from anything but `print`. That made the most ordinary line in a
//! Python program, `print(f"result: {xs}")`, impossible to write.
//!
//! The fix is that the print routines now write through an output sink, and
//! `str()` captures what `print` would have emitted. So the two agree by
//! construction rather than by two implementations kept in step -- element
//! reprs, quoting, nesting and all.
//!
//! `repr()` and `ascii()` also did not exist as builtins, only as f-string
//! `!r` / `!a` conversions; they are the same lowering, now reachable by
//! name. `ascii()` of a container stays rejected: it would have to escape
//! non-ASCII *inside* the elements, which the shared rendering does not do.
//!
//! Set iteration is insertion-ordered here and hash-ordered in CPython, a
//! pre-existing documented divergence, so these cases build sets in an order
//! the two agree on.

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
            .join(format!("pyrs-containerstr-{tag}-{}", std::process::id())),
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
// str() of each container
// ---------------------------------------------------------------------------

#[test]
fn str_of_a_list_matches_print() {
    matches_python(
        "list",
        r#"
xs = [1, 2, 3]
print(xs)
print(str(xs))
print(str([1.5, 2.0]), str([True, False]), str(["a", "b"]))
"#,
    );
}

#[test]
fn str_of_a_tuple_matches_print() {
    matches_python(
        "tuple",
        r#"
t = (1, "a", 2.5)
print(t)
print(str(t))
print(str((1,)), str(()))
"#,
    );
}

#[test]
fn str_of_a_dict_matches_print() {
    matches_python(
        "dict",
        r#"
d = {"a": 1, "b": 2}
print(d)
print(str(d))
print(str({1: "x"}))
"#,
    );
}

#[test]
fn str_of_a_set_matches_print() {
    matches_python(
        "set",
        r#"
s = {1}
print(s)
print(str(s))
"#,
    );
}

#[test]
fn empty_containers_render_like_cpython() {
    matches_python(
        "empty",
        r#"
xs: list[int] = []
d: dict[str, int] = {}
s: set[int] = set()
print(str(xs), str(d), str(s), str(()))
"#,
    );
}

// ---------------------------------------------------------------------------
// Elements are rendered as reprs, nested and all
// ---------------------------------------------------------------------------

#[test]
fn string_elements_are_quoted_and_escaped() {
    matches_python(
        "quoting",
        r#"
print(str(["a'b", 'c"d', "e\nf", "g\th"]))
print(str({"k'": "v\\"}))
"#,
    );
}

#[test]
fn nested_containers_render_recursively() {
    matches_python(
        "nested",
        r#"
print(str([[1], [2, 3], []]))
print(str([(1, 2), (3, 4)]))
print(str({"a": [1, 2], "b": []}))
print(str(((1, 2), (3, 4))))
"#,
    );
}

#[test]
fn none_and_floats_inside_a_container_render_like_print() {
    matches_python(
        "none-floats",
        r#"
print(str([None, 1]))
print(str([1.0, 2.5, -0.0]))
print(str((1, 2.5)))
"#,
    );
}

#[test]
fn a_non_ascii_element_is_passed_through() {
    matches_python(
        "non-ascii",
        r#"
print(str(["héllo", "🐍"]))
print(["héllo", "🐍"])
"#,
    );
}

// ---------------------------------------------------------------------------
// The interpolation forms
// ---------------------------------------------------------------------------

#[test]
fn an_f_string_can_interpolate_a_container() {
    matches_python(
        "fstring",
        r#"
xs = [1, 2, 3]
d = {"a": 1}
t = (1, "b")
print(f"{xs} {d} {t}")
print(f"result: {xs}")
print(f"{[]}")
"#,
    );
}

#[test]
fn percent_and_format_interpolate_containers() {
    matches_python(
        "percent-format",
        r#"
xs = [1, 2, 3]
print("%s and %s" % (xs, (4, 5)))
print("{} then {}".format(xs, {"k": 1}))
"#,
    );
}

#[test]
fn the_f_string_repr_conversion_works_on_a_container() {
    matches_python(
        "fstring-repr",
        r#"
xs = [1, 2]
print(f"{xs!r}")
"#,
    );
}

#[test]
fn a_rendered_container_is_an_ordinary_string() {
    matches_python(
        "is-a-string",
        r#"
xs = [1, 2, 3]
s = str(xs)
print(len(s), s[0], s[-1], s.split(", "))
print("items: " + str(xs))
print(str(xs).upper(), str(xs).startswith("["))
"#,
    );
}

// ---------------------------------------------------------------------------
// repr() / ascii() builtins
// ---------------------------------------------------------------------------

#[test]
fn repr_of_a_container_equals_its_str() {
    matches_python(
        "repr-container",
        r#"
xs = [1, 2, 3]
print(repr(xs), str(xs), repr(xs) == str(xs))
print(repr({"k": (1, 2)}))
"#,
    );
}

#[test]
fn repr_and_ascii_work_on_scalars() {
    matches_python(
        "repr-scalars",
        r#"
print(repr("hi"), repr(42), repr(2.5), repr(True))
print(repr("a'b"), repr("c\nd"))
print(ascii("héllo"), ascii("hi"), ascii(42))
"#,
    );
}

// ---------------------------------------------------------------------------
// Programs
// ---------------------------------------------------------------------------

#[test]
fn a_report_interpolates_the_values_it_computed() {
    matches_python(
        "report",
        r#"
def summarize(rows: list[int]) -> str:
    evens = [r for r in rows if r % 2 == 0]
    by_parity = {"even": evens, "odd": [r for r in rows if r % 2 == 1]}
    return f"{len(rows)} rows, evens {evens}, split {by_parity}"


print(summarize([1, 2, 3, 4, 5]))
print(summarize([]))
"#,
    );
}

#[test]
fn rendering_many_containers_survives_collection() {
    matches_python(
        "gc-stress",
        r#"
total = 0
last = ""
for i in range(2000):
    xs = [i, i + 1, i + 2]
    s = str(xs)
    total += len(s)
    last = s
print(total, last)
"#,
    );
}

// ---------------------------------------------------------------------------
// Rejections
// ---------------------------------------------------------------------------

#[test]
fn a_format_spec_on_a_container_is_rejected() {
    // CPython raises TypeError: unsupported format string passed to
    // list.__format__ -- the same rejection, at compile time.
    let msg = rejects(
        "spec-container",
        r#"
xs = [1, 2]
print(f"{xs:>10}")
"#,
    );
    assert!(
        msg.contains("format() cannot convert list[int]"),
        "unexpected diagnostic: {msg}"
    );
}

#[test]
fn repr_takes_exactly_one_argument() {
    let msg = rejects("repr-arity", "print(repr(1, 2))\n");
    assert!(
        msg.contains("repr() takes exactly one argument"),
        "unexpected diagnostic: {msg}"
    );
}

#[test]
fn an_exception_inside_a_container_renders_as_repr() {
    // A container element is a repr, like every other slot: CPython prints
    // [ValueError('x')], not [x]. Only a top-level print uses str.
    matches_python(
        "exc-element",
        r#"
try:
    raise ValueError("x")
except ValueError as e:
    print([e])
    print(str([e]))
    print(e, str(e), repr(e))
try:
    raise KeyError("k")
except KeyError as e:
    print([e], repr(e))
"#,
    );
}

#[test]
fn ascii_of_a_container_escapes_non_ascii_elements() {
    matches_python(
        "ascii-container",
        r#"
print(ascii(["h\u00e9llo", "\U0001f40d", "ok"]))
print(ascii(("\u00e9",)), ascii({"k\u00e9": "v\u00e9"}), ascii({"\u00e9"}))
print(ascii([["n\u00e4sted"]]), ascii([1, 2]))
print(repr(["h\u00e9llo"]), str(["h\u00e9llo"]))
"#,
    );
}

#[test]
fn an_empty_format_spec_renders_a_container() {
    // `{x:}` is `{x}` for every type in CPython; only a *non-empty* spec
    // reaches list.__format__ and raises.
    matches_python(
        "empty-spec",
        r#"
xs = [1, 2]
d = {"a": 1}
print(f"{xs:}", f"{d:}")
print("{:}".format(xs))
print(f"{5:}", f"{'s':}", f"{2.5:}")
"#,
    );
}

// ---------------------------------------------------- class elements

/// A container renders its elements with `__repr__`, never `__str__`.
///
/// `print(obj)` prefers `__str__` and always did; `print([obj])` rendered
/// `<Name object>` until 0.135, because the runtime formats container
/// elements from a numeric type tag and had no way back into user code. It
/// now calls through a per-class function-pointer table the compiled program
/// registers, alongside the class-name table that produced the old fallback.
#[test]
fn a_container_renders_elements_with_repr_not_str() {
    matches_python(
        "class-elem-repr",
        r#"
class Both:
    def __str__(self) -> str:
        return "S"
    def __repr__(self) -> str:
        return "R"

b = Both()
print(b)
print([b])
print((b, b))
print({"k": b})
print(str([b]), repr([b]))
print(f"{[b]}")
"#,
    );
}

/// The table is indexed by runtime type id, so a subclass without its own
/// `__repr__` must find its parent's — `ClassInfo::methods` holds only a
/// class's own methods.
#[test]
fn an_inherited_repr_reaches_container_elements() {
    matches_python(
        "class-elem-inherited",
        r#"
class Base:
    def __init__(self, v: int) -> None:
        self.v: int = v
    def __repr__(self) -> str:
        return "Base(" + str(self.v) + ")"

class Child(Base):
    pass

class Louder(Base):
    def __repr__(self) -> str:
        return "Louder(" + str(self.v) + ")"

items: list[Base] = [Base(1), Child(2), Louder(3)]
print(items)
for it in items:
    print(it)
"#,
    );
}

/// Nesting, and a class element inside every container kind, since each has
/// its own element-printing path in the runtime.
#[test]
fn class_elements_render_through_every_container_kind() {
    matches_python(
        "class-elem-nested",
        r#"
class P:
    def __init__(self, x: int, y: int) -> None:
        self.x: int = x
        self.y: int = y
    def __repr__(self) -> str:
        return "P(" + str(self.x) + ", " + str(self.y) + ")"

print([[P(1, 2)], [P(3, 4)]])
print([(P(1, 1), P(2, 2))])
print({"a": [P(9, 9)]})
print([{"k": P(0, 0)}])
rows: list[list[P]] = []
for i in range(3):
    rows.append([P(i, i * 2)])
print(rows)
"#,
    );
}
