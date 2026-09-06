//! Generator expressions: `(elem for target in iter if cond)`.
//!
//! These were a parse error, which made the four most common consuming idioms
//! — `sum(x for x in xs)`, `any(...)`, `max(...)`, `",".join(...)` — all
//! unavailable at once. They were the largest single cluster in a survey of
//! common Python constructs against CPython.
//!
//! The property that distinguishes a generator expression from the list
//! comprehension it resembles is laziness, so the tests print from inside the
//! producing code: a comprehension-based desugaring would run the whole thing
//! eagerly and the traces would differ even where the final answer agreed.

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
            .join(format!("pyrs-genexp-{tag}-{}", std::process::id())),
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
// The consuming idioms
// ---------------------------------------------------------------------------

#[test]
fn the_common_consumers_accept_a_bare_generator_expression() {
    matches_python(
        "consumers",
        r#"
xs = [1, 2, 3, 4]
print(sum(x for x in xs))
print(sum(x * 2 for x in xs))
print(any(x > 3 for x in xs), all(x > 0 for x in xs))
print(max(x for x in xs), min(x for x in xs))
print(list(x for x in xs))
print(sorted(x for x in xs))
print(",".join(str(x) for x in xs))
print(len(set(x % 2 for x in xs)))
"#,
    );
}

#[test]
fn filters_and_multiple_clauses_work() {
    matches_python(
        "clauses",
        r#"
xs = [1, 2, 3, 4, 5]
print(list(x for x in xs if x % 2 == 0))
print(list(x for x in xs if x > 1 if x < 5))
print(sum(x * y for x in [1, 2] for y in [10, 20]))
print(list(x + y for x in [1, 2] for y in [10, 20] if x != 1))
"#,
    );
}

#[test]
fn a_parenthesized_generator_expression_is_a_value() {
    matches_python(
        "value",
        r#"
xs = [1, 2, 3]
g = (x + 1 for x in xs)
for v in g:
    print(v)
h = (x for x in xs if x > 1)
print(list(h))
print(sum((x for x in xs)))
"#,
    );
}

#[test]
fn element_types_other_than_int_are_inferred() {
    matches_python(
        "elem-types",
        r#"
xs = [1, 2]
ss = ["a", "b"]
print(list(str(x) for x in xs))
print(list(s.upper() for s in ss))
print(list(x * 1.5 for x in xs))
print(list(x > 1 for x in xs))
print(",".join(s + "!" for s in ss))
"#,
    );
}

#[test]
fn a_generator_expression_can_iterate_many_kinds_of_iterable() {
    matches_python(
        "iterables",
        r#"
print(sum(x for x in range(5)))
print(list(c for c in "abc"))
print(sorted(list(k for k in {"b": 1, "a": 2})))
print(sorted(list(v for v in {3, 1, 2})))
print(list(x for x in (1, 2, 3)))
def g():
    yield 1
    yield 2
print(list(x * 10 for x in g()))
"#,
    );
}

// ---------------------------------------------------------------------------
// Laziness
// ---------------------------------------------------------------------------

#[test]
fn the_element_expression_runs_only_on_demand() {
    matches_python(
        "lazy-elem",
        r#"
def trace(v: int) -> int:
    print("made", v)
    return v

g = (trace(x) for x in [1, 2])
print("created")
print(list(g))
"#,
    );
}

#[test]
fn any_and_all_short_circuit_through_a_generator_expression() {
    matches_python(
        "lazy-short-circuit",
        r#"
def loud(n: int):
    for i in range(n):
        print("gen", i)
        yield i

print(any(x > 1 for x in loud(5)))
print(all(x < 1 for x in loud(5)))
"#,
    );
}

#[test]
fn the_outermost_iterable_is_evaluated_when_the_generator_is_created() {
    matches_python(
        "eager-iterable",
        r#"
def source() -> list[int]:
    print("source called")
    return [1, 2]

g = (x for x in source())
print("created")
print(list(g))
"#,
    );
}

// ---------------------------------------------------------------------------
// Scoping
// ---------------------------------------------------------------------------

#[test]
fn a_generator_expression_captures_enclosing_locals() {
    matches_python(
        "capture",
        r#"
def f() -> int:
    xs = [1, 2, 3]
    n = 10
    return sum(x + n for x in xs)

def g(n: int) -> int:
    return sum(x * n for x in [1, 2])

print(f(), g(3))
"#,
    );
}

#[test]
fn a_generator_expression_can_call_module_functions() {
    matches_python(
        "call-funcs",
        r#"
def dbl(v: int) -> int:
    return v * 2

print(sum(dbl(x) for x in [1, 2, 3]))
print(list(dbl(x) for x in range(3)))
"#,
    );
}

#[test]
fn the_loop_variable_does_not_leak() {
    matches_python(
        "no-leak",
        r#"
def f() -> int:
    x = 99
    total = sum(x for x in [1, 2, 3])
    return total + x

print(f())
"#,
    );
}

// ---------------------------------------------------------------------------
// Rejections
// ---------------------------------------------------------------------------

#[test]
fn a_bare_generator_expression_must_be_the_only_argument() {
    // CPython requires parentheses when there are other arguments.
    let err = rejects("bare-multi", "print(sum(x for x in [1], 2))\n");
    assert!(err.contains("must be parenthesized"), "{err}");
}

#[test]
fn capturing_a_module_level_variable_is_rejected_with_guidance() {
    // The closure's cell would never be filled, because the assignment writes
    // a global. This used to fail at run time with a NameError.
    let err = rejects(
        "global-capture",
        "xs = [1, 2]\nn = 10\nprint(sum(x + n for x in xs))\n",
    );
    assert!(err.contains("module-level variable"), "{err}");
    assert!(err.contains("Move the code into a function"), "{err}");
}

// ---------------------------------------------------------------------------
// Comprehensions must be unaffected
// ---------------------------------------------------------------------------

#[test]
fn comprehensions_still_behave_as_before() {
    matches_python(
        "comprehensions",
        r#"
xs = [1, 2, 3, 4]
print([x for x in xs])
print([x for x in xs if x % 2 == 0])
print({x: x * 2 for x in xs})
print(sorted(list({x % 2 for x in xs})))
print([[i * j for j in range(2)] for i in range(2)])
print([x + y for x in [1, 2] for y in [10]])
"#,
    );
}

#[test]
fn tuples_and_parenthesized_expressions_are_unaffected() {
    matches_python(
        "parens",
        r#"
t = (1, 2, 3)
print(t, (4), (5,), ())
print((1 + 2) * 3)
print(sum([x for x in t]))
"#,
    );
}
