//! Differential coverage for class inequality dispatch and its diagnostics.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

const PYRS: &str = env!("CARGO_BIN_EXE_pyrs");

struct Program(PathBuf);

impl Program {
    fn new(tag: &str, source: &str) -> Self {
        let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "pyrs-class-inequality-{tag}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("prog.py"), source).unwrap();
        Self(dir)
    }

    fn run_pyrs(&self) -> Output {
        Command::new(PYRS)
            .args(["run", "-i"])
            .arg(self.0.join("prog.py"))
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

fn assert_matches_python(tag: &str, source: &str) {
    let program = Program::new(tag, source);
    let python = Command::new("python3")
        .arg(program.0.join("prog.py"))
        .output()
        .expect("failed to spawn python3");
    assert!(
        python.status.success(),
        "python3 failed: {}",
        String::from_utf8_lossy(&python.stderr)
    );
    let pyrs = program.run_pyrs();
    assert!(
        pyrs.status.success(),
        "PyRs failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&pyrs.stdout),
        String::from_utf8_lossy(&pyrs.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&pyrs.stdout),
        String::from_utf8_lossy(&python.stdout),
        "stdout differs for {tag}"
    );
}

fn assert_diagnostic(tag: &str, source: &str, expected: &[&str]) {
    let program = Program::new(tag, source);
    let output = program.run_pyrs();
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

#[test]
fn explicit_ne_is_independent_of_eq() {
    assert_matches_python(
        "independent",
        r#"
class Value:
    def __eq__(self, other: Value) -> bool:
        print("eq")
        return True
    def __ne__(self, other: Value) -> bool:
        print("ne")
        return True
a = Value()
b = Value()
print(a == b)
print(a != b)
print(a != a)
"#,
    );
}

#[test]
fn ne_without_eq_controls_inequality_only() {
    assert_matches_python(
        "ne_only",
        r#"
class Value:
    def __ne__(self, other: Value) -> bool:
        print("ne")
        return False
a = Value()
b = Value()
print(a == a)
print(a == b)
print(a != a)
print(a != b)
"#,
    );
}

#[test]
fn inherited_ne_is_used_even_when_child_defines_eq() {
    assert_matches_python(
        "inherited",
        r#"
class Base:
    def __ne__(self, other: Base) -> bool:
        print("base ne")
        return True
class Child(Base):
    def __eq__(self, other: Base) -> bool:
        print("child eq")
        return True
print(Child() != Child())
print(Child() == Child())
print(Base() != Child())
"#,
    );
}

#[test]
fn base_typed_ne_calls_dispatch_to_overrides() {
    assert_matches_python(
        "virtual",
        r#"
class Base:
    def __ne__(self, other: Base) -> bool:
        print("base ne")
        return False
class Child(Base):
    def __ne__(self, other: Base) -> bool:
        print("child ne")
        return True
def different(a: Base, b: Base) -> bool:
    return a != b
print(different(Base(), Base()))
print(different(Child(), Base()))
print(different(Child(), Child()))
"#,
    );
}

#[test]
fn reflected_ne_handles_primitives_and_missing_left_slot() {
    assert_matches_python(
        "reflected",
        r#"
class Number:
    def __init__(self, value: int):
        self.value = value
    def __ne__(self, other: int) -> bool:
        print("number ne")
        return self.value != other
print(1 != Number(1))
print(2 != Number(1))
print(Number(1) != 1)
print(Number(1) != 2)
class Plain:
    pass
class Receiver:
    def __ne__(self, other: Plain) -> bool:
        print("receiver ne")
        return False
print(Plain() != Receiver())
print(Receiver() != Plain())
"#,
    );
}

#[test]
fn reflected_ne_is_used_even_when_eq_disagrees() {
    assert_matches_python(
        "reflected_both",
        r#"
class Number:
    def __init__(self, value: int):
        self.value = value
    def __eq__(self, other: int) -> bool:
        print("number eq")
        return True
    def __ne__(self, other: int) -> bool:
        print("number ne")
        return self.value != other
print(1 != Number(1))
print(2 != Number(1))
print(1 == Number(1))
print(Number(1) != 1)
"#,
    );
}

#[test]
fn subclass_ne_is_used_even_when_eq_disagrees() {
    assert_matches_python(
        "subclass_both",
        r#"
class Base:
    def __eq__(self, other: Base) -> bool:
        print("base eq")
        return True
    def __ne__(self, other: Base) -> bool:
        print("base ne")
        return False
class Child(Base):
    def __eq__(self, other: Base) -> bool:
        print("child eq")
        return False
    def __ne__(self, other: Base) -> bool:
        print("child ne")
        return True
print(Base() != Child())
print(Child() != Base())
print(Base() != Base())
print(Base() == Child())
"#,
    );
}

#[test]
fn statically_proper_right_subclass_gets_ne_priority() {
    assert_matches_python(
        "subclass_priority",
        r#"
class Base:
    def __ne__(self, other: Base) -> bool:
        print("base ne")
        return False
class Child(Base):
    def __ne__(self, other: Base) -> bool:
        print("child ne")
        return True
print(Base() != Child())
print(Child() != Base())
print(Base() != Base())
"#,
    );
}

#[test]
fn missing_ne_inverts_direct_inherited_and_reflected_eq() {
    assert_matches_python(
        "eq_fallback",
        r#"
class Value:
    def __init__(self, value: int):
        self.value = value
    def __eq__(self, other: Value) -> bool:
        print("value eq")
        return self.value == other.value
class Child(Value):
    pass
print(Value(1) != Value(1))
print(Value(1) != Value(2))
print(Child(1) != Child(1))
class Number:
    def __eq__(self, other: int) -> bool:
        print("number eq")
        return other == 7
print(7 != Number())
print(8 != Number())
"#,
    );
}

#[test]
fn left_eq_fallback_precedes_unrelated_reflected_ne() {
    assert_matches_python(
        "left_eq_precedence",
        r#"
class Left:
    def __eq__(self, other: Right) -> bool:
        print("left eq")
        return True
class Right:
    def __ne__(self, other: Left) -> bool:
        print("right ne")
        return True
print(Left() != Right())
print(Right() != Left())
"#,
    );
}

#[test]
fn missing_comparison_slots_fall_back_to_identity() {
    assert_matches_python(
        "identity",
        r#"
class Base:
    pass
class Child(Base):
    pass
class Other:
    pass
a = Base()
b = Base()
child = Child()
alias: Base = child
print(a != a)
print(a != b)
print(a != child)
print(child != alias)
print(alias != child)
print(a != Other())
"#,
    );
}

#[test]
fn ne_rejects_a_missing_operand_parameter() {
    assert_diagnostic(
        "no_operand",
        r#"
class Value:
    def __ne__(self) -> bool:
        return True
print(Value() != Value())
"#,
        &["__ne__", "argument"],
    );
}

#[test]
fn ne_rejects_an_extra_required_parameter() {
    assert_diagnostic(
        "extra_parameter",
        r#"
class Value:
    def __ne__(self, other: Value, extra: int) -> bool:
        return True
print(Value() != Value())
"#,
        &["__ne__", "missing required argument", "extra"],
    );
}

#[test]
fn ne_rejects_an_incompatible_operand_type() {
    assert_diagnostic(
        "wrong_type",
        r#"
class Value:
    def __ne__(self, other: str) -> bool:
        return True
print(Value() != 1)
"#,
        &["expected str", "found int"],
    );
}
