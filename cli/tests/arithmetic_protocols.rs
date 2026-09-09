//! Arithmetic, bitwise and unary operators on user classes.
//!
//! Before this milestone `class V: def __add__(...)` was rejected outright —
//! there was no `__add__` string anywhere in the compiler — so no vector,
//! matrix, money, unit or interval type could be written at all. The
//! comparison dunders already dispatched correctly, and this extends that
//! path rather than adding a second one.
//!
//! Three things are easy to get subtly wrong, and each has tests here:
//!
//! - **Which slot is chosen.** CPython tries a proper subclass's reflected
//!   slot first, then the left operand's, then the right operand's reflected
//!   one. `2.0 * vec` has no float implementation to find, so it must reflect.
//! - **Evaluation order.** Both operands are spilled to temps *before*
//!   dispatch, because a reflected call makes the right operand the receiver
//!   and would otherwise evaluate it first. `cli/tests/protocol_order.rs`
//!   pins the same property for comparisons.
//! - **What `+=` means.** `__iadd__` mutates and returns self, so the object
//!   identity survives; falling back to `__add__` produces a new object. The
//!   difference is observable through an alias, and both must match.

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
            .join(format!("pyrs-arith-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    (dir, src)
}

/// Differential at every optimization level, plus GC stress: every one of
/// these operators allocates a result object, so a mis-rooted temporary is a
/// use-after-free rather than a wrong number.
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

/// The compile-time diagnostic for a program that must be rejected.
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

const VEC: &str = r#"
class Vec:
    def __init__(self, x: float, y: float) -> None:
        self.x: float = x
        self.y: float = y
    def __add__(self, o: 'Vec') -> 'Vec':
        return Vec(self.x + o.x, self.y + o.y)
    def __sub__(self, o: 'Vec') -> 'Vec':
        return Vec(self.x - o.x, self.y - o.y)
    def __mul__(self, k: float) -> 'Vec':
        return Vec(self.x * k, self.y * k)
    def __rmul__(self, k: float) -> 'Vec':
        return Vec(self.x * k, self.y * k)
    def __truediv__(self, k: float) -> 'Vec':
        return Vec(self.x / k, self.y / k)
    def __neg__(self) -> 'Vec':
        return Vec(-self.x, -self.y)
    def __matmul__(self, o: 'Vec') -> float:
        return self.x * o.x + self.y * o.y
    def __repr__(self) -> str:
        return "Vec(" + str(self.x) + ", " + str(self.y) + ")"
"#;

// -------------------------------------------------------------- the family

#[test]
fn every_binary_arithmetic_dunder_dispatches() {
    matches_python(
        "binary-family",
        r#"
class N:
    def __init__(self, v: int) -> None:
        self.v: int = v
    def __add__(self, o: 'N') -> 'N':
        return N(self.v + o.v)
    def __sub__(self, o: 'N') -> 'N':
        return N(self.v - o.v)
    def __mul__(self, o: 'N') -> 'N':
        return N(self.v * o.v)
    def __truediv__(self, o: 'N') -> float:
        return float(self.v) / float(o.v)
    def __floordiv__(self, o: 'N') -> 'N':
        return N(self.v // o.v)
    def __mod__(self, o: 'N') -> 'N':
        return N(self.v % o.v)
    def __pow__(self, o: 'N') -> 'N':
        return N(self.v ** o.v)
    def __repr__(self) -> str:
        return "N(" + str(self.v) + ")"

a = N(17)
b = N(5)
print(a + b, a - b, a * b)
print(a / b, a // b, a % b)
print(N(2) ** N(10))
"#,
    );
}

#[test]
fn every_bitwise_dunder_dispatches() {
    matches_python(
        "bitwise-family",
        r#"
class Bits:
    def __init__(self, v: int) -> None:
        self.v: int = v
    def __and__(self, o: 'Bits') -> 'Bits':
        return Bits(self.v & o.v)
    def __or__(self, o: 'Bits') -> 'Bits':
        return Bits(self.v | o.v)
    def __xor__(self, o: 'Bits') -> 'Bits':
        return Bits(self.v ^ o.v)
    def __lshift__(self, k: int) -> 'Bits':
        return Bits(self.v << k)
    def __rshift__(self, k: int) -> 'Bits':
        return Bits(self.v >> k)
    def __invert__(self) -> 'Bits':
        return Bits(~self.v)
    def __repr__(self) -> str:
        return "Bits(" + str(self.v) + ")"

a = Bits(0b1100)
b = Bits(0b1010)
print(a & b, a | b, a ^ b)
print(a << 2, a >> 1, ~a)
"#,
    );
}

#[test]
fn unary_dunders_dispatch() {
    matches_python(
        "unary-family",
        r#"
class Signed:
    def __init__(self, v: int) -> None:
        self.v: int = v
    def __neg__(self) -> 'Signed':
        return Signed(-self.v)
    def __pos__(self) -> 'Signed':
        return Signed(abs(self.v))
    def __invert__(self) -> 'Signed':
        return Signed(~self.v)
    def __repr__(self) -> str:
        return "Signed(" + str(self.v) + ")"

s = Signed(-7)
print(-s, +s, ~s)
print(-(-s), +(+s))
"#,
    );
}

/// `@` parsed as a decorator only until this milestone. It now dispatches to
/// `__matmul__`, and the two uses of the token must stay distinguishable.
#[test]
fn matmul_operator_and_decorators_coexist() {
    matches_python(
        "matmul",
        r#"
def twice(f):
    def inner(n: int) -> int:
        return f(f(n))
    return inner

@twice
def bump(n: int) -> int:
    return n + 1

class M:
    def __init__(self, rows: list[list[float]]) -> None:
        self.rows: list[list[float]] = rows
    def __matmul__(self, o: 'M') -> 'M':
        out: list[list[float]] = []
        for i in range(len(self.rows)):
            row: list[float] = []
            for j in range(len(o.rows[0])):
                total = 0.0
                for k in range(len(o.rows)):
                    total += self.rows[i][k] * o.rows[k][j]
                row.append(total)
            out.append(row)
        return M(out)
    def __repr__(self) -> str:
        return "M(" + str(self.rows) + ")"

print(bump(1))
a = M([[1.0, 2.0], [3.0, 4.0]])
i = M([[1.0, 0.0], [0.0, 1.0]])
print(a @ i)
print(a @ a)
print((a @ i).rows == a.rows)
"#,
    );
}

// ------------------------------------------------------------ slot choice

/// `v * 2.0` finds `__mul__`; `2.0 * v` has no float implementation to find
/// and must reflect onto the right operand.
#[test]
fn a_non_class_left_operand_reflects() {
    matches_python(
        "reflect-scalar",
        &format!("{VEC}\nv = Vec(1.0, 2.0)\nprint(v * 3.0)\nprint(3.0 * v)\nprint(v / 2.0)\n"),
    );
}

/// CPython gives a proper subclass on the right the first attempt, so a
/// subclass can override its base's arithmetic from either side.
#[test]
fn a_subclass_on_the_right_wins_the_first_attempt() {
    matches_python(
        "reflect-subclass",
        r#"
class Base:
    def __init__(self, v: int) -> None:
        self.v: int = v
    def __add__(self, o: 'Base') -> str:
        return "base.add"
    def __radd__(self, o: 'Base') -> str:
        return "base.radd"

class Sub(Base):
    def __radd__(self, o: 'Base') -> str:
        return "sub.radd"

print(Base(1) + Sub(2))
print(Sub(1) + Base(2))
print(Base(1) + Base(2))
"#,
    );
}

/// The dispatch goes through the same virtual switch a method call does, so
/// a base-typed binding reaches the override.
#[test]
fn arithmetic_dispatches_virtually() {
    matches_python(
        "virtual",
        r#"
class Shape:
    def __init__(self, n: int) -> None:
        self.n: int = n
    def __add__(self, o: 'Shape') -> int:
        return self.n + o.n

class Doubling(Shape):
    def __add__(self, o: 'Shape') -> int:
        return (self.n + o.n) * 2

def combine(a: Shape, b: Shape) -> int:
    return a + b

print(combine(Shape(2), Shape(3)))
print(combine(Doubling(2), Shape(3)))
shapes: list[Shape] = [Shape(1), Doubling(1)]
print([combine(s, Shape(10)) for s in shapes])
"#,
    );
}

/// The result keeps the dunder's own return type rather than being coerced
/// to bool the way a comparison is — that distinction is the whole reason
/// the comparison path could not simply be reused.
#[test]
fn the_result_keeps_the_dunder_return_type() {
    matches_python(
        "result-type",
        &format!(
            "{VEC}\n\
             a = Vec(3.0, 4.0)\n\
             b = Vec(1.0, 1.0)\n\
             total = a + b\n\
             print(total.x + total.y)\n\
             dot = a @ b\n\
             print(dot + 1.0)\n\
             print((a + b + b).x)\n"
        ),
    );
}

// ------------------------------------------------------- evaluation order

/// Both operands are spilled before dispatch, so source order holds even
/// when the *right* one becomes the receiver through a reflected call.
#[test]
fn operands_evaluate_left_to_right_even_when_reflected() {
    matches_python(
        "order",
        r#"
log: list[str] = []

def left(v: float) -> float:
    log.append("left")
    return v

def right(v: int) -> 'Scaled':
    log.append("right")
    return Scaled(v)

class Scaled:
    def __init__(self, v: int) -> None:
        self.v: int = v
    def __rmul__(self, k: float) -> float:
        return k * float(self.v)

print(left(2.0) * right(5))
print(log)
"#,
    );
}

/// An exception in the right operand happens before any dispatch, so the
/// dunder is never entered.
#[test]
fn a_failing_operand_prevents_the_dunder_call() {
    matches_python(
        "order-raise",
        r#"
class Loud:
    def __init__(self) -> None:
        self.hit: int = 0
    def __add__(self, o: int) -> int:
        print("dunder ran")
        return 1

def boom() -> int:
    raise ValueError("operand")

try:
    print(Loud() + boom())
except ValueError as e:
    print("caught", e)
"#,
    );
}

// ------------------------------------------------------------- in-place

/// `__iadd__` mutates and returns self, so an alias sees the change and the
/// identity survives. Without `__iadd__` the fallback to `__add__` produces
/// a new object and the alias does not move.
#[test]
fn in_place_mutates_while_the_fallback_rebinds() {
    matches_python(
        "inplace",
        r#"
class Acc:
    def __init__(self, v: int) -> None:
        self.v: int = v
    def __iadd__(self, o: int) -> 'Acc':
        self.v += o
        return self
    def __repr__(self) -> str:
        return "Acc(" + str(self.v) + ")"

class Pure:
    def __init__(self, v: int) -> None:
        self.v: int = v
    def __add__(self, o: int) -> 'Pure':
        return Pure(self.v + o)
    def __repr__(self) -> str:
        return "Pure(" + str(self.v) + ")"

a = Acc(1)
alias_a = a
a += 5
print(a, alias_a, a is alias_a)

p = Pure(1)
alias_p = p
p += 5
print(p, alias_p, p is alias_p)

total = Acc(0)
for i in range(5):
    total += i
print(total)
"#,
    );
}

#[test]
fn every_in_place_operator_reaches_its_dunder() {
    matches_python(
        "inplace-family",
        r#"
class Box:
    def __init__(self, v: int) -> None:
        self.v: int = v
    def __iadd__(self, o: int) -> 'Box':
        self.v += o
        return self
    def __isub__(self, o: int) -> 'Box':
        self.v -= o
        return self
    def __imul__(self, o: int) -> 'Box':
        self.v *= o
        return self
    def __ifloordiv__(self, o: int) -> 'Box':
        self.v //= o
        return self
    def __iand__(self, o: int) -> 'Box':
        self.v &= o
        return self
    def __ior__(self, o: int) -> 'Box':
        self.v |= o
        return self
    def __ixor__(self, o: int) -> 'Box':
        self.v ^= o
        return self
    def __ilshift__(self, o: int) -> 'Box':
        self.v <<= o
        return self
    def __irshift__(self, o: int) -> 'Box':
        self.v >>= o
        return self
    def __repr__(self) -> str:
        return "Box(" + str(self.v) + ")"

b = Box(10)
b += 5
b -= 3
b *= 4
b //= 2
b &= 255
b |= 1
b ^= 2
b <<= 2
b >>= 1
print(b)
"#,
    );
}

// ------------------------------------------------- comparisons unaffected

/// The comparison path was refactored to share operand spilling with the new
/// arithmetic one, so its own behaviour must not have moved — including the
/// `__ne__`-from-`__eq__` synthesis and the identity fallback.
#[test]
fn comparison_dunders_still_behave() {
    matches_python(
        "comparisons",
        r#"
class Money:
    def __init__(self, cents: int) -> None:
        self.cents: int = cents
    def __eq__(self, o: 'Money') -> bool:
        return self.cents == o.cents
    def __lt__(self, o: 'Money') -> bool:
        return self.cents < o.cents
    def __add__(self, o: 'Money') -> 'Money':
        return Money(self.cents + o.cents)
    def __repr__(self) -> str:
        return "Money(" + str(self.cents) + ")"

a = Money(100)
b = Money(250)
# `>` reflects onto `__lt__`; `<=` and `>=` are deliberately absent, because
# CPython does not derive them from `__lt__` either.
print(a == b, a != b, a < b, a > b)
print(a == Money(100))
prices: list[Money] = [Money(300), Money(100), Money(200)]
prices.sort()
# Printed element by element: printing the list itself does not reach
# `__repr__` yet, a pre-existing divergence recorded in the README.
for item in prices:
    print(item)
print(a + b, (a + b) == Money(350))

class Bare:
    def __init__(self) -> None:
        self.x: int = 1

p = Bare()
q = Bare()
print(p == p, p == q, p != q)
"#,
    );
}

// ----------------------------------------------------------- diagnostics

#[test]
fn a_missing_slot_names_both_operand_types() {
    let msg = rejects(
        "no-slot",
        "class P:\n    def __init__(self) -> None:\n        self.x: int = 1\nprint(P() + 1)\n",
    );
    assert!(
        msg.contains("type P"),
        "the class should be named, not an internal id: {msg}"
    );
    assert!(
        !msg.contains("class#"),
        "internal class id leaked into a user diagnostic: {msg}"
    );
}

#[test]
fn a_missing_unary_slot_names_the_dunder() {
    let msg = rejects(
        "no-neg",
        "class P:\n    def __init__(self) -> None:\n        self.x: int = 1\nprint(-P())\n",
    );
    assert!(
        msg.contains("bad operand type for unary -"),
        "should use CPython's wording: {msg}"
    );
    assert!(msg.contains("__neg__"), "should name the dunder: {msg}");
}

/// No builtin type implements `@`, so on two ints there is nothing to fall
/// back to — and the message must say that rather than the pre-0.135 "PyRs
/// has no array type", which stopped being the reason.
#[test]
fn matmul_on_builtins_explains_why_there_is_no_fallback() {
    let msg = rejects("matmul-ints", "print(2 @ 3)\n");
    assert!(
        msg.contains("'@' is not supported between int and int"),
        "should name both operands: {msg}"
    );
    assert!(msg.contains("__matmul__"), "should name the dunder: {msg}");
}

/// `+x` was discarded by the parser before this milestone, which would have
/// made `__pos__` a silent no-op. CPython rejects it on `str`, so PyRs must
/// too rather than quietly returning the operand.
#[test]
fn unary_plus_is_not_a_no_op() {
    let msg = rejects("pos-str", "print(+\"a\")\n");
    assert!(
        msg.contains("bad operand type for unary +"),
        "should match CPython's wording: {msg}"
    );
    matches_python("pos-numeric", "print(+5, +(-3), +2.5, +True)\n");
}
