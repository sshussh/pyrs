//! What a command-line program needs, and what a parser for one needs.
//!
//! Written by building an argument parser and finding where it stopped
//! compiling, the same way `stdlib/json.py` drove the dynamic-container work.
//! Four things blocked the canonical shape of a CLI:
//!
//! * a module-level dispatch table was invisible inside functions, because
//!   global seeding could not give a name a *closure* type;
//! * `Callable[[A], R]` did not parse, so a handler could not be stored in a
//!   typed dict, a field, or a parameter;
//! * `self.handler(x)` read as a method call and never looked for a field;
//! * `str()` of a dynamic value was refused, which is what printing a parsed
//!   option of mixed type needs.
//!
//! Plus the OS surface a program is not usable without: the standard streams
//! as file objects, the environment, and the path predicates.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

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
            .join(format!("pyrs-cli-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    (dir, src)
}

/// Differential at every optimization level, plus GC stress.
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

fn rejects(tag: &str, source: &str) -> String {
    let (_dir, src) = write_prog(tag, source);
    let out = Command::new(PYRS)
        .args(["check", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(!out.status.success(), "{tag} was accepted:\n{source}");
    String::from_utf8_lossy(&out.stderr).to_string()
}

// ------------------------------------------------------- dispatch tables

/// The shape every subcommand program has: handlers defined at module level,
/// collected into a table beside them, and looked up inside `main`. The table
/// was invisible from inside a function, because seeding globals ran before
/// any expression was typed and could not tell that `cmd_build` names a
/// function.
#[test]
fn a_module_level_dispatch_table_is_visible_inside_functions() {
    matches_python(
        "table",
        r#"
def cmd_build(args: list[str]) -> int:
    return len(args) + 10


def cmd_test(args: list[str]) -> int:
    return len(args) + 20


TABLE = {"build": cmd_build, "test": cmd_test}
ALIAS = cmd_build
LIST = [cmd_build, cmd_test]


def run(name: str) -> int:
    handler = TABLE[name]
    return handler(["x"])


def indirect() -> int:
    return ALIAS([]) + LIST[1](["a", "b"])


for key in sorted(TABLE.keys()):
    print(key, run(key))
print(indirect())
"#,
    );
}

/// `Callable[[A, B], R]` as an annotation: on a dict value, a parameter, a
/// local, and a field. It resolves to a capture-free closure, which is what a
/// module-level function becomes in value position.
#[test]
fn callable_annotates_a_function_value() {
    matches_python(
        "callable",
        r#"
from typing import Callable


def inc(x: int) -> int:
    return x + 1


def shout(s: str) -> None:
    print(s.upper())


def twice(f: Callable[[int], int], v: int) -> int:
    return f(f(v))


HANDLERS: dict[str, Callable[[int], int]] = {"inc": inc}
SHOUT: Callable[[str], None] = shout


class Command:
    def __init__(self, name: str, run: Callable[[str], None]):
        self.name: str = name
        self.run: Callable[[str], None] = run


print(twice(inc, 1))
print(HANDLERS["inc"](41))
SHOUT("hi")
Command("greet", shout).run("there")
"#,
    );
}

/// `Callable[..., R]` cannot be represented: a call site needs the parameter
/// types to pass them. Say so, rather than failing on the bracket.
#[test]
fn callable_with_an_ellipsis_is_named() {
    let msg = rejects(
        "callable_ellipsis",
        "from typing import Callable\nf: Callable[..., int] = None\n",
    );
    assert!(msg.contains("Callable[..., R]"), "{msg}");
    assert!(msg.contains("Callable[[int, str], R]"), "{msg}");
}

/// A field holding a function is called like a method in Python, because both
/// are just attributes. Here they live in different namespaces, so the field
/// has to be tried before reporting a missing method -- and a field that is
/// not callable still gets an error that says which it is.
#[test]
fn a_non_callable_field_says_what_it_is() {
    let msg = rejects(
        "field_not_callable",
        "class H:\n    def __init__(self):\n        self.x: int = 1\n\n\nprint(H().x())\n",
    );
    assert!(msg.contains("has no method 'x'"), "{msg}");
    assert!(msg.contains("field 'x' of type int"), "{msg}");
}

// ------------------------------------------------------------- printing

/// Printing a parsed value whose type varies is the reason `str` of a dynamic
/// value has to work: an option table holds strings, flags and `None`
/// together. `repr` differs only in quoting a top-level string.
#[test]
fn str_and_repr_render_a_dynamic_value() {
    matches_python(
        "str_any",
        r#"
values: list[object] = ["hi", 5, None, True, 2.5, ["x", 1], {"a": 1}, (1, "b")]
for v in values:
    print(str(v), "|", repr(v))
print(f"[{values[0]}] [{values[2]}] [{values[5]}]")

parsed: dict[str, object] = {"out": "a.tar", "verbose": True, "level": None}
for key in sorted(parsed.keys()):
    print(key + "=" + str(parsed[key]))
"#,
    );
}

// -------------------------------------------------------------- streams

/// The three standard streams are file objects, so the whole file surface
/// applies to them: read, readline, readlines, write, and iteration.
#[test]
fn the_standard_streams_are_files() {
    let (_dir, src) = write_prog(
        "streams",
        "import sys\n\
         count = 0\n\
         for line in sys.stdin:\n\
         \x20   count += 1\n\
         \x20   sys.stdout.write(str(count) + \":\" + line)\n\
         sys.stdout.flush()\n\
         print(\"lines\", count)\n\
         print(sys.stdout is sys.stdout, sys.stdin is sys.stdin)\n",
    );
    for opt in ["0", "2", "3"] {
        let mut child = Command::new(PYRS)
            .args(["run", "--no-cache", "-O", opt, "-i"])
            .arg(&src)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"alpha\nbeta\n")
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "1:alpha\n2:beta\nlines 2\nTrue True\n",
            "streams differ at -O{opt}"
        );
    }
}

/// Closing one would break every later print with no way back, and a
/// compiled program has no reason to want it. CPython allows it; this is a
/// deliberate difference, so it is pinned.
#[test]
fn a_standard_stream_cannot_be_closed() {
    let (_dir, src) = write_prog(
        "close_std",
        "import sys\n\
         try:\n\
         \x20   sys.stdout.close()\n\
         except ValueError as exc:\n\
         \x20   print(\"refused:\", exc)\n",
    );
    let out = Command::new(PYRS)
        .args(["run", "--no-cache", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "refused: cannot close a standard stream\n"
    );
}

// ---------------------------------------------------------- environment

/// A CLI reads its environment. `os.environ` is a snapshot dict, so `in`,
/// `[]`, `.get` and iteration all behave; `getenv` is ordinary PyRs over it.
#[test]
fn the_environment_is_readable() {
    let (_dir, src) = write_prog(
        "environ",
        "import os\n\
         print(os.getenv(\"PYRS_CLI_TEST\", \"unset\"))\n\
         print(os.getenv(\"PYRS_CLI_ABSENT\", \"fallback\"))\n\
         print(\"PYRS_CLI_TEST\" in os.environ)\n\
         print(os.environ.get(\"PYRS_CLI_TEST\", \"?\"))\n\
         print(os.environ[\"PYRS_CLI_TEST\"])\n",
    );
    let expected = Command::new("python3")
        .arg(&src)
        .env("PYRS_CLI_TEST", "present")
        .output()
        .unwrap();
    let out = Command::new(PYRS)
        .args(["run", "--no-cache", "-i"])
        .arg(&src)
        .env("PYRS_CLI_TEST", "present")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&expected.stdout)
    );
}

// ----------------------------------------------------------------- paths

/// The path predicates and the lexical helpers, against CPython's own
/// `posixpath` -- including the cases that are easy to get wrong: a leading
/// dot is not an extension, and `//` is preserved.
#[test]
fn path_helpers_match_posixpath() {
    matches_python(
        "paths",
        r#"
import os.path

print(os.path.exists("/tmp"), os.path.exists("/no/such/thing"))
print(os.path.isdir("/tmp"), os.path.isfile("/tmp"))
for p in ["a.txt", "/x/y.tar.gz", ".bashrc", "no_ext", "/a/.b", "dir.d/f"]:
    print(p, os.path.splitext(p))
print(os.path.isabs("/a"), os.path.isabs("a"))
for p in ["a//b/../c", "/a/./b//", "..", "/../a", "", "//a/b", "a/../..", "/"]:
    print(repr(p), repr(os.path.normpath(p)))
print(os.path.abspath("/a/b/../c"))
"#,
    );
}

// ------------------------------------------------------- the whole thing

/// A parser with the surface a real one has, driven over argument vectors:
/// long and short flags, inline `--opt=value`, clustered short flags, a
/// value taken from the next token, `--`, positionals, and errors.
#[test]
fn an_argument_parser_behaves_like_pythons() {
    matches_python(
        "parser",
        r#"
from typing import Callable


class UsageError(Exception):
    pass


class Opt:
    def __init__(self, name: str, *, short: str = "", value: bool = False):
        self.name: str = name
        self.short: str = short
        self.value: bool = value


OPTS = [
    Opt("verbose", short="v"),
    Opt("output", short="o", value=True),
    Opt("level", value=True),
]


def find(token: str, short: bool) -> Opt:
    for opt in OPTS:
        if short and opt.short == token:
            return opt
        if not short and opt.name == token:
            return opt
    raise UsageError("unrecognized: " + token)


def parse(argv: list[str]) -> dict[str, object]:
    out: dict[str, object] = {}
    rest: list[str] = []
    i: int = 0
    bare: bool = False
    while i < len(argv):
        tok: str = argv[i]
        i += 1
        if bare or not tok.startswith("-") or tok == "-":
            rest.append(tok)
        elif tok == "--":
            bare = True
        elif tok.startswith("--"):
            body: str = tok[2:]
            inline: bool = "=" in body
            given: str = ""
            if inline:
                body, _sep, given = body.partition("=")
            opt: Opt = find(body, False)
            if not opt.value:
                out[opt.name] = True
            elif inline:
                out[opt.name] = given
            elif i < len(argv):
                out[opt.name] = argv[i]
                i += 1
            else:
                raise UsageError("--" + body + " needs a value")
        else:
            letters: str = tok[1:]
            j: int = 0
            while j < len(letters):
                opt = find(letters[j], True)
                j += 1
                if not opt.value:
                    out[opt.name] = True
                elif j < len(letters):
                    out[opt.name] = letters[j:]
                    j = len(letters)
                elif i < len(argv):
                    out[opt.name] = argv[i]
                    i += 1
                else:
                    raise UsageError("-" + opt.short + " needs a value")
    out["_rest"] = rest
    return out


def render(values: dict[str, object]) -> str:
    parts: list[str] = []
    for key in sorted(values.keys()):
        parts.append(key + "=" + str(values[key]))
    return " ".join(parts)


VECTORS = [
    ["src"],
    ["-v", "src"],
    ["--output", "a.tar", "src"],
    ["--output=b.tar"],
    ["-oc.tar"],
    ["-o", "d.tar"],
    ["-v", "--level", "9", "x", "y"],
    ["--", "-dash"],
    ["-"],
    ["--nope"],
    ["--output"],
]

for argv in VECTORS:
    try:
        print(str(argv).ljust(34), render(parse(argv)))
    except UsageError as exc:
        print(str(argv).ljust(34), "error:", exc)
"#,
    );
}
