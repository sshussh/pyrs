//! A call to a function that always raises terminates.
//!
//! Found by writing `stdlib/json.py` in PyRs and noticing what the parser had
//! to say that Python would not.
//!
//! **A call to a function that always raises terminates.** Without this, a
//! helper like `def fail(msg): raise ValueError(msg)` forced every caller to
//! write an unreachable `return` after calling it, purely to satisfy "every
//! path through a value-returning function must return". Whether a function
//! never returns is *inferred* from its body, on the AST and before anything
//! is lowered, so it does not matter whether the helper is defined above or
//! below its callers. `NoReturn` is accepted as an annotation for
//! compatibility with type-checked Python, but the inference is what carries
//! the meaning.
//!
//! Narrowing `object` to a *container* was tried alongside this and reverted:
//! `object` can hold a `list[int]` as well as a `list[object]`, `isinstance`
//! is true for both, and peeling turned `print(v)` on the former into a trap.
//! The explicit `items: list[object] = v` stays, and is checked.

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
            .join(format!("pyrs-never-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    (dir, src)
}

fn matches_python(tag: &str, source: &str) {
    let (_dir, src) = write_prog(tag, source);
    let expected = Command::new("python3")
        .arg(&src)
        .current_dir(&_dir.0)
        .output()
        .unwrap();
    assert!(
        expected.status.success(),
        "CPython failed for {tag}: {}",
        String::from_utf8_lossy(&expected.stderr)
    );
    for (opt, stress) in [("0", false), ("2", false), ("3", false), ("2", true)] {
        let mut cmd = Command::new(PYRS);
        cmd.args(["run", "--no-cache", "-O", opt, "-i"])
            .arg(&src)
            .current_dir(&_dir.0);
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

// ------------------------------------------------------- never returns

/// A free function, a method, and one defined *after* its caller — the
/// inference runs on the AST before anything is lowered, so order is free.
#[test]
fn a_call_that_always_raises_terminates() {
    matches_python(
        "always-raises",
        r#"
def take(x: int) -> int:
    if x > 0:
        return x
    fail("not positive")

def fail(message: str) -> None:
    raise ValueError(message)

class Reader:
    def __init__(self, items: list[int]) -> None:
        self.items: list[int] = items

    def stop(self, why: str) -> None:
        raise ValueError(why)

    def first(self) -> int:
        if len(self.items) > 0:
            return self.items[0]
        self.stop("empty")

print(take(7))
try:
    take(-1)
except ValueError as e:
    print("caught", e)

print(Reader([5]).first())
try:
    Reader([]).first()
except ValueError as e:
    print("caught", e)
"#,
    );
}

/// Every shape the analysis accepts as "always raises", and the loop form
/// that only counts when there is no `break`.
#[test]
fn the_shapes_that_always_raise() {
    matches_python(
        "shapes",
        r#"
def both_branches(flag: bool) -> None:
    if flag:
        raise ValueError("a")
    else:
        raise ValueError("b")

def inside_with(path: str) -> None:
    with open(path, "r") as handle:
        raise ValueError("read " + str(len(handle.read()) >= 0))

def spinning() -> None:
    while True:
        raise ValueError("spun")

def caller(which: int) -> int:
    if which == 0:
        return 0
    if which == 1:
        both_branches(True)
    if which == 2:
        inside_with("prog.py")
    spinning()

for w in [0, 1, 2, 3]:
    try:
        print(caller(w))
    except ValueError as e:
        print("caught", e)
"#,
    );
}

/// A function that only *sometimes* raises must not count, or the check it
/// feeds would stop catching real missing returns.
#[test]
fn a_conditional_raise_does_not_count() {
    let msg = rejects(
        "sometimes",
        "def maybe(m: str) -> None:\n    if m == \"x\":\n        raise ValueError(m)\n\
         def f(x: int) -> int:\n    if x > 0:\n        return x\n    maybe(\"neg\")\nprint(f(1))\n",
    );
    assert!(
        msg.contains("can reach the end of its body"),
        "the missing-return check must still fire: {msg}"
    );
    // A `while True` that can break falls through to the end of the
    // function, so it does not count either. (A loop that breaks and *then*
    // raises does count, and correctly so.)
    let msg = rejects(
        "breakable",
        "def loops() -> None:\n    while True:\n        break\n\
         def g() -> int:\n    loops()\nprint(g())\n",
    );
    assert!(
        msg.contains("can reach the end of its body"),
        "a breakable loop must not count as always-raising: {msg}"
    );
}

/// `NoReturn` compiles, for code that carries the annotation mypy wants.
#[test]
fn the_no_return_annotation_is_accepted() {
    matches_python(
        "annotation",
        r#"
from typing import NoReturn

def fail(message: str) -> NoReturn:
    raise ValueError(message)

def get(values: list[int], index: int) -> int:
    if index < len(values):
        return values[index]
    fail("index " + str(index) + " out of range")

print(get([1, 2, 3], 1))
try:
    get([1], 5)
except ValueError as e:
    print("caught", e)
"#,
    );
}
