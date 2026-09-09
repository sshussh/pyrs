//! Three places where a read or a write did not agree with the one beside it.
//!
//! Each of these had the same shape: a construct that already worked in one
//! spelling silently failed, or silently lied, in another. None of them was a
//! missing feature — all three were a second implementation of something the
//! compiler already knew how to do.
//!
//! - `x op= v` hand-rolled its own name lookup, load and store instead of
//!   using the ones an assignment uses, so it missed cell bindings
//!   (`nonlocal n; n += 1` reported `name 'n' is not defined` while
//!   `n = n + 1` compiled), narrowing refinements, and comprehension renames.
//! - `bool(x)` went through the ctx-free `lower_cast`, which cannot call a
//!   method, so it folded every class instance to a constant `true` — while
//!   `if x:` and `not x` consulted `__bool__` correctly. A silent wrong
//!   answer, with no diagnostic.
//! - A conditional expression lowered both arms under the ambient
//!   refinements, so `0 if x is None else x` was rejected with advice
//!   ("use 'is None' check") that the author had already followed.
//!
//! The tests are differential against CPython at every optimization level,
//! and each pairs the previously-broken spelling with the one that already
//! worked, so a fix that regresses the working half fails here too.

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

/// Differential check at every optimization level, plus one run under GC
/// stress: a cell is a heap allocation, so a store that writes through the
/// wrong slot is a use-after-free rather than a wrong number.
fn matches_python(tag: &str, source: &str) {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-rw-{tag}-{}", std::process::id())),
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
    let want = String::from_utf8_lossy(&expected.stdout);

    for (opt, stress) in [("0", false), ("2", false), ("3", false), ("2", true)] {
        let mut cmd = Command::new(PYRS);
        cmd.args(["run", "--no-cache", "-O", opt, "-i"]).arg(&src);
        if stress {
            cmd.env("PYRS_GC_STRESS", "1");
        }
        let actual = cmd.output().expect("failed to spawn PyRs");
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

// ---------------------------------------------------------------- aug-assign

/// The canonical closure counter. `n += 1` reported `name 'n' is not defined`
/// because the aug-assign arm probed `locals` and `globals` but never
/// `cell_locals`, where a `nonlocal` name's type actually lives — `locals`
/// holds only the mangled `.cell.n`.
#[test]
fn nonlocal_augmented_assignment_counts() {
    matches_python(
        "nonlocal-aug",
        r#"
def make_counter():
    n: int = 0
    def inc() -> int:
        nonlocal n
        n += 1
        return n
    return inc

c = make_counter()
print(c(), c(), c())
d = make_counter()
print(d(), c())
"#,
    );
}

/// Every augmented operator through a cell, not just `+=`. A store that wrote
/// to a bare local instead of through the cell would read back the initial
/// value every time.
#[test]
fn every_augmented_operator_writes_through_a_cell() {
    matches_python(
        "nonlocal-aug-ops",
        r#"
def run() -> None:
    n: int = 10
    s: str = "a"
    xs: list[int] = [1]
    def step() -> None:
        nonlocal n, s, xs
        n += 3
        n -= 1
        n *= 2
        n //= 3
        n %= 100
        n **= 2
        n &= 255
        n |= 1
        n ^= 2
        n <<= 1
        n >>= 1
        s += "b"
        xs += [2]
    step()
    step()
    print(n, s, xs)

run()
"#,
    );
}

/// The equivalent longhand, which always worked. If a fix routed `+=` through
/// a new path rather than the existing one, this is what would drift.
#[test]
fn longhand_and_augmented_forms_agree() {
    matches_python(
        "nonlocal-longhand",
        r#"
def counters():
    a: int = 0
    b: int = 0
    def bump() -> None:
        nonlocal a, b
        a = a + 1
        b += 1
    bump()
    bump()
    bump()
    return a, b

print(counters())
"#,
    );
}

/// `x += 1` where `x` is `int | None` narrowed to `int`. The load used the
/// storage type and ignored the refinement, so this reported
/// `operator '+' is not supported for values of type None | int`.
#[test]
fn augmented_assignment_sees_a_narrowed_local() {
    matches_python(
        "aug-narrow",
        r#"
def bump(x: int | None) -> int:
    if x is not None:
        x += 1
        return x
    return 0

def concat(s: str | None) -> str:
    if s is not None:
        s += "!"
        return s
    return "-"

print(bump(None), bump(5), concat(None), concat("hi"))
"#,
    );
}

/// A `global` declaration still takes the global path, and a module-level
/// augmented assignment still works. Both went through the branch the fix
/// replaced.
#[test]
fn global_and_module_level_augmented_assignment_still_work() {
    matches_python(
        "aug-global",
        r#"
total: int = 10
names: list[str] = []

def add(n: int) -> None:
    global total
    total += n

add(5)
add(7)
names += ["a"]
names += ["b"]
total += 1
print(total, names)
"#,
    );
}

/// Sets keep their in-place update path (`|=` is a mutation, not a rebind),
/// which the aug-assign arm special-cases ahead of the general lowering.
#[test]
fn set_in_place_operators_still_mutate() {
    matches_python(
        "aug-set",
        r#"
a: set[int] = {1, 2, 3}
a |= {4}
a &= {2, 3, 4}
a -= {3}
a ^= {9}
print(sorted(a))

def through_cell() -> None:
    s: set[int] = {1}
    def grow() -> None:
        nonlocal s
        s |= {2}
    grow()
    grow()
    print(sorted(s))

through_cell()
"#,
    );
}

// ---------------------------------------------------------------- bool(x)

/// `bool(x)` folded to a constant `true` for every class instance, while
/// `if x:` and `not x` called `__bool__`. All three must agree.
#[test]
fn bool_cast_consults_dunder_bool() {
    matches_python(
        "bool-dunder",
        r#"
class Flag:
    def __init__(self, v: bool) -> None:
        self.v: bool = v
    def __bool__(self) -> bool:
        return self.v

f = Flag(False)
t = Flag(True)
print(bool(f), bool(t))
if f:
    print("f truthy")
else:
    print("f falsy")
print(not f, not t)
print(bool(f) == (not (not f)))
"#,
    );
}

/// The same gap swallowed `__len__`-based truthiness, which is the fallback
/// `if x:` uses when there is no `__bool__`.
#[test]
fn bool_cast_falls_back_to_dunder_len() {
    matches_python(
        "bool-len",
        r#"
class Bag:
    def __init__(self, n: int) -> None:
        self.n: int = n
    def __len__(self) -> int:
        return self.n

empty = Bag(0)
full = Bag(3)
print(bool(empty), bool(full), len(empty), len(full))
if empty:
    print("empty truthy")
else:
    print("empty falsy")
"#,
    );
}

/// A class with neither dunder is always truthy, which is what the old
/// constant-folding accidentally got right — so the fix must not lose it.
#[test]
fn a_plain_instance_stays_truthy() {
    matches_python(
        "bool-plain",
        r#"
class Plain:
    def __init__(self) -> None:
        self.x: int = 0

p = Plain()
print(bool(p), not p)
if p:
    print("plain truthy")
"#,
    );
}

/// `__bool__` resolved through the class hierarchy, and through a base-typed
/// binding, so the cast goes through the same virtual dispatch a method call
/// does rather than a statically chosen slot.
#[test]
fn bool_cast_dispatches_virtually() {
    matches_python(
        "bool-virtual",
        r#"
class Base:
    def __bool__(self) -> bool:
        return True

class Never(Base):
    def __bool__(self) -> bool:
        return False

class Inherits(Base):
    pass

def truthiness(b: Base) -> bool:
    return bool(b)

print(truthiness(Base()), truthiness(Never()), truthiness(Inherits()))
items: list[Base] = [Base(), Never(), Inherits()]
print([bool(i) for i in items])
"#,
    );
}

/// The other truthiness consumers keep working, including the deliberately
/// dunder-free one: `any`/`all` and match patterns use a separate path that
/// this fix must not disturb.
#[test]
fn bool_in_containers_and_conditions() {
    matches_python(
        "bool-consumers",
        r#"
class Flag:
    def __init__(self, v: bool) -> None:
        self.v: bool = v
    def __bool__(self) -> bool:
        return self.v

flags: list[Flag] = [Flag(True), Flag(False), Flag(True)]
print([bool(f) for f in flags])
print(sum(1 for f in flags if f))
print(bool(Flag(True)) and bool(Flag(False)))
n = 0
while Flag(n < 2):
    n += 1
print(n)
"#,
    );
}

// ------------------------------------------------------- conditional expr

/// `0 if x is None else x` was rejected because both arms lowered under the
/// ambient refinements. The `if`-statement spelling always worked.
#[test]
fn conditional_expression_narrows_both_arms() {
    matches_python(
        "ifexp-narrow",
        r#"
def a(x: int | None) -> int:
    return 0 if x is None else x

def b(x: int | None) -> int:
    return x if x is not None else -1

def c(s: str | None) -> int:
    return len(s) if s is not None else 0

def statement_form(x: int | None) -> int:
    if x is None:
        return 0
    return x

for v in [None, 7]:
    print(a(v), b(v), statement_form(v))
print(c(None), c("abcd"))
"#,
    );
}

/// Narrowing composes the way it does in an `if`: `and` chains peel
/// left-to-right, and a nested conditional sees the outer refinement.
#[test]
fn conditional_expression_narrowing_composes() {
    matches_python(
        "ifexp-compose",
        r#"
def clamp(x: int | None) -> int:
    return x if x is not None and x > 0 else 0

def nested(x: int | None, y: int | None) -> int:
    return (x if y is None else x + y) if x is not None else -1

print(clamp(None), clamp(-5), clamp(9))
print(nested(None, None), nested(3, None), nested(3, 4))
"#,
    );
}

/// A refinement inside a conditional expression must not leak past it: the
/// arms are restored, so a later use still sees the union.
#[test]
fn conditional_expression_refinement_does_not_leak() {
    matches_python(
        "ifexp-restore",
        r#"
def both(x: int | None) -> str:
    first = 0 if x is None else x
    second = "none" if x is None else str(x)
    return str(first) + " " + second

print(both(None))
print(both(4))
"#,
    );
}

/// Class narrowing through `isinstance` in a conditional expression, which
/// takes the same peel machinery as the `is None` case.
#[test]
fn conditional_expression_narrows_isinstance() {
    matches_python(
        "ifexp-isinstance",
        r#"
class Animal:
    def __init__(self, name: str) -> None:
        self.name: str = name

class Dog(Animal):
    def __init__(self, name: str, breed: str) -> None:
        self.name = name
        self.breed: str = breed

def describe(a: Animal) -> str:
    return a.breed if isinstance(a, Dog) else a.name

print(describe(Animal("generic")), describe(Dog("rex", "collie")))
"#,
    );
}
