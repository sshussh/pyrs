//! A handler builds an exception object only when something reads it.
//!
//! `pyrs_exc_object()` allocates twice — the object, and a `PyrsStr` rebuilt
//! from the formatted message — and it used to run at *every* matched handler
//! entry, plus a second time when the handler bound a name. Its only consumers
//! are a bound name and a bare `raise`, so a plain `except E:` paid for two GC
//! allocations it discarded, 171k times in the exceptions benchmark.
//!
//! The emitter now writes the call speculatively and splices the line back out
//! if the handler body never reads it. That decision is made by actual use
//! rather than by scanning the body for a bare `raise`, so no statement kind
//! can be overlooked — but it means the shapes below are the contract: each
//! one is a different answer to "did anything read the object".

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
            .join(format!("pyrs-handler-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    (dir, src)
}

fn matches_python(tag: &str, source: &str) {
    let (_dir, src) = write_prog(tag, source);
    let expected = Command::new("python3").arg(&src).output().unwrap();
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
            .unwrap();
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

/// Count `pyrs_exc_object` calls in the emitted IR for a program.
fn exc_object_calls(tag: &str, source: &str) -> usize {
    let (_dir, src) = write_prog(tag, source);
    let out = src.with_extension("bin");
    let status = Command::new(PYRS)
        .args(["compile", "--no-cache", "--emit-llvm", "-O", "0", "-i"])
        .arg(&src)
        .arg("-o")
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "compile failed for {tag}: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    fs::read_to_string(out.with_extension("ll"))
        .unwrap()
        .matches("call ptr @pyrs_exc_object()")
        .count()
}

/// The benchmark's shape, and the one that motivated the change.
#[test]
fn a_handler_that_neither_binds_nor_reraises_builds_nothing() {
    let source = "\
def boom(n: int) -> int:
    if n % 2 == 0:
        raise ValueError(\"even\")
    return n

hits = 0
for i in range(6):
    try:
        hits += boom(i)
    except ValueError:
        hits -= 1
print(hits)
";
    matches_python("plain", source);
    assert_eq!(
        exc_object_calls("plain-ir", source),
        0,
        "a handler that reads nothing still built an exception object"
    );
}

/// Binding used to call `pyrs_exc_object` twice — once for the re-raise slot
/// and once for the name — producing two objects with identical contents.
#[test]
fn binding_a_name_builds_exactly_one_object() {
    let source = "\
try:
    raise KeyError(\"k\")
except KeyError as e:
    print(e, repr(e))
";
    matches_python("bind", source);
    assert_eq!(
        exc_object_calls("bind-ir", source),
        1,
        "binding should reuse one object, not allocate a second"
    );
}

/// A bare `raise` is the other consumer: the object must survive to be
/// re-raised, so it cannot be elided even though no name is bound.
#[test]
fn a_bare_raise_keeps_the_object() {
    let source = "\
def outer(n: int) -> str:
    try:
        try:
            if n > 0:
                raise ValueError(\"v{}\".format(n))
            return \"ok\"
        except ValueError:
            raise
    except ValueError as e:
        return \"reraised {}\".format(e)

for i in [0, 1, 2]:
    print(outer(i))
";
    matches_python("reraise", source);
    // The inner handler re-raises and the outer binds: one each.
    assert_eq!(exc_object_calls("reraise-ir", source), 2);
}

/// A bare `raise` nested inside control flow within the handler still counts,
/// which is the case an AST scan would be most likely to miss.
#[test]
fn a_bare_raise_under_nested_control_flow_still_counts() {
    matches_python(
        "nested-raise",
        "\
def guard(n: int) -> str:
    try:
        try:
            raise ValueError(\"v{}\".format(n))
        except ValueError:
            for k in range(3):
                if k == n:
                    raise
            return \"swallowed\"
    except ValueError as e:
        return \"out {}\".format(e)

for i in [0, 1, 5]:
    print(guard(i))
",
    );
}

/// A handler that binds a name it never uses still binds it: the name is in
/// scope for the whole handler and CPython would have bound it too.
#[test]
fn an_unused_binding_is_still_bound() {
    matches_python(
        "unused-bind",
        "\
seen = 0
for i in range(4):
    try:
        raise IndexError(\"i{}\".format(i))
    except IndexError as e:
        seen += 1
print(seen)
",
    );
}

/// Generators keep handler locals in a heap frame rather than an alloca, so
/// the binding path differs; it must reuse the same single object too.
#[test]
fn a_generator_handler_binds_from_one_object() {
    matches_python(
        "generator",
        "\
from typing import Iterator


def gen(n: int) -> Iterator[str]:
    for i in range(n):
        try:
            if i % 2 == 0:
                raise ValueError(\"even {}\".format(i))
            yield \"odd {}\".format(i)
        except ValueError as e:
            yield \"caught {}\".format(e)

print(list(gen(6)))
",
    );
}

/// Multi-type and chained handlers each get their own decision, so a program
/// mixing all three shapes must keep them straight.
#[test]
fn mixed_handler_shapes_in_one_try() {
    let source = "\
def boom(n: int) -> int:
    if n % 3 == 0:
        raise ValueError(\"v\")
    if n % 3 == 1:
        raise KeyError(\"k\")
    raise IndexError(\"i\")

for i in range(6):
    try:
        boom(i)
    except ValueError:
        print(i, \"value\")
    except (KeyError, IndexError) as e:
        print(i, \"other\", e)
";
    matches_python("mixed", source);
    // Only the binding handler builds one.
    assert_eq!(exc_object_calls("mixed-ir", source), 1);
}
