//! What `build-extension` refuses, and why.
//!
//! The extension boundary is where PyRs hands compiled code to a CPython
//! process that will call it with objects PyRs did not create. Every
//! rejection here is a promise about what cannot cross that boundary — and
//! an untested promise is one that quietly stops being true. The runtime
//! side lives in `compatibility/test_extension.py`; this is the *entry*
//! contract, which nothing checked before.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
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

fn workspace(tag: &str) -> (TempDir, PathBuf) {
    let dir = TempDir(
        Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-ext-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let root = dir.0.clone();
    (dir, root)
}

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn build_in(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(PYRS)
        .arg("build-extension")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to spawn PyRs")
}

/// The rejection message for `source`, asserting it was in fact rejected.
fn rejects(tag: &str, source: &str) -> String {
    let (_d, root) = workspace(tag);
    write(&root.join("kernels.py"), source);
    let out = build_in(&root, &["-i", "kernels.py", "--module", "k"]);
    assert!(
        !out.status.success(),
        "accepted:\n{source}\nstdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    // Nothing half-built left behind.
    let stray: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "kernels.py")
        .collect();
    assert!(stray.is_empty(), "a rejected build left {stray:?}");
    String::from_utf8_lossy(&out.stderr).to_string()
}

// ---------------------------------------------------------------------------
// The module boundary
// ---------------------------------------------------------------------------

#[test]
fn a_module_name_that_is_not_an_identifier_is_refused() {
    let (_d, root) = workspace("bad-name");
    write(
        &root.join("k.py"),
        "def f(x: float) -> float:\n    return x\n",
    );
    for name in ["not-a-name", "2fast", "with space", "kernels.sub", ""] {
        let out = build_in(&root, &["-i", "k.py", "--module", name]);
        assert!(!out.status.success(), "accepted module name {name:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("identifier") || err.contains("required"),
            "{name:?}: {err}"
        );
    }
}

#[test]
fn module_level_code_is_refused_rather_than_silently_dropped() {
    // The extension has no module initialization, so a top-level statement
    // would simply never run — the worst kind of acceptance.
    let message = rejects(
        "module-code",
        "TABLE = 3.0\n\n\ndef f(x: float) -> float:\n    return x * TABLE\n",
    );
    assert!(
        message.contains("numerical function definitions only")
            || message.contains("module globals"),
        "{message}"
    );
}

#[test]
fn imports_are_refused() {
    let message = rejects(
        "imports",
        "import math\n\n\ndef f(x: float) -> float:\n    return x\n",
    );
    assert!(message.contains("imports"), "{message}");
}

#[test]
fn a_docstring_is_allowed_beside_the_functions() {
    // The one module-level statement that carries no behaviour.
    let (_d, root) = workspace("docstring");
    write(
        &root.join("k.py"),
        "\"\"\"Kernels.\"\"\"\n\n\ndef f(x: float) -> float:\n    return x * 2.0\n",
    );
    let out = build_in(&root, &["-i", "k.py", "--module", "k"]);
    assert!(
        out.status.success(),
        "a docstring was rejected: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_class_is_refused() {
    let message = rejects(
        "class",
        "class Point:\n    def __init__(self) -> None:\n        self.x: float = 0.0\n",
    );
    assert!(
        message.contains("numerical function definitions only") || message.contains("class state"),
        "{message}"
    );
}

// ---------------------------------------------------------------------------
// The function signature boundary
// ---------------------------------------------------------------------------

#[test]
fn decorators_defaults_and_varargs_are_refused() {
    for (tag, source) in [
        ("default", "def f(x: float = 1.0) -> float:\n    return x\n"),
        ("vararg", "def f(*xs: float) -> float:\n    return 0.0\n"),
        ("kwarg", "def f(**kw: float) -> float:\n    return 0.0\n"),
        (
            "decorator",
            "def d(f: float) -> float:\n    return f\n\n\n@staticmethod\ndef f(x: float) -> float:\n    return x\n",
        ),
    ] {
        let message = rejects(tag, source);
        assert!(
            message.contains("decorators, defaults, *args or **kwargs")
                || message.contains("not supported"),
            "{tag}: {message}"
        );
    }
}

#[test]
fn a_source_with_no_public_function_is_refused() {
    // A leading underscore means private, so this exports nothing and the
    // resulting module would be an importable no-op.
    let message = rejects(
        "private-only",
        "def _helper(x: float) -> float:\n    return x\n",
    );
    assert!(
        message.contains("no public numerical functions"),
        "{message}"
    );
}

// ---------------------------------------------------------------------------
// The kernel body boundary
// ---------------------------------------------------------------------------

#[test]
fn object_identity_is_refused_because_it_cannot_be_honored() {
    let message = rejects("identity", "def f(x: float) -> bool:\n    return x is x\n");
    assert!(message.contains("object identity"), "{message}");
}

#[test]
fn calls_outside_the_module_are_refused() {
    for (tag, call) in [
        ("sorted", "sorted([x])[0]"),
        ("print", "print(x)"),
        ("undefined", "undefined_fn(x)"),
    ] {
        let message = rejects(
            tag,
            &format!("def f(x: float) -> float:\n    return {call}\n"),
        );
        assert!(message.contains("is not supported"), "{tag}: {message}");
        // The message has to say what *is* allowed, or it only tells you to
        // guess again.
        assert!(message.contains("numerical builtins"), "{tag}: {message}");
    }
}

#[test]
fn the_numerical_builtins_and_the_conversions_are_allowed() {
    // The complement of the list above, pinned because it is not obvious:
    // `int`, `float` and `str` are parsed as conversions rather than calls,
    // so they never reach the call allow-list at all. That is consistent
    // with `chr` being on it — a kernel may already produce a string — but
    // it is the kind of asymmetry that silently changes if nothing checks.
    let (_d, root) = workspace("allowed-calls");
    write(
        &root.join("k.py"),
        "def f(x: float) -> float:\n             return abs(x) + float(min(1, 2)) + float(len(chr(65))) + float(len(str(x)))\n",
    );
    let out = build_in(&root, &["-i", "k.py", "--module", "k"]);
    assert!(
        out.status.success(),
        "a documented builtin was rejected: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_call_to_a_sibling_kernel_is_allowed() {
    // The complement of the test above: the restriction is on leaving the
    // module, not on calling at all.
    let (_d, root) = workspace("sibling-call");
    write(
        &root.join("k.py"),
        "def double(x: float) -> float:\n    return x * 2.0\n\n\n\
         def quad(x: float) -> float:\n    return double(double(x))\n",
    );
    let out = build_in(&root, &["-i", "k.py", "--module", "k"]);
    assert!(
        out.status.success(),
        "a sibling call was rejected: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn dynamic_constructs_in_a_kernel_body_are_refused() {
    for (tag, body) in [
        (
            "comprehension",
            "    ys = [y * 2.0 for y in [x]]\n    return ys[0]\n",
        ),
        ("dict", "    d = {\"a\": x}\n    return d[\"a\"]\n"),
        ("lambda", "    g = lambda v: v + 1.0\n    return g(x)\n"),
    ] {
        let message = rejects(tag, &format!("def f(x: float) -> float:\n{body}"));
        assert!(!message.is_empty(), "{tag} was rejected without a reason");
    }
}

// ---------------------------------------------------------------------------
// Output safety
// ---------------------------------------------------------------------------

#[test]
fn the_output_can_never_overwrite_its_own_source() {
    // Losing the source to a build of the source is unrecoverable.
    let (_d, root) = workspace("overwrite");
    let source = root.join("k.py");
    write(&source, "def f(x: float) -> float:\n    return x\n");
    let out = build_in(&root, &["-i", "k.py", "--module", "k", "-o", "k.py"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("must not overwrite"), "{err}");
    assert_eq!(
        fs::read_to_string(&source).unwrap(),
        "def f(x: float) -> float:\n    return x\n",
        "the source was modified by a build that failed"
    );
}

// ---------------------------------------------------------------------------
// The manifest
// ---------------------------------------------------------------------------

#[test]
fn the_manifest_supplies_module_and_source() {
    // What [tool.pyrs.extension] exists for: not retyping both on every call.
    let (_d, root) = workspace("manifest");
    write(
        &root.join("pyproject.toml"),
        "[project]\nname = \"app\"\n\n[tool.pyrs]\nentry = \"main.py\"\n\n\
         [tool.pyrs.extension]\nmodule = \"kernels_native\"\nsource = \"kernels.py\"\n",
    );
    write(&root.join("main.py"), "print(1)\n");
    write(
        &root.join("kernels.py"),
        "def scale(x: float) -> float:\n    return x * 3.0\n",
    );

    let out = build_in(&root, &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let built: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("kernels_native") && n.ends_with(".so"))
        .collect();
    assert_eq!(built.len(), 1, "expected one extension, found {built:?}");
}

#[test]
fn a_flag_overrides_the_manifest() {
    let (_d, root) = workspace("manifest-override");
    write(
        &root.join("pyproject.toml"),
        "[project]\nname = \"app\"\n\n[tool.pyrs]\nentry = \"main.py\"\n\n\
         [tool.pyrs.extension]\nmodule = \"from_manifest\"\nsource = \"kernels.py\"\n",
    );
    write(&root.join("main.py"), "print(1)\n");
    write(
        &root.join("kernels.py"),
        "def scale(x: float) -> float:\n    return x * 3.0\n",
    );

    let out = build_in(&root, &["--module", "from_flag"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let names: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n.starts_with("from_flag")),
        "flag ignored: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.starts_with("from_manifest")),
        "the manifest won: {names:?}"
    );
}

#[test]
fn without_a_manifest_or_flags_the_missing_input_is_named() {
    let (_d, root) = workspace("no-config");
    let out = build_in(&root, &[]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--input") || err.contains("source") || err.contains("-i"),
        "{err}"
    );
}
