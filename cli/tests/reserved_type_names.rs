//! The builtin type names are usable as ordinary identifiers.
//!
//! `int`, `float`, `bool`, `str`, `file`, `list`, `tuple`, `dict` and `set`
//! are reserved words in this lexer so that annotations and casts parse
//! without lookahead. They are **not** keywords in Python: eight are
//! shadowable builtins, and `file` is not even that — it was a Python 2
//! builtin, removed in Python 3, and the `file` *annotation* is this
//! compiler's own invention.
//!
//! The reservation leaked into every position that names something, so
//! `with open(path) as file:` — ordinary Python, and the obvious name for a
//! file handle — was a parse error. Assignment and `for` targets happened to
//! work because they do not go through `expect_ident`.
//!
//! Every caller of `expect_ident` introduces or refers to a name, never a
//! type, so accepting the spelling there is unambiguous.

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
            .join(format!("pyrs-resv-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();

    let expected = Command::new("python3")
        .arg(&src)
        .current_dir(&dir.0)
        .output()
        .unwrap();
    assert!(
        expected.status.success(),
        "CPython failed for {tag}: {}",
        String::from_utf8_lossy(&expected.stderr)
    );
    for opt in ["0", "2", "3"] {
        let actual = Command::new(PYRS)
            .args(["run", "--no-cache", "-O", opt, "-i"])
            .arg(&src)
            .current_dir(&dir.0)
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
            "{tag} differs at -O{opt}"
        );
    }
}

/// The case that surfaced this: the obvious name for a file handle.
#[test]
fn a_context_manager_target_may_be_named_file() {
    matches_python(
        "with-as-file",
        r#"
with open("prog.py", "r") as file:
    text = file.read()
print(len(text) > 0)

def load(path: str) -> str:
    with open(path, "r") as file:
        return file.read()

print(len(load("prog.py")) > 0)
"#,
    );
}

/// Every position that names something rather than declaring a type.
#[test]
fn type_names_bind_in_every_naming_position() {
    matches_python(
        "positions",
        r#"
import math as file
from math import sqrt as dict

class list:
    def __init__(self) -> None:
        self.set: int = 1

def tuple(str: int, *, bool: int = 2) -> int:
    return str + bool

g = lambda int: int + 1

float: int = 0

def bump() -> None:
    global float
    float = 7

try:
    raise ValueError("v")
except ValueError as set:
    caught = str(set)

bump()
print(file.floor(2.5), dict(9.0))
print(list().set)
print(tuple(1), tuple(1, bool=10))
print(g(1), float, caught)
"#,
    );
}

/// `file(...)` is not a conversion in any Python, so a call can only be to a
/// name the user bound — treating it as a cast refused a name that is theirs.
#[test]
fn file_in_call_position_is_an_ordinary_call() {
    matches_python(
        "file-call",
        r#"
from math import sqrt as file
print(file(9.0))

def helper() -> int:
    return 7
"#,
    );
    matches_python(
        "file-def",
        r#"
def file(n: int) -> int:
    return n * 2
print(file(21))
"#,
    );
}

/// The reservation exists for annotations and casts, and both still parse.
#[test]
fn annotations_and_casts_are_unaffected() {
    matches_python(
        "annotations",
        r#"
x: int = 1
y: float = 2.5
b: bool = True
s: str = "a"
xs: list[int] = [1, 2]
t: tuple[int, str] = (1, "a")
d: dict[str, int] = {"k": 2}
st: set[int] = {1}

def takes(h: file) -> int:
    return len(h.read())

print(int("4"), float("1.5"), str(3), bool(0))
print(list("ab"), len(xs), len(t), len(d), len(st))
print(x, y, b, s)
with open("prog.py") as h:
    print(takes(h) > 0)
"#,
    );
}
