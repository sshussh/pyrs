//! `--infer ml` and `--infer dl`: learned types where call sites say nothing.
//!
//! A prediction stands in only for a parameter no call site types, so in a
//! whole program it can reach only code nothing calls, and a program's
//! behaviour never depends on the engine. These tests hold that line against
//! CPython, including the shapes that broke it during development: a nested
//! closure, and a function passed as a value.

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

fn workspace(tag: &str, source: &str) -> TempDir {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-learned-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    fs::write(dir.0.join("prog.py"), source).unwrap();
    dir
}

/// Every engine, at -O0 and -O2, prints what CPython prints.
fn every_engine_matches_python(tag: &str, source: &str) {
    let dir = workspace(tag, source);
    let expected = Command::new("python3")
        .arg("prog.py")
        .current_dir(&dir.0)
        .output()
        .unwrap();
    assert!(expected.status.success(), "CPython failed for {tag}");
    for engine in ["static", "ml", "dl"] {
        for opt in ["0", "2"] {
            let out = Command::new(PYRS)
                .args(["run", "--infer", engine, "-O", opt, "-i", "prog.py"])
                .current_dir(&dir.0)
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&expected.stdout),
                "{tag}: --infer {engine} -O{opt} differs\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

fn show_types(tag: &str, source: &str, engine: &str) -> String {
    let dir = workspace(tag, source);
    let out = Command::new(PYRS)
        .args(["check", "--show-types", "--infer", engine, "-i", "prog.py"])
        .current_dir(&dir.0)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{tag}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_program_behaves_the_same_under_every_engine() {
    every_engine_matches_python(
        "closures",
        "def twice(f):\n    def inner(n):\n        return f(f(n))\n    return inner\n\
         @twice\ndef bump(n):\n    return n + 1\n\
         def mk(flag):\n    def inner(a):\n        return a if flag else -a\n    return inner\n\
         def unused(count, name):\n    return name * count\n\
         print(bump(1), mk(True)(3), mk(False)(3))\n",
    );
    every_engine_matches_python(
        "callbacks",
        "def apply(g, x):\n    return g(x)\ndef sq(n):\n    return n * n\n\
         def up(s):\n    return s.upper()\n\
         print(apply(sq, 3), apply(up, 'a'), sorted(['bb', 'a'], key=len))\n",
    );
}

/// With call sites, inference owns the answer; a predictor only fills a
/// function nothing calls.
#[test]
fn call_sites_outrank_a_prediction() {
    let src = "def scale(values, factor):\n    return [v * factor for v in values]\n\
               print(scale([1, 2], 3))\n";
    for engine in ["static", "ml", "dl"] {
        let types = show_types(&format!("callsites-{engine}"), src, engine);
        assert!(
            types.contains("scale(values: list[int], factor: int)"),
            "--infer {engine}: {types}"
        );
    }
}

/// A function nothing calls has no evidence but its body and names; the
/// learned engines give its parameters types where the static engine leaves
/// them dynamic.
#[test]
fn a_learned_engine_types_a_function_nothing_calls() {
    let src = "def describe(label, verbose):\n    if verbose:\n        return label\n    return ''\n\
               print('ok')\n";
    let static_types = show_types("uncalled-static", src, "static");
    assert!(
        static_types.contains("describe(label: Any, verbose: Any)"),
        "{static_types}"
    );
    for engine in ["ml", "dl"] {
        let types = show_types(&format!("uncalled-{engine}"), src, engine);
        assert!(
            types.contains("describe(label: str, verbose: bool)"),
            "--infer {engine}: {types}"
        );
    }
}
