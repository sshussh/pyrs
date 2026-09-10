//! Types as values, and pay-per-use shapes.
//!
//! A class name is a value (`k = C`, `type(x)`, `k()`). `getattr` / `setattr`
//! / `hasattr` on a class instance allocate an overflow dict only on classes
//! the program actually uses that way.

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
            .join(format!("pyrs-typeshapes-{tag}-{}", std::process::id())),
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
            .join(format!("pyrs-typeshapes-{tag}-{}", std::process::id())),
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
fn a_class_name_is_a_value_and_a_factory() {
    matches_python(
        "factory",
        r#"
class Point:
    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y

k = Point
p = k(1, 2)
print(p.x, p.y)
print(type(p))
print(isinstance(p, k))
print(isinstance(p, Point))
"#,
    );
}

#[test]
fn type_of_an_instance_matches_python() {
    matches_python(
        "type-of",
        r#"
class A:
    def __init__(self):
        self.n = 1
class B(A):
    def __init__(self):
        self.n = 2
a = A()
b = B()
print(type(a))
print(type(b))
print(type(a) == type(b))
print(type(b) == B)
"#,
    );
}

#[test]
fn getattr_setattr_hasattr_on_an_instance() {
    matches_python(
        "shape",
        r#"
class Box:
    def __init__(self, n: int):
        self.n = n

b = Box(3)
print(b.n)
print(getattr(b, "n"))
print(hasattr(b, "n"))
print(hasattr(b, "extra"))
setattr(b, "extra", 9)
print(getattr(b, "extra"))
print(hasattr(b, "extra"))
print(b.n)
"#,
    );
}

#[test]
fn setattr_of_a_layout_field_still_assigns() {
    matches_python(
        "set-field",
        r#"
class C:
    def __init__(self):
        self.x = 1
c = C()
setattr(c, "x", 5)
print(c.x)
print(getattr(c, "x"))
"#,
    );
}

#[test]
fn a_class_nobody_reflects_stays_closed() {
    matches_python(
        "closed",
        r#"
class Closed:
    def __init__(self, n: int):
        self.n = n
print(Closed(4).n)
"#,
    );
}

#[test]
fn type_of_an_int_names_the_gap() {
    let err = rejects("type-int", "print(type(1))\n");
    assert!(
        err.contains("not supported yet") || err.contains("user-class"),
        "{err}"
    );
}

#[test]
fn getattr_on_an_int_names_the_gap() {
    let err = rejects("getattr-int", "print(getattr(1, \"x\"))\n");
    assert!(err.contains("class instance"), "{err}");
}
