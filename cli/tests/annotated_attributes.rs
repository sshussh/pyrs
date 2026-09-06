//! `self.x: T = value` — annotated attribute assignment.
//!
//! The parser rejected an annotation on anything but a bare name, which made
//! this unwritable — and it is the only way to declare an attribute whose
//! initial value has no inferable type. `self.xs = []` reported `'C' object
//! has no attribute 'xs'` and `self.d = {}` could not infer a dict type, so an
//! empty list or dict attribute could not be created at all.
//!
//! Found by running small realistic programs against CPython: a state-machine
//! script kept a `self.log: list[str] = []`.

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
            .join(format!("pyrs-annattr-{tag}-{}", std::process::id())),
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
// The cases that were unwritable
// ---------------------------------------------------------------------------

#[test]
fn an_empty_container_attribute_can_be_declared() {
    matches_python(
        "empty-containers",
        r#"
class Box:
    def __init__(self) -> None:
        self.xs: list[int] = []
        self.d: dict[str, int] = {}

    def add(self, v: int) -> int:
        self.xs.append(v)
        self.d[str(v)] = v
        return len(self.xs)

b = Box()
print(b.xs, b.d)
print(b.add(1), b.add(2))
print(b.xs, sorted(b.d.items()))
"#,
    );
}

#[test]
fn nested_container_attributes_work() {
    matches_python(
        "nested",
        r#"
class Table:
    def __init__(self) -> None:
        self.rows: list[list[str]] = []

    def push(self, r: list[str]) -> int:
        self.rows.append(r)
        return len(self.rows)

t = Table()
print(t.push(["a"]), t.push([]))
print(t.rows)
"#,
    );
}

#[test]
fn annotations_work_for_scalars_and_optionals_too() {
    matches_python(
        "scalars",
        r#"
class C:
    def __init__(self) -> None:
        self.n: int = 0
        self.name: str = "b"
        self.ratio: float = 1.5
        self.flag: bool = True
        self.opt: int | None = None

c = C()
print(c.n, c.name, c.ratio, c.flag, c.opt)
c.opt = 5
c.n = 7
print(c.opt, c.n)
"#,
    );
}

#[test]
fn an_annotated_attribute_may_be_initialized_from_a_parameter() {
    matches_python(
        "from-param",
        r#"
class C:
    def __init__(self, n: int, tags: list[str]) -> None:
        self.n: int = n
        self.tags: list[str] = tags

c = C(3, ["a"])
print(c.n, c.tags)
"#,
    );
}

#[test]
fn the_state_machine_that_found_this_works() {
    matches_python(
        "fsm",
        r#"
class Machine:
    def __init__(self) -> None:
        self.state = "idle"
        self.log: list[str] = []

    def step(self, event: str) -> str:
        if self.state == "idle" and event == "start":
            self.state = "running"
        elif self.state == "running" and event == "stop":
            self.state = "idle"
        elif event == "reset":
            self.state = "idle"
        else:
            self.log.append("ignored {} in {}".format(event, self.state))
        return self.state

m = Machine()
for e in ["start", "bogus", "stop", "reset"]:
    print(e, "->", m.step(e))
print(m.log)
"#,
    );
}

// ---------------------------------------------------------------------------
// Rejections
// ---------------------------------------------------------------------------

#[test]
fn an_annotation_that_disagrees_with_its_value_is_rejected() {
    // CPython does not check annotations at run time; a typed compiler does,
    // consistently with how it treats every other annotation.
    let err = rejects(
        "ann-mismatch",
        "class C:\n    def __init__(self) -> None:\n        self.n: int = \"s\"\n",
    );
    assert!(err.contains("type mismatch"), "{err}");
}

#[test]
fn a_subscript_annotation_is_still_rejected() {
    // `xs[0]: int = 5` is legal Python but the annotation has no effect there.
    let err = rejects("subscript-ann", "xs = [0]\nxs[0]: int = 5\n");
    assert!(err.contains("variable or an attribute"), "{err}");
}

// ---------------------------------------------------------------------------
// Nothing regressed
// ---------------------------------------------------------------------------

#[test]
fn unannotated_attributes_still_infer_from_their_value() {
    matches_python(
        "inferred",
        r#"
class Point:
    def __init__(self, x: int, y: int) -> None:
        self.x = x
        self.y = y
        self.tags = ["p"]

    def total(self) -> int:
        return self.x + self.y

p = Point(1, 2)
print(p.x, p.y, p.tags, p.total())
p.x = 9
print(p.x, p.total())
"#,
    );
}

#[test]
fn annotated_locals_and_globals_are_unchanged() {
    matches_python(
        "locals-globals",
        r#"
G: list[int] = []

def f() -> int:
    xs: list[str] = []
    d: dict[str, int] = {}
    n: int = 3
    xs.append("a")
    d["k"] = 1
    return len(xs) + len(d) + n

print(f(), G)
"#,
    );
}
