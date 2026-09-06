//! Class-body constants: `class C: LIMIT = 10`.
//!
//! Any assignment in a class body was rejected, so a class could not carry a
//! constant at all — enum-like values, limits, `PI`. The stated reason was
//! that a class attribute with a default would leave zeroed instance storage,
//! which is true of an instance *field* default but not of a class constant.
//!
//! These are literals, substituted where they are read rather than stored.
//! That is exact for something immutable, needs no storage and no
//! initialisation ordering — and it is why assigning to one is rejected:
//! there is nothing to assign to.

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
            .join(format!("pyrs-classconst-{tag}-{}", std::process::id())),
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
// Reading
// ---------------------------------------------------------------------------

#[test]
fn a_class_constant_is_read_through_the_class() {
    matches_python(
        "through-class",
        r#"
class Color:
    RED = "red"
    GREEN = "green"
    COUNT = 2

print(Color.RED, Color.GREEN, Color.COUNT)
"#,
    );
}

#[test]
fn a_class_constant_is_read_through_an_instance() {
    matches_python(
        "through-self",
        r#"
class Limits:
    MAX = 10
    NAME = "limits"

    def check(self, v: int) -> bool:
        return v < self.MAX

    def label(self) -> str:
        return self.NAME

lim = Limits()
print(lim.check(5), lim.check(50), lim.label())
print(lim.MAX, Limits.MAX)
"#,
    );
}

#[test]
fn every_literal_type_works() {
    matches_python(
        "types",
        r#"
class K:
    I = 7
    F = 2.5
    S = "s"
    B = True
    NEG = -3

print(K.I, K.F, K.S, K.B, K.NEG)
print(K.I + 1, K.F * 2, K.S + "!", not K.B)
"#,
    );
}

#[test]
fn an_annotated_class_constant_works() {
    matches_python(
        "annotated",
        r#"
class C:
    N: int = 5
    NAME: str = "c"

print(C.N, C.NAME)
"#,
    );
}

#[test]
fn constants_are_inherited() {
    matches_python(
        "inherited",
        r#"
class Base:
    LIMIT = 10
    KIND = "base"

class Child(Base):
    KIND = "child"

    def limit(self) -> int:
        return self.LIMIT

print(Base.LIMIT, Base.KIND)
print(Child.LIMIT, Child.KIND, Child().limit())
"#,
    );
}

#[test]
fn an_instance_field_shadows_a_constant_of_the_same_name() {
    matches_python(
        "shadowed",
        r#"
class C:
    N = 1

    def __init__(self) -> None:
        self.N = 9

print(C().N, C.N)
"#,
    );
}

#[test]
fn a_constant_used_in_a_method_body_and_a_default() {
    matches_python(
        "in-methods",
        r#"
class Circle:
    PI = 3.14159

    def __init__(self, r: float) -> None:
        self.r = r

    def area(self) -> float:
        return Circle.PI * self.r * self.r

    def circumference(self) -> float:
        return 2.0 * self.PI * self.r

c = Circle(2.0)
print("{:.3f} {:.3f}".format(c.area(), c.circumference()))
"#,
    );
}

// ---------------------------------------------------------------------------
// Rejections — each names the actual problem
// ---------------------------------------------------------------------------

#[test]
fn a_computed_class_attribute_is_rejected_with_the_reason() {
    let err = rejects("computed", "class C:\n    N = 1 + 1\n");
    assert!(err.contains("must be a literal"), "{err}");
    assert!(err.contains("__init__"), "{err}");
}

#[test]
fn assigning_to_a_class_constant_is_rejected() {
    // Not "name 'C' is not defined", which is what lowering the base gave.
    let err = rejects("assign-class", "class C:\n    N = 1\nC.N = 2\n");
    assert!(
        err.contains("class constant and cannot be assigned"),
        "{err}"
    );

    let err = rejects(
        "assign-self",
        "class C:\n    N = 1\n    def f(self) -> int:\n        self.N = 2\n        return self.N\n",
    );
    assert!(
        err.contains("class constant and cannot be assigned"),
        "{err}"
    );
}

#[test]
fn an_unknown_class_attribute_names_the_class() {
    let err = rejects("unknown", "class C:\n    N = 1\nprint(C.M)\n");
    assert!(err.contains("class 'C' has no attribute 'M'"), "{err}");
}

#[test]
fn an_annotation_that_disagrees_with_the_literal_is_rejected() {
    let err = rejects("bad-ann", "class C:\n    N: str = 5\n");
    assert!(err.contains("annotated str"), "{err}");
}

// ---------------------------------------------------------------------------
// Nothing regressed
// ---------------------------------------------------------------------------

#[test]
fn ordinary_classes_are_unaffected() {
    matches_python(
        "ordinary",
        r#"
class Point:
    def __init__(self, x: int, y: int) -> None:
        self.x = x
        self.y = y

    def total(self) -> int:
        return self.x + self.y

class Shifted(Point):
    def total(self) -> int:
        return self.x + self.y + 1

p = Point(1, 2)
print(p.x, p.total(), Shifted(1, 2).total())
p.x = 5
print(p.x, p.total())
"#,
    );
}
