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

fn assert_pyrs_output(tag: &str, source: &str, expected: &str) {
    let program = Program::new(tag, source);
    let pyrs = program.run_pyrs_at("2");
    assert!(
        pyrs.status.success(),
        "PyRs failed for {tag}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&pyrs.stdout),
        String::from_utf8_lossy(&pyrs.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&pyrs.stdout),
        expected,
        "stdout differs for {tag}"
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
        print('base-eq', self.x, other.x)
        return self.x == other.x
class Child(Base):
    pass
class Sub(Base):
    def __eq__(self, other: Base) -> bool:
        print('sub-eq', self.x, other.x)
        return False
def cmp(xs: list[Base], ys: list[Base]) -> bool:
    return xs == ys
print(cmp([Sub(1)], [Base(1)]))
print(cmp([Base(1)], [Base(1)]))
print([Sub(1)] == [Sub(1)])
print([Child(1)] == [Child(1)])
s = Sub(1)
print(cmp([s], [s]))
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
print((P(1),) != (P(1),))
print((P(1),) != (P(2),))
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
print(xs.index(P(1), -100))
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
fn list_index_end_bound_miss_is_value_error() {
    assert_runtime_error(
        "list-index-end-miss",
        &format!(
            "\
{POINT}
print([P(1), P(2), P(1)].index(P(1), 1, 2))
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
    assert_matches_python_at_all_opt_levels(
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
def mk() -> list[P]:
    print('mk')
    return [P(1)]
print(mk() == mk())
def n() -> P:
    print('n')
    return P(1)
def h() -> list[P]:
    print('h')
    return [P(1)]
print(n() in h())
def boom() -> P:
    raise ValueError('n')
def later() -> list[P]:
    print('later')
    return [P(1)]
try:
    print(boom() in later())
except ValueError as e:
    print('caught-n', e)
class Boom:
    def __init__(self, x: int):
        self.x = x
    def __eq__(self, other: Boom) -> bool:
        raise ValueError('boom')
try:
    print([Boom(1)] == [Boom(1)])
except ValueError as e:
    print('caught', e)
try:
    print([Boom(1)].index(Boom(1)))
except ValueError as e:
    print('caught-index', e)
try:
    xs = [Boom(1)]
    xs.remove(Boom(1))
except ValueError as e:
    print('caught-remove', e)
",
    );
}

#[test]
fn contains_compares_needle_to_item() {
    assert_matches_python_at_all_opt_levels(
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

#[test]
fn container_identity_skips_eq() {
    assert_matches_python(
        "ident-skip-eq",
        "\
class P:
    def __init__(self, x: int):
        self.x = x
    def __eq__(self, other: P) -> bool:
        print('eq')
        return False
a = P(1)
print([a] == [a])
print(a in [a])
print([a].index(a), [a].count(a))
xs = [a]
xs.remove(a)
print(len(xs))
print((a,) == (a,))
print(a in (a, a))
print((a, a).index(a), (a, a).count(a))
print(a == a)
",
    );
}

#[test]
fn mixed_tuple_eq_with_dict() {
    assert_matches_python(
        "tuple-dict-eq",
        &format!(
            "\
{POINT}
print(({{1: 2}}, P(1)) == ({{1: 2}}, P(1)))
print(({{1: 2}}, P(1)) == ({{1: 3}}, P(1)))
print(({{1: 2}}, P(1)) == ({{1: 2}}, P(2)))
"
        ),
    );
}

#[test]
fn mixed_tuple_membership_stays_identity() {
    assert_pyrs_output(
        "mixed-tuple-in",
        &format!(
            "\
{POINT}
a = P(1)
print(a in (a, 7))
print(P(1) in (a, 7))
print((a, 7).index(a), (a, 7).count(a))
print((a, 7).count(P(1)))
"
        ),
        "\
True
False
0 1
0
",
    );
}

#[test]
fn homogeneous_tuple_membership_uses_class_eq() {
    assert_matches_python(
        "tuple-in-eq",
        &format!(
            "\
{POINT}
print(P(1) in (P(1), P(2)))
print(P(9) in (P(1), P(2)))
print((P(1), P(2), P(1)).index(P(1)), (P(1), P(2), P(1)).count(P(1)))
"
        ),
    );
}

#[test]
fn list_index_evaluates_bounds_before_len() {
    assert_matches_python(
        "index-mut-bounds",
        &format!(
            "\
{POINT}
def do_start(xs: list[P]) -> int:
    xs.append(P(2))
    return 0
def do_end(ys: list[P]) -> int:
    ys.clear()
    return 2
xs = [P(1)]
print(xs.index(P(2), do_start(xs)))
ys = [P(1), P(2)]
try:
    print(ys.index(P(1), 0, do_end(ys)))
except ValueError as e:
    print('miss', e)
"
        ),
    );
}
