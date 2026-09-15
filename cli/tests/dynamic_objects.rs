//! Dynamic values holding user objects, and the rest of the generic kernel.
//!
//! The plan's invariant is that no program is refused for being too dynamic:
//! an attribute, a method, an operator, a subscript or a call on a value whose
//! type is known only at run time dispatches on what the value holds. For a
//! user class that is a closed switch on the object's class, which whole-program
//! analysis can enumerate; for a builtin it is the pair-ABI kernel. Monkey-
//! patching rides on the same whole-program view: every `C.m = f` is known, so
//! only a patched method pays for an indirection.

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

fn temp_source(tag: &str, source: &str) -> TempDir {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-dynobj-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    fs::write(dir.0.join("prog.py"), source).unwrap();
    dir
}

/// Differential against CPython at every optimization level and under
/// collection pressure. The kernel allocates a box per operand and a box for
/// its result, so it is a real allocation site.
fn matches_python(tag: &str, source: &str) {
    let dir = temp_source(tag, source);
    let expected = Command::new("python3")
        .arg("prog.py")
        .current_dir(&dir.0)
        .output()
        .expect("failed to spawn CPython");
    assert!(
        expected.status.success(),
        "CPython failed for {tag}: {}",
        String::from_utf8_lossy(&expected.stderr)
    );
    for opt in ["0", "2", "3"] {
        for stress in ["0", "1"] {
            let actual = Command::new(PYRS)
                .args(["run", "-O", opt, "-i", "prog.py"])
                .env("PYRS_GC_STRESS", stress)
                .current_dir(&dir.0)
                .output()
                .expect("failed to spawn PyRs");
            assert_eq!(
                String::from_utf8_lossy(&actual.stdout),
                String::from_utf8_lossy(&expected.stdout),
                "{tag}: stdout differs at -O{opt} (GC stress {stress})\nstderr: {}",
                String::from_utf8_lossy(&actual.stderr)
            );
            assert_eq!(
                actual.status.code(),
                expected.status.code(),
                "{tag}: exit status differs at -O{opt}"
            );
        }
    }
}

/// Both engines must fail with the same final stderr line. Only the last line
/// is compared: PyRs prints no traceback, a recorded divergence.
fn fails_like_python(tag: &str, source: &str) {
    let dir = temp_source(tag, source);
    let expected = Command::new("python3")
        .arg("prog.py")
        .current_dir(&dir.0)
        .output()
        .expect("failed to spawn CPython");
    assert!(
        !expected.status.success(),
        "{tag}: CPython was expected to fail"
    );
    let want = last_line(&expected.stderr);
    for opt in ["0", "2", "3"] {
        let actual = Command::new(PYRS)
            .args(["run", "-O", opt, "-i", "prog.py"])
            .current_dir(&dir.0)
            .output()
            .expect("failed to spawn PyRs");
        assert!(!actual.status.success(), "{tag}: PyRs succeeded at -O{opt}");
        assert_eq!(
            last_line(&actual.stderr),
            want,
            "{tag}: error differs at -O{opt}"
        );
    }
}

fn last_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .unwrap_or_default()
        .to_string()
}

/// `v.x`, `v.x = y` and `v.x += y` on a dynamic value dispatch on the class it holds.
#[test]
fn attributes_on_a_dynamic_value() {
    matches_python(
        "attr",
        r#"
class P:
    def __init__(self, x: float) -> None:
        self.x = x
def f(o: object):
    return o.x
print(f(P(2.5)))
"#,
    );
    matches_python(
        "floatadd",
        r#"
class V:
    def __init__(self, x: float) -> None:
        self.x = x
    def __add__(self, o: object) -> "V":
        return V(self.x + o.x)
print((V(1.0) + V(2.0)).x)
"#,
    );
    matches_python(
        "anyfieldset",
        r#"
class C:
    def __init__(self) -> None:
        self.n = 0
def bump(c: object) -> None:
    c.n = c.n + 1
c = C()
bump(c)
print(c.n)
"#,
    );
    matches_python(
        "dyn_property",
        r#"
class T:
    def __init__(self) -> None:
        self._n = 4
    @property
    def n(self) -> int:
        return self._n * 2
def get(o: object):
    return o.n
print(get(T()))
"#,
    );
    matches_python(
        "anyreturn_field",
        r#"
class C:
    def __init__(self) -> None:
        self.v = 0
    def set(self, v: object) -> None:
        self.v = v
c = C()
c.set(3)
print(c.v + 1)
"#,
    );
}

/// A method call on a dynamic value reaches the user class that defines it, virtually.
#[test]
fn methods_on_a_dynamic_value() {
    matches_python(
        "meth",
        r#"
class P:
    def m(self, k: int) -> int:
        return k * 2
def f(o: object):
    return o.m(3)
print(f(P()))
"#,
    );
    fails_like_python(
        "dyn_virtual",
        r#"
class Animal:
    def speak(self) -> str:
        return "..."
class Dog(Animal):
    def speak(self) -> str:
        return "woof"
def talk(a: object) -> None:
    print(a.speak())
talk(Animal())
talk(Dog())
talk("str has no speak? no")
"#,
    );
    matches_python(
        "dyn_method_kw",
        r#"
class S:
    def scale(self, x: int, factor: int = 2) -> int:
        return x * factor
def call(o: object) -> None:
    print(o.scale(3, factor=5), o.scale(4))
call(S())
"#,
    );
    matches_python(
        "untyped_duck",
        r#"
class Duck:
    def quack(self):
        return "Quack"
class Robot:
    def quack(self):
        return "Beep"
def make_noise(x):
    return x.quack()
things = [Duck(), Robot()]
for t in things:
    print(make_noise(t))
"#,
    );
}

/// Operators on dynamic values call a user class's dunders, reflected ones included.
#[test]
fn dunders_on_a_dynamic_value() {
    matches_python(
        "dyn_eq_dunder",
        r#"
class P:
    def __init__(self, v: int) -> None:
        self.v = v
    def __eq__(self, o: object) -> bool:
        return isinstance(o, P) and self.v == o.v
def same(a: object, b: object) -> bool:
    return a == b
print(same(P(1), P(1)), same(P(1), P(2)), same(1, 1), same(P(1), 3))
"#,
    );
    matches_python(
        "dyn_add_dunder",
        r#"
class V:
    def __init__(self, x: float) -> None:
        self.x = x
    def __add__(self, o: "V") -> "V":
        return V(self.x + o.x)
    def __radd__(self, k: int) -> "V":
        return V(self.x + k)
    def __repr__(self) -> str:
        return "V(" + str(self.x) + ")"
def plus(a: object, b: object):
    return a + b
print(plus(V(1.0), V(2.0)), plus(1, 2), plus(3, V(1.5)), plus("a", "b"))
"#,
    );
    matches_python(
        "dyn_lt_sorted_objs",
        r#"
class K:
    def __init__(self, k: int) -> None:
        self.k = k
    def __lt__(self, o: "K") -> bool:
        return self.k < o.k
def less(a: object, b: object) -> bool:
    return a < b
print(less(K(1), K(2)), less(K(3), K(2)), less(1, 2))
"#,
    );
}

/// Subscripts, stores and slices dispatch on the container a dynamic value holds.
#[test]
fn items_and_slices_on_a_dynamic_value() {
    matches_python(
        "anyindex",
        r#"
def f(xs: object, i: object):
    return xs[i]
print(f([1,2,3], 1), f({"a": 1}, "a"))
"#,
    );
    matches_python(
        "dyn_errors",
        r#"
def get(v: object, k: object):
    try:
        return v[k]
    except TypeError as e:
        return "TypeError: " + str(e)
    except IndexError as e:
        return "IndexError: " + str(e)
    except KeyError as e:
        return "KeyError: " + str(e)
print(get([1, 2], "a"), get((1,), 5), get("ab", "x"), get(3, 0), get({"a": 1}, "b"))
"#,
    );
    matches_python(
        "dyn_setitem",
        r#"
def put(v: object, k: object, x: object) -> None:
    v[k] = x
xs: list[int] = [1, 2, 3]
put(xs, 1, 20)
d: dict[str, int] = {"a": 1}
put(d, "b", 2)
print(xs, d)
"#,
    );
    matches_python(
        "anyslice",
        r#"
def f(s: object):
    return s[1:]
print(f("abc"), f([1,2,3]))
"#,
    );
    matches_python(
        "anydictiter",
        r#"
def f(d: object) -> None:
    for k, v in d.items():
        print(k, v)
    print(list(d.values()), list(d.keys()))
f({"a": 1, "b": [2]})
f({})
f({1: "x"})
"#,
    );
}

/// `int`, `float`, unpacking, `sum`, `min`/`max`, `join` and `sorted(key=)` on dynamic values.
#[test]
fn conversions_and_builtins_on_a_dynamic_value() {
    matches_python(
        "anytoint",
        r#"
def f(n: object) -> int:
    return int(n) + 1
print(f("3"), f(2.5))
"#,
    );
    matches_python(
        "dyn_conv",
        r#"
def conv(v: object):
    return int(v), float(v)
print(conv("3"), conv(2.75), conv(True))
"#,
    );
    matches_python(
        "anyunpack",
        r#"
def f(p: object):
    a, b = p
    return a + b
print(f((1, 2)))
"#,
    );
    matches_python(
        "anyminmax",
        r#"
def f(a: object, b: object):
    return max(a, b), min(a, b)
print(f(3, 4))
"#,
    );
    matches_python(
        "anysum",
        r#"
def f(xs: object):
    return sum(xs)
print(f([1, 2, 3]))
"#,
    );
    matches_python(
        "dyn_sum_mixed",
        r#"
def total(*args):
    return sum(args)
print(total(1, 2, 3), total(1.5, 2))
"#,
    );
    matches_python(
        "dyn_minmax_iter",
        r#"
def mm(v: object):
    return min(v), max(v)
print(mm([3, 1, 2]), mm("bca"), mm((2.5, 1)))
"#,
    );
    matches_python(
        "anyjoin",
        r#"
def f(xs: object) -> str:
    return ",".join(xs)
print(f(["a", "b"]))
"#,
    );
    matches_python(
        "anysorted_key",
        r#"
def f(xs: object):
    return sorted(xs, key=len)
print(f(["ccc", "a", "bb"]))
"#,
    );
    matches_python(
        "anysorted_lambda",
        r#"
def f(xs: object):
    return sorted(xs, key=lambda s: -len(s)), sorted(xs, key=str, reverse=True)
print(f(["ccc", "a", "bb"]))
"#,
    );
}

/// A union operand that no typed rule accepts goes to the kernel, as CPython dispatches it.
#[test]
fn union_values_use_the_kernel() {
    matches_python(
        "unionop",
        r#"
def f(n: int) -> int | str:
    return n if n > 0 else "neg"
w: int | str = f(5)
print(w * 2, str(w), w + 1)
"#,
    );
    matches_python(
        "union_ops",
        r#"
def pick(n: int) -> int | str:
    if n > 0:
        return n
    return "neg"
a = pick(3)
b = pick(-1)
print(a + 1, b + "!", a * 2, b * 2, str(a), str(b), a == 3, b < "z", -a if isinstance(a, int) else 0)
"#,
    );
}

/// A function stored in a dynamic value is called with its own types.
#[test]
fn calling_a_dynamic_value() {
    matches_python(
        "anycall",
        r#"
def f(g: object, x: object):
    return g(x)
def sq(n: int) -> int:
    return n*n
print(f(sq, 3))
"#,
    );
    fails_like_python(
        "anycall_multi",
        r#"
def apply(g, x):
    return g(x)
def sq(n):
    return n * n
def up(s):
    return s.upper()
handlers = {"sq": sq, "up": up}
print(apply(sq, 3), apply(up, "a"), apply(lambda v: v + 1, 1))
def call_dyn(f: object, *a):
    return f(a[0])
print(call_dyn(sq, 4))
def notfn(f: object):
    return f(1)
notfn(3)
"#,
    );
}

/// `C.m = f` rebinds the method for every instance, subclasses that do not override it included.
#[test]
fn monkey_patching_a_method() {
    matches_python(
        "monkey",
        r#"
class C:
    def m(self) -> int:
        return 1
def other(self) -> int:
    return 2
c = C()
print(c.m())
C.m = other
print(c.m())
"#,
    );
    matches_python(
        "monkey_untyped",
        r#"
class Greeter:
    def __init__(self, name):
        self.name = name
    def greet(self):
        return "hello " + self.name
def shout(self):
    return "HELLO " + self.name.upper()
g = Greeter("ann")
print(g.greet())
Greeter.greet = shout
print(g.greet(), Greeter("bo").greet())
"#,
    );
    matches_python(
        "monkey_lambda_virtual",
        r#"
class Base:
    def v(self) -> int:
        return 1
class Sub(Base):
    pass
class Own(Base):
    def v(self) -> int:
        return 3
xs: list[Base] = [Base(), Sub(), Own()]
print([x.v() for x in xs])
Base.v = lambda self: 10
print([x.v() for x in xs])
"#,
    );
}

/// A dynamic operation CPython rejects fails with CPython's own message.
#[test]
fn dynamic_errors_are_cpythons() {
    fails_like_python(
        "dyn_attr_missing_int",
        r#"
def f(o: object):
    return o.zzz
print(f(3))
"#,
    );
    fails_like_python(
        "dyn_attr_missing_cls",
        r#"
class A:
    def __init__(self) -> None:
        self.a = 1
class B:
    pass
def f(o: object):
    return o.a
print(f(A()))
print(f(B()))
"#,
    );
    fails_like_python(
        "dyn_unpack_err",
        r#"
def f(p: object):
    a, b = p
    return a
print(f([1, 2]))
f([1, 2, 3])
"#,
    );
    fails_like_python(
        "dyn_join_err",
        r#"
def j(v: object) -> str:
    return "-".join(v)
print(j(["a", "b"]), j(("c",)))
print(j([1]))
"#,
    );
}
