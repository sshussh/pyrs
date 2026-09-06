//! `body if test else orelse` — Python's conditional expression.
//!
//! It was a parse error before this milestone, which is unusual company for
//! the other gaps: it is not an exotic corner but one of the most common
//! expressions in Python, and `x = a if c else b` had no spelling at all.
//!
//! The two properties worth pinning are the ones a naive lowering loses:
//! only the chosen branch is evaluated (so the other one's side effects and
//! traps never happen), and the result keeps that branch's own type, so
//! `1 if c else 2.5` is `1` and not `1.0`.

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
            .join(format!("pyrs-ifexp-{tag}-{}", std::process::id())),
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

/// Compile only; returns the diagnostic for a program that must be rejected.
fn rejects(tag: &str, source: &str) -> String {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-ifexp-rej-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
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
// Values and placement
// ---------------------------------------------------------------------------

#[test]
fn picks_the_branch_the_condition_selects() {
    matches_python(
        "basic",
        r#"
x = 5
print("big" if x > 3 else "small")
print("big" if x > 9 else "small")
print(1 if True else 2, 1 if False else 2)
"#,
    );
}

#[test]
fn truthiness_follows_python_not_just_bools() {
    matches_python(
        "truthy",
        r#"
for v in [0, 1, -1]:
    print("t" if v else "f")
for s in ["", "x"]:
    print("t" if s else "f")
empty: list[int] = []
print("t" if empty else "f")
print("t" if [0] else "f")
print("t" if None else "f")
print("t" if 0.0 else "f", "t" if 0.5 else "f")
"#,
    );
}

#[test]
fn works_in_every_expression_position() {
    matches_python(
        "positions",
        r#"
c = True
xs = [10, 20]
def g(a: int, b: int = 0) -> int:
    return a + b
1 if c else 2
print([1 if c else 2, 3], (1 if c else 2, 3))
print({"a": 1 if c else 2}, {1 if c else 2})
print(xs[0 if c else 1], xs[1 if c else 0:])
print(g(1 if c else 2, b=3 if c else 4))
print(f"{'yes' if c else 'no'}")
n = 0
while n < (2 if c else 1):
    n += 1
print(n)
assert (1 if c else 0) == 1
for v in ([10] if c else [20]):
    print(v)
"#,
    );
}

// ---------------------------------------------------------------------------
// Laziness: the branch not taken must not run
// ---------------------------------------------------------------------------

#[test]
fn only_the_selected_branch_is_evaluated() {
    matches_python(
        "lazy-effects",
        r#"
def side(tag: str) -> int:
    print("ran", tag)
    return 1
print(side("then") if True else side("else"))
print(side("then") if False else side("else"))
"#,
    );
}

#[test]
fn the_branch_not_taken_does_not_trap() {
    // The classic guard idiom: the division must never be reached.
    matches_python(
        "lazy-traps",
        r#"
n = 0
print(1 // n if n else -1)
xs: list[int] = []
print(xs[0] if xs else "empty")
d: dict[str, int] = {}
print(d["k"] if "k" in d else "missing")
"#,
    );
}

#[test]
fn the_condition_is_evaluated_exactly_once() {
    matches_python(
        "once",
        r#"
def check() -> str:
    calls = [0]
    def cond() -> bool:
        calls[0] = calls[0] + 1
        return True
    got = "a" if cond() else "b"
    return got + str(calls[0])
print(check())
"#,
    );
}

// ---------------------------------------------------------------------------
// Precedence and associativity
// ---------------------------------------------------------------------------

#[test]
fn chains_are_right_associative() {
    matches_python(
        "assoc",
        r#"
for k in [1, 2, 3, 4]:
    print("a" if k == 1 else "b" if k == 2 else "c" if k == 3 else "d")
"#,
    );
}

#[test]
fn or_and_not_bind_tighter_than_the_conditional() {
    matches_python(
        "precedence",
        r#"
c = True
print(0 or 2 if False else 9)
print(not c if c else False)
print(1 if 2 in [1, 2] else 0)
print((1 if c else 2) < 3)
print(1 if c and not c else 2)
"#,
    );
}

#[test]
fn a_bare_conditional_is_not_a_comprehension_filter() {
    // CPython rejects `[x for x in a if b else c]` for the same reason: the
    // trailing `if` belongs to the comprehension. Filters keep working.
    matches_python(
        "comprehension",
        r#"
print([i for i in range(6) if i % 2 == 0])
print([("even" if i % 2 == 0 else "odd") for i in range(4)])
print({i: ("hi" if i else "lo") for i in range(2)})
st = {("hi" if i else "lo") for i in range(2)}
print(len(st), "hi" in st, "lo" in st)
print([i for i in ([1, 2] if True else [3]) if i > 1])
"#,
    );
}

#[test]
fn a_bare_conditional_in_a_comprehension_filter_is_rejected() {
    let err = rejects(
        "comp-filter",
        "print([x for x in range(3) if 1 if True else 0])\n",
    );
    assert!(err.contains("error"), "{err}");
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[test]
fn mixed_numeric_branches_keep_their_own_type() {
    // The 0.89 rule, applied here: collapsing to one numeric type would print
    // `1.0` where CPython prints `1`.
    matches_python(
        "numeric-fidelity",
        r#"
c = True
print(1 if c else 2.5)
print(2.5 if not c else 1)
print(True if c else 2, 2 if not c else True)
print(1 if c else 2)
print(1.5 if c else 2.5)
"#,
    );
}

#[test]
fn unrelated_branch_types_become_a_union() {
    matches_python(
        "union",
        r#"
c = True
print(1 if c else "s")
print("s" if not c else 1)
print(1 if c else None)
print("a" if not c else None)
print([1] if c else None)
"#,
    );
}

#[test]
fn container_and_class_branches_work() {
    matches_python(
        "containers",
        r#"
c = True
print([1] if c else [2])
print((1, 2) if c else (3, 4))
print({"a": 1} if c else {"b": 2})
class A:
    def __init__(self, n: int) -> None:
        self.n = n
print((A(1) if c else A(2)).n)
"#,
    );
}

#[test]
fn optional_results_narrow_afterwards() {
    matches_python(
        "narrowing",
        r#"
c = True
s = "hi" if c else None
if s is not None:
    print(s.upper())
t = None if c else "lo"
print(t is None)
"#,
    );
}

// ---------------------------------------------------------------------------
// Interaction with functions, closures and generators
// ---------------------------------------------------------------------------

#[test]
fn names_used_only_in_a_conditional_are_captured() {
    // Regression: the closure's free-variable walker did not descend into a
    // conditional expression, so `flag` looked undefined inside `inner`.
    matches_python(
        "capture",
        r#"
def mk(flag: bool):
    def inner(a: int) -> int:
        return a if flag else -a
    return inner
print(mk(True)(3), mk(False)(3))

def mk2(lo: int, hi: int):
    def pick(c: bool) -> int:
        return lo if c else hi
    return pick
print(mk2(1, 2)(True), mk2(1, 2)(False))
"#,
    );
}

#[test]
fn works_in_returns_methods_and_defaults() {
    matches_python(
        "functions",
        r#"
def f(n: int) -> int:
    return n if n > 0 else -n
print(f(-3), f(3))

class Box:
    def __init__(self, n: int) -> None:
        self.n = n
    def label(self) -> str:
        return "pos" if self.n > 0 else "neg"
print(Box(1).label(), Box(-1).label())

def d(a: int = 1 if True else 2) -> int:
    return a
print(d(), d(5))
"#,
    );
}

#[test]
fn works_inside_a_generator() {
    matches_python(
        "generator",
        r#"
def gen(c: bool):
    for i in range(3):
        yield i if c else -i
for v in gen(True):
    print(v)
for v in gen(False):
    print(v)
"#,
    );
}

#[test]
fn augmented_assignment_and_reassignment() {
    matches_python(
        "assign",
        r#"
c = True
x = 0
x += 2 if c else 3
print(x)
y = 1 if c else 2
y = 9
print(y)
a, b = (1 if c else 2), (3 if c else 4)
print(a, b)
q = (7 if c else 8)
print(q)
"#,
    );
}
