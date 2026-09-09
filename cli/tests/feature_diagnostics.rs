//! Unsupported features are named, not merely refused.
//!
//! [docs/GUIDE.md](../../docs/GUIDE.md) promises that "unsupported Python
//! features produce parse/semantic errors that name the feature". For the
//! most common ones they did not. `eval(x)` reported `function 'eval' is not
//! defined` — indistinguishable from a typo — and
//! `if __name__ == "__main__":`, the single most common idiom in Python,
//! reported `name '__name__' is not defined`.
//!
//! Every test here asserts two things: that the message names the feature,
//! and that a *genuine* typo still gets the plain message. The second half
//! matters more than the first — a table that swallows real typos would be a
//! worse compiler, not a better one.

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

/// The diagnostic for a program that must be rejected.
fn rejects(tag: &str, source: &str) -> String {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-diag-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    let out = Command::new(PYRS)
        .args(["check", "-i"])
        .arg(&src)
        .output()
        .expect("failed to spawn PyRs");
    assert!(!out.status.success(), "{tag} was accepted:\n{source}");
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// Assert the message names the feature and offers a way forward.
fn names(tag: &str, source: &str, expect: &[&str]) {
    let message = rejects(tag, source);
    for want in expect {
        assert!(
            message.contains(want),
            "{tag}: message does not mention {want:?}:\n{message}"
        );
    }
    assert!(
        !message.contains("is not defined"),
        "{tag}: fell through to the generic message:\n{message}"
    );
}

// ---------------------------------------------------------------------------
// The idiom everyone writes first
// ---------------------------------------------------------------------------

#[test]
fn other_module_attributes_are_named_together() {
    names("file-attr", "print(__file__)\n", &["__file__", "module"]);
}

// ---------------------------------------------------------------------------
// Dynamism the closed-world model rules out
// ---------------------------------------------------------------------------

#[test]
fn eval_and_exec_say_why_and_point_at_compat() {
    for (tag, source) in [("eval", "x = eval(\"1\")\n"), ("exec", "exec(\"x = 1\")\n")] {
        names(tag, source, &["eval()", "--compat"]);
    }
}

#[test]
fn attribute_reflection_is_named() {
    for (tag, source) in [
        ("getattr", "print(getattr(1, \"x\"))\n"),
        ("hasattr", "print(hasattr(1, \"x\"))\n"),
        ("setattr", "setattr(1, \"x\", 2)\n"),
    ] {
        names(tag, source, &["reflection", "statically"]);
    }
}

#[test]
fn type_and_the_scope_helpers_are_named() {
    names("type", "print(type(1))\n", &["type()", "isinstance"]);
    names("globals", "print(globals())\n", &["globals()"]);
}

/// `isinstance(x, A or B)` compiles under CPython and silently tests only
/// `A`, because `or` yields its first truthy operand and a type object is
/// always truthy. Rejecting it is right, but the message has to say *that*,
/// or it reads as "PyRs cannot do multiple types" — which it can, with the
/// tuple form the message names.
#[test]
fn isinstance_with_or_names_the_trap_and_the_tuple_form() {
    let msg = rejects(
        "isinstance_or",
        "v: object = (1, \"a\")\nprint(isinstance(v, list or tuple))\n",
    );
    for want in ["(list, tuple)", "tests only the first type", "truthy"] {
        assert!(msg.contains(want), "message lacks {want:?}:\n{msg}");
    }

    // The tuple form is accepted, so the message points somewhere real.
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-diag-isinstance_ok-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(
        &src,
        "v: object = (1, \"a\")\nprint(isinstance(v, (list, tuple)))\n",
    )
    .unwrap();
    let out = Command::new(PYRS)
        .args(["run", "--no-cache", "-i"])
        .arg(&src)
        .output()
        .expect("failed to spawn PyRs");
    assert!(
        out.status.success(),
        "the suggested spelling was rejected:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "True\n");
}

#[test]
fn slots_and_dict_are_named() {
    names(
        "slots",
        "class C:\n    __slots__ = 1\n\n    def __init__(self) -> None:\n        \
         self.x: int = 0\n\n\nprint(1)\n",
        &["__slots__"],
    );
}

// ---------------------------------------------------------------------------
// Types with no representation
// ---------------------------------------------------------------------------

#[test]
fn bytes_is_named_as_a_call_and_as_an_annotation() {
    names("bytes-call", "b = bytes([1])\n", &["bytes", "UTF-8"]);
    names(
        "bytes-anno",
        "def f(b: bytes) -> int:\n    return 0\n",
        &["bytes"],
    );
}

#[test]
fn complex_and_frozenset_are_named() {
    names("complex", "z = complex(1, 2)\n", &["complex"]);
    names("frozenset", "s = frozenset([1])\n", &["frozenset"]);
}

#[test]
fn generics_are_named_rather_than_reported_as_an_unknown_name() {
    names(
        "typevar",
        "from typing import TypeVar\n\nT = TypeVar(\"T\")\n",
        &["generics", "TypeVar"],
    );
}

// ---------------------------------------------------------------------------
// Syntax that reached no rule
// ---------------------------------------------------------------------------

#[test]
fn exception_chaining_is_named_in_every_raise_form() {
    for (tag, source) in [
        ("from-bare", "raise RuntimeError from None\n"),
        ("from-empty", "raise RuntimeError() from None\n"),
        ("from-arg", "raise ValueError(\"a\") from None\n"),
    ] {
        let message = rejects(tag, source);
        assert!(message.contains("exception chaining"), "{tag}: {message}");
        assert!(
            !message.contains("found 'from'"),
            "{tag}: still naming the token:\n{message}"
        );
    }
}

#[test]
fn async_and_await_are_named_at_statement_and_expression_positions() {
    for (tag, source) in [
        ("async-def", "async def f() -> None:\n    pass\n"),
        ("async-with", "async with open(\"f\") as h:\n    pass\n"),
        (
            "await-expr",
            "def g() -> int:\n    return 1\n\n\nx = await g()\n",
        ),
    ] {
        let message = rejects(tag, source);
        assert!(message.contains("async and await"), "{tag}: {message}");
        assert!(
            !message.contains("expected an expression"),
            "{tag}: still naming the token:\n{message}"
        );
    }
}

// ---------------------------------------------------------------------------
// Standard-library modules
// ---------------------------------------------------------------------------

#[test]
fn an_unshipped_stdlib_module_says_so_and_points_at_compat() {
    for (tag, module, note) in [
        ("re", "re", "regular expressions"),
        ("collections", "collections", "deque"),
        ("itertools", "itertools", "itertools"),
        ("datetime", "datetime", "dates and times"),
        ("random", "random", "random"),
        ("argparse", "argparse", "sys.argv"),
    ] {
        let message = rejects(tag, &format!("import {module}\n"));
        assert!(message.contains(note), "{tag}: {message}");
        assert!(message.contains("--compat"), "{tag}: {message}");
    }
}

#[test]
fn a_submodule_is_covered_by_its_package() {
    // `logging.handlers` is a submodule of a package the table lists, so the
    // note has to come from the top-level name rather than the full path.
    let message = rejects("submodule", "import logging.handlers\n");
    assert!(message.contains("--compat"), "{message}");
}

// ---------------------------------------------------------------------------
// The half that matters more: typos are still typos
// ---------------------------------------------------------------------------

#[test]
fn a_misspelled_function_still_gets_the_plain_message() {
    let message = rejects("typo-func", "print(lenght([1]))\n");
    assert!(
        message.contains("function 'lenght' is not defined"),
        "{message}"
    );
}

#[test]
fn a_misspelled_variable_still_gets_the_plain_message() {
    let message = rejects("typo-name", "x: int = 1\nprint(xx)\n");
    assert!(message.contains("name 'xx' is not defined"), "{message}");
}

#[test]
fn a_misspelled_module_still_gets_the_plain_message() {
    let message = rejects("typo-module", "import nonexistent_thing\n");
    assert!(
        message.contains("No module named 'nonexistent_thing'"),
        "{message}"
    );
    assert!(
        !message.contains("--compat"),
        "a typo must not be advertised as a missing feature:\n{message}"
    );
}

#[test]
fn a_misspelled_annotation_still_gets_the_plain_message() {
    let message = rejects("typo-type", "def f(x: itn) -> int:\n    return 0\n");
    assert!(message.contains("unknown type 'itn'"), "{message}");
}

#[test]
fn supported_code_is_still_accepted() {
    // The table sits on the error paths, so nothing valid should reach it.
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-diag-ok-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(
        &src,
        "import math\nimport os.path\n\n\
         def main() -> None:\n    \
         print(math.sqrt(4.0), os.path.basename(\"/a/b\"), isinstance(1, int))\n\n\n\
         main()\n",
    )
    .unwrap();
    let out = Command::new(PYRS)
        .args(["check", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "valid program rejected:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
