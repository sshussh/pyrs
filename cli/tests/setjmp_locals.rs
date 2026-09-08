//! Only locals a `try` writes are kept in memory across a `longjmp`.
//!
//! C's setjmp rule (7.13.2.1) covers automatic objects **changed between the
//! `setjmp` and the `longjmp`**; one written only before the `setjmp` keeps
//! its value by the contract of `setjmp` itself. PyRs used to ignore the
//! distinction and mark every local in a function `volatile` as soon as the
//! function contained a `try` anywhere, which defeats `mem2reg` for the whole
//! body — so a numeric kernel with a validity check around it lost SSA
//! promotion for its accumulators.
//!
//! Narrowing that is a correctness-critical change in the opposite direction
//! from the usual: getting it wrong means a local silently reads back garbage
//! in a handler, only under optimization, only after a real raise. These tests
//! are the differential check for exactly that, at every optimization level.

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
            .join(format!("pyrs-setjmp-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    (dir, src)
}

/// Differential against CPython at -O0/-O2/-O3. The optimization level is the
/// whole point here: `-O0` never promotes anything, so a wrong volatile rule
/// only shows up once `mem2reg` runs.
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
            .args(["run", "--no-cache", "-O", opt, "-i"])
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

/// The case the rule exists for: a local assigned inside the `try` and read in
/// the handler must show the value it had when the raise happened, not the one
/// it had on entry and not garbage.
#[test]
fn a_local_written_inside_the_try_survives_the_longjmp() {
    matches_python(
        "written-inside",
        r#"
def boom(n: int) -> int:
    if n > 2:
        raise ValueError("too big")
    return n * 10

def probe(n: int) -> str:
    before = n + 100
    partial = -1
    seen = 0
    try:
        partial = 7
        seen = boom(n)
        partial = 99
    except ValueError as e:
        return "caught {} before={} partial={} seen={}".format(e, before, partial, seen)
    finally:
        before = before + 1
    return "ok before={} partial={} seen={}".format(before, partial, seen)

for n in range(5):
    print(probe(n))
"#,
    );
}

/// A local written only *before* the try is promotable, and must still read
/// back correctly in the handler — that is `setjmp`'s own guarantee rather
/// than something `volatile` provides.
#[test]
fn a_local_written_only_before_the_try_reads_back_in_the_handler() {
    matches_python(
        "written-before",
        r#"
def probe(n: int) -> str:
    a = n * 2
    b = a + 1
    c = b * b
    d = c - a
    try:
        if n % 2 == 0:
            raise KeyError("even")
        return "ok {} {} {} {}".format(a, b, c, d)
    except KeyError:
        return "caught {} {} {} {}".format(a, b, c, d)

for n in range(6):
    print(probe(n))
"#,
    );
}

/// The shape the narrowing is for: a hot loop with several live locals in a
/// function that also contains a `try`. Every one of these is written outside
/// the try, so none needs volatile — and the loop must still compute the same
/// thing once they are promoted.
#[test]
fn a_hot_loop_beside_a_try_computes_the_same_promoted() {
    matches_python(
        "hot-loop",
        r#"
def sim(n: int) -> float:
    x = 1.0
    y = 2.0
    z = 3.0
    vx = 0.1
    vy = 0.2
    vz = 0.3
    i = 0
    while i < n:
        vx = vx + x * 0.001
        vy = vy + y * 0.001
        vz = vz + z * 0.001
        x = x - vx * 0.001
        y = y - vy * 0.001
        z = z - vz * 0.001
        i += 1
    try:
        if x != x:
            raise ValueError("nan")
    except ValueError:
        return -1.0
    return x + y + z

print("{:.6f}".format(sim(200000)))
"#,
    );
}

/// A local written inside a *nested* try, and one written in a handler, both
/// read after a raise that crosses two frames.
#[test]
fn nested_tries_and_handler_writes() {
    matches_python(
        "nested",
        r#"
def nested(n: int) -> int:
    acc = 0
    tag = 0
    for i in range(n):
        try:
            try:
                if i % 3 == 0:
                    tag = i
                    raise KeyError("k")
                acc += i
            except KeyError:
                acc += 100
                if i % 2 == 0:
                    raise ValueError("re")
        except ValueError:
            acc += 1000 + tag
    return acc

print(nested(12))
"#,
    );
}

/// A loop counter incremented outside the try but read inside a handler. The
/// increment happens before the next iteration's `setjmp`, so it is safe
/// non-volatile — the rule must not be fooled by the lexical nesting.
#[test]
fn a_loop_counter_outside_the_try_is_read_correctly_inside_it() {
    matches_python(
        "counter",
        r#"
def scan(n: int) -> str:
    hits: list[int] = []
    last = -1
    i = 0
    while i < n:
        try:
            if i % 4 == 3:
                raise IndexError("bad")
            last = i
        except IndexError:
            hits.append(i * 10 + last)
        i += 1
    return "{} {}".format(hits, last)

print(scan(20))
"#,
    );
}

/// Values live across a raise must still be found by the conservative
/// collector. A promoted local held in a register at a collection point is the
/// case that would regress if promotion and root visibility ever disagreed.
#[test]
fn heap_values_live_across_a_raise_are_not_collected() {
    matches_python(
        "gc",
        r#"
def churn(n: int) -> str:
    keep: list[str] = []
    held = "sentinel-" + str(n)
    other = [1, 2, 3]
    try:
        for i in range(n):
            keep.append("item-{}".format(i))
        if n > 3:
            raise RuntimeError("stop")
    except RuntimeError:
        return "{} {} {} {}".format(held, len(keep), other, keep[0])
    return "{} {} {}".format(held, len(keep), other)

for n in range(6):
    print(churn(n))
"#,
    );
}

/// Generators keep their locals in a heap frame that already survives a
/// resume, so none of them takes the volatile path. A `try` inside a generator
/// must not change that.
#[test]
fn a_generator_with_a_try_keeps_frame_storage() {
    matches_python(
        "generator",
        r#"
def gen(n: int):
    total = 0
    for i in range(n):
        try:
            if i % 3 == 0:
                raise ValueError("skip")
            total += i
            yield total
        except ValueError:
            total += 100
            yield -total

print(list(gen(10)))
"#,
    );
}
