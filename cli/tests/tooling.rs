//! The commands that exist to make PyRs usable rather than to compile
//! anything: `doctor`, `clean`, `completions`, and the project-aware
//! `build`.
//!
//! Everything here must work on a machine with no uv, no virtual environment
//! and no network, because that is the machine PyRs claims to support.

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
            .join(format!("pyrs-tooling-{tag}-{}", std::process::id())),
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

/// A `src/` layout project with a package import, which is what exercises
/// the declared import root.
fn scaffold(root: &Path, extra: &str) {
    write(
        &root.join("pyproject.toml"),
        &format!(
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n\n\
             [tool.pyrs]\nentry = \"src/demo/main.py\"\nroot = \"src\"\n{extra}"
        ),
    );
    write(&root.join("src/demo/__init__.py"), "");
    write(
        &root.join("src/demo/util.py"),
        "def greet(who: str) -> str:\n    return f\"hello, {who}\"\n",
    );
    write(
        &root.join("src/demo/main.py"),
        "from demo import util\n\nprint(util.greet(\"world\"))\n",
    );
}

// ---------------------------------------------------------------------------
// build
// ---------------------------------------------------------------------------

#[test]
fn build_in_a_project_writes_target_and_resolves_the_import_root() {
    let (_d, root, cache) = project("build-default");
    scaffold(&root, "");

    ok(&root, &cache, &["build"]);
    let binary = root.join("target/demo");
    assert!(binary.is_file(), "expected target/demo, found nothing");

    let out = Command::new(&binary).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello, world\n");
    // Nothing in the working directory: the old default put `a.out` here.
    assert!(!root.join("a.out").exists());
}

#[test]
fn build_is_the_same_command_as_compile() {
    let (_d, root, cache) = project("build-alias");
    scaffold(&root, "");
    ok(&root, &cache, &["compile"]);
    assert!(root.join("target/demo").is_file());
}

#[test]
fn an_explicit_output_still_wins_over_the_project_default() {
    let (_d, root, cache) = project("build-output");
    scaffold(&root, "");
    ok(&root, &cache, &["build", "-o", "custom"]);
    assert!(root.join("custom").is_file());
    assert!(!root.join("target").exists());
}

#[test]
fn the_target_directory_is_configurable() {
    let (_d, root, cache) = project("build-target-key");
    scaffold(&root, "target = \"build\"\n");
    ok(&root, &cache, &["build"]);
    assert!(root.join("build/demo").is_file());
    assert!(!root.join("target").exists());
}

#[test]
fn build_outside_a_project_still_needs_an_input() {
    let (_d, root, cache) = project("build-no-project");
    let message = err(&root, &cache, &["build"]);
    assert!(message.contains("no input"), "{message}");
}

#[test]
fn build_outside_a_project_writes_a_out_as_before() {
    let (_d, root, cache) = project("build-bare");
    write(&root.join("prog.py"), "print(1)\n");
    ok(&root, &cache, &["build", "-i", "prog.py"]);
    assert!(root.join("a.out").is_file());
}

// ---------------------------------------------------------------------------
// clean
// ---------------------------------------------------------------------------

#[test]
fn clean_removes_the_target_directory() {
    let (_d, root, cache) = project("clean");
    scaffold(&root, "");
    ok(&root, &cache, &["build"]);
    assert!(root.join("target/demo").is_file());

    let out = ok(&root, &cache, &["clean"]);
    assert!(out.contains("removed"), "{out}");
    assert!(!root.join("target").exists());
    // The source is not part of "build output".
    assert!(root.join("src/demo/main.py").is_file());
}

#[test]
fn clean_does_not_touch_the_shared_build_cache() {
    let (_d, root, cache) = project("clean-cache");
    scaffold(&root, "");
    ok(&root, &cache, &["build"]);
    let programs = fs::read_dir(cache.join("programs")).unwrap().count();
    assert!(programs > 0);

    ok(&root, &cache, &["clean"]);
    assert_eq!(
        fs::read_dir(cache.join("programs")).unwrap().count(),
        programs,
        "clearing one project's outputs must not slow down every build on the machine"
    );
}

#[test]
fn a_clean_dry_run_removes_nothing() {
    let (_d, root, cache) = project("clean-dry");
    scaffold(&root, "");
    ok(&root, &cache, &["build"]);
    let out = ok(&root, &cache, &["clean", "--dry-run"]);
    assert!(out.contains("would remove"), "{out}");
    assert!(root.join("target/demo").is_file());
}

#[test]
fn cleaning_nothing_is_not_an_error() {
    let (_d, root, cache) = project("clean-empty");
    scaffold(&root, "");
    let out = ok(&root, &cache, &["clean"]);
    assert!(out.contains("nothing to clean"), "{out}");
}

#[test]
fn clean_outside_a_project_says_which_command_was_meant() {
    let (_d, root, cache) = project("clean-outside");
    let message = err(&root, &cache, &["clean"]);
    assert!(message.contains("not in a PyRs project"), "{message}");
    // The neighbouring command is the likely intent, so name it.
    assert!(message.contains("pyrs cache clean"), "{message}");
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

#[test]
fn doctor_reports_the_toolchain_the_build_actually_uses() {
    let (_d, root, cache) = project("doctor");
    let out = ok(&root, &cache, &["doctor"]);
    for expected in ["pyrs ", "target", "C compiler", "interpreter", "cache"] {
        assert!(out.contains(expected), "missing {expected}: {out}");
    }
    assert!(out.contains(&cache.to_string_lossy().to_string()), "{out}");
    assert!(out.contains("no problems found"), "{out}");
}

#[test]
fn doctor_reports_the_project_it_would_build() {
    let (_d, root, cache) = project("doctor-project");
    scaffold(&root, "");
    let out = ok(&root, &cache, &["doctor"]);
    assert!(out.contains("src/demo/main.py"), "{out}");
    assert!(out.contains("target"), "{out}");
}

#[test]
fn doctor_fails_when_the_manifest_it_reports_is_broken() {
    let (_d, root, cache) = project("doctor-broken");
    write(
        &root.join("pyproject.toml"),
        "[tool.pyrs]\nentry = \"main.py\"\nnonsense = 1\n",
    );
    let out = pyrs_in(&root, &cache, &["doctor"]);
    // A report that says "no problems" about a project that cannot build
    // would be worse than no report.
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("unknown key 'nonsense'"), "{text}");
    assert!(text.contains("problem"), "{text}");
}

// ---------------------------------------------------------------------------
// completions
// ---------------------------------------------------------------------------

#[test]
fn completions_are_generated_for_every_supported_shell() {
    let (_d, root, cache) = project("completions");
    for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
        let out = ok(&root, &cache, &["completions", shell]);
        assert!(out.len() > 200, "{shell}: suspiciously short: {out}");
        assert!(out.contains("pyrs"), "{shell}: does not mention pyrs");
        // Completing a command that does not exist is worse than not
        // completing at all.
        assert!(
            out.contains("compile"),
            "{shell}: missing a real subcommand"
        );
        assert!(out.contains("cache"), "{shell}: missing a real subcommand");
    }
}

#[test]
fn an_unsupported_shell_is_refused_with_the_list() {
    let (_d, root, cache) = project("completions-bad");
    let message = err(&root, &cache, &["completions", "tcsh"]);
    assert!(message.contains("tcsh"), "{message}");
    assert!(message.contains("bash"), "{message}");
}

// ---------------------------------------------------------------------------
// Argument errors
// ---------------------------------------------------------------------------

#[test]
fn a_mistyped_flag_names_itself_and_suggests_the_real_one() {
    let (_d, root, cache) = project("flag-typo");
    let message = err(&root, &cache, &["check", "--inpt", "x.py"]);
    assert!(
        message.contains("--inpt"),
        "the flag must be named: {message}"
    );
    assert!(message.contains("--input"), "{message}");
}

#[test]
fn a_mistyped_nested_subcommand_suggests_the_real_one() {
    let (_d, root, cache) = project("nested-typo");
    let message = err(&root, &cache, &["cache", "prun"]);
    assert!(message.contains("prune"), "{message}");
}
