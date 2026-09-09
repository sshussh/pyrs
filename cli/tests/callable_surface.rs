//! Signatures a library can actually publish, and functions as values.
//!
//! Three things were missing, and the second was the one that mattered most:
//!
//! - **`/` and `*` in a parameter list.** `def f(a, *, b)` died at the parser,
//!   which required a name after `*`; `def f(a, /, b)` died because `/` was
//!   never matched as a separator. You could not transcribe a signature from
//!   any library's documentation.
//! - **Keyword arguments on a method.** `C().m(1, b=3)` was refused outright —
//!   not just for a keyword-only parameter, but for *any* keyword on *any*
//!   instance method, while a free function accepted them. Since library APIs
//!   are overwhelmingly methods (`df.sort_values(by=...)`), this was the real
//!   blocker. The binding logic already lived in `lower_call_with_sig`; the
//!   method path simply never handed it the keywords.
//! - **Module-level functions as values.** Nested `def`s and lambdas had been
//!   first-class since closures existed; the arm for a module-level one was
//!   never written.
//!
//! A silent wrong answer fell out of the second: `ClassName.static(1, b=2)`
//! *dropped* the keyword and used the default, returning 11 where CPython
//! returns 12. That path ignored keywords rather than rejecting them.

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
            .join(format!("pyrs-call-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    (dir, src)
}

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

// ------------------------------------------------------- parameter markers

#[test]
fn keyword_only_parameters() {
    matches_python(
        "kwonly",
        r#"
def f(a: int, *, b: int) -> int:
    return a * 10 + b

def defaulted(a: int, *, b: int = 5) -> int:
    return a * 10 + b

# Legal only because a keyword-only argument is supplied by name: the
# "no non-default after default" rule applies within the positional run.
def either_order(*, a: int = 1, b: int) -> int:
    return a * 10 + b

def with_varargs(a: int, *rest: int, k: int) -> int:
    return a + sum(rest) + k

print(f(1, b=2))
print(defaulted(1), defaulted(1, b=2))
print(either_order(b=2), either_order(a=9, b=2))
print(with_varargs(1, 2, 3, k=10))
"#,
    );
}

#[test]
fn positional_only_parameters() {
    matches_python(
        "posonly",
        r#"
def f(a: int, /, b: int) -> int:
    return a * 10 + b

def both(a: int, /, b: int, *, c: int) -> int:
    return a * 100 + b * 10 + c

print(f(1, 2), f(1, b=2))
print(both(1, 2, c=3), both(1, b=2, c=3))
"#,
    );
}

/// The markers survive every place a signature is rebuilt: a method (where
/// dropping `self` shifts both indices by one), an inherited override, and a
/// nested `def`.
#[test]
fn markers_survive_methods_and_nesting() {
    matches_python(
        "markers-carried",
        r#"
class Base:
    def scale(self, v: int, /, by: int = 2, *, offset: int = 0) -> int:
        return v * by + offset

class Sub(Base):
    def scale(self, v: int, /, by: int = 3, *, offset: int = 0) -> int:
        return v * by + offset

def outer() -> int:
    def inner(a: int, *, b: int) -> int:
        return a + b
    return inner(1, b=2)

def dispatch(x: Base) -> int:
    return x.scale(5, by=4, offset=1)

print(Base().scale(5), Sub().scale(5))
print(dispatch(Base()), dispatch(Sub()))
print(outer())
"#,
    );
}

#[test]
fn marker_misuse_is_named() {
    for (tag, src, want) in [
        (
            "kw-for-posonly",
            "def f(a: int, /, b: int) -> int:\n    return a + b\nprint(f(a=1, b=2))\n",
            "positional-only",
        ),
        (
            "pos-for-kwonly",
            "def f(a: int, *, b: int) -> int:\n    return a + b\nprint(f(1, 2))\n",
            "takes 1 positional argument(s) but more were given",
        ),
        (
            "kwonly-omitted",
            "def f(a: int, *, b: int) -> int:\n    return a + b\nprint(f(1))\n",
            "missing required keyword-only argument",
        ),
        (
            "bare-star-alone",
            "def f(a: int, *) -> int:\n    return a\nprint(f(1))\n",
            "named parameters must follow a bare '*'",
        ),
        (
            "slash-first",
            "def f(/, a: int) -> int:\n    return a\nprint(f(1))\n",
            "at least one parameter must precede '/'",
        ),
        (
            "slash-after-star",
            "def f(a: int, *, b: int, /) -> int:\n    return a\nprint(f(1, b=2))\n",
            "'/' must appear before '*'",
        ),
    ] {
        let msg = rejects(tag, src);
        assert!(msg.contains(want), "{tag}: expected {want:?}, got: {msg}");
    }
}

// ------------------------------------------------- keywords on methods

/// Keywords were refused for *any* instance method, keyword-only or not.
#[test]
fn methods_accept_keyword_arguments() {
    matches_python(
        "method-kwargs",
        r#"
class Frame:
    def __init__(self) -> None:
        self.rows: list[int] = [3, 1, 2]

    def sorted_rows(self, *, ascending: bool = True) -> list[int]:
        out = sorted(self.rows)
        if not ascending:
            out.reverse()
        return out

    def scaled(self, factor: int, offset: int = 0) -> list[int]:
        return [r * factor + offset for r in self.rows]

    def assign(self, values: list[int], sort: bool = False) -> None:
        self.rows = sorted(values) if sort else values

f = Frame()
print(f.sorted_rows(), f.sorted_rows(ascending=False))
print(f.scaled(2), f.scaled(2, offset=1), f.scaled(factor=3))
f.assign([9, 7, 8], sort=True)
print(f.rows)
"#,
    );
}

/// The keyword reached a virtual call through a base-typed binding, not just
/// a statically known receiver.
#[test]
fn keywords_survive_virtual_dispatch() {
    matches_python(
        "method-kwargs-virtual",
        r#"
class Shape:
    def area(self, scale: int = 1) -> int:
        return 1 * scale

class Square(Shape):
    def area(self, scale: int = 1) -> int:
        return 4 * scale

def measure(s: Shape) -> int:
    return s.area(scale=10)

shapes: list[Shape] = [Shape(), Square()]
print([measure(s) for s in shapes])
"#,
    );
}

/// `ClassName.method(...)` silently *dropped* its keywords and used the
/// defaults — a wrong answer with no diagnostic, which is why this is pinned
/// separately from the instance path.
#[test]
fn class_name_calls_do_not_drop_keywords() {
    matches_python(
        "classname-kwargs",
        r#"
class C:
    @staticmethod
    def stat(a: int, b: int = 1) -> int:
        return a * 10 + b

    @classmethod
    def made(cls, tag: str = "x") -> str:
        return "made:" + tag

print(C.stat(1, b=2), C.stat(1))
print(C.made(tag="y"), C.made())
"#,
    );
}

/// A builtin type's method table has no keyword surface, and must still say
/// so rather than dropping the keyword the way the class path used to.
#[test]
fn builtin_methods_still_refuse_keywords() {
    let msg = rejects(
        "builtin-kwargs",
        "s = \"abc\"\nprint(s.startswith(prefix=\"a\"))\n",
    );
    assert!(
        msg.contains("keyword arguments are not supported"),
        "expected a keyword rejection: {msg}"
    );
}

// ------------------------------------------------- functions as values

#[test]
fn a_module_level_function_is_a_value() {
    matches_python(
        "fn-value",
        r#"
def double(x: int) -> int:
    return x * 2

def apply(f, x: int) -> int:
    return f(x)

def by_len(s: str) -> int:
    return len(s)

def square(x: int) -> int:
    return x * x

print(apply(double, 21))
print(sorted(["bbb", "a", "cc"], key=by_len))
print(list(map(square, [1, 2, 3])))
"#,
    );
}

/// The shape a command-line tool wants, and one of the library cores that
/// was blocked before this milestone.
#[test]
fn functions_live_in_a_dispatch_table() {
    matches_python(
        "dispatch",
        r#"
def cmd_build(args: list[str]) -> int:
    return len(args) + 10

def cmd_test(args: list[str]) -> int:
    return len(args) + 20

TABLE = {"build": cmd_build, "test": cmd_test}
for name in sorted(TABLE.keys()):
    print(name, TABLE[name](["x"]))

fs = [cmd_build, cmd_test]
print([f([]) for f in fs])
"#,
    );
}

/// A closure value has a fixed parameter list and carries no defaults, so the
/// shapes that cannot be one are refused by name rather than silently losing
/// arguments.
#[test]
fn shapes_that_cannot_be_a_closure_value_are_named() {
    for (tag, src, want) in [
        (
            "varargs",
            "def f(*xs: int) -> int:\n    return sum(xs)\ng = f\nprint(g(1))\n",
            "*args or **kwargs",
        ),
        (
            "defaults",
            "def f(a: int, b: int = 1) -> int:\n    return a + b\ng = f\nprint(g(1))\n",
            "default arguments",
        ),
        (
            "mismatched-sigs",
            "def a(x: int) -> int:\n    return x\n\
             def b(s: str) -> str:\n    return s\nfs = [a, b]\nprint(len(fs))\n",
            "share one type",
        ),
    ] {
        let msg = rejects(tag, src);
        assert!(msg.contains(want), "{tag}: expected {want:?}, got: {msg}");
    }
}
