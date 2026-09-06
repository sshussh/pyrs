//! User-defined exception classes: `class E(Exception)`, `raise E`, `except E`.
//!
//! Before this milestone the exception type in `raise` / `except` was resolved
//! by the *parser* against a hardcoded list of builtins, so a custom exception
//! had no spelling at all and there was no workaround — unlike most gaps, you
//! could not fall back to different syntax, only to a builtin type that loses
//! the distinction the program is trying to make.
//!
//! The properties worth pinning are the hierarchy (a subclass is caught by any
//! ancestor and by `Exception`, but a base is *not* caught by its subclass)
//! and that builtins keep working unchanged alongside.

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
            .join(format!("pyrs-userexc-{tag}-{}", std::process::id())),
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

/// An uncaught exception must fail with CPython's final traceback line.
fn uncaught_matches_python(tag: &str, source: &str) {
    let (_dir, src) = write_prog(tag, source);
    let expected = Command::new("python3").arg(&src).output().unwrap();
    assert!(!expected.status.success(), "CPython unexpectedly succeeded");
    let want = String::from_utf8_lossy(&expected.stderr);
    let want_last = want.trim_end().lines().last().unwrap_or("").to_string();
    for opt in ["0", "2", "3"] {
        let actual = Command::new(PYRS)
            .args(["run", "-O", opt, "-i"])
            .arg(&src)
            .output()
            .unwrap();
        assert!(
            !actual.status.success(),
            "{tag} at -O{opt} should have failed"
        );
        let got = String::from_utf8_lossy(&actual.stderr);
        let got_last = got.trim_end().lines().last().unwrap_or("");
        assert_eq!(
            got_last, want_last,
            "uncaught text differs for {tag} at -O{opt}"
        );
        assert_eq!(
            String::from_utf8_lossy(&actual.stdout),
            String::from_utf8_lossy(&expected.stdout),
            "stdout before the raise differs for {tag}"
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
// Raising and catching
// ---------------------------------------------------------------------------

#[test]
fn a_user_exception_is_caught_by_its_own_type() {
    matches_python(
        "own-type",
        r#"
class AppError(Exception):
    pass

try:
    raise AppError("boom")
except AppError as e:
    print("caught:", e)
"#,
    );
}

#[test]
fn a_subclass_is_caught_by_any_ancestor() {
    matches_python(
        "ancestors",
        r#"
class A(Exception):
    pass
class B(A):
    pass
class C(B):
    pass

try:
    raise C("c")
except B as e:
    print("parent:", e)

try:
    raise C("c2")
except A as e:
    print("grandparent:", e)

try:
    raise C("c3")
except Exception as e:
    print("Exception:", e)
"#,
    );
}

#[test]
fn a_base_is_not_caught_by_its_subclass() {
    // The direction that a naive "same family" check would get wrong.
    matches_python(
        "not-upward",
        r#"
class A(Exception):
    pass
class B(A):
    pass

try:
    raise A("base")
except B:
    print("WRONG: subclass caught its base")
except A:
    print("base handler ran")
"#,
    );
}

#[test]
fn unrelated_user_exceptions_do_not_catch_each_other() {
    matches_python(
        "unrelated",
        r#"
class A(Exception):
    pass
class B(Exception):
    pass

try:
    raise B("b")
except A:
    print("WRONG")
except B as e:
    print("right handler:", e)
"#,
    );
}

#[test]
fn builtins_and_user_exceptions_coexist() {
    matches_python(
        "coexist",
        r#"
class AppError(Exception):
    pass

try:
    raise ValueError("v")
except AppError:
    print("WRONG")
except ValueError as e:
    print("builtin:", e)

try:
    raise AppError("a")
except ValueError:
    print("WRONG")
except AppError as e:
    print("user:", e)

try:
    raise FileNotFoundError("f")
except OSError as e:
    print("builtin hierarchy still works:", e)
"#,
    );
}

#[test]
fn a_tuple_of_types_may_mix_user_and_builtin() {
    matches_python(
        "tuple-filter",
        r#"
class A(Exception):
    pass
class B(Exception):
    pass

for which in [0, 1, 2]:
    try:
        if which == 0:
            raise A("a")
        elif which == 1:
            raise B("b")
        else:
            raise ValueError("v")
    except (A, B, ValueError) as e:
        print("caught:", e)
"#,
    );
}

// ---------------------------------------------------------------------------
// raise forms
// ---------------------------------------------------------------------------

#[test]
fn raise_accepts_a_bare_name_and_empty_parens() {
    matches_python(
        "raise-forms",
        r#"
class E(Exception):
    pass

try:
    raise E
except E as e:
    print("bare:", len(str(e)))

try:
    raise E()
except E as e:
    print("empty:", len(str(e)))

try:
    raise E("msg")
except E as e:
    print("msg:", e)

try:
    raise ValueError
except ValueError as e:
    print("builtin bare:", len(str(e)))
"#,
    );
}

// ---------------------------------------------------------------------------
// Uncaught
// ---------------------------------------------------------------------------

#[test]
fn an_uncaught_user_exception_prints_its_own_name() {
    uncaught_matches_python(
        "uncaught-msg",
        r#"
class AppError(Exception):
    pass
print("before")
raise AppError("boom")
"#,
    );
}

#[test]
fn an_uncaught_exception_without_a_message_omits_the_colon() {
    // CPython prints "AppError", not "AppError: ".
    uncaught_matches_python(
        "uncaught-empty",
        r#"
class AppError(Exception):
    pass
raise AppError()
"#,
    );
}

#[test]
fn an_uncaught_builtin_without_a_message_omits_the_colon() {
    uncaught_matches_python("uncaught-builtin", "raise ValueError()\n");
}

// ---------------------------------------------------------------------------
// Control flow
// ---------------------------------------------------------------------------

#[test]
fn finally_and_else_run_around_a_user_exception() {
    matches_python(
        "finally",
        r#"
class E(Exception):
    pass

try:
    raise E("x")
except E:
    print("handler")
finally:
    print("finally")

try:
    pass
except E:
    print("WRONG")
else:
    print("else")
finally:
    print("finally 2")
"#,
    );
}

#[test]
fn a_user_exception_propagates_out_of_functions_and_nested_trys() {
    matches_python(
        "propagate",
        r#"
class E(Exception):
    pass

def inner() -> int:
    raise E("deep")

def outer() -> int:
    return inner()

try:
    outer()
except E as e:
    print("from call:", e)

try:
    try:
        raise E("nested")
    except ValueError:
        print("WRONG")
    finally:
        print("inner finally")
except E as e:
    print("outer caught:", e)
"#,
    );
}

#[test]
fn a_bare_except_catches_a_user_exception() {
    matches_python(
        "bare-except",
        r#"
class E(Exception):
    pass
try:
    raise E("x")
except:
    print("bare handler")
"#,
    );
}

#[test]
fn a_user_exception_crosses_a_generator_boundary() {
    matches_python(
        "generator",
        r#"
class E(Exception):
    pass

def gen():
    yield 1
    raise E("from gen")

try:
    for v in gen():
        print(v)
except E as e:
    print("caught:", e)
"#,
    );
}

// ---------------------------------------------------------------------------
// Rejections — each must say what is actually wrong
// ---------------------------------------------------------------------------

#[test]
fn methods_on_an_exception_class_are_rejected_clearly() {
    let err = rejects(
        "exc-methods",
        "class E(Exception):\n    def f(self) -> int:\n        return 1\n",
    );
    assert!(
        err.contains("may only contain 'pass' or a docstring"),
        "{err}"
    );
}

#[test]
fn constructing_an_exception_class_as_a_value_is_rejected_clearly() {
    // Not "name is not defined": the name exists, the use does not.
    let err = rejects("exc-value", "class E(Exception):\n    pass\nx = E(\"m\")\n");
    assert!(err.contains("is an exception class"), "{err}");
    assert!(err.contains("raise E"), "{err}");
}

#[test]
fn subclassing_generator_exit_is_rejected_with_the_reason() {
    let err = rejects("genexit", "class E(GeneratorExit):\n    pass\n");
    assert!(err.contains("BaseException-only"), "{err}");
}

#[test]
fn an_unknown_exception_name_suggests_defining_one() {
    let err = rejects("unknown", "try:\n    pass\nexcept Nope:\n    pass\n");
    assert!(err.contains("class Nope(Exception)"), "{err}");
}

#[test]
fn a_base_declared_after_its_subclass_is_rejected_like_cpython() {
    // CPython raises NameError here; resolving it anyway would accept a
    // program Python rejects.
    let err = rejects(
        "forward-base",
        "class B(A):\n    pass\nclass A(Exception):\n    pass\n",
    );
    assert!(err.contains("unknown base class 'A'"), "{err}");
}

#[test]
fn a_docstring_body_is_allowed() {
    matches_python(
        "docstring",
        r#"
class E(Exception):
    """Raised when the thing goes wrong."""

try:
    raise E("d")
except E as e:
    print("ok:", e)
"#,
    );
}

#[test]
fn regular_classes_are_unaffected() {
    matches_python(
        "regular-classes",
        r#"
class Point:
    def __init__(self, x: int, y: int) -> None:
        self.x = x
        self.y = y
    def sum(self) -> int:
        return self.x + self.y

class Shifted(Point):
    def sum(self) -> int:
        return self.x + self.y + 1

print(Point(1, 2).sum(), Shifted(1, 2).sum())

class E(Exception):
    pass
try:
    raise E("still works")
except E as e:
    print(e)
"#,
    );
}

#[test]
fn keyerror_displays_its_argument_as_a_repr() {
    // CPython's KeyError.__str__ is repr(args[0]), not str of it: a str key
    // quotes, an int key does not. Storage stays raw, so `e.args[0]` is the
    // key itself -- storing the quoted form made a one-character key three
    // characters, a wrong value rather than merely wrong text.
    matches_python(
        "keyerror-display",
        r#"
try:
    raise KeyError("k")
except KeyError as e:
    print(str(e), repr(e), e.args[0], len(e.args[0]))

d = {"a": 1}
try:
    d["z"]
except KeyError as e:
    print(str(e), repr(e), e.args[0], len(e.args[0]))

s = {2}
try:
    s.remove(3)
except KeyError as e:
    print(str(e), repr(e))

try:
    raise ValueError("v")
except ValueError as e:
    print(str(e), repr(e), e.args[0])
"#,
    );
}

#[test]
fn keyerror_from_empty_containers_keeps_its_message() {
    matches_python(
        "keyerror-empty-containers",
        r#"
d: dict[str, int] = {}
try:
    d.popitem()
except KeyError as e:
    print(str(e), e.args[0])
s: set[int] = set()
try:
    s.pop()
except KeyError as e:
    print(str(e), e.args[0])
"#,
    );
}
