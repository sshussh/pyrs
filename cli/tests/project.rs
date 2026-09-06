//! Project configuration: `[tool.pyrs]` in `pyproject.toml`, `pyrs init`,
//! and interpreter resolution.
//!
//! PyRs source is valid Python, so a PyRs project is a Python project — the
//! configuration goes in the file the rest of the Python toolchain already
//! reads, under the `[tool.<name>]` table they already agree on.
//!
//! uv is preferred but never required, so every test here must pass on a
//! machine without it: an optional dependency whose absent path is untested
//! is not optional in practice.

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
            .join(format!("pyrs-project-{tag}-{}", std::process::id())),
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

fn ok(dir: &Path, cache: &Path, args: &[&str]) -> String {
    let out = pyrs_in(dir, cache, args);
    assert!(
        out.status.success(),
        "PyRs failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn err(dir: &Path, cache: &Path, args: &[&str]) -> String {
    let out = pyrs_in(dir, cache, args);
    assert!(!out.status.success(), "expected a failure, got success");
    String::from_utf8_lossy(&out.stderr).to_string()
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

#[test]
fn init_writes_a_manifest_and_a_runnable_entry() {
    let (_d, root, cache) = project("init-fresh");
    ok(&root, &cache, &["init", "."]);

    let manifest = fs::read_to_string(root.join("pyproject.toml")).unwrap();
    assert!(manifest.contains("[tool.pyrs]"), "{manifest}");
    assert!(manifest.contains("entry = \"main.py\""), "{manifest}");
    // The interpreter PyRs was built against, not uv's default: a mismatch
    // otherwise shows up only when something Unicode-shaped disagrees.
    assert!(manifest.contains("requires-python"), "{manifest}");
    assert_eq!(ok(&root, &cache, &["run"]), "Hello from PyRs!\n");
}

#[test]
fn init_adds_its_table_to_an_existing_manifest() {
    let (_d, root, cache) = project("init-existing");
    write(
        &root.join("pyproject.toml"),
        "[project]\nname = \"existing\"\nversion = \"9.9.9\"\n",
    );
    ok(&root, &cache, &["init", "."]);
    let manifest = fs::read_to_string(root.join("pyproject.toml")).unwrap();
    assert!(
        manifest.contains("name = \"existing\""),
        "clobbered: {manifest}"
    );
    assert!(
        manifest.contains("version = \"9.9.9\""),
        "clobbered: {manifest}"
    );
    assert!(manifest.contains("[tool.pyrs]"), "{manifest}");
}

#[test]
fn init_refuses_to_overwrite_an_existing_table() {
    let (_d, root, cache) = project("init-clobber");
    ok(&root, &cache, &["init", "."]);
    let message = err(&root, &cache, &["init", "."]);
    assert!(
        message.contains("already has a [tool.pyrs] table"),
        "{message}"
    );
}

#[test]
fn init_preserves_an_existing_entry_file() {
    let (_d, root, cache) = project("init-keep-entry");
    write(&root.join("main.py"), "print(\"mine\")\n");
    ok(&root, &cache, &["init", "."]);
    assert_eq!(ok(&root, &cache, &["run"]), "mine\n");
}

// ---------------------------------------------------------------------------
// Discovery and precedence
// ---------------------------------------------------------------------------

#[test]
fn discovery_walks_up_to_the_nearest_manifest() {
    let (_d, root, cache) = project("discover-up");
    ok(&root, &cache, &["init", "."]);
    write(&root.join("main.py"), "print(\"found\")\n");
    let deep = root.join("a").join("b").join("c");
    fs::create_dir_all(&deep).unwrap();
    assert_eq!(ok(&deep, &cache, &["run"]), "found\n");
}

#[test]
fn a_pyproject_without_the_table_is_not_a_pyrs_project() {
    // It belongs to some other Python project, so it must not capture
    // `pyrs run` and turn a stdin pipe into a project build.
    let (_d, root, cache) = project("foreign-manifest");
    write(
        &root.join("pyproject.toml"),
        "[project]\nname = \"someone-else\"\n",
    );
    let out = pyrs_in(&root, &cache, &["check"]);
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("no input"), "{text}");
}

#[test]
fn an_explicit_input_bypasses_discovery() {
    let (_d, root, cache) = project("explicit-input");
    ok(&root, &cache, &["init", "."]);
    write(&root.join("main.py"), "print(\"project entry\")\n");
    let other = root.join("other.py");
    write(&other, "print(\"explicit\")\n");
    assert_eq!(
        ok(&root, &cache, &["run", "-i", other.to_str().unwrap()]),
        "explicit\n"
    );
}

#[test]
fn the_manifest_supplies_the_optimization_level_and_the_flag_wins() {
    let (_d, root, cache) = project("opt-level");
    ok(&root, &cache, &["init", "."]);
    write(&root.join("main.py"), "print(\"opt\")\n");
    let manifest = root.join("pyproject.toml");
    let text = fs::read_to_string(&manifest)
        .unwrap()
        .replace("opt-level = 2", "opt-level = 0");
    write(&manifest, &text);

    assert_eq!(ok(&root, &cache, &["run"]), "opt\n");
    assert_eq!(ok(&root, &cache, &["run", "-O", "3"]), "opt\n");
    // Two entries: the manifest's level and the overriding flag's.
    let programs = fs::read_dir(cache.join("programs")).unwrap().count();
    assert_eq!(programs, 2, "the -O flag did not override the manifest");
}

// ---------------------------------------------------------------------------
// Import root
// ---------------------------------------------------------------------------

#[test]
fn a_declared_root_makes_a_src_layout_importable() {
    // uv scaffolds src/ layouts, and the resolver otherwise roots only at the
    // entry script's directory, so a sibling package is invisible without it.
    let (_d, root, cache) = project("src-layout");
    write(
        &root.join("src").join("pkg").join("__init__.py"),
        "def hello() -> str:\n    return \"packaged\"\n",
    );
    write(
        &root.join("src").join("app").join("main.py"),
        "import pkg\nprint(pkg.hello())\n",
    );
    ok(&root, &cache, &["init", "--entry", "src/app/main.py", "."]);

    let manifest = root.join("pyproject.toml");
    let without_root = fs::read_to_string(&manifest).unwrap();
    let message = err(&root, &cache, &["run"]);
    assert!(message.contains("No module named 'pkg'"), "{message}");

    write(&manifest, &format!("{without_root}root = \"src\"\n"));
    assert_eq!(ok(&root, &cache, &["run"]), "packaged\n");
}

// ---------------------------------------------------------------------------
// Execution mode
// ---------------------------------------------------------------------------

fn set_execution(root: &Path, mode: &str) {
    let manifest = root.join("pyproject.toml");
    let text = fs::read_to_string(&manifest)
        .unwrap()
        .replace("execution = \"native\"", &format!("execution = \"{mode}\""));
    write(&manifest, &text);
}

#[test]
fn declared_compat_runs_a_program_native_compilation_rejects() {
    let (_d, root, cache) = project("compat-declared");
    ok(&root, &cache, &["init", "."]);
    // `sys.implementation` is not in PyRs's subset, so this program can only
    // run under CPython -- which makes the mode observable.
    write(
        &root.join("main.py"),
        "import sys\nprint(sys.implementation.name)\n",
    );
    let message = err(&root, &cache, &["run"]);
    assert!(message.contains("sys.implementation"), "{message}");

    set_execution(&root, "compat");
    assert_eq!(ok(&root, &cache, &["run"]), "cpython\n");
}

#[test]
fn no_compat_overrides_a_manifest_that_asks_for_compat() {
    // Needed so a project can test whether its program has become natively
    // compilable without editing the file.
    let (_d, root, cache) = project("no-compat");
    ok(&root, &cache, &["init", "."]);
    write(
        &root.join("main.py"),
        "import sys\nprint(sys.implementation.name)\n",
    );
    set_execution(&root, "compat");
    assert_eq!(ok(&root, &cache, &["run"]), "cpython\n");

    let message = err(&root, &cache, &["run", "--no-compat"]);
    assert!(message.contains("sys.implementation"), "{message}");
}

#[test]
fn compat_is_never_selected_by_dependencies() {
    // Recorded as a decision, not an oversight: selecting a whole-program
    // execution mode from a metadata table would be exactly the invisible
    // switch the product contract rules out.
    let (_d, root, cache) = project("no-inference");
    ok(&root, &cache, &["init", "."]);
    write(
        &root.join("main.py"),
        "import sys\nprint(sys.implementation.name)\n",
    );
    let manifest = root.join("pyproject.toml");
    let text = fs::read_to_string(&manifest)
        .unwrap()
        .replace("[project]", "[project]\ndependencies = [\"requests\"]");
    write(&manifest, &text);

    let message = err(&root, &cache, &["run"]);
    assert!(
        message.contains("sys.implementation"),
        "declaring a dependency silently switched execution mode: {message}"
    );
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_key_is_rejected_with_the_ones_that_exist() {
    let (_d, root, cache) = project("unknown-key");
    ok(&root, &cache, &["init", "."]);
    let manifest = root.join("pyproject.toml");
    let text = format!(
        "{}entrypoint = \"main.py\"\n",
        fs::read_to_string(&manifest).unwrap()
    );
    write(&manifest, &text);

    let message = err(&root, &cache, &["run"]);
    assert!(message.contains("unknown key 'entrypoint'"), "{message}");
    assert!(
        message.contains("'entry'"),
        "the message should list valid keys: {message}"
    );
}

#[test]
fn a_wrongly_typed_value_names_the_key() {
    let (_d, root, cache) = project("bad-type");
    ok(&root, &cache, &["init", "."]);
    let manifest = root.join("pyproject.toml");
    let text = fs::read_to_string(&manifest)
        .unwrap()
        .replace("opt-level = 2", "opt-level = \"two\"");
    write(&manifest, &text);
    let message = err(&root, &cache, &["run"]);
    assert!(
        message.contains("'opt-level' must be an integer"),
        "{message}"
    );
}

#[test]
fn an_out_of_range_optimization_level_is_rejected() {
    let (_d, root, cache) = project("bad-opt");
    ok(&root, &cache, &["init", "."]);
    let manifest = root.join("pyproject.toml");
    let text = fs::read_to_string(&manifest)
        .unwrap()
        .replace("opt-level = 2", "opt-level = 9");
    write(&manifest, &text);
    let message = err(&root, &cache, &["run"]);
    assert!(message.contains("must be 0-3"), "{message}");
}

#[test]
fn an_unknown_execution_mode_lists_the_real_ones() {
    let (_d, root, cache) = project("bad-execution");
    ok(&root, &cache, &["init", "."]);
    set_execution(&root, "hybrid");
    let message = err(&root, &cache, &["run"]);
    assert!(message.contains("\"native\" or \"compat\""), "{message}");
}

#[test]
fn malformed_toml_is_reported_as_such() {
    let (_d, root, cache) = project("bad-toml");
    ok(&root, &cache, &["init", "."]);
    let manifest = root.join("pyproject.toml");
    let text = format!(
        "{}\nnot = = valid\n",
        fs::read_to_string(&manifest).unwrap()
    );
    write(&manifest, &text);
    let message = err(&root, &cache, &["run"]);
    assert!(message.contains("invalid TOML"), "{message}");
}

// ---------------------------------------------------------------------------
// check reports what was resolved
// ---------------------------------------------------------------------------

#[test]
fn check_reports_the_project_and_the_interpreter() {
    // Which interpreter is in use must never be a guess: the default now
    // depends on whether uv is installed and whether this is a uv project.
    let (_d, root, cache) = project("check-report");
    ok(&root, &cache, &["init", "."]);
    write(&root.join("main.py"), "print(\"ok\")\n");
    let report = ok(&root, &cache, &["check"]);
    for field in [
        "project:",
        "entry:",
        "import root:",
        "execution:",
        "interpreter:",
    ] {
        assert!(report.contains(field), "missing {field} in:\n{report}");
    }
    assert!(report.contains("PyRs targets"), "{report}");
}

// ---------------------------------------------------------------------------
// Working without a project at all
// ---------------------------------------------------------------------------

#[test]
fn an_explicit_source_needs_no_project_or_manifest() {
    // The property that makes PyRs worth having: it compiles with no uv, no
    // virtual environment and no manifest.
    let (_d, root, cache) = project("no-project");
    let prog = root.join("standalone.py");
    write(&prog, "print(\"standalone\")\n");
    assert_eq!(
        ok(&root, &cache, &["run", "-i", prog.to_str().unwrap()]),
        "standalone\n"
    );
    assert!(!root.join("pyproject.toml").exists());
}
