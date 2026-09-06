//! Generators as arguments to the eager builtins.
//!
//! A generator function could only be consumed by a `for` loop (or a
//! comprehension): every eager builtin rejected one, so `list(g())` — probably
//! the most common thing anyone does with a generator — was a compile error,
//! and there was no way to get the values out except writing the loop by hand.
//!
//! Two properties matter. The consumers that drain their argument anyway
//! (`list`, `set`, `sorted`, `sum`, `max`, `min`, `join`) may materialize
//! first, because side effects, order and result are then identical. `any` and
//! `all` may not: they stop as soon as the answer is known, so the tests below
//! print from inside the generator to pin exactly how far it ran.
//!
//! Also covered: an unannotated generator used to hard-code its yield type to
//! `int`, so `def g(): yield "a"` was rejected outright.

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
            .join(format!("pyrs-genconsume-{tag}-{}", std::process::id())),
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

// ---------------------------------------------------------------------------
// Draining consumers
// ---------------------------------------------------------------------------

#[test]
fn list_and_sorted_accept_a_generator() {
    matches_python(
        "list-sorted",
        r#"
def g():
    yield 3
    yield 1
    yield 2

print(list(g()))
print(sorted(g()))
print(sorted(g(), reverse=True))
print(len(list(g())))
"#,
    );
}

#[test]
fn numeric_folds_accept_a_generator() {
    matches_python(
        "folds",
        r#"
def g():
    yield 3
    yield 1
    yield 2

print(sum(g()))
print(max(g()), min(g()))

def gf():
    yield 1.5
    yield 2.5
print(sum(gf()), max(gf()), min(gf()))
"#,
    );
}

#[test]
fn set_and_join_accept_a_generator() {
    matches_python(
        "set-join",
        r#"
def gi():
    yield 1
    yield 2
    yield 1

s = set(gi())
print(len(s), 1 in s, 2 in s, 3 in s)

def gs():
    yield "a"
    yield "b"
print(",".join(gs()))
print("".join(gs()))
"#,
    );
}

#[test]
fn an_empty_generator_drains_to_an_empty_result() {
    matches_python(
        "empty",
        r#"
def empty():
    if False:
        yield 1

print(list(empty()), sorted(empty()), sum(empty()))
print(len(set(empty())))
"#,
    );
}

#[test]
fn a_generator_is_drained_exactly_once_and_in_order() {
    matches_python(
        "order-effects",
        r#"
def g():
    for i in range(3):
        print("yield", i)
        yield i

print(list(g()))
print(sum(g()))
"#,
    );
}

#[test]
fn a_generator_argument_composes_with_other_expressions() {
    matches_python(
        "compose",
        r#"
def g():
    yield 1
    yield 2

print(sum(g()) + max(g()))
print(list(g())[0], len(list(g())))
xs = list(g())
xs.append(9)
print(xs)
print([v * 2 for v in list(g())])
"#,
    );
}

// ---------------------------------------------------------------------------
// Short-circuiting consumers
// ---------------------------------------------------------------------------

#[test]
fn any_and_all_over_a_generator_give_the_right_answer() {
    matches_python(
        "any-all-values",
        r#"
def zeros():
    yield 0
    yield 0
def mixed():
    yield 0
    yield 1
def ones():
    yield 1
    yield 2
def empty():
    if False:
        yield 1

print(any(zeros()), all(zeros()))
print(any(mixed()), all(mixed()))
print(any(ones()), all(ones()))
print(any(empty()), all(empty()))
"#,
    );
}

#[test]
fn any_stops_at_the_first_truthy_element() {
    // The generator prints as it runs, so the output pins how far it got: a
    // draining implementation would visit every element.
    matches_python(
        "any-short-circuit",
        r#"
def loud():
    for i in range(5):
        print("visit", i)
        yield i

print(any(loud()))
"#,
    );
}

#[test]
fn all_stops_at_the_first_falsy_element() {
    matches_python(
        "all-short-circuit",
        r#"
def loud():
    for i in range(5):
        print("visit", i)
        yield i

print(all(loud()))
"#,
    );
}

#[test]
fn any_and_all_still_work_on_lists_and_strings() {
    matches_python(
        "any-all-other",
        r#"
print(any([0, 1]), all([1, 2]), any([]), all([]))
print(any("a"), all(""))
print(any((0, 2)), all((1, 2)))
"#,
    );
}

// ---------------------------------------------------------------------------
// Yield type inference
// ---------------------------------------------------------------------------

#[test]
fn an_unannotated_generator_infers_its_yield_type() {
    // Previously every unannotated generator was assumed to yield int, so a
    // str/float/bool one was a hard error at the first yield.
    matches_python(
        "infer-yield",
        r#"
def gs():
    yield "a"
    yield "b"
def gf():
    yield 1.5
def gb():
    yield True
def gi():
    yield 7

print(list(gs()), list(gf()), list(gb()), list(gi()))
for v in gs():
    print(v.upper())
"#,
    );
}

#[test]
fn a_yield_type_is_inferred_from_an_annotated_parameter() {
    matches_python(
        "infer-param",
        r#"
def echo(x: str):
    yield x
    yield x

print(list(echo("hi")))
print(",".join(echo("z")))
"#,
    );
}

#[test]
fn a_yield_inside_control_flow_is_still_found() {
    matches_python(
        "infer-nested",
        r#"
def g(flag: bool):
    if flag:
        yield "yes"
    else:
        yield "no"

def h():
    for i in range(2):
        yield "item"

def k():
    try:
        yield "t"
    finally:
        pass

print(list(g(True)), list(g(False)), list(h()), list(k()))
"#,
    );
}

#[test]
fn generators_still_work_in_loops_and_comprehensions() {
    matches_python(
        "unchanged",
        r#"
def g():
    yield 1
    yield 2
    yield 3

for v in g():
    print(v)
print([v for v in g()])
print({v for v in g()} == {1, 2, 3})
print({v: v for v in g()})
total = 0
for v in g():
    total += v
print(total)
"#,
    );
}
