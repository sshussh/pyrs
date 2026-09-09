//! `object` as an annotation.
//!
//! `object` is Python's top type and a common way to annotate "any value" —
//! `def loads(s: str) -> object`. It was rejected with
//! `unknown type 'object' (not a builtin or defined class)`, which turned
//! away working Python for no representational reason: this subset's `Any`
//! is exactly what `object` means here.
//!
//! The mapping is closer than the name suggests. `Any` in a type checker
//! accepts any operation; `object` requires narrowing before use. PyRs's
//! `Any` requires narrowing before use, so it implements `object`'s
//! semantics rather than `Any`'s.

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

fn matches_python(tag: &str, source: &str) {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-obj-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();

    let expected = Command::new("python3").arg(&src).output().unwrap();
    assert!(
        expected.status.success(),
        "CPython failed for {tag}: {}",
        String::from_utf8_lossy(&expected.stderr)
    );
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
            String::from_utf8_lossy(&expected.stdout),
            "{tag} differs at -O{opt} ({label})"
        );
    }
}

/// Every position an annotation can appear in, spelled `object`.
#[test]
fn object_annotates_like_any() {
    matches_python(
        "object-positions",
        r#"
def identity(v: object) -> object:
    return v

def describe(v: object) -> str:
    if isinstance(v, int):
        return "int:" + str(v + 1)
    if isinstance(v, str):
        return "str:" + v.upper()
    return "other"

class Box:
    def __init__(self, v: object) -> None:
        self.value: object = v

    def get(self) -> object:
        return self.value

items: list[object] = [1, "a", 2.5, True, None]
table: dict[str, object] = {"n": 1, "s": "x"}

for it in items:
    print(describe(it))
print(describe(identity("hi")))
print(Box(7).get(), Box("s").get())
for k in sorted(table.keys()):
    print(k, table[k])
"#,
    );
}

/// `object` and `Any` are the same type, so they interoperate freely and a
/// narrowed value is usable directly.
#[test]
fn object_and_any_are_interchangeable() {
    matches_python(
        "object-any",
        r#"
from typing import Any

def takes_any(v: Any) -> str:
    if isinstance(v, int):
        return "i" + str(v)
    return "?"

def takes_object(v: object) -> str:
    return takes_any(v)

def gives_object() -> object:
    return 41

a: Any = 5
o: object = a
back: Any = o
print(takes_object(1), takes_any(o), takes_object(back))

got = gives_object()
if isinstance(got, int):
    print(got + 1)
"#,
    );
}

/// A recursive-descent parser returning `object`, which is the shape that
/// surfaced this: classes, a user exception, unions, containers of `object`,
/// and narrowing on the way out.
#[test]
fn a_parser_returning_object_round_trips() {
    matches_python(
        "object-parser",
        r#"
class ParseError(Exception):
    pass

class Reader:
    def __init__(self, source: str) -> None:
        self.source: str = source
        self.position: int = 0
        self.length: int = len(source)

    def value(self) -> object:
        if self.position >= self.length:
            raise ParseError("end of input")
        c = self.source[self.position]
        if c == "t":
            self.position += 4
            return True
        if c == "n":
            self.position += 4
            return None
        if c == '"':
            self.position += 1
            start = self.position
            while self.source[self.position] != '"':
                self.position += 1
            out = self.source[start : self.position]
            self.position += 1
            return out
        return self.number()

    def number(self) -> int | float:
        start = self.position
        while self.position < self.length and self.source[self.position].isdigit():
            self.position += 1
        return int(self.source[start : self.position])

def read(src: str) -> object:
    return Reader(src).value()

for text in ['"hello"', "true", "null", "42"]:
    v = read(text)
    print(v)

try:
    read("")
except ParseError as e:
    print("caught", e)
"#,
    );
}
