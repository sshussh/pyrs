//! `pyrs test`: compile the project's tests and run them natively.
//!
//! This is the one testing job that is unambiguously PyRs's. pytest under
//! CPython already tests whether your *logic* is right, and it does that
//! better than anything PyRs would build. What it cannot do is tell you
//! whether the **compiled** program agrees with it — and that is exactly the
//! failure mode of a compiler for a Python subset. Running the same
//! assertions through the compiler closes that gap and nothing else does.
//!
//! The runner is a *generated program*, not a runtime feature. PyRs is
//! closed-world and has no reflection, so there is no way to enumerate test
//! functions at run time; instead the driver parses the test modules it
//! found, emits a `__main__` that calls each one inside a `try`, and
//! compiles that like any other program. The generated source stays inside
//! the documented subset, so a test run exercises the compiler on ordinary
//! code rather than on a private back door.

use std::fs;
use std::path::{Path, PathBuf};

use parser::ast;

/// A test module the runner will import.
pub struct TestModule {
    /// Dotted import name, relative to the root it was found under.
    pub name: String,
    /// Zero-parameter top-level `test_*` functions, in source order.
    pub tests: Vec<String>,
}

/// What one test did.
pub struct Outcome {
    pub name: String,
    pub passed: bool,
    pub message: String,
}

/// Whether a file name is a test module, using pytest's conventions so the
/// same files work under both engines.
fn is_test_file(name: &str) -> bool {
    name.ends_with(".py")
        && (name.starts_with("test_") || name.trim_end_matches(".py").ends_with("_test"))
}

/// Directories that never hold importable source.
fn is_skipped_dir(name: &str) -> bool {
    name == "__pycache__"
        || name == "target"
        || name == ".venv"
        || name == "node_modules"
        || name.starts_with('.')
}

/// Every test module under `root`, named as `root` would import it.
pub fn discover(root: &Path) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    walk(root, &mut Vec::new(), &mut found);
    // Deterministic order: a test suite that reports its results in
    // directory-listing order is a test suite whose output diffs randomly.
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

fn walk(dir: &Path, prefix: &mut Vec<String>, found: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if path.is_dir() {
            if is_skipped_dir(&name) || !is_identifier(&name) {
                continue;
            }
            prefix.push(name);
            walk(&path, prefix, found);
            prefix.pop();
        } else if is_test_file(&name) {
            let stem = name.trim_end_matches(".py");
            if !is_identifier(stem) {
                continue;
            }
            let mut parts = prefix.clone();
            parts.push(stem.to_string());
            found.push((parts.join("."), path));
        }
    }
}

/// Whether `name` can appear in an import path. A directory or file that
/// cannot be named in an `import` cannot be run, so skipping it is more
/// honest than emitting source that will not parse.
fn is_identifier(name: &str) -> bool {
    !name.is_empty()
        && !name.chars().next().is_some_and(|c| c.is_ascii_digit())
        && name.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// Top-level `test_*` functions taking no parameters.
///
/// Parameters are excluded rather than rejected: pytest fixtures are the
/// obvious reason a test takes arguments, PyRs cannot supply them, and
/// failing the whole run over a file that pytest handles fine would make
/// `pyrs test` unusable next to it.
pub fn collect_tests(module: &ast::Module) -> Vec<String> {
    module
        .body
        .iter()
        .filter_map(|stmt| match &stmt.kind {
            ast::StmtKind::FuncDef(f)
                if f.name.starts_with("test_")
                    && f.params.is_empty()
                    && f.vararg.is_none()
                    && f.kwarg.is_none()
                    && f.decorators.is_empty() =>
            {
                Some(f.name.clone())
            }
            _ => None,
        })
        .collect()
}

/// The `__main__` that runs the suite.
///
/// Results go to a file rather than to stdout, so a test's own printing
/// stays exactly what the user wrote — no sentinel to collide with, and
/// nothing for the driver to strip back out. Writes are flushed
/// immediately, so a test that crashes the process still leaves every
/// result up to the crash on disk.
pub fn generate(modules: &[TestModule], results: &Path) -> String {
    let mut out = String::new();
    out.push_str("# Generated by `pyrs test`.\n");
    for module in modules {
        out.push_str(&format!("import {}\n", module.name));
    }
    out.push_str(&format!(
        "\n_pyrs_out = open(\"{}\", \"w\")\n",
        escape(&results.to_string_lossy())
    ));

    for module in modules {
        for test in &module.tests {
            let id = format!("{}::{}", module.name, test);
            out.push_str(&format!(
                "\ntry:\n    \
                 {}.{}()\n    \
                 _pyrs_out.write(\"ok\\t{}\\t\\n\")\n\
                 except Exception as _pyrs_e:\n    \
                 _pyrs_out.write(\"fail\\t{}\\t\" + _pyrs_line(str(_pyrs_e)) + \"\\n\")\n",
                module.name,
                test,
                escape(&id),
                escape(&id),
            ));
        }
    }
    out.push_str("\n_pyrs_out.close()\n");

    // The message is user text and can contain newlines, which would split
    // one result into two records.
    let helper = "def _pyrs_line(text: str) -> str:\n    \
                  return text.replace(\"\\n\", \" \").replace(\"\\t\", \" \")\n\n\n";
    let import_end = out.find("\n_pyrs_out = open").unwrap_or(0);
    out.insert_str(import_end + 1, helper);
    out
}

/// Escape a string for a double-quoted Python literal.
fn escape(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

/// Read back what the generated program recorded.
pub fn read_results(path: &Path) -> Vec<Outcome> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let status = parts.next()?;
            let name = parts.next()?;
            let message = parts.next().unwrap_or("");
            Some(Outcome {
                name: name.to_string(),
                passed: status == "ok",
                message: message.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(name: &str, tests: &[&str]) -> TestModule {
        TestModule {
            name: name.to_string(),
            tests: tests.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn test_files_follow_pytest_conventions() {
        for name in ["test_math.py", "math_test.py", "test_.py"] {
            assert!(is_test_file(name), "{name}");
        }
        for name in ["math.py", "testing.py", "test_math.txt", "contest.py"] {
            assert!(!is_test_file(name), "{name}");
        }
    }

    #[test]
    fn only_zero_parameter_test_functions_are_collected() {
        // A fixture-taking test is skipped rather than failing the run:
        // pytest handles those, and refusing the whole file would make
        // `pyrs test` unusable beside it.
        let source = "def test_ok() -> None:\n    pass\n\n\n\
                      def test_fixture(tmp: str) -> None:\n    pass\n\n\n\
                      def helper() -> None:\n    pass\n\n\n\
                      def test_args(*a: int) -> None:\n    pass\n";
        let module = parser::parse(source).expect("parses");
        assert_eq!(collect_tests(&module), vec!["test_ok".to_string()]);
    }

    #[test]
    fn the_generated_program_is_ordinary_python() {
        let generated = generate(
            &[module("pkg.test_a", &["test_one", "test_two"])],
            Path::new("/tmp/results"),
        );
        assert!(generated.contains("import pkg.test_a\n"), "{generated}");
        assert!(generated.contains("pkg.test_a.test_one()"), "{generated}");
        assert!(
            generated.contains("except Exception as _pyrs_e:"),
            "{generated}"
        );
        // The parser is the real check that it stays inside the subset.
        parser::parse(&generated).expect("generated source must parse");
    }

    #[test]
    fn a_hostile_path_cannot_escape_the_generated_literal() {
        let generated = generate(
            &[module("test_a", &["test_one"])],
            Path::new("/tmp/we\"ird\\path"),
        );
        assert!(
            generated.contains(r#"open("/tmp/we\"ird\\path", "w")"#),
            "{generated}"
        );
        parser::parse(&generated).expect("generated source must parse");
    }

    #[test]
    fn results_are_parsed_back_including_an_empty_message() {
        let text = "ok\ttest_a::test_one\t\nfail\ttest_a::test_two\tboom\n";
        let path = std::env::temp_dir().join(format!("pyrs-results-{}", std::process::id()));
        fs::write(&path, text).unwrap();
        let outcomes = read_results(&path);
        let _ = fs::remove_file(&path);

        assert_eq!(outcomes.len(), 2);
        assert!(outcomes[0].passed);
        assert_eq!(outcomes[0].name, "test_a::test_one");
        assert!(!outcomes[1].passed);
        assert_eq!(outcomes[1].message, "boom");
    }

    #[test]
    fn a_missing_results_file_is_no_results_rather_than_a_panic() {
        // What a crashed test run leaves behind.
        assert!(read_results(Path::new("/nonexistent/pyrs/results")).is_empty());
    }

    #[test]
    fn directories_that_cannot_be_imported_are_skipped() {
        for name in ["__pycache__", "target", ".venv", ".git"] {
            assert!(is_skipped_dir(name), "{name}");
        }
        assert!(!is_identifier("my-tests"), "a hyphen cannot be imported");
        assert!(!is_identifier("2fast"));
        assert!(is_identifier("tests"));
    }
}
