//! End-to-end tests: drive the real `pyrs` binary, compile actual programs
//! to native executables, run them, and compare output with what CPython
//! would produce.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

const PYRS: &str = env!("CARGO_BIN_EXE_pyrs");

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pyrs-e2e-{tag}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Compile-and-run `source`, asserting success; returns stdout.
fn run_program(tag: &str, source: &str) -> String {
    let dir = TempDir::new(tag);
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    let out = Command::new(PYRS)
        .args(["run", "-i"])
        .arg(&src)
        .output()
        .expect("failed to spawn pyrs");
    assert!(
        out.status.success(),
        "pyrs run failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Compile-and-run `source`, expecting failure; returns (exit_code, stderr).
fn run_program_expect_fail(tag: &str, source: &str) -> (i32, String) {
    let dir = TempDir::new(tag);
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    let out = Command::new(PYRS)
        .args(["run", "-i"])
        .arg(&src)
        .output()
        .expect("failed to spawn pyrs");
    assert!(!out.status.success(), "expected failure but program succeeded");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn fibonacci() {
    let out = run_program(
        "fib",
        "\
def fib(n: int) -> int:
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)

print(fib(20))
",
    );
    assert_eq!(out, "6765\n");
}

#[test]
fn while_loop_with_break_continue() {
    let out = run_program(
        "loop",
        "\
total = 0
i = 0
while True:
    i += 1
    if i > 10:
        break
    if i % 2 == 0:
        continue
    total += i
print(total)
",
    );
    // 1+3+5+7+9 = 25
    assert_eq!(out, "25\n");
}

#[test]
fn python_floored_division_and_modulo() {
    let out = run_program(
        "floordiv",
        "\
print(-7 // 2)
print(7 // 2)
print(-7 % 3)
print(7 % -3)
print(7.0 // 2.0)
print(-7.5 % 2.0)
",
    );
    // exactly what CPython prints
    assert_eq!(out, "-4\n3\n2\n-2\n3.0\n0.5\n");
}

#[test]
fn float_printing_roundtrips_like_python() {
    let out = run_program(
        "floats",
        "\
print(0.1 + 0.2)
print(7 / 2)
print(1.0)
print(2.5e-2)
print(-0.0)
",
    );
    assert_eq!(out, "0.30000000000000004\n3.5\n1.0\n0.025\n-0.0\n");
}

#[test]
fn bools_comparisons_and_logic() {
    let out = run_program(
        "bools",
        "\
print(True, False)
print(1 < 2, 2 <= 1)
print(not True)
print(True and False or True)
print(1 == 1.0)
",
    );
    assert_eq!(out, "True False\nTrue False\nFalse\nTrue\nTrue\n");
}

#[test]
fn casts_match_python() {
    let out = run_program(
        "casts",
        "\
print(int(2.9))
print(int(-2.9))
print(float(3))
print(bool(0), bool(3))
print(int(True))
",
    );
    assert_eq!(out, "2\n-2\n3.0\nFalse True\n1\n");
}

#[test]
fn print_mixed_arguments_and_strings() {
    let out = run_program(
        "printmix",
        "\
print(\"result:\", 42, 1.5, True)
print()
print(\"escaped\\ttab\")
",
    );
    assert_eq!(out, "result: 42 1.5 True\n\nescaped\ttab\n");
}

#[test]
fn functions_promote_arguments() {
    let out = run_program(
        "promote",
        "\
def halve(x: float) -> float:
    return x / 2

print(halve(7))
",
    );
    assert_eq!(out, "3.5\n");
}

#[test]
fn entry_point_calls_main_when_no_script() {
    let out = run_program(
        "mainentry",
        "\
def main():
    print(\"from main\")
",
    );
    assert_eq!(out, "from main\n");
}

#[test]
fn zero_division_traps_at_runtime() {
    let (code, stderr) = run_program_expect_fail(
        "zerodiv",
        "\
def div(a: int, b: int) -> int:
    return a // b

print(div(1, 0))
",
    );
    assert_eq!(code, 1);
    assert!(stderr.contains("ZeroDivisionError"), "stderr: {stderr}");
}

#[test]
fn type_error_is_reported_with_source_snippet() {
    let dir = TempDir::new("typeerr");
    let src = dir.0.join("prog.py");
    fs::write(&src, "x = 1\nx = 2.5\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .args(["-o"])
        .arg(dir.0.join("prog"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("error[semantic]"), "stderr: {stderr}");
    assert!(stderr.contains("prog.py:2"), "stderr: {stderr}");
    assert!(stderr.contains("^"), "stderr: {stderr}");
}

#[test]
fn syntax_error_is_reported_with_location() {
    let dir = TempDir::new("syntaxerr");
    let src = dir.0.join("prog.py");
    fs::write(&src, "if x\n    pass\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("error[parse]"), "stderr: {stderr}");
}

#[test]
fn compile_produces_standalone_executable() {
    let dir = TempDir::new("standalone");
    let src = dir.0.join("prog.py");
    let exe = dir.0.join("prog");
    fs::write(&src, "print(6 * 7)\n").unwrap();

    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .arg("-o")
        .arg(&exe)
        .args(["--emit-llvm"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "compile failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // the executable runs on its own
    let run = Command::new(&exe).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "42\n");

    // --emit-llvm wrote the IR
    let ll = fs::read_to_string(dir.0.join("prog.ll")).unwrap();
    assert!(ll.contains("define i32 @main()"));
}

#[test]
fn optimization_levels_all_work() {
    for level in ["0", "1", "2", "3"] {
        let dir = TempDir::new(&format!("opt{level}"));
        let src = dir.0.join("prog.py");
        fs::write(
            &src,
            "def f(n: int) -> int:\n    return n * 3\nprint(f(14))\n",
        )
        .unwrap();
        let out = Command::new(PYRS)
            .args(["run", "-O", level, "-i"])
            .arg(&src)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "-O{level} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), "42\n", "-O{level}");
    }
}

#[test]
fn recursion_and_mutual_calls() {
    let out = run_program(
        "mutual",
        "\
def is_even(n: int) -> bool:
    if n == 0:
        return True
    return is_odd(n - 1)

def is_odd(n: int) -> bool:
    if n == 0:
        return False
    return is_even(n - 1)

print(is_even(10), is_odd(7))
",
    );
    assert_eq!(out, "True True\n");
}

#[test]
fn shadowing_libc_names_is_fine() {
    // user symbols are mangled, so a function named `printf` cannot collide
    let out = run_program(
        "libcshadow",
        "\
def printf(x: int) -> int:
    return x + 1

print(printf(41))
",
    );
    assert_eq!(out, "42\n");
}
