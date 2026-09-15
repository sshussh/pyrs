//! Call-graph and typeshed inference for unannotated parameters.
//!
//! A unique type from the body still wins. A conflicting observed set is
//! `Any` (the kernel those sites already have). Call sites and a typeshed
//! table seed the rest. An unconstrained parameter still needs an annotation.

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

fn matches_python(tag: &str, source: &str) {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-infer-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();

    let expected = Command::new("python3").arg(&src).output().unwrap();
    assert!(
        expected.status.success(),
        "CPython failed for {tag}: {}",
        String::from_utf8_lossy(&expected.stderr)
    );
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
            String::from_utf8_lossy(&expected.stdout),
            "{tag} differs at -O{opt} ({label})"
        );
    }
}

fn rejects(tag: &str, source: &str) -> String {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-infer-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    let out = Command::new(PYRS)
        .args(["check", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "{tag} was expected to be rejected but compiled"
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn identity_takes_its_type_from_the_call_site() {
    matches_python(
        "identity",
        r#"
def identity(x):
    return x
print(identity(3))
print(identity(3) + 1)
"#,
    );
}

#[test]
fn a_callee_annotation_seeds_the_caller() {
    matches_python(
        "callee",
        r#"
def g(x: int) -> int:
    return x + 1
def f(y):
    return g(y)
print(f(3))
"#,
    );
}

#[test]
fn two_call_sites_of_different_types_are_dynamic() {
    matches_python(
        "poly",
        r#"
def identity(x):
    return x
print(identity(3))
print(identity("ab"))
"#,
    );
}

#[test]
fn typeshed_math_sqrt_seeds_a_float() {
    matches_python(
        "math-sqrt",
        r#"
import math
def f(x):
    return math.sqrt(x)
print(f(9.0))
"#,
    );
}

#[test]
fn typeshed_range_seeds_an_int() {
    matches_python(
        "range",
        r#"
def f(n):
    s = 0
    for i in range(n):
        s += i
    return s
print(f(4))
"#,
    );
}

#[test]
fn isinstance_tuple_is_a_dynamic_parameter() {
    matches_python(
        "isinstance-tuple",
        r#"
def f(x):
    if isinstance(x, (int, float)):
        return x + 1
    return 0
print(f(3))
print(f(1.5))
"#,
    );
}

/// Statement vs expression: a method call in both positions, and a nested
/// def, still infer from the body.
#[test]
fn methods_and_nested_defs_still_infer_from_the_body() {
    matches_python(
        "method-nested",
        r#"
class C:
    def add1(self, x):
        return x + 1
    def show(self, x):
        print(x + 1)

def outer():
    def inner(x):
        return x + 2
    return inner(3)

c = C()
print(c.add1(4))
c.show(5)
print(outer())
"#,
    );
}

#[test]
fn unconstrained_still_needs_an_annotation() {
    let err = rejects("unconstrained", "def f(x):\n    return x\n");
    assert!(
        err.contains("missing a type annotation") || err.contains("could not infer"),
        "{err}"
    );
}

/// The case the earlier test missed. `identity(x): return x` puts no
/// constraint on `x`, so it never reached body inference; `x + x` does, and
/// body inference commits to one type for it. A second call site with another
/// type has to win, because CPython runs this program.
#[test]
fn a_call_site_conflict_overrides_a_body_inferred_type() {
    matches_python(
        "poly-body",
        r#"
def show(x):
    return x + x
def scale(x):
    return x * 2
print(show(2), show("ab"))
print(scale(2), scale("ab"), scale([1]), scale(2.5))
"#,
    );
}

/// Widening propagates: when `outer` becomes dynamic, what it passes to
/// `inner` is dynamic too.
#[test]
fn widening_propagates_through_the_call_graph() {
    matches_python(
        "poly-chain",
        r#"
def inner(v):
    return v + v
def outer(x):
    return inner(x)
print(outer(3), outer("z"))
"#,
    );
}

/// Methods, through a constructed local, through `self`, and inherited.
#[test]
fn a_call_site_conflict_widens_a_method_parameter() {
    matches_python(
        "poly-method",
        r#"
class Base:
    def twice(self, x):
        return x + x
    def mixed(self):
        return str(self.twice(1)) + self.twice("s")
class Sub(Base):
    pass
b = Base()
s = Sub()
print(b.twice(1), b.twice("q"), b.mixed(), s.twice(4), s.twice("w"))
def run():
    k = Base()
    return str(k.twice(5)) + k.twice("r")
print(run())
"#,
    );
}

/// The conflict only fires on a real disagreement: a function every call site
/// agrees on keeps its unboxed signature, and a numeric promotion is not a
/// conflict.
#[test]
fn agreeing_call_sites_keep_the_typed_signature() {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-infer-poly-mono-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    fs::write(
        dir.0.join("prog.py"),
        "def step(x):\n    return x * 2 + 1\nclass K:\n    def step(self, x):\n        return x * 2 + 1\n\
         k = K()\nprint(step(3), step(4), k.step(3), k.step(True))\n",
    )
    .unwrap();
    let out = Command::new(PYRS)
        .args([
            "compile",
            "--no-cache",
            "--emit-llvm",
            "-O",
            "0",
            "-i",
            "prog.py",
            "-o",
            "out",
        ])
        .current_dir(&dir.0)
        .output()
        .expect("failed to spawn PyRs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let ll = fs::read_to_string(dir.0.join("out.ll")).expect("no LLVM output");
    assert!(ll.contains("define i64 @pyrs_step(i64 %p.x)"), "{ll}");
    assert!(
        ll.contains("define i64 @pyrs_K.step(ptr %p.self, i64 %p.x)"),
        "{ll}"
    );
}

/// An annotation is a contract, so a mismatch against one stays an error
/// rather than widening.
#[test]
fn an_annotated_parameter_does_not_widen() {
    let msg = rejects(
        "poly-annotated",
        "def strict(x: int):\n    return x + x\nprint(strict(2), strict(\"ab\"))\n",
    );
    assert!(msg.contains("type mismatch"), "{msg}");
}

/// `self.x: float = x` writes the parameter's type down beside it. The value
/// is printed, so an int where CPython keeps the float would show.
#[test]
fn an_annotated_field_assignment_types_its_parameter() {
    matches_python(
        "field-annotation",
        r#"
class Vec2:
    def __init__(self, x, y):
        self.x: float = x
        self.y: float = y
    def length_sq(self):
        return self.x * self.x + self.y * self.y
v = Vec2(1.5, 2.5)
print(v.x, v.y, v.length_sq())
"#,
    );
}

/// A constructor's call site is not yet used as evidence for `__init__`
/// parameters: class layouts are finalized before any call site is visited,
/// so a field assigned from a parameter typed that late would be missing from
/// the layout. Until that ordering changes, the diagnostic must name the
/// parameter -- not claim the object lacks a field it plainly assigns.
#[test]
fn an_uninferable_constructor_parameter_is_named_not_disguised() {
    let msg = rejects(
        "ctor-call-site",
        "class Circle:\n    def __init__(self, r):\n        self.r = r\n\
         c = Circle(2.5)\nprint(c.r)\n",
    );
    assert!(msg.contains("parameter 'r'"), "{msg}");
    assert!(!msg.contains("has no attribute"), "{msg}");
}
