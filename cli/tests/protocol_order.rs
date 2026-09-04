//! Class protocols must dispatch only after evaluating source operands in order.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

const PYRS: &str = env!("CARGO_BIN_EXE_pyrs");

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn matches_python_at_all_opt_levels(tag: &str, source: &str) {
    let dir = TempDir(
        std::env::temp_dir().join(format!("pyrs-protocol-order-{tag}-{}", std::process::id())),
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
        "CPython failed: {}",
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
            "PyRs {tag} at -O{opt} failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&actual.stdout),
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&actual.stdout),
            String::from_utf8_lossy(&expected.stdout),
            "protocol evaluation order differs for {tag} at -O{opt}"
        );
    }
}

#[test]
fn membership_evaluates_needle_before_container_once() {
    matches_python_at_all_opt_levels(
        "membership",
        r#"
trace: list[str] = []
state: list[int] = [0]

class Box:
    def __init__(self, limit: int):
        self.limit = limit
    def __contains__(self, needle: int) -> int:
        trace.append("contains")
        if needle < self.limit:
            return 7
        return 0

class Child(Box):
    def __contains__(self, needle: int) -> int:
        trace.append("child contains")
        return super().__contains__(needle)

def needle() -> int:
    trace.append("needle")
    return state[0]

def container() -> Box:
    trace.append("container")
    state[0] += 1
    return Child(state[0])

def bad_needle() -> int:
    trace.append("bad needle")
    raise ValueError("needle failed")

def bad_container() -> Box:
    trace.append("bad container")
    raise ValueError("container failed")

print(needle() in container())
print(needle() not in container())
try:
    print(bad_needle() in container())
except ValueError as e:
    print(e)
try:
    print(needle() not in bad_container())
except ValueError as e:
    print(e)
print(state)
print(trace)
"#,
    );
}

#[test]
fn direct_reflected_and_subclass_comparisons_preserve_source_order() {
    matches_python_at_all_opt_levels(
        "comparison_dispatch",
        r#"
trace: list[str] = []
state: list[int] = [0]

class Direct:
    def __init__(self, value: int):
        self.value = value
    def __lt__(self, other: Direct) -> bool:
        trace.append("lt")
        return self.value < other.value
    def __le__(self, other: Direct) -> bool:
        trace.append("le")
        return self.value <= other.value
    def __eq__(self, other: Direct) -> bool:
        trace.append("eq")
        return self.value == other.value

class Reflected:
    def __init__(self, value: int):
        self.value = value
    def __gt__(self, other: int) -> bool:
        trace.append("reflected gt")
        return self.value > other
    def __ge__(self, other: int) -> bool:
        trace.append("reflected ge")
        return self.value >= other
    def __lt__(self, other: int) -> bool:
        trace.append("reflected lt")
        return self.value < other
    def __le__(self, other: int) -> bool:
        trace.append("reflected le")
        return self.value <= other
    def __eq__(self, other: int) -> bool:
        trace.append("reflected eq")
        return self.value == other

class Child(Direct):
    def __gt__(self, other: Direct) -> bool:
        trace.append("child gt")
        return self.value > other.value
    def __eq__(self, other: Direct) -> bool:
        trace.append("child eq")
        return self.value == other.value

def direct(label: str) -> Direct:
    trace.append(label)
    state[0] += 1
    return Direct(state[0])

def number() -> int:
    trace.append("number")
    return state[0]

def reflected() -> Reflected:
    trace.append("reflected")
    state[0] += 1
    return Reflected(state[0])

def child() -> Child:
    trace.append("child")
    state[0] += 1
    return Child(state[0])

print(direct("left") < direct("right"))
print(direct("left") <= direct("right"))
print(direct("left") > direct("right"))
print(direct("left") >= direct("right"))
print(direct("left") == direct("right"))
print(direct("left") != direct("right"))
print(number() < reflected())
print(number() <= reflected())
print(number() > reflected())
print(number() >= reflected())
print(number() == reflected())
print(number() != reflected())
print(direct("base") < child())
print(direct("base") == child())
print(direct("base") != child())
print(state)
print(trace)
"#,
    );
}

#[test]
fn comparison_chains_evaluate_each_operand_once_and_short_circuit() {
    matches_python_at_all_opt_levels(
        "comparison_chains",
        r#"
trace: list[str] = []

class Middle:
    def __init__(self, value: int):
        self.value = value
    def __gt__(self, other: int) -> bool:
        trace.append("middle gt")
        return self.value > other
    def __lt__(self, other: int) -> bool:
        trace.append("middle lt")
        return self.value < other

def number(label: str, value: int) -> int:
    trace.append(label)
    return value

def middle(value: int) -> Middle:
    trace.append("middle")
    return Middle(value)

def bad_first() -> int:
    trace.append("bad first")
    raise ValueError("first failed")

def bad_last() -> int:
    trace.append("bad last")
    raise ValueError("last failed")

print(number("first", 1) < middle(2) < number("last", 3))
print(number("first", 3) < middle(2) < number("skipped", 4))
print(number("first", 1) < middle(2) > number("last", 0))
print(number("first", 3) < middle(2) < bad_last())
try:
    print(bad_first() < middle(2) < number("skipped", 3))
except ValueError as e:
    print(e)
try:
    print(number("first", 1) < middle(2) < bad_last())
except ValueError as e:
    print(e)
print(trace)
"#,
    );
}

#[test]
fn operand_exceptions_prevent_later_operands_and_protocol_calls() {
    matches_python_at_all_opt_levels(
        "comparison_exceptions",
        r#"
trace: list[str] = []

class Right:
    def __gt__(self, other: int) -> bool:
        trace.append("gt")
        return True
    def __eq__(self, other: int) -> bool:
        trace.append("eq")
        return False

def left() -> int:
    trace.append("left")
    return 1

def right() -> Right:
    trace.append("right")
    return Right()

def bad_left() -> int:
    trace.append("bad left")
    raise ValueError("left failed")

def bad_right() -> Right:
    trace.append("bad right")
    raise ValueError("right failed")

try:
    print(bad_left() < right())
except ValueError as e:
    print(e)
try:
    print(bad_left() == right())
except ValueError as e:
    print(e)
try:
    print(bad_left() != right())
except ValueError as e:
    print(e)
try:
    print(left() < bad_right())
except ValueError as e:
    print(e)
try:
    print(left() == bad_right())
except ValueError as e:
    print(e)
try:
    print(left() != bad_right())
except ValueError as e:
    print(e)
print(trace)
"#,
    );
}
