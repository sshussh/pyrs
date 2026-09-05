//! Differential coverage for list/tuple equality using class `__eq__`.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

const PYRS: &str = env!("CARGO_BIN_EXE_pyrs");

struct Program(PathBuf);

impl Program {
    fn new(tag: &str, source: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pyrs-container-class-eq-{tag}-{}",
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

fn assert_runtime_error(tag: &str, source: &str, expected: &[&str]) {
    let program = Program::new(tag, source);
    let python = python3(&program.path());
    assert!(
        !python.status.success(),
        "expected python3 to fail for {tag}"
    );
    let pyrs = program.run_pyrs_at("2");
    assert!(
        !pyrs.status.success(),
        "expected PyRs to fail for {tag}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&pyrs.stdout),
        String::from_utf8_lossy(&pyrs.stderr)
    );
    let stderr = String::from_utf8_lossy(&pyrs.stderr);
    for fragment in expected {
        assert!(
            stderr.contains(fragment),
            "expected {fragment:?} in PyRs stderr for {tag}: {stderr}"
        );
    }
}

const POINT: &str = "\
class P:
    def __init__(self, x: int):
        self.x = x
    def __eq__(self, other: P) -> bool:
        return self.x == other.x
";

#[test]
fn list_eq_uses_class_eq() {
    assert_matches_python_at_all_opt_levels(
        "list-eq",
        &format!(
            "\
{POINT}
print([P(1)] == [P(1)])
print([P(1)] == [P(2)])
print([P(1)] != [P(1)])
print([P(1)] != [P(2)])
print([P(1), P(2)] == [P(1), P(2)])
print([P(1), P(2)] == [P(1), P(3)])
print([P(1)] == [P(1), P(1)])
xs: list[P] = []
ys: list[P] = []
print(xs == ys)
a = P(1)
print([a] == [a])
"
        ),
    );
}

#[test]
fn list_eq_identity_without_dunder() {
    assert_matches_python(
        "list-eq-id",
        "\
class A:
    pass
print([A()] == [A()])
a = A()
print([a] == [a])
print([a] != [A()])
",
    );
}

#[test]
fn list_eq_virtual_and_inherited() {
    assert_matches_python(
        "list-eq-virt",
        "\
class Base:
    def __init__(self, x: int):
        self.x = x
    def __eq__(self, other: Base) -> bool:
        return self.x == other.x
class Sub(Base):
    def __eq__(self, other: Base) -> bool:
        return False
def cmp(xs: list[Base], ys: list[Base]) -> bool:
    return xs == ys
print(cmp([Sub(1)], [Base(1)]))
print(cmp([Base(1)], [Base(1)]))
print([Sub(1)] == [Sub(1)])
",
    );
}

#[test]
fn list_ne_uses_element_eq_not_ne() {
    assert_matches_python(
        "list-ne-eq",
        "\
class P:
    def __init__(self, x: int):
        self.x = x
    def __eq__(self, other: P) -> bool:
        return self.x == other.x
    def __ne__(self, other: P) -> bool:
        return True
print(P(1) != P(1))
print([P(1)] != [P(1)])
print([P(1)] != [P(2)])
",
    );
}

#[test]
fn nested_list_eq_uses_class_eq() {
    assert_matches_python(
        "nested-list-eq",
        &format!(
            "\
{POINT}
print([[P(1)]] == [[P(1)]])
print([[P(1)]] == [[P(2)]])
print([[P(1), P(2)]] == [[P(1), P(2)]])
print([P(1)] in [[P(1)], [P(2)]])
print([P(3)] in [[P(1)], [P(2)]])
"
        ),
    );
}

#[test]
fn list_contains_uses_class_eq() {
    assert_matches_python(
        "list-in",
        &format!(
            "\
{POINT}
xs = [P(1), P(2), P(3)]
print(P(2) in xs)
print(P(9) in xs)
print(P(2) not in xs)
print(P(9) not in xs)
empty: list[P] = []
print(P(1) in empty)
"
        ),
    );
}

#[test]
fn list_index_count_remove_use_class_eq() {
    assert_matches_python(
        "list-methods",
        &format!(
            "\
{POINT}
xs = [P(1), P(2), P(1), P(3)]
print(xs.index(P(1)), xs.index(P(2)), xs.index(P(1), 1))
print(xs.index(P(3), -1), xs.index(P(2), -3, -1))
print(xs.count(P(1)), xs.count(P(2)), xs.count(P(9)))
xs.remove(P(1))
print(xs[0].x, xs[1].x, xs[2].x)
xs.remove(P(1))
print(len(xs), xs[0].x, xs[1].x)
"
        ),
    );
}

#[test]
fn list_index_miss_is_value_error() {
    assert_runtime_error(
        "list-index-miss",
        &format!(
            "\
{POINT}
print([P(1)].index(P(2)))
"
        ),
        &["ValueError: list.index(x): x not in list"],
    );
}

#[test]
fn list_remove_miss_is_value_error() {
    assert_runtime_error(
        "list-remove-miss",
        &format!(
            "\
{POINT}
xs = [P(1)]
xs.remove(P(2))
"
        ),
        &["ValueError: list.remove(x): x not in list"],
    );
}

#[test]
fn tuple_eq_uses_class_eq() {
    assert_matches_python(
        "tuple-eq",
        &format!(
            "\
{POINT}
print((P(1),) == (P(1),))
print((P(1),) == (P(2),))
print((P(1), 7) == (P(1), 7))
print((P(1), 7) == (P(1), 8))
print((P(1), 7) != (P(2), 7))
print((P(1), P(2)) == (P(1), P(2)))
"
        ),
    );
}

#[test]
fn eq_side_effects_and_exceptions() {
    assert_matches_python(
        "eq-effects",
        "\
class P:
    def __init__(self, x: int):
        self.x = x
    def __eq__(self, other: P) -> bool:
        print('eq', self.x, other.x)
        return self.x == other.x
print([P(1), P(2)] == [P(1), P(2)])
print([P(1), P(2)] == [P(1), P(9)])
print([P(1)] == [P(1), P(2)])
print(P(2) in [P(1), P(2), P(3)])
class Boom:
    def __init__(self, x: int):
        self.x = x
    def __eq__(self, other: Boom) -> bool:
        raise ValueError('boom')
try:
    print([Boom(1)] == [Boom(1)])
except ValueError as e:
    print('caught', e)
",
    );
}

#[test]
fn contains_compares_needle_to_item() {
    assert_matches_python(
        "in-order",
        "\
class P:
    def __init__(self, x: int):
        self.x = x
    def __eq__(self, other: P) -> bool:
        print('eq', self.x, other.x)
        return self.x == other.x
print(P(2) in [P(1), P(2), P(3)])
",
    );
}
