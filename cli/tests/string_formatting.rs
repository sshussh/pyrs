//! `"...".format(...)` and `"..." % (...)` on literal format strings.
//!
//! Neither existed: `.format` was not in the str method table and `%` was
//! rejected as an operator on str, so a large amount of ordinary Python could
//! not be compiled at all. f-strings covered new code but are not a rewrite
//! anyone wants to do by hand across a codebase.
//!
//! Both desugar into the `JoinedStr` parts f-strings already produce, so the
//! whole format mini-language — precision, width, alignment, fill, `!r` —
//! comes from the code that already implements it, and nothing new reaches
//! the runtime. That is also why the format string must be a literal: a
//! runtime one would need a runtime parser and a heterogeneous argument list.

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

fn write_prog(tag: &str, source: &str) -> (TempDir, PathBuf) {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-strfmt-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    (dir, src)
}

/// Differential check at every optimization level, comparing stdout and exit
/// status against the CPython oracle.
fn matches_python(tag: &str, source: &str) {
    let (_dir, src) = write_prog(tag, source);
    let expected = Command::new("python3")
        .arg(&src)
        .output()
        .expect("failed to spawn CPython");
    assert!(
        expected.status.success(),
        "CPython failed for {tag}: {}",
        String::from_utf8_lossy(&expected.stderr)
    );
    for opt in ["0", "2", "3"] {
        let actual = Command::new(PYRS)
            .args(["run", "-O", opt, "-i"])
            .arg(&src)
            .output()
            .expect("failed to spawn PyRs");
        assert!(
            actual.status.success(),
            "PyRs {tag} at -O{opt} failed:\n{}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&actual.stdout),
            String::from_utf8_lossy(&expected.stdout),
            "stdout differs for {tag} at -O{opt}"
        );
    }
}

/// Returns the diagnostic for a program that must be rejected.
fn rejects(tag: &str, source: &str) -> String {
    let (_dir, src) = write_prog(tag, source);
    let out = Command::new(PYRS)
        .args(["check", "-i"])
        .arg(&src)
        .output()
        .expect("failed to spawn PyRs");
    assert!(
        !out.status.success(),
        "{tag} was accepted, expected an error"
    );
    String::from_utf8_lossy(&out.stderr).to_string()
}

// ---------------------------------------------------------------------------
// .format()
// ---------------------------------------------------------------------------

#[test]
fn format_fills_fields_in_every_addressing_mode() {
    matches_python(
        "format-modes",
        r#"
print("{} and {}".format(1, 2))
print("{0}-{1}".format("a", "b"))
print("{1}-{0}".format("a", "b"))
print("{0} {0}".format("dup"))
print("{name} is {age}".format(name="x", age=3))
n = 7
print("mixed {} {k}".format(n, k="kw"))
print("no fields".format())
"#,
    );
}

#[test]
fn format_supports_the_spec_mini_language() {
    matches_python(
        "format-spec",
        r#"
print("{:.2f}".format(3.14159))
print("{:>8}|".format("hi"))
print("{:<8}|".format("hi"))
print("{:^8}|".format("hi"))
print("{:05d}".format(42))
print("{:8.3f}|".format(2.5))
print("{:*^7}|".format("ab"))
print("{0:.1f} {1:.1f}".format(1.25, 2.5))
"#,
    );
}

#[test]
fn format_supports_conversions_and_escaped_braces() {
    matches_python(
        "format-conv",
        r#"
print("{!r}".format("q"))
print("{!s}".format(5))
print("{{literal}} {}".format(9))
print("{{}}".format())
print("a{}b{}c".format(1, 2))
"#,
    );
}

#[test]
fn format_works_with_every_scalar_type() {
    matches_python(
        "format-types",
        r#"
print("{} {} {} {}".format(1, 2.5, True, "s"))
print("{}|{}".format(-3, -1.5))
xs = [1, 2]
print("{}".format(len(xs)))
def f(n: int) -> int:
    return n * 2
print("{}".format(f(3)))
"#,
    );
}

// ---------------------------------------------------------------------------
// % formatting
// ---------------------------------------------------------------------------

#[test]
fn percent_formatting_handles_the_common_conversions() {
    matches_python(
        "percent-basic",
        r#"
print("%d-%s" % (3, "a"))
print("%s" % "solo")
print("%d" % 7)
print("%s and %s" % ("x", "y"))
print("%f" % 1.5)
print("%r" % "q")
n = 3
print("n=%d" % n)
"#,
    );
}

#[test]
fn percent_formatting_handles_flags_width_and_precision() {
    matches_python(
        "percent-spec",
        r#"
print("%.2f" % 3.14159)
print("%5d|" % 42)
print("%-5s|" % "ab")
print("%05d" % 42)
print("%8.3f|" % 2.5)
print("%x %o" % (255, 8))
print("%+d %+d" % (5, -5))
"#,
    );
}

#[test]
fn percent_escapes_itself() {
    matches_python(
        "percent-escape",
        r#"
print("100%% done: %d" % 5)
print("%%")
print("%d%%" % 50)
"#,
    );
}

// ---------------------------------------------------------------------------
// Rejections — each names the actual problem
// ---------------------------------------------------------------------------

#[test]
fn a_runtime_format_string_is_rejected_with_the_reason() {
    let err = rejects("runtime-format", "f = \"{}\"\nprint(f.format(1))\n");
    assert!(err.contains("literal format string"), "{err}");
    assert!(err.contains("f-string"), "{err}");
}

#[test]
fn a_runtime_percent_format_string_is_rejected_with_the_reason() {
    let err = rejects("runtime-percent", "f = \"%d\"\nprint(f % 1)\n");
    assert!(err.contains("literal format string"), "{err}");
}

#[test]
fn argument_count_and_name_mismatches_are_caught_at_compile_time() {
    // CPython raises IndexError / KeyError at run time; the compiler can see
    // these before the program starts.
    let err = rejects("too-few", "print(\"{} {}\".format(1))\n");
    assert!(err.contains("needs at least 2 positional"), "{err}");

    let err = rejects("bad-index", "print(\"{5}\".format(1))\n");
    assert!(err.contains("out of range"), "{err}");

    let err = rejects("bad-kw", "print(\"{k}\".format(a=1))\n");
    assert!(err.contains("no keyword argument 'k'"), "{err}");

    let err = rejects("pct-few", "print(\"%d %d\" % (1,))\n");
    assert!(err.contains("not enough arguments"), "{err}");

    let err = rejects("pct-many", "print(\"%d\" % (1, 2))\n");
    assert!(err.contains("not all arguments converted"), "{err}");
}

#[test]
fn malformed_format_strings_are_rejected() {
    let err = rejects("single-brace", "print(\"}\".format())\n");
    assert!(err.contains("single '}'"), "{err}");

    let err = rejects("bad-conv", "print(\"%q\" % 1)\n");
    assert!(err.contains("unsupported format character"), "{err}");

    // A nested field in a spec means an argument in .format() and an
    // expression in an f-string; rejected rather than quietly doing one.
    let err = rejects("nested-spec", "print(\"{:{}}\".format(1, 5))\n");
    assert!(err.contains("nested"), "{err}");
}

// ---------------------------------------------------------------------------
// f-strings unaffected
// ---------------------------------------------------------------------------

#[test]
fn f_strings_still_behave_as_before() {
    matches_python(
        "fstrings",
        r#"
n = 42
pi = 3.14159
s = "hi"
print(f"{n} {pi:.2f} {s!r}")
print(f"{{literal}} {n}")
print(f"[{s:>8}]")
print(f"{n:{'0'}5d}" if False else f"{n:05d}")
"#,
    );
}
