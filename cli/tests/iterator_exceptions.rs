//! User-iterator StopIteration isolation and shared for/comprehension iterables.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

const PYRS: &str = env!("CARGO_BIN_EXE_pyrs");

struct Program(PathBuf);

impl Program {
    fn new(tag: &str, source: &str) -> Self {
        let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "pyrs-iterator-exceptions-{tag}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("prog.py"), source).unwrap();
        Self(dir)
    }

    fn path(&self) -> PathBuf {
        self.0.join("prog.py")
    }

    fn run_pyrs_at(&self, opt: &str) -> Output {
        Command::new(PYRS)
            .args(["run", "-O", opt, "-i"])
            .arg(self.path())
            .output()
            .expect("failed to spawn PyRs")
    }
}

impl Drop for Program {
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

fn python3(path: &PathBuf) -> Output {
    Command::new("python3")
        .arg(path)
        .output()
        .expect("failed to spawn python3")
}

fn assert_matches_python(tag: &str, source: &str) {
    assert_matches_python_at(tag, source, "2");
}

fn assert_matches_python_at_all_opt_levels(tag: &str, source: &str) {
    for opt in ["0", "2", "3"] {
        assert_matches_python_at(&format!("{tag}-O{opt}"), source, opt);
    }
}

fn assert_matches_python_at(tag: &str, source: &str, opt: &str) {
    let program = Program::new(tag, source);
    let python = python3(&program.path());
    assert!(
        python.status.success(),
        "python3 failed for {tag}: {}",
        String::from_utf8_lossy(&python.stderr)
    );
    let pyrs = program.run_pyrs_at(opt);
    assert!(
        pyrs.status.success(),
        "PyRs failed for {tag} at -O{opt}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&pyrs.stdout),
        String::from_utf8_lossy(&pyrs.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&pyrs.stdout),
        String::from_utf8_lossy(&python.stdout),
        "stdout differs for {tag} at -O{opt}"
    );
}

fn assert_diagnostic(tag: &str, source: &str, expected: &[&str]) {
    let program = Program::new(tag, source);
    let output = program.run_pyrs_at("2");
    assert!(
        !output.status.success(),
        "expected a semantic error for {tag}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for fragment in expected {
        assert!(
            stderr.contains(fragment),
            "expected {fragment:?} in diagnostic for {tag}: {stderr}"
        );
    }
}

const COUNTER: &str = r#"
class Counter:
    def __init__(self, n: int):
        self.n = n
        self.i = 0
    def __iter__(self) -> Counter:
        return self
    def __next__(self) -> int:
        if self.i >= self.n:
            raise StopIteration("done")
        v = self.i
        self.i = self.i + 1
        return v
"#;

#[test]
fn body_stopiteration_propagates_and_skips_else() {
    assert_matches_python_at_all_opt_levels(
        "body-si",
        &(COUNTER.to_string()
            + r#"
try:
    for x in Counter(1):
        raise StopIteration("body")
    else:
        print("loop-else")
except StopIteration as e:
    print(e)
"#),
    );
}

#[test]
fn exhaustion_runs_else_without_leaking_stopiteration() {
    assert_matches_python_at_all_opt_levels(
        "exhaust-else",
        &(COUNTER.to_string()
            + r#"
try:
    for x in Counter(0):
        print("body", x)
    else:
        print("loop-else")
except StopIteration as e:
    print("leaked", e)
"#),
    );
}

#[test]
fn next_valueerror_is_not_exhaustion() {
    assert_matches_python(
        "next-valueerror",
        r#"
class Boom:
    def __iter__(self) -> Boom:
        return self
    def __next__(self) -> int:
        raise ValueError("nope")
try:
    for x in Boom():
        print(x)
    else:
        print("loop-else")
except ValueError as e:
    print(e)
"#,
    );
}

#[test]
fn break_skips_else_continue_requests_next() {
    assert_matches_python(
        "break-continue",
        &(COUNTER.to_string()
            + r#"
for x in Counter(4):
    if x == 1:
        continue
    if x == 3:
        break
    print(x)
else:
    print("loop-else")
"#),
    );
}

#[test]
fn return_from_user_iter_loop() {
    assert_matches_python(
        "return-loop",
        &(COUNTER.to_string()
            + r#"
def f() -> int:
    for x in Counter(3):
        if x == 1:
            return x
    return -1
print(f())
"#),
    );
}

#[test]
fn finally_runs_around_body_stopiteration() {
    assert_matches_python(
        "finally-si",
        &(COUNTER.to_string()
            + r#"
try:
    try:
        for x in Counter(1):
            raise StopIteration("body")
    finally:
        print("finally")
except StopIteration as e:
    print(e)
"#),
    );
}

#[test]
fn nested_inner_exhaustion_does_not_swallow_outer_body_si() {
    assert_matches_python(
        "nested-si",
        &(COUNTER.to_string()
            + r#"
try:
    for x in Counter(1):
        for y in Counter(0):
            print("inner", y)
        raise StopIteration("outer-body")
except StopIteration as e:
    print(e)
"#),
    );
}

#[test]
fn inherited_iter_and_distinct_iterator_class() {
    assert_matches_python(
        "virtual-iter",
        r#"
class BaseIter:
    def __init__(self, n: int):
        self.n = n
        self.i = 0
    def __next__(self) -> int:
        if self.i >= self.n:
            raise StopIteration("done")
        v = self.i
        self.i = self.i + 1
        return v

class ChildIter(BaseIter):
    def __next__(self) -> int:
        return super().__next__() * 10

class Box:
    def __init__(self, n: int):
        self.n = n
    def __iter__(self) -> ChildIter:
        return ChildIter(self.n)

print([x for x in Box(3)])
for x in Box(2):
    print(x)
"#,
    );
}

#[test]
fn unpack_valueerror_during_bind_propagates() {
    assert_matches_python(
        "bind-valueerror",
        r#"
class Once:
    def __init__(self):
        self.i = 0
    def __iter__(self) -> Once:
        return self
    def __next__(self) -> list[int]:
        if self.i >= 1:
            raise StopIteration("done")
        self.i = self.i + 1
        return [1]
try:
    for a, b in Once():
        print(a, b)
    else:
        print("loop-else")
except ValueError as e:
    print("value-error")
"#,
    );
}

#[test]
fn comprehension_matrix_tuple_dict_set_generator() {
    assert_matches_python(
        "comp-matrix",
        r#"
print([x for x in (1, 2, 3)])
print([k for k in {"a": 1, "b": 2}])
print(sorted([x for x in {3, 1, 2}]))
def g():
    yield 4
    yield 5
print([x for x in g() if x > 4])
print({x for x in (1, 2, 1)})
print({k: 1 for k in {"z": 0, "y": 0}})
"#,
    );
}

#[test]
fn comprehension_over_user_iterator() {
    assert_matches_python_at_all_opt_levels(
        "comp-user-iter",
        &(COUNTER.to_string()
            + r#"
print([x for x in Counter(4) if x % 2 == 0])
print({x for x in Counter(3)})
"#),
    );
}

#[test]
fn comprehension_body_stopiteration_propagates() {
    assert_matches_python(
        "comp-body-si",
        &(COUNTER.to_string()
            + r#"
def boom(x: int) -> int:
    raise StopIteration("comp-body")
try:
    print([boom(x) for x in Counter(1)])
except StopIteration as e:
    print(e)
"#),
    );
}

#[test]
fn for_and_comp_over_file() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "pyrs-iterator-file-{}-{}",
        std::process::id(),
        "txt"
    ));
    fs::create_dir_all(&dir).unwrap();
    let data = dir.join("data.txt");
    fs::write(&data, "alpha\nbeta\n").unwrap();
    let path = data.display().to_string().replace('\\', "\\\\");
    let source = format!(
        r#"
f = open("{path}")
print([line.strip() for line in f if line.strip() != "alpha"])
f.close()
g = open("{path}")
for line in g:
    print(line.strip())
g.close()
"#
    );
    let result = std::panic::catch_unwind(|| {
        assert_matches_python("file-iter", &source);
    });
    let _ = fs::remove_dir_all(&dir);
    result.unwrap();
}

#[test]
fn non_iterable_class_is_rejected_in_for_and_comp() {
    assert_diagnostic(
        "for-not-iter",
        r#"
class C:
    def __init__(self):
        self.x = 1
for x in C():
    print(x)
"#,
        &["not iterable"],
    );
    assert_diagnostic(
        "comp-not-iter",
        r#"
class C:
    def __init__(self):
        self.x = 1
print([x for x in C()])
"#,
        &["not iterable"],
    );
}
