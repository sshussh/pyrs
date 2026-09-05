//! Values and comparison slots must match CPython.
//!
//! Two defects measured against CPython 3.14 before this milestone:
//!
//! * `[1, 2.5, 1]` printed `[1.0, 2.5, 1.0]` -- mixed numeric list literals
//!   collapsed to one element type, changing both values and their types.
//! * `a != b`, where `a` is `Base`-typed but holds a `Child` defining
//!   `__ne__`, ran the negation of `__eq__` instead of `Child.__ne__`,
//!   losing the result *and* the side effects.

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

/// Differential check at every optimization level, comparing stdout and exit
/// status against the CPython oracle.
fn matches_python(tag: &str, source: &str) {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-value-fidelity-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();

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

// ---------------------------------------------------------------------------
// Numeric element fidelity
// ---------------------------------------------------------------------------

#[test]
fn mixed_numeric_list_literal_keeps_element_types() {
    matches_python(
        "mixed-literal",
        r#"
xs = [1, 2.5, 1]
print(xs)
ys = [1, True]
print(ys)
zs = [1, 2.5, True]
print(zs)
"#,
    );
}

#[test]
fn mixed_numeric_elements_keep_types_through_indexing_and_iteration() {
    matches_python(
        "mixed-access",
        r#"
xs = [1, 2.5, 3]
print(xs[0])
print(xs[1])
print(xs[2])
for x in xs:
    print(x)
"#,
    );
}

#[test]
fn homogeneous_numeric_lists_are_unchanged() {
    matches_python(
        "homogeneous",
        r#"
ints = [1, 2, 3]
floats = [1.0, 2.5]
bools = [True, False]
print(ints)
print(floats)
print(bools)
print(sum(ints))
"#,
    );
}

#[test]
fn mixed_numeric_equality_matches_cpython() {
    matches_python(
        "mixed-eq",
        r#"
xs = [1, 2.5, 1]
ys = [1, 2.5, 1]
print(xs == ys)
print(xs == [1, 2.5, 2])
print(1 in xs)
print(2.5 in xs)
"#,
    );
}

#[test]
fn nested_mixed_numeric_lists() {
    // Both inner lists mix int/float identically, so they join to the same
    // `list[int | float]` element type. Nesting sub-lists whose element types
    // differ (`[[1, 2.5], [3, 4]]`, one all-int) needs general `list[T1]` ->
    // `list[T2]` re-coercion, a separate, larger, pre-existing gap: even
    // `fs: list[float] = xs` from a `list[int]` fails today. That is
    // documented in README.md and is not this milestone's scope.
    matches_python(
        "nested-mixed",
        r#"
grid = [[1, 2.5], [3, 4.5]]
print(grid)
print(grid[0][0])
print(grid[1][1])
"#,
    );
}

// ---------------------------------------------------------------------------
// Comparison slot selection
// ---------------------------------------------------------------------------

#[test]
fn base_typed_receiver_reaches_a_subclass_ne() {
    // The measured defect: printed `False` with no side effect.
    matches_python(
        "subclass-ne",
        r#"
class Base:
    def __init__(self, v: int) -> None:
        self.v = v
    def __eq__(self, other: Base) -> bool:
        return self.v == other.v

class Child(Base):
    def __ne__(self, other: Base) -> bool:
        print("Child.__ne__ ran")
        return True

def check(a: Base, b: Base) -> None:
    print(a != b)

check(Child(1), Base(1))
check(Base(1), Base(1))
"#,
    );
}

#[test]
fn default_ne_follows_an_overridden_eq() {
    // The synthesized `__ne__` must call `self.__eq__` virtually, so a
    // subclass overriding only `__eq__` still changes `!=`.
    matches_python(
        "virtual-eq",
        r#"
class Base:
    def __init__(self, v: int) -> None:
        self.v = v
    def __eq__(self, other: Base) -> bool:
        print("Base.__eq__")
        return self.v == other.v

class Child(Base):
    def __eq__(self, other: Base) -> bool:
        print("Child.__eq__")
        return self.v != other.v

def check(a: Base, b: Base) -> None:
    print(a != b)

check(Child(1), Base(1))
check(Base(1), Base(1))
"#,
    );
}

#[test]
fn an_inherited_explicit_ne_is_not_shadowed() {
    // Synthesizing wherever a class declares `__eq__` would shadow
    // `Base.__ne__` here; CPython resolves to the inherited one.
    matches_python(
        "inherited-ne",
        r#"
class Base:
    def __init__(self, v: int) -> None:
        self.v = v
    def __ne__(self, other: Base) -> bool:
        print("Base.__ne__")
        return True

class Child(Base):
    def __eq__(self, other: Base) -> bool:
        print("Child.__eq__")
        return self.v == other.v

def check(a: Base, b: Base) -> None:
    print(a != b)

check(Child(1), Child(1))
check(Base(1), Base(1))
"#,
    );
}

#[test]
fn ne_side_effects_happen_exactly_once() {
    matches_python(
        "ne-once",
        r#"
calls: list[str] = []

class P:
    def __init__(self, v: int) -> None:
        self.v = v
    def __eq__(self, other: P) -> bool:
        calls.append("eq")
        return self.v == other.v

print(P(1) != P(2))
print(len(calls))
"#,
    );
}

#[test]
fn three_level_inheritance_resolves_the_nearest_ne() {
    matches_python(
        "three-level",
        r#"
class A:
    def __init__(self, v: int) -> None:
        self.v = v
    def __eq__(self, other: A) -> bool:
        print("A.__eq__")
        return self.v == other.v

class B(A):
    def __ne__(self, other: A) -> bool:
        print("B.__ne__")
        return False

class C(B):
    pass

def check(x: A, y: A) -> None:
    print(x != y)

check(C(1), C(2))
check(B(1), B(2))
check(A(1), A(2))
"#,
    );
}
