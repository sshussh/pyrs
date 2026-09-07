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

// ---------------------------------------------------------------------------
// Machine-readable diagnostics
// ---------------------------------------------------------------------------
//
// An editor cannot get a span out of prose. These pin the fields a consumer
// would index on, and the property that makes the format usable at all:
// every failure comes out as JSON, including the ones with no position.

/// Minimal field lookup — enough to assert on the shape without a JSON
/// dependency in the test suite either.
fn field<'a>(json: &'a str, key: &str) -> &'a str {
    let at = json
        .find(&format!("\"{key}\":"))
        .unwrap_or_else(|| panic!("no key {key} in {json}"));
    let rest = &json[at + key.len() + 3..];
    if let Some(stripped) = rest.strip_prefix('"') {
        let mut end = 0;
        let bytes = stripped.as_bytes();
        while end < bytes.len() {
            if bytes[end] == b'\\' {
                end += 2;
                continue;
            }
            if bytes[end] == b'"' {
                break;
            }
            end += 1;
        }
        &stripped[..end]
    } else {
        let end = rest.find([',', '}']).unwrap_or(rest.len());
        &rest[..end]
    }
}

#[test]
fn json_diagnostics_carry_a_span_an_editor_can_use() {
    let (_d, root, cache) = project("json-semantic");
    write(&root.join("prog.py"), "x: int = 1\ny: int = 2.5\n");
    let out = pyrs_in(
        &root,
        &cache,
        &["check", "-i", "prog.py", "--message-format", "json"],
    );
    assert!(!out.status.success());
    let json = String::from_utf8_lossy(&out.stderr);
    let json = json.trim();

    assert!(json.starts_with('{') && json.ends_with('}'), "{json}");
    assert_eq!(json.lines().count(), 1, "line-delimited: {json}");
    assert_eq!(field(json, "level"), "error");
    assert_eq!(field(json, "phase"), "semantic");
    assert_eq!(field(json, "file"), "prog.py");
    assert_eq!(field(json, "line"), "2");
    assert_eq!(field(json, "column"), "10");
    // The pretty rendering travels with it, so a tool need not reimplement
    // the renderer to show what the terminal would have shown.
    assert!(
        field(json, "rendered").contains("--> prog.py:2:10"),
        "{json}"
    );
}

#[test]
fn json_diagnostics_cover_every_phase_that_can_fail() {
    let (_d, root, cache) = project("json-phases");
    for (source, phase) in [
        ("def f(:\n", "parse"),
        ("x: int = 2.5\n", "semantic"),
        ("import nothing_at_all\n", "load"),
    ] {
        write(&root.join("prog.py"), source);
        let out = pyrs_in(
            &root,
            &cache,
            &["check", "-i", "prog.py", "--message-format", "json"],
        );
        assert!(!out.status.success(), "{source} compiled");
        let json = String::from_utf8_lossy(&out.stderr);
        let json = json.trim();
        assert!(json.starts_with('{'), "{phase} was not JSON: {json}");
        assert_eq!(field(json, "phase"), phase, "{json}");
    }
}

#[test]
fn a_failure_with_no_source_position_is_still_json() {
    // A tool's parser must not break on exactly the errors it did not
    // anticipate, so the format is honored even when there is no span.
    let (_d, root, cache) = project("json-positionless");
    let out = pyrs_in(
        &root,
        &cache,
        &["check", "-i", "absent.py", "--message-format", "json"],
    );
    assert!(!out.status.success());
    let json = String::from_utf8_lossy(&out.stderr);
    let json = json.trim();
    assert!(json.starts_with('{') && json.ends_with('}'), "{json}");
    assert!(field(json, "message").contains("absent.py"), "{json}");
}

#[test]
fn run_and_build_honor_the_format_too() {
    let (_d, root, cache) = project("json-commands");
    write(&root.join("prog.py"), "x: int = 2.5\n");
    for command in ["run", "build"] {
        let out = pyrs_in(
            &root,
            &cache,
            &[command, "-i", "prog.py", "--message-format", "json"],
        );
        assert!(!out.status.success(), "{command}");
        let json = String::from_utf8_lossy(&out.stderr);
        assert!(json.trim().starts_with('{'), "{command}: {json}");
    }
}

#[test]
fn the_human_format_is_the_default_and_is_unchanged() {
    let (_d, root, cache) = project("json-default");
    write(&root.join("prog.py"), "x: int = 2.5\n");
    let out = pyrs_in(&root, &cache, &["check", "-i", "prog.py"]);
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.starts_with("error[semantic]"), "{text}");
    assert!(text.contains("^"), "{text}");
    assert!(!text.starts_with('{'), "{text}");
}

// ---------------------------------------------------------------------------
// tree
// ---------------------------------------------------------------------------

/// A project whose graph has a diamond: `main` and `util` both import
/// `shared`, which is what exercises the repeat marker.
fn diamond(root: &Path) {
    write(
        &root.join("pyproject.toml"),
        "[project]\nname = \"app\"\n\n[tool.pyrs]\nentry = \"src/app/main.py\"\nroot = \"src\"\n",
    );
    write(&root.join("src/app/__init__.py"), "");
    write(
        &root.join("src/app/shared.py"),
        "def g() -> int:\n    return 2\n",
    );
    write(
        &root.join("src/app/util.py"),
        "from app import shared\n\n\ndef f() -> int:\n    return shared.g()\n",
    );
    write(
        &root.join("src/app/main.py"),
        "from app import util, shared\n\nprint(util.f() + shared.g())\n",
    );
}

#[test]
fn tree_shows_the_graph_the_compiler_resolved() {
    let (_d, root, cache) = project("tree");
    diamond(&root);
    let out = ok(&root, &cache, &["tree"]);

    assert!(out.starts_with("__main__\n"), "{out}");
    assert!(out.contains("app.util"), "{out}");
    assert!(out.contains("app.shared"), "{out}");
    // A module reached twice is printed once and marked, or a diamond turns
    // into an unreadable expansion. `app.shared` is reached from both
    // `__main__` and `app.util`; the second is a leaf.
    assert_eq!(out.matches("app.shared").count(), 2, "{out}");
    assert!(out.contains("app.shared (*)"), "{out}");
    assert!(out.contains("4 modules"), "{out}");
    // Box drawing that actually nests.
    assert!(out.contains("└──") && out.contains("├──"), "{out}");
    assert!(out.contains("│   "), "the guide column is not drawn: {out}");
}

#[test]
fn tree_can_show_where_each_module_came_from() {
    let (_d, root, cache) = project("tree-paths");
    diamond(&root);
    let out = ok(&root, &cache, &["tree", "--paths"]);
    assert!(out.contains("src/app/shared.py"), "{out}");
    assert!(out.contains("src/app/util.py"), "{out}");
}

#[test]
fn tree_depth_limits_what_is_expanded_not_what_is_counted() {
    let (_d, root, cache) = project("tree-depth");
    diamond(&root);
    let out = ok(&root, &cache, &["tree", "--depth", "1"]);
    // The count is the whole graph regardless: it is what will be compiled.
    assert!(out.contains("4 modules"), "{out}");
    assert!(!out.contains("│   "), "depth 1 expanded a child: {out}");
}

#[test]
fn tree_works_without_a_project() {
    let (_d, root, cache) = project("tree-bare");
    write(&root.join("prog.py"), "print(1)\n");
    let out = ok(&root, &cache, &["tree", "-i", "prog.py"]);
    assert!(out.contains("__main__"), "{out}");
    assert!(out.contains("1 module\n"), "{out}");
}

#[test]
fn tree_reports_a_broken_import_rather_than_a_partial_graph() {
    let (_d, root, cache) = project("tree-broken");
    write(&root.join("prog.py"), "import nothing_at_all\n");
    let message = err(&root, &cache, &["tree", "-i", "prog.py"]);
    assert!(message.contains("nothing_at_all"), "{message}");
}
