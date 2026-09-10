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
