//! `pyrs test`.
//!
//! The point of the command is narrow and worth restating in the tests:
//! pytest under CPython already checks whether the logic is right. What only
//! a native runner can check is whether the *compiled* program agrees — the
//! exact failure mode of a compiler for a Python subset.

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

fn project(tag: &str) -> (TempDir, PathBuf, PathBuf) {
    let dir = TempDir(
        Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-test-{tag}-{}", std::process::id())),
    );
    let root = dir.0.join("proj");
    let cache = dir.0.join("cache");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&cache).unwrap();
    (dir, root, cache)
}

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn pyrs_in(dir: &Path, cache: &Path, args: &[&str]) -> std::process::Output {
    Command::new(PYRS)
        .args(args)
        .current_dir(dir)
        .env("PYRS_CACHE_DIR", cache)
        .output()
        .expect("failed to spawn PyRs")
}

fn stdout_of(dir: &Path, cache: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&pyrs_in(dir, cache, args).stdout).to_string()
}

/// A project with a `src/` layout and a `tests/` directory — the shape
/// `pyrs init` scaffolds, and pytest's convention.
fn scaffold(root: &Path, tests: &str) {
    write(
        &root.join("pyproject.toml"),
        "[project]\nname = \"app\"\n\n[tool.pyrs]\nentry = \"src/app/main.py\"\nroot = \"src\"\n",
    );
    write(&root.join("src/app/__init__.py"), "");
    write(
        &root.join("src/app/util.py"),
        "def add(a: int, b: int) -> int:\n    return a + b\n",
    );
    write(&root.join("src/app/main.py"), "print(1)\n");
    write(&root.join("tests/test_util.py"), tests);
}

const PASSING: &str = "from app import util\n\n\n\
                       def test_add() -> None:\n    assert util.add(2, 3) == 5\n\n\n\
                       def test_zero() -> None:\n    assert util.add(0, 0) == 0\n";

#[test]
fn tests_run_against_the_compiled_program() {
    let (_d, root, cache) = project("pass");
    scaffold(&root, PASSING);

    let out = pyrs_in(&root, &cache, &["test"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(text.starts_with("running 2 tests\n"), "{text}");
    assert!(text.contains("test test_util::test_add ... ok"), "{text}");
    assert!(
        text.contains("test result: ok. 2 passed; 0 failed"),
        "{text}"
    );
}

#[test]
fn a_failing_assertion_fails_the_run_and_names_the_test() {
    let (_d, root, cache) = project("fail");
    scaffold(
        &root,
        "from app import util\n\n\n\
         def test_ok() -> None:\n    assert util.add(1, 1) == 2\n\n\n\
         def test_broken() -> None:\n    \
         assert util.add(1, 1) == 3, \"one plus one is not three\"\n",
    );

    let out = pyrs_in(&root, &cache, &["test"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(
        text.contains("test test_util::test_broken ... FAILED"),
        "{text}"
    );
    assert!(text.contains("failures:"), "{text}");
    // The assertion's own message is what tells you what went wrong.
    assert!(text.contains("one plus one is not three"), "{text}");
    assert!(
        text.contains("test result: FAILED. 1 passed; 1 failed"),
        "{text}"
    );
    // A failure in one test must not stop the others.
    assert!(text.contains("test test_util::test_ok ... ok"), "{text}");
}

#[test]
fn any_exception_fails_the_test_not_just_an_assertion() {
    let (_d, root, cache) = project("raises");
    scaffold(
        &root,
        "def test_raises() -> None:\n    raise ValueError(\"deliberate\")\n",
    );
    let out = pyrs_in(&root, &cache, &["test"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("test_raises ... FAILED"), "{text}");
    assert!(text.contains("deliberate"), "{text}");
}

#[test]
fn the_filter_selects_by_test_or_module_name() {
    let (_d, root, cache) = project("filter");
    scaffold(&root, PASSING);

    let text = stdout_of(&root, &cache, &["test", "zero"]);
    assert!(text.starts_with("running 1 test\n"), "{text}");
    assert!(text.contains("test_zero"), "{text}");
    assert!(!text.contains("test_add"), "{text}");

    // The module name matches too, so a whole file can be selected.
    let text = stdout_of(&root, &cache, &["test", "test_util"]);
    assert!(text.starts_with("running 2 tests\n"), "{text}");
}

#[test]
fn list_shows_what_would_run_without_running_it() {
    let (_d, root, cache) = project("list");
    scaffold(&root, PASSING);
    let text = stdout_of(&root, &cache, &["test", "--list"]);
    assert_eq!(
        text, "test_util::test_add\ntest_util::test_zero\n",
        "listing must be deterministic and complete"
    );
}

#[test]
fn only_zero_parameter_test_functions_are_run() {
    // pytest's fixtures are the reason a test takes arguments. PyRs cannot
    // supply them, and failing the whole run over a file pytest handles
    // fine would make `pyrs test` unusable beside it.
    let (_d, root, cache) = project("params");
    scaffold(
        &root,
        "def test_plain() -> None:\n    assert True\n\n\n\
         def test_fixture(tmp_path: str) -> None:\n    assert True\n\n\n\
         def helper() -> None:\n    assert True\n",
    );
    let text = stdout_of(&root, &cache, &["test", "--list"]);
    assert_eq!(text, "test_util::test_plain\n", "{text}");
}

#[test]
fn a_project_with_no_tests_is_not_a_failure() {
    // Failing here would make `pyrs test` unusable in CI from day one.
    let (_d, root, cache) = project("none");
    scaffold(&root, "");
    fs::remove_file(root.join("tests/test_util.py")).unwrap();

    let out = pyrs_in(&root, &cache, &["test"]);
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("no tests found"),
        "{:?}",
        out.stdout
    );
}

#[test]
fn a_filter_matching_nothing_says_so() {
    let (_d, root, cache) = project("filter-none");
    scaffold(&root, PASSING);
    let text = stdout_of(&root, &cache, &["test", "nonexistent"]);
    assert!(text.contains("no test matched the filter"), "{text}");
}

#[test]
fn tests_beside_the_source_are_found_too() {
    // Not every project puts tests in `tests/`; a module next to the code
    // it tests is just as conventional.
    let (_d, root, cache) = project("inline");
    scaffold(&root, PASSING);
    fs::remove_file(root.join("tests/test_util.py")).unwrap();
    write(
        &root.join("src/app/test_inline.py"),
        "def test_here() -> None:\n    assert 1 == 1\n",
    );
    let text = stdout_of(&root, &cache, &["test", "--list"]);
    assert_eq!(text, "app.test_inline::test_here\n", "{text}");
}

#[test]
fn a_single_file_can_be_tested_without_a_project() {
    let (_d, root, cache) = project("single");
    write(
        &root.join("test_alone.py"),
        "def test_one() -> None:\n    assert 2 + 2 == 4\n",
    );
    let out = pyrs_in(&root, &cache, &["test", "-i", "test_alone.py"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("test test_alone::test_one ... ok"), "{text}");
}

#[test]
fn a_test_that_prints_keeps_its_own_output() {
    // Results go to a file, not stdout, so nothing has to be stripped back
    // out of what the user's own code printed.
    let (_d, root, cache) = project("printing");
    scaffold(
        &root,
        "def test_talks() -> None:\n    print(\"hello from the test\")\n    assert True\n",
    );
    let text = stdout_of(&root, &cache, &["test"]);
    assert!(text.contains("hello from the test"), "{text}");
    assert!(text.contains("test result: ok. 1 passed"), "{text}");
}

#[test]
fn a_test_file_that_does_not_compile_is_reported() {
    let (_d, root, cache) = project("broken-source");
    scaffold(&root, "def test_bad() -> None:\n    x: int = 2.5\n");
    let out = pyrs_in(&root, &cache, &["test"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("type mismatch"), "{err}");
}

#[test]
fn a_crash_mid_suite_is_not_reported_as_a_pass() {
    // The harness records each result as it happens, so a process that dies
    // leaves the survivors on disk. Reporting only those would turn a crash
    // into a green run.
    let (_d, root, cache) = project("crash");
    scaffold(
        &root,
        "def test_first() -> None:\n    assert True\n\n\n\
         def test_crashes() -> None:\n    xs: list[int] = []\n    print(xs[5])\n\n\n\
         def test_third() -> None:\n    assert True\n",
    );
    let out = pyrs_in(&root, &cache, &["test"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("test test_util::test_first ... ok"), "{text}");
    // Whether the trap is catchable decides which branch this takes; either
    // way the run must not be reported as clean.
    if text.contains("not run") {
        assert!(text.contains("the harness stopped after"), "{text}");
        assert!(text.contains("test result: FAILED"), "{text}");
        assert_eq!(out.status.code(), Some(1), "{text}");
    } else {
        assert!(text.contains("test_crashes ... FAILED"), "{text}");
        assert_eq!(out.status.code(), Some(1), "{text}");
    }
}

#[test]
fn tests_are_ordinary_python_and_still_run_under_cpython() {
    // The whole premise: the same file is valid input to both engines, so
    // PyRs's answer can be compared against CPython's rather than trusted.
    let (_d, root, cache) = project("cpython");
    scaffold(&root, PASSING);
    assert!(pyrs_in(&root, &cache, &["test"]).status.success());

    let out = Command::new("python3")
        .arg("-c")
        .arg(
            "import sys; sys.path[:0] = ['src', 'tests']; \
             import test_util; test_util.test_add(); test_util.test_zero(); print('ok')",
        )
        .current_dir(&root)
        .output()
        .expect("failed to spawn python3");
    assert!(
        out.status.success(),
        "the same tests failed under CPython: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n");
}
