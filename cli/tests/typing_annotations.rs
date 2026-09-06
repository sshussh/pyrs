//! `typing` imports, and `Iterator[T]` for generator parameters.
//!
//! Two gaps, one blocking the other. `from typing import Optional` failed to
//! *load* — `No module named 'typing'` — so an ordinary typed Python file
//! could not be compiled at all, however simple. And a generator could be
//! created, iterated and passed to a builtin, but not to a user function:
//! there was no way to annotate a generator parameter, and nothing in a `for`
//! loop body determines whether its subject is a list, a str or a generator.
//!
//! `Iterator[T]` and `Generator[T, None, None]` are how Python annotates one,
//! so the same source still runs under CPython. `Iterable[T]` is deliberately
//! not accepted: it covers a list as well, and those are distinct types here.

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
            .join(format!("pyrs-typing-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    (dir, src)
}

/// Differential check at every optimization level, comparing stdout and exit
/// status against the CPython oracle.
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
            .args(["run", "-O", opt, "-i"])
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

/// Returns the diagnostic for a program that must be rejected.
fn rejects(tag: &str, source: &str) -> String {
    let (_dir, src) = write_prog(tag, source);
    let out = Command::new(PYRS)
        .args(["check", "-i"])
        .arg(&src)
        .output()
        .expect("failed to spawn PyRs");
    assert!(
        !out.status.success(),
        "{tag} was accepted, expected an error"
    );
    String::from_utf8_lossy(&out.stderr).to_string()
}

// ---------------------------------------------------------------------------
// typing imports
// ---------------------------------------------------------------------------

#[test]
fn typing_imports_load_and_bind_nothing_at_run_time() {
    matches_python(
        "imports",
        r#"
from typing import Optional
import typing
from collections.abc import Iterator

def f(x: Optional[int]) -> int:
    if x is None:
        return 0
    return x

print(f(None), f(3))
"#,
    );
}

#[test]
fn a_typing_import_does_not_run_a_module_body() {
    // The import must not emit a module-init call for a module that has no
    // body — that produced an undefined symbol at codegen.
    matches_python(
        "no-init",
        r#"
import typing
from typing import Optional
print("loaded")
"#,
    );
}

// ---------------------------------------------------------------------------
// Iterator[T]
// ---------------------------------------------------------------------------

#[test]
fn a_generator_can_be_passed_to_a_function() {
    matches_python(
        "pipeline",
        r#"
from typing import Iterator

def numbers(n: int) -> Iterator[int]:
    for i in range(n):
        yield i

def evens(src: Iterator[int]) -> Iterator[int]:
    for v in src:
        if v % 2 == 0:
            yield v

def squares(src: Iterator[int]) -> Iterator[int]:
    for v in src:
        yield v * v

print(list(squares(evens(numbers(10)))))
print(sum(squares(evens(numbers(10)))))
print(any(v > 50 for v in squares(evens(numbers(10)))))
"#,
    );
}

#[test]
fn iterator_annotations_work_for_non_int_yields() {
    matches_python(
        "str-yield",
        r#"
from typing import Iterator

def words() -> Iterator[str]:
    yield "a"
    yield "b"

def upper(src: Iterator[str]) -> Iterator[str]:
    for v in src:
        yield v.upper()

print(list(upper(words())))
print(",".join(upper(words())))
"#,
    );
}

#[test]
fn the_full_generator_spelling_works_too() {
    matches_python(
        "generator-form",
        r#"
def gen() -> Generator[int, None, None]:
    yield 1
    yield 2

def take(g: Iterator[int], n: int) -> list[int]:
    out: list[int] = []
    for v in g:
        if len(out) >= n:
            break
        out.append(v)
    return out

print(list(gen()), sum(gen()))
print(take(gen(), 1))
"#,
    );
}

#[test]
fn an_annotated_generator_still_works_where_it_did_before() {
    matches_python(
        "unchanged",
        r#"
from typing import Iterator

def g() -> Iterator[int]:
    yield 1
    yield 2

for v in g():
    print(v)
print(list(g()), sorted(g()), max(g()))
print([v * 2 for v in g()])
"#,
    );
}

// ---------------------------------------------------------------------------
// Rejections
// ---------------------------------------------------------------------------

#[test]
fn iterable_is_rejected_with_the_reason_and_the_alternatives() {
    // It covers both a list and a generator, which are distinct types here,
    // so there is nothing to resolve it to.
    let err = rejects(
        "iterable",
        "from typing import Iterable\ndef f(x: Iterable[int]) -> int:\n    return 0\n",
    );
    assert!(err.contains("covers both a list and a generator"), "{err}");
    assert!(err.contains("Iterator[T]"), "{err}");
}

#[test]
fn a_generator_with_send_or_return_types_is_rejected() {
    let err = rejects(
        "gen-send",
        "def g() -> Generator[int, int, None]:\n    yield 1\n",
    );
    assert!(err.contains("must be None in this subset"), "{err}");
}

#[test]
fn star_importing_typing_is_rejected() {
    let err = rejects("star", "from typing import *\n");
    assert!(err.contains("import the names you use"), "{err}");
}

// ---------------------------------------------------------------------------
// Nothing regressed
// ---------------------------------------------------------------------------

#[test]
fn unannotated_generators_and_bare_optional_still_work() {
    matches_python(
        "old-spellings",
        r#"
def g():
    yield 1
    yield 2

def h(x: Optional[int]) -> int:
    if x is None:
        return 0
    return x

print(list(g()), h(None), h(2))
"#,
    );
}

#[test]
fn real_modules_still_load_and_run_their_bodies() {
    matches_python(
        "sys-import",
        r#"
import sys
print(len(sys.argv) >= 1)
"#,
    );
}
