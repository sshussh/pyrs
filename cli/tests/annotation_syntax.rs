//! String (forward-reference) annotations and `__future__` directives must
//! behave like CPython, and the two syntaxes PyRs cannot support must say so
//! plainly rather than surfacing a misleading parse error.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

const PYRS: &str = env!("CARGO_BIN_EXE_pyrs");

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        // Keep the inputs when a test fails. CI uploads `target/tmp`, so a
        // directory deleted on the way out makes a CI-only failure
        // impossible to reproduce from the artifact.
        if std::thread::panicking() {
            eprintln!("retaining failure artifacts in {}", self.0.display());
            return;
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn temp_source(tag: &str, source: &str) -> (TempDir, PathBuf) {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-annotation-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    (dir, src)
}

/// Differential check against the CPython oracle at every optimization level.
/// Exit status is compared as well as stdout, so a program that prints the
/// right bytes but fails cannot pass.
fn matches_python_at_all_opt_levels(tag: &str, source: &str) {
    let (_dir, src) = temp_source(tag, source);
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
        assert_eq!(
            actual.status.success(),
            expected.status.success(),
            "exit status differs for {tag} at -O{opt}\nstderr: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&actual.stdout),
            String::from_utf8_lossy(&expected.stdout),
            "stdout differs for {tag} at -O{opt}"
        );
    }
}

/// The program must be rejected, with `needle` in the diagnostic, and nothing
/// may be executed.
fn rejected_with(tag: &str, source: &str, needle: &str) {
    let (_dir, src) = temp_source(tag, source);
    let out = Command::new(PYRS)
        .args(["run", "-i"])
        .arg(&src)
        .output()
        .expect("failed to spawn PyRs");
    assert!(
        !out.status.success(),
        "{tag} compiled but should have been rejected"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(needle),
        "{tag}: expected {needle:?} in diagnostic, got:\n{stderr}"
    );
    assert!(
        out.stdout.is_empty(),
        "{tag}: produced output before rejecting: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn string_annotation_names_the_enclosing_class() {
    matches_python_at_all_opt_levels(
        "self-ref",
        r#"
class Node:
    def __init__(self, v: int) -> None:
        self.v = v

    def same(self, other: "Node") -> bool:
        return self.v == other.v

print(Node(1).same(Node(1)))
print(Node(1).same(Node(2)))
"#,
    );
}

#[test]
fn string_annotations_cover_generics_and_returns() {
    matches_python_at_all_opt_levels(
        "generics",
        r#"
def total(xs: "list[int]", label: "str") -> "int":
    n = 0
    for x in xs:
        n += x
    print(label + ": " + str(n))
    return n

total([1, 2, 3], "sum")
total([], "empty")
"#,
    );
}

#[test]
fn string_annotations_cover_unions_and_optionals() {
    matches_python_at_all_opt_levels(
        "unions",
        r#"
def describe(x: "int | None") -> "str":
    if x is None:
        return "none"
    return str(x)

def pick(x: "Optional[int]") -> "int":
    if x is None:
        return -1
    return x

print(describe(3))
print(describe(None))
print(pick(7))
print(pick(None))
"#,
    );
}

#[test]
fn string_annotation_on_a_local_variable() {
    matches_python_at_all_opt_levels(
        "local-var",
        r#"
y: "int" = 21

def double(x: "int") -> "int":
    return x * 2

print(double(y))
"#,
    );
}

#[test]
fn nested_string_annotation_resolves() {
    matches_python_at_all_opt_levels(
        "nested",
        r#"
class Item:
    def __init__(self, n: int) -> None:
        self.n = n

def first(xs: "list[Item]") -> "int":
    return xs[0].n

print(first([Item(5), Item(6)]))
"#,
    );
}

#[test]
fn future_annotations_import_is_a_noop() {
    matches_python_at_all_opt_levels(
        "future-annotations",
        r#"
from __future__ import annotations

def double(x: int) -> int:
    return x * 2

print(double(21))
"#,
    );
}

#[test]
fn future_import_accepts_python3_mandatory_features() {
    matches_python_at_all_opt_levels(
        "future-mandatory",
        r#"
from __future__ import annotations, division, print_function

print(7 / 2)
"#,
    );
}

#[test]
fn unknown_future_feature_is_rejected_like_cpython() {
    rejected_with(
        "future-unknown",
        "from __future__ import nonexistent_feature\nprint(1)\n",
        "future feature nonexistent_feature is not defined",
    );
}

#[test]
fn future_star_import_is_rejected() {
    rejected_with(
        "future-star",
        "from __future__ import *\nprint(1)\n",
        "not allowed",
    );
}

#[test]
fn plain_import_future_still_reports_a_missing_module() {
    // Only the `from` form is a directive; `import __future__` is an ordinary
    // module import and must not be silently accepted.
    rejected_with(
        "future-plain-import",
        "import __future__\nprint(1)\n",
        "No module named '__future__'",
    );
}

#[test]
fn empty_string_annotation_is_rejected() {
    rejected_with(
        "empty-annotation",
        "def f(x: \"\") -> None:\n    print(x)\n\nf(1)\n",
        "empty string",
    );
}

#[test]
fn malformed_string_annotation_is_rejected() {
    rejected_with(
        "malformed-annotation",
        "def f(x: \"list[\") -> None:\n    print(x)\n\nf(1)\n",
        "string annotation",
    );
}

#[test]
fn tuple_subscript_reports_its_own_limitation() {
    // Previously "expected ']' to close the subscript, found ','", which did
    // not say what was actually unsupported.
    rejected_with(
        "tuple-subscript",
        "xs = [[1, 2], [3, 4]]\nprint(xs[0, 1])\n",
        "tuple subscripts",
    );
}

#[test]
fn matmul_operator_reports_its_own_limitation() {
    // Previously "expected ')' after call arguments, found '@'".
    rejected_with(
        "matmul",
        "a = 2\nb = 3\nprint(a @ b)\n",
        "matrix multiplication operator",
    );
}

#[test]
fn decorators_still_parse_after_the_matmul_diagnostic() {
    // `@` at statement start must keep working; the new error is only for `@`
    // in operator position.
    matches_python_at_all_opt_levels(
        "decorator",
        r#"
def twice(f):
    def inner(n: int) -> int:
        return f(f(n))
    return inner

@twice
def inc(n: int) -> int:
    return n + 1

print(inc(1))
"#,
    );
}
