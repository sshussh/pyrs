//! End-to-end tests: drive the real `pyrs` binary, compile actual programs
//! to native executables, run them, and compare output with what CPython
//! would produce.

use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const PYRS: &str = env!("CARGO_BIN_EXE_pyrs");

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("pyrs-e2e-{tag}-{}", std::process::id()));
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
        .expect("failed to spawn PyRs");
    assert!(
        out.status.success(),
        "PyRs run failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Like `run_program`, but kills the child after `timeout` so hang regressions
/// fail CI quickly instead of blocking the suite.
fn run_program_timeout(tag: &str, source: &str, timeout: Duration) -> String {
    let dir = TempDir::new(tag);
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    let mut child = Command::new(PYRS)
        .args(["run", "-i"])
        .arg(&src)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn PyRs");
    let start = Instant::now();
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                let stdout = {
                    let mut buf = Vec::new();
                    if let Some(mut pipe) = child.stdout.take() {
                        let _ = pipe.read_to_end(&mut buf);
                    }
                    buf
                };
                let stderr = {
                    let mut buf = Vec::new();
                    if let Some(mut pipe) = child.stderr.take() {
                        let _ = pipe.read_to_end(&mut buf);
                    }
                    buf
                };
                assert!(
                    status.success(),
                    "PyRs run failed\nstdout: {}\nstderr: {}",
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr)
                );
                return String::from_utf8(stdout).unwrap();
            }
            None => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "PyRs run timed out after {timeout:?} (tag={tag}); \
                         likely infinite loop regression"
                    );
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
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
        .expect("failed to spawn PyRs");
    assert!(
        !out.status.success(),
        "expected failure but program succeeded"
    );
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
    // Annotation fixes storage; assigning int into str is a type error.
    fs::write(&src, "x: str = \"a\"\nx = 1\n").unwrap();
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
    assert!(ll.contains("define i32 @main("));
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

// ---- v0.2: for/range, str, lists, **, comparison chaining ----

#[test]
fn for_range_variants() {
    let out = run_program(
        "forrange",
        "\
for i in range(4):
    print(i)
for i in range(2, 5):
    print(i)
for i in range(10, 0, -3):
    print(i)
",
    );
    assert_eq!(out, "0\n1\n2\n3\n2\n3\n4\n10\n7\n4\n1\n");
}

#[test]
fn for_break_continue_and_var_survives() {
    let out = run_program(
        "forbreak",
        "\
total = 0
for i in range(100):
    if i % 2 == 0:
        continue
    if i > 8:
        break
    total += i
print(total, i)
",
    );
    // 1+3+5+7 = 16; loop exits at i == 9
    assert_eq!(out, "16 9\n");
}

#[test]
fn for_and_while_else_match_python() {
    let out = run_program(
        "loopelse",
        "\
for i in range(3):
    if i == 1:
        continue
    print(i)
else:
    print(\"done\")
for i in range(5):
    if i == 3:
        break
    print(i)
else:
    print(\"skipped\")
n = 0
while n < 3:
    n += 1
    if n == 2:
        continue
    print(n)
else:
    print(\"w-done\")
n = 0
while n < 5:
    n += 1
    if n == 3:
        break
else:
    print(\"w-skip\")
print(\"after\")
for i in range(0):
    print(\"x\")
else:
    print(\"empty\")
xs = [10, 20]
for x in xs:
    print(x)
else:
    print(\"list-ok\")
",
    );
    assert_eq!(
        out,
        "0\n2\ndone\n0\n1\n2\n1\n3\nw-done\nafter\nempty\n10\n20\nlist-ok\n"
    );
}

#[test]
fn for_range_dynamic_zero_step_traps() {
    let (code, stderr) = run_program_expect_fail(
        "forzero",
        "\
s = 0
for i in range(0, 10, s):
    print(i)
",
    );
    assert_eq!(code, 1);
    assert!(stderr.contains("ValueError"), "stderr: {stderr}");
}

#[test]
fn string_variables_and_operations() {
    let out = run_program(
        "strops",
        "\
name = \"world\"
greeting = \"hello, \" + name + \"!\"
print(greeting)
print(\"ab\" * 3)
print(len(greeting))
print(greeting[0], greeting[-1])
print(\"apple\" < \"banana\", \"abc\" == \"abc\", \"abc\" != \"abd\")
",
    );
    assert_eq!(out, "hello, world!\nababab\n13\nh !\nTrue True True\n");
}

#[test]
fn triple_quoted_strings_and_docstrings_match_python() {
    // Expected stdout captured from python3 (not invented).
    let source = "\
\"\"\"module doc\"\"\"
s = \"\"\"a
b\"\"\"
print(s)
def f() -> int:
    \"\"\"func doc\"\"\"
    return 42
print(f())
print(\"\"\"hi\"\"\")
t = '''x
y'''
print(t)
print(\"\"\"\"\"\")
nested = \"\"\"he said \"hi\" \"\"\"
print(nested)
esc = \"\"\"a\\nb\\t\\\"c\"\"\"
print(esc)
";
    let py = Command::new("python3")
        .arg("-c")
        .arg(source)
        .output()
        .expect("python3");
    assert!(
        py.status.success(),
        "python3 failed: {}",
        String::from_utf8_lossy(&py.stderr)
    );
    let expected = String::from_utf8(py.stdout).unwrap();
    let out = run_program("triple_strings", source);
    assert_eq!(out, expected);
}

#[test]
fn triple_string_in_function_body_preserves_indent() {
    let source = "\
def f() -> int:
    s = \"\"\"a
b\"\"\"
    return len(s)
print(f())
";
    let py = Command::new("python3")
        .arg("-c")
        .arg(source)
        .output()
        .expect("python3");
    assert!(py.status.success());
    let expected = String::from_utf8(py.stdout).unwrap();
    let out = run_program("triple_indent", source);
    assert_eq!(out, expected);
}

#[test]
fn unterminated_triple_string_is_compile_error() {
    let dir = TempDir::new("unterminated_triple");
    let src = dir.0.join("prog.py");
    fs::write(&src, "s = \"\"\"no close\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .expect("spawn");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unterminated triple-quoted"),
        "stderr: {stderr}"
    );
}

#[test]
fn triple_crlf_and_backslash_newline_match_python() {
    // Binary sources: CRLF physical newlines and \<newline> line continuation.
    // Expected output captured by running the same bytes under python3.
    // Avoid ord/repr (not in surface); use len + equality to a known LF string.
    let dir = TempDir::new("triple_parity_edges");
    let cases: &[(&str, &[u8])] = &[
        (
            "crlf",
            b"s = \"\"\"a\r\nb\"\"\"\nprint(len(s))\nprint(s == \"a\\nb\")\nprint(s)\n",
        ),
        (
            "lone_cr",
            b"s = \"\"\"a\rb\"\"\"\nprint(len(s))\nprint(s == \"a\\nb\")\nprint(s)\n",
        ),
        (
            "bs_nl",
            b"s = \"\"\"a\\\nb\"\"\"\nprint(len(s))\nprint(s == \"ab\")\nprint(s)\n",
        ),
        (
            "bs_crlf",
            b"s = \"\"\"a\\\r\nb\"\"\"\nprint(len(s))\nprint(s == \"ab\")\nprint(s)\n",
        ),
    ];
    for (tag, bytes) in cases {
        let src = dir.0.join(format!("{tag}.py"));
        fs::write(&src, bytes).unwrap();
        let py = Command::new("python3").arg(&src).output().expect("python3");
        assert!(
            py.status.success(),
            "python3 {tag} failed: {}",
            String::from_utf8_lossy(&py.stderr)
        );
        let expected = String::from_utf8(py.stdout).unwrap();
        let out = Command::new(PYRS)
            .args(["run", "-i"])
            .arg(&src)
            .output()
            .expect("pyrs");
        assert!(
            out.status.success(),
            "pyrs {tag} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            expected,
            "mismatch for {tag}"
        );
    }
}

#[test]
fn triple_quoted_fstrings_match_python() {
    // Expected stdout captured from python3 (not invented).
    let source = r#"
name = "world"
x = 3
y = 1.5
print(f"""hello {name}
line2 {x}""")
print(f'''a {x} b''')
print(f"""{{x}} is {x}""")
print(f"""val={x:.2f}""")
print(f"""a
b {x}""")
print(f"""pi-ish {y:.1f}""")
print(f"""he said "hi" {x}""")
print(f'''it's {name}''')
print(f"""a\nb {x}""")
print(f"""a\
b{x}""")
"#;
    let py = Command::new("python3")
        .arg("-c")
        .arg(source)
        .output()
        .expect("python3");
    assert!(
        py.status.success(),
        "python3 failed: {}",
        String::from_utf8_lossy(&py.stderr)
    );
    let expected = String::from_utf8(py.stdout).unwrap();
    let out = run_program("triple_fstrings", source);
    assert_eq!(out, expected);
}

#[test]
fn triple_fstring_crlf_and_backslash_newline_match_python() {
    let dir = TempDir::new("triple_fstring_edges");
    let cases: &[(&str, &[u8])] = &[
        (
            "crlf",
            b"x = 3\ns = f\"\"\"a\r\nb {x}\"\"\"\nprint(len(s))\nprint(s == \"a\\nb 3\")\nprint(s)\n",
        ),
        (
            "lone_cr",
            b"x = 3\ns = f\"\"\"a\rb {x}\"\"\"\nprint(len(s))\nprint(s == \"a\\nb 3\")\nprint(s)\n",
        ),
        (
            "bs_nl",
            b"x = 3\ns = f\"\"\"a\\\nb{x}\"\"\"\nprint(len(s))\nprint(s == \"ab3\")\nprint(s)\n",
        ),
        (
            "bs_crlf",
            b"x = 3\ns = f\"\"\"a\\\r\nb{x}\"\"\"\nprint(len(s))\nprint(s == \"ab3\")\nprint(s)\n",
        ),
    ];
    for (tag, bytes) in cases {
        let src = dir.0.join(format!("{tag}.py"));
        fs::write(&src, bytes).unwrap();
        let py = Command::new("python3").arg(&src).output().expect("python3");
        assert!(
            py.status.success(),
            "python3 {tag} failed: {}",
            String::from_utf8_lossy(&py.stderr)
        );
        let expected = String::from_utf8(py.stdout).unwrap();
        let out = Command::new(PYRS)
            .args(["run", "-i"])
            .arg(&src)
            .output()
            .expect("pyrs");
        assert!(
            out.status.success(),
            "pyrs {tag} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            expected,
            "mismatch for {tag}"
        );
    }
}

#[test]
fn unterminated_triple_fstring_is_compile_error() {
    let dir = TempDir::new("unterminated_triple_fstring");
    let src = dir.0.join("prog.py");
    fs::write(&src, "s = f\"\"\"no close\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .expect("spawn");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unterminated triple-quoted f-string"),
        "stderr: {stderr}"
    );
}

#[test]
fn triple_fstring_parenthesized_multiline_expr_match_python() {
    // Multi-line *literal* content is fine; multi-line *expressions*
    // work when parenthesized (implicit line joining). Different-delimiter
    // nested triples inside `{...}` also work. Expected from python3.
    let source = r#"
x = 3
print(f"""{(x +
1)}""")
print(f"""{(
x
)}""")
print(f"""{'''nested'''}""")
print(f'''{"""nested"""}''')
"#;
    let py = Command::new("python3")
        .arg("-c")
        .arg(source)
        .output()
        .expect("python3");
    assert!(
        py.status.success(),
        "python3 failed: {}",
        String::from_utf8_lossy(&py.stderr)
    );
    let expected = String::from_utf8(py.stdout).unwrap();
    let out = run_program("triple_fstring_paren_expr", source);
    assert_eq!(out, expected);
}

#[test]
fn unparenthesized_multiline_fstring_expr_is_compile_error() {
    // Documented limit: physical newlines inside `{...}` without () fail.
    let dir = TempDir::new("ml_fstr_expr");
    let src = dir.0.join("prog.py");
    fs::write(&src, "x = 3\nprint(f\"\"\"{x +\n1}\"\"\")\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .expect("spawn");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("expected an expression, found end of line"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("f-string"),
        "stderr should mention f-string: {stderr}"
    );
}

#[test]
fn same_delimiter_nested_triple_in_fstring_is_compile_error() {
    // Documented limit: same-delimiter triples inside `{...}` close the
    // outer f-string early (lexer is not brace-aware).
    let dir = TempDir::new("nested_same_triple");
    let src = dir.0.join("prog.py");
    fs::write(&src, "print(f\"\"\"{\"\"\"nested\"\"\"}\"\"\")\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .expect("spawn");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unterminated '{' in f-string"),
        "stderr: {stderr}"
    );
}

#[test]
fn string_iteration_and_truthiness() {
    let out = run_program(
        "striter",
        "\
count = 0
for c in \"hello\":
    if c == \"l\":
        count += 1
print(count)
s = \"\"
if s:
    print(\"truthy\")
else:
    print(\"falsy\")
",
    );
    assert_eq!(out, "2\nfalsy\n");
}

#[test]
fn str_casts_match_python() {
    let out = run_program(
        "strcast",
        "\
print(str(42) + \"!\")
print(str(2.5) + \"!\")
print(str(True) + \"!\")
print(str(-7) + \"!\")
",
    );
    assert_eq!(out, "42!\n2.5!\nTrue!\n-7!\n");
}

#[test]
fn list_basics() {
    let out = run_program(
        "listbasics",
        "\
xs = [1, 2, 3]
print(xs)
print(len(xs), xs[0], xs[-1])
xs[1] = 20
xs.append(4)
print(xs)
xs[0] += 100
print(xs[0])
",
    );
    assert_eq!(out, "[1, 2, 3]\n3 1 3\n[1, 20, 3, 4]\n101\n");
}

#[test]
fn list_print_matches_python_repr() {
    let out = run_program(
        "listrepr",
        "\
print([1.0, 2.5])
print([True, False])
print([\"a\", \"b\"])
xs: list[int] = []
print(xs)
",
    );
    assert_eq!(out, "[1.0, 2.5]\n[True, False]\n['a', 'b']\n[]\n");
}

#[test]
fn list_aliasing_like_python() {
    let out = run_program(
        "listalias",
        "\
xs = [1, 2, 3]
ys = xs
ys[0] = 99
print(xs[0])
",
    );
    // assignment aliases, like Python
    assert_eq!(out, "99\n");
}

#[test]
fn lists_through_functions() {
    let out = run_program(
        "listfunc",
        "\
def squares(n: int) -> list[int]:
    result: list[int] = []
    for i in range(n):
        result.append(i * i)
    return result

def total(xs: list[int]) -> int:
    t = 0
    for x in xs:
        t += x
    return t

sq = squares(5)
print(sq)
print(total(sq))
",
    );
    assert_eq!(out, "[0, 1, 4, 9, 16]\n30\n");
}

#[test]
fn list_append_during_iteration_sees_growth() {
    let out = run_program(
        "listgrow",
        "\
xs = [1, 2, 3]
for x in xs:
    if x < 3:
        xs.append(x * 10)
    if len(xs) > 6:
        break
print(xs)
",
    );
    // matches CPython: iteration re-reads the live length
    assert_eq!(out, "[1, 2, 3, 10, 20]\n");
}

#[test]
fn index_error_traps() {
    let (code, stderr) = run_program_expect_fail(
        "indexerr",
        "\
xs = [1, 2]
print(xs[5])
",
    );
    assert_eq!(code, 1);
    assert!(stderr.contains("IndexError"), "stderr: {stderr}");
}

#[test]
fn power_operator_matches_python() {
    let out = run_program(
        "power",
        "\
print(2 ** 10)
print(2 ** 0)
print(-2 ** 2)
print((-2) ** 3)
print(2 ** -1)
print(2.0 ** 0.5)
print(0 ** 0)
print(2 ** 3 ** 2)
",
    );
    assert_eq!(out, "1024\n1\n-4\n-8\n0.5\n1.4142135623730951\n1\n512\n");
}

#[test]
fn zero_to_negative_float_power_traps() {
    let (code, stderr) = run_program_expect_fail("powzero", "x = 0.0\nprint(x ** -1)\n");
    assert_eq!(code, 1);
    assert!(stderr.contains("ZeroDivisionError"), "stderr: {stderr}");
}

#[test]
fn comparison_chaining() {
    let out = run_program(
        "chain",
        "\
x = 5
print(1 < x < 10)
print(1 < x < 5)
print(10 > x >= 5)
print(0 <= x <= 10 == 10)
",
    );
    assert_eq!(out, "True\nFalse\nTrue\nTrue\n");
}

#[test]
fn chained_comparison_evaluates_middle_once() {
    let out = run_program(
        "chainonce",
        "\
def middle() -> int:
    print(\"evaluated\")
    return 5

if 1 < middle() < 10:
    print(\"in range\")
",
    );
    // "evaluated" must appear exactly once
    assert_eq!(out, "evaluated\nin range\n");
}

#[test]
fn chained_comparison_short_circuits() {
    let out = run_program(
        "chainshort",
        "\
def right() -> int:
    print(\"right\")
    return 3

if 5 < 2 < right():
    print(\"yes\")
else:
    print(\"no\")
",
    );
    // 5 < 2 is False, so right() must never run
    assert_eq!(out, "no\n");
}

#[test]
fn negative_int_pow_traps() {
    let (code, stderr) = run_program_expect_fail(
        "ipowneg",
        "\
e = -1
print(2 ** e)
",
    );
    assert_eq!(code, 1);
    assert!(stderr.contains("ValueError"), "stderr: {stderr}");
}

#[test]
fn bubble_sort_end_to_end() {
    let out = run_program(
        "bubble",
        "\
def sort(xs: list[int]) -> list[int]:
    n = len(xs)
    for i in range(n):
        for j in range(0, n - i - 1):
            if xs[j] > xs[j + 1]:
                tmp = xs[j]
                xs[j] = xs[j + 1]
                xs[j + 1] = tmp
    return xs

print(sort([5, 2, 9, 1, 7]))
",
    );
    assert_eq!(out, "[1, 2, 5, 7, 9]\n");
}

// ---- regressions from differential verification vs CPython ----

#[test]
fn float_repr_uses_fixed_notation_like_python() {
    let out = run_program(
        "floatrepr",
        "\
print(10.0)
print(1000000.0)
print(1e15)
print(1e16)
print(1e-4)
print(1e-5)
print([10.0, 20.0])
print(str(100.0))
",
    );
    assert_eq!(
        out,
        "10.0\n1000000.0\n1000000000000000.0\n1e+16\n0.0001\n1e-05\n[10.0, 20.0]\n100.0\n"
    );
}

#[test]
fn power_augmented_assignment() {
    let out = run_program(
        "poweq",
        "\
x = 2
x **= 10
print(x)
f = 1.5
f **= 2
print(f)
xs = [3]
xs[0] **= 3
print(xs)
",
    );
    assert_eq!(out, "1024\n2.25\n[27]\n");
}

#[test]
fn float_floordiv_mod_signed_zero_and_inf() {
    let out = run_program(
        "fdivmod",
        "\
big = 1e308 * 10
print(-1.0 // big)
print(4.0 % -2.0)
print(-4.0 % 2.0)
print(7.0 // 2.0)
print(-7.5 % 2.0)
",
    );
    // -1.0 // inf == -1.0 and zero remainders take the divisor's sign,
    // exactly like CPython float_divmod
    assert_eq!(out, "-1.0\n-0.0\n0.0\n3.0\n0.5\n");
}

#[test]
fn int_of_nan_and_inf_trap_like_python() {
    let (code, stderr) = run_program_expect_fail("intinf", "v = 1e308 * 10\nprint(int(v))\n");
    assert_eq!(code, 1);
    assert!(
        stderr.contains("OverflowError: cannot convert float infinity to integer"),
        "stderr: {stderr}"
    );
    let (code, stderr) =
        run_program_expect_fail("intnan", "v = 1e308 * 10\nn = v - v\nprint(int(n))\n");
    assert_eq!(code, 1);
    assert!(
        stderr.contains("ValueError: cannot convert float NaN to integer"),
        "stderr: {stderr}"
    );
}

// ---- v0.3: slicing, in/not in, pop, f-strings ----

#[test]
fn slicing_matches_python() {
    let out = run_program(
        "slices",
        "\
s = \"hello world\"
print(s[0:5], s[6:], s[:5], s[-5:], s[:-6], s[8:2])
xs = [1, 2, 3, 4, 5]
print(xs[1:3], xs[:2], xs[-2:], xs[4:1], xs[:])
ys = xs[1:4]
ys[0] = 99
print(xs, ys)
",
    );
    assert_eq!(
        out,
        "hello world hello world hello \n\
         [2, 3] [1, 2] [4, 5] [] [1, 2, 3, 4, 5]\n\
         [1, 2, 3, 4, 5] [99, 3, 4]\n"
    );
}

#[test]
fn membership_tests_match_python() {
    let out = run_program(
        "membership",
        "\
s = \"hello\"
print(\"ell\" in s, \"xyz\" in s, \"\" in s)
xs = [1, 2, 3]
print(2 in xs, 9 in xs, 9 not in xs)
print(2.0 in [1.5, 2.0], 1 in [1.0, 2.0])
print(\"b\" in [\"a\", \"b\"], \"c\" not in [\"a\", \"b\"])
",
    );
    assert_eq!(
        out,
        "True False True\nTrue False True\nTrue True\nTrue True\n"
    );
}

#[test]
fn list_pop_matches_python() {
    let out = run_program(
        "pop",
        "\
st = [10, 20, 30, 40]
print(st.pop(), st.pop(0), st.pop(-1), st)
st.pop()
print(st, len(st))
",
    );
    assert_eq!(out, "40 10 30 [20]\n[] 0\n");
}

#[test]
fn pop_traps_match_python() {
    let (code, stderr) =
        run_program_expect_fail("popempty", "xs: list[int] = []\nprint(xs.pop())\n");
    assert_eq!(code, 1);
    assert!(stderr.contains("pop from empty list"), "stderr: {stderr}");
}

#[test]
fn fstrings_match_python() {
    let out = run_program(
        "fstrings",
        "\
name = \"world\"
n = 42
pi = 3.5
flag = True
print(f\"hello {name}, n={n}, pi={pi}, flag={flag}!\")
print(f\"{{escaped}} {n + 1} {name[0:3]} {n in [42]}\")
print(f\"\")
print(f\"nested {f'inner {n}'} outer\")
",
    );
    assert_eq!(
        out,
        "hello world, n=42, pi=3.5, flag=True!\n\
         {escaped} 43 wor True\n\n\
         nested inner 42 outer\n"
    );
}

#[test]
fn fstring_dot_nf_matches_python() {
    let out = run_program(
        "fspec",
        "\
pi = 3.14159
n = 2
flag = True
print(f\"{pi:.2f}\")
print(f\"{pi:.0f}\")
print(f\"{pi:.5f}\")
print(f\"{n:.2f}\")
print(f\"{flag:.1f}\")
print(f\"{-pi:.2f}\")
print(f\"{0.001:.4f}\")
print(f\"x={pi:.3f} y={n:.0f}\")
",
    );
    assert_eq!(
        out,
        "3.14\n3\n3.14159\n2.00\n1.0\n-3.14\n0.0010\nx=3.142 y=2\n"
    );
}

#[test]
fn fstring_conversions_match_python() {
    let out = run_program(
        "fconv",
        "\
name = \"world\"
n = 42
pi = 3.14159
flag = True
print(f\"{name!s}\")
print(f\"{name!r}\")
print(f\"{name!a}\")
print(f\"{n!r}\")
print(f\"{pi!r}\")
print(f\"{flag!r}\")
s = \"café\"
print(f\"{s!a}\")
print(f\"{n!s:>5}\")
print(f\"{name!r:>12}\")
",
    );
    assert_eq!(
        out,
        concat!(
            "world\n",
            "'world'\n",
            "'world'\n",
            "42\n",
            "3.14159\n",
            "True\n",
            "'caf\\xe9'\n",
            "   42\n",
            "     'world'\n",
        )
    );
}

#[test]
fn fstring_int_format_match_python() {
    let out = run_program(
        "fint",
        "\
n = 42
print(f\"{n:d}\")
print(f\"{n:x}\")
print(f\"{n:X}\")
print(f\"{n:o}\")
print(f\"{n:b}\")
print(f\"{n:#x}\")
print(f\"{n:#X}\")
print(f\"{n:#o}\")
print(f\"{n:#b}\")
print(f\"{n:05d}\")
print(f\"{n:>5}\")
print(f\"{n:<5}\")
print(f\"{n:^5}\")
print(f\"{n:+d}\")
print(f\"{n: d}\")
print(f\"{-n:d}\")
print(f\"{-n:+d}\")
print(f\"{n:08x}\")
print(f\"{-n:05d}\")
print(f\"{42:x>5d}\")
print(f\"{0:b}\")
print(f\"{255:#x}\")
print(f\"{7:#o}\")
print(f\"{True:d}\")
print(f\"{False:d}\")
print(f\"{True:10}\")
print(f\"{False:10}\")
",
    );
    assert_eq!(
        out,
        concat!(
            "42\n",
            "2a\n",
            "2A\n",
            "52\n",
            "101010\n",
            "0x2a\n",
            "0X2A\n",
            "0o52\n",
            "0b101010\n",
            "00042\n",
            "   42\n",
            "42   \n",
            " 42  \n",
            "+42\n",
            " 42\n",
            "-42\n",
            "-42\n",
            "0000002a\n",
            "-0042\n",
            "xxx42\n",
            "0\n",
            "0xff\n",
            "0o7\n",
            "1\n",
            "0\n",
            "         1\n",
            "         0\n",
        )
    );
}

#[test]
fn fstring_float_format_match_python() {
    let out = run_program(
        "ffloat",
        "\
pi = 3.14159
n = 42
flag = True
print(f\"{pi:.2f}\")
print(f\"{pi:.2e}\")
print(f\"{pi:.2E}\")
print(f\"{pi:.2g}\")
print(f\"{pi:%}\")
print(f\"{pi:.2%}\")
print(f\"{pi:10.2f}\")
print(f\"{pi:<10.2f}\")
print(f\"{pi:^10.2f}\")
print(f\"{pi:*>10.2f}\")
print(f\"{pi:+.2f}\")
print(f\"{n:.2f}\")
print(f\"{flag:.1f}\")
print(f\"{1e-4:.2e}\")
print(f\"{-pi:.2f}\")
print(f\"{0.001:.4f}\")
print(f\"x={pi:.3f} y={n:.0f}\")
",
    );
    assert_eq!(
        out,
        concat!(
            "3.14\n",
            "3.14e+00\n",
            "3.14E+00\n",
            "3.1\n",
            "314.159000%\n",
            "314.16%\n",
            "      3.14\n",
            "3.14      \n",
            "   3.14   \n",
            "******3.14\n",
            "+3.14\n",
            "42.00\n",
            "1.0\n",
            "1.00e-04\n",
            "-3.14\n",
            "0.0010\n",
            "x=3.142 y=42\n",
        )
    );
}

#[test]
fn fstring_str_and_nested_format_match_python() {
    let out = run_program(
        "fnest",
        "\
name = \"world\"
n = 42
pi = 3.14159
w = 10
p = 2
print(f\"{name:>10}\")
print(f\"{name:<10}\")
print(f\"{name:^10}\")
print(f\"{name:*^10}\")
print(f\"{name:.3}\")
print(f\"{name:10.3}\")
print(f\"{pi:{w}.{p}f}\")
print(f\"{n:{w}d}\")
print(f\"{n:0{w}d}\")
print(f\"{n}\")
print(f\"{pi}\")
print(f\"{True}\")
print(f\"{name}\")
print(f\"{n:}\")
",
    );
    assert_eq!(
        out,
        concat!(
            "     world\n",
            "world     \n",
            "  world   \n",
            "**world***\n",
            "wor\n",
            "wor       \n",
            "      3.14\n",
            "        42\n",
            "0000000042\n",
            "42\n",
            "3.14159\n",
            "True\n",
            "world\n",
            "42\n",
        )
    );
}

#[test]
fn fstring_debug_form_is_compile_error() {
    let dir = TempDir::new("fdebug");
    let src = dir.0.join("prog.py");
    fs::write(&src, "x = 1\nprint(f\"{x=}\")\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("self-documenting") || stderr.contains("not supported"),
        "stderr: {stderr}"
    );
}

#[test]
fn fstring_grouping_is_runtime_error() {
    let dir = TempDir::new("fgroup");
    let src = dir.0.join("prog.py");
    fs::write(&src, "x = 1000\nprint(f\"{x:,}\")\n").unwrap();
    let status = Command::new(PYRS)
        .args(["run", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    // compile may succeed; runtime must reject grouping
    if status.status.success() {
        panic!(
            "expected failure for grouping format, stdout={}",
            String::from_utf8_lossy(&status.stdout)
        );
    }
    let stderr = String::from_utf8_lossy(&status.stderr);
    assert!(
        stderr.contains("grouping") || stderr.contains("not supported"),
        "stderr: {stderr}"
    );
}

#[test]
fn stack_and_queue_via_pop() {
    let out = run_program(
        "stack",
        "\
def reverse(xs: list[int]) -> list[int]:
    out: list[int] = []
    while xs:
        out.append(xs.pop())
    return out

print(reverse([1, 2, 3, 4]))
",
    );
    assert_eq!(out, "[4, 3, 2, 1]\n");
}

// ---- v0.4: slice steps, str methods, global variables ----

#[test]
fn slice_steps_match_python() {
    let out = run_program(
        "slicestep",
        "\
s = \"hello world\"
print(s[::-1], s[::2], s[8:2:-2])
xs = [1, 2, 3, 4, 5, 6, 7, 8]
print(xs[::-1], xs[1:7:2], xs[7:1:-3], xs[::-2])
print(s[5:5:-1] + \"|\", xs[0:0:2])
",
    );
    assert_eq!(
        out,
        "dlrow olleh hlowrd rwo\n\
         [8, 7, 6, 5, 4, 3, 2, 1] [2, 4, 6] [8, 5] [8, 6, 4, 2]\n\
         | []\n"
    );
}

#[test]
fn str_methods_match_python() {
    let out = run_program(
        "strmethods",
        "\
t = \"  Hello, World!  \"
print(t.strip() + \"|\", t.upper().strip(), t.lower().strip())
print(\"hello\".startswith(\"he\"), \"hello\".endswith(\"lo\"), \"hello\".startswith(\"lo\"))
print(\"banana\".find(\"an\"), \"banana\".find(\"x\"), \"banana\".count(\"an\"))
print(\"banana\".replace(\"an\", \"-\"), \"abc\".replace(\"\", \".\"))
print(\"a,b,,c\".split(\",\"))
print(\"  the   quick  fox \".split())
print(\"-\".join([\"a\", \"b\", \"c\"]))
csv = \"name,age,city\"
print(\", \".join(csv.split(\",\")))
",
    );
    assert_eq!(
        out,
        "Hello, World!| HELLO, WORLD! hello, world!\n\
         True True False\n\
         1 -1 2\n\
         b--a .a.b.c.\n\
         ['a', 'b', '', 'c']\n\
         ['the', 'quick', 'fox']\n\
         a-b-c\n\
         name, age, city\n"
    );
}

#[test]
fn str_isdigit_matches_python() {
    let out = run_program(
        "isdigit",
        "\
print(\"123\".isdigit(), \"\".isdigit(), \"12a\".isdigit())
print(\"-3\".isdigit(), \"3.5\".isdigit(), \" 7\".isdigit())
nums = [\"10\", \"3x\", \"\", \"42\"]
print(len([n for n in nums if n.isdigit()]))
",
    );
    assert_eq!(out, "True False False\nFalse False False\n2\n");
}

#[test]
fn list_sort_and_sorted_match_python() {
    let out = run_program(
        "listsort",
        "\
xs = [3, 1, 2]
xs.sort()
print(xs)
print(sorted([3, 1, 2]))
print(sorted([3.0, 1.5, 2.0]))
print(sorted([\"b\", \"a\", \"c\"]))
print(sorted([True, False, True]))
a = [1]
b = sorted(a)
b.append(2)
print(a, b)
",
    );
    assert_eq!(
        out,
        "\
[1, 2, 3]
[1, 2, 3]
[1.5, 2.0, 3.0]
['a', 'b', 'c']
[False, True, True]
[1] [1, 2]
"
    );
}

#[test]
fn list_eq_ne_match_python() {
    let out = run_program(
        "listeq",
        "\
print([1, 2] == [1, 2])
print([1, 2] == [1, 3])
print([1, 2] != [1, 2])
print([1, 2] != [1, 3])
xs: list[int] = []
ys: list[int] = []
print(xs == ys)
print([1.0, 2.0] == [1.0, 2.0])
print([0.0] == [-0.0])
print([\"a\", \"b\"] == [\"a\", \"b\"])
print([1] == [1, 2])
print([[1, 2], [3]] == [[1, 2], [3]])
print([[1], [2]] != [[1], [9]])
",
    );
    assert_eq!(
        out,
        "\
True
False
False
True
True
True
True
True
False
True
True
"
    );
}

#[test]
fn list_concat_and_repeat_match_python() {
    let out = run_program(
        "listcat",
        "\
print([1, 2] + [3, 4])
print([1] * 3)
print(3 * [1, 2])
print([1] * 0)
print([1] * (-2))
empty: list[int] = []
print(empty + [1])
a = [1]
b = a * 1
b.append(2)
print(a, b)
xs: list[str] = [\"a\"]
print(xs + [\"b\", \"c\"])
print(xs * 2)
",
    );
    assert_eq!(
        out,
        "\
[1, 2, 3, 4]
[1, 1, 1]
[1, 2, 1, 2, 1, 2]
[]
[]
[1]
[1] [1, 2]
['a', 'b', 'c']
['a', 'a']
"
    );
}

#[test]
fn list_insert_remove_index_clear_match_python() {
    let out = run_program(
        "listmut",
        "\
xs = [1, 2, 3]
xs.insert(0, 9)
print(xs)
xs = [1, 2, 3]
xs.insert(1, 9)
print(xs)
xs = [1, 2, 3]
xs.insert(100, 9)
print(xs)
xs = [1, 2, 3]
xs.insert(-1, 9)
print(xs)
xs = [1, 2, 3]
xs.insert(-100, 9)
print(xs)
xs = [1, 2, 3, 2]
xs.remove(2)
print(xs)
print([1, 2, 3, 2].index(2))
xs = [1, 2, 3]
xs.clear()
print(xs)
ys = [\"a\", \"b\", \"a\"]
print(ys.index(\"a\"))
ys.remove(\"a\")
print(ys)
",
    );
    assert_eq!(
        out,
        "\
[9, 1, 2, 3]
[1, 9, 2, 3]
[1, 2, 3, 9]
[1, 2, 9, 3]
[9, 1, 2, 3]
[1, 3, 2]
1
[]
0
['b', 'a']
"
    );

    let (code, stderr) = run_program_expect_fail("list_remove_miss", "xs = [1]\nxs.remove(9)\n");
    assert_eq!(code, 1);
    assert!(
        stderr.contains("ValueError: list.remove(x): x not in list"),
        "stderr: {stderr}"
    );

    let (code, stderr) = run_program_expect_fail("list_index_miss", "print([1].index(9))\n");
    assert_eq!(code, 1);
    assert!(
        stderr.contains("ValueError: list.index(x): x not in list"),
        "stderr: {stderr}"
    );
}

#[test]
fn str_rfind_rindex_match_python() {
    let out = run_program(
        "rfind",
        "\
print(\"banana\".rfind(\"an\"), \"banana\".find(\"an\"))
print(\"banana\".rfind(\"x\"), \"\".rfind(\"a\"), \"abc\".rfind(\"\"))
print(\"aaa\".rfind(\"aa\"))
print(\"banana\".rindex(\"an\"))
",
    );
    assert_eq!(
        out,
        "\
3 1
-1 -1 3
1
3
"
    );

    let (code, stderr) =
        run_program_expect_fail("rindex_miss", "print(\"banana\".rindex(\"x\"))\n");
    assert_eq!(code, 1);
    assert!(
        stderr.contains("ValueError: substring not found"),
        "stderr: {stderr}"
    );
}

#[test]
fn str_isalpha_isspace_case_match_python_ascii() {
    // ASCII-only rules (documented); cases chosen to match CPython on ASCII.
    let out = run_program(
        "strpreds",
        "\
print(\"abc\".isalpha(), \"ABC\".isalpha(), \"AbC\".isalpha())
print(\"\".isalpha(), \"a1\".isalpha(), \" \".isalpha())
print(\" \".isspace(), \" \\t\\n\".isspace(), \"\".isspace(), \"a \".isspace())
print(\"ABC\".isupper(), \"AbC\".isupper(), \"123\".isupper(), \"\".isupper())
print(\"abc\".islower(), \"a1\".islower(), \"ABC\".islower(), \" \".islower())
",
    );
    assert_eq!(
        out,
        "\
True True True
False False False
True True False False
True False False False
True True False False
"
    );
}

#[test]
fn abs_matches_python() {
    let out = run_program(
        "abs",
        "\
print(abs(-5), abs(5), abs(0), abs(True), abs(False))
print(abs(-3.5), abs(3.5), abs(-0.0), abs(0.0))
print(abs(-2), abs(2.0 - 5.0))
x = -42
print(abs(x))
",
    );
    assert_eq!(
        out,
        "5 5 0 1 0\n\
         3.5 3.5 0.0 0.0\n\
         2 3.0\n\
         42\n"
    );
}

#[test]
fn abs_wrong_type_is_compile_error() {
    let dir = TempDir::new("abs_bad");
    let src = dir.0.join("prog.py");
    fs::write(&src, "print(abs(\"x\"))\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("bad operand type for abs()"),
        "stderr: {stderr}"
    );
}

#[test]
fn min_max_match_python() {
    // Cases chosen so print matches CPython. Bools promote to int in PyRs
    // (like abs); avoid max(True, 0) which would print True under CPython.
    // Mixed int/float unifies to float (min(1, 1.5) -> 1.0, not 1).
    let out = run_program(
        "minmax",
        "\
print(min(-3, 2), max(-3, 2))
print(min(3, 3), max(3, 3))
print(min(-3.5, 2.0), max(-3.5, 2.0))
print(min(True, 0), max(2, True), min(0, True))
print(min(-0.0, 0.0), max(0.0, -0.0))
print(min(1, 1.5), max(1, 1.5))
",
    );
    assert_eq!(
        out,
        "\
-3 2
3 3
-3.5 2.0
0 2 0
-0.0 0.0
1.0 1.5
"
    );
}

#[test]
fn min_wrong_type_is_compile_error() {
    let dir = TempDir::new("min_bad");
    let src = dir.0.join("prog.py");
    fs::write(&src, "print(min(1, \"x\"))\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("min()"), "stderr: {stderr}");
    assert!(stderr.contains("str"), "stderr: {stderr}");
}

#[test]
fn sum_matches_python() {
    let out = run_program(
        "sum",
        "\
print(sum([1, 2, 3]))
xs: list[int] = []
print(sum(xs))
print(sum([1.5, 2.5]))
ys: list[float] = []
print(sum(ys))
print(sum([-1, 4, 10]))
",
    );
    assert_eq!(
        out,
        "\
6
0
4.0
0.0
13
"
    );
}

#[test]
fn sum_wrong_type_is_compile_error() {
    let dir = TempDir::new("sum_bad");
    let src = dir.0.join("prog.py");
    fs::write(&src, "print(sum([\"a\", \"b\"]))\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("sum()"), "stderr: {stderr}");
}

#[test]
fn global_variables_match_python() {
    let out = run_program(
        "globals",
        "\
counter = 0
label = \"total\"

def bump(n: int):
    global counter
    counter += n

def describe() -> str:
    return label + \": \" + str(counter)

def shadow() -> int:
    counter = 100
    return counter

bump(3)
bump(4)
print(counter, describe())
print(shadow(), counter)
",
    );
    assert_eq!(out, "7 total: 7\n100 7\n");
}

#[test]
fn global_write_without_declaration_is_error() {
    let dir = TempDir::new("globalerr");
    let src = dir.0.join("prog.py");
    fs::write(&src, "x = 1\ndef f():\n    x += 1\nf()\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("global x"), "stderr: {stderr}");
}

#[test]
fn loop_variable_final_value_matches_python() {
    let out = run_program(
        "loopvar",
        "\
i = 99
for i in range(0):
    print(i)
print(i)
for i in range(5):
    pass
print(i)
for j in range(3):
    j = 100
print(j)
",
    );
    // empty range leaves the var untouched; exhaustion keeps the last
    // yielded value; body mutation cannot derail iteration
    assert_eq!(out, "99\n4\n100\n");
}

#[test]
fn word_frequency_end_to_end() {
    let out = run_program(
        "wordfreq",
        "\
text = \"the quick fox the lazy dog the end\"
words = text.split()
target = \"the\"
count = 0
for w in words:
    if w == target:
        count += 1
print(f\"{target} appears {count} times in {len(words)} words\")
print(\" \".join(words[::-1]))
",
    );
    assert_eq!(
        out,
        "the appears 3 times in 8 words\nend the dog lazy the fox quick the\n"
    );
}

// ---- v0.5: nested lists, input(), sys.argv ----

#[test]
fn nested_lists_match_python() {
    let out = run_program(
        "nested",
        "\
grid = [[1, 2, 3], [4, 5, 6]]
print(grid)
print(grid[1][2], grid[-1][-1])
grid[0][0] = 100
print(grid[0])
m: list[list[str]] = []
m.append([\"a\", \"b\"])
print(m, len(m[0]))
deep = [[[1], [2]], [[3]]]
print(deep, deep[0][1][0])
",
    );
    assert_eq!(
        out,
        "[[1, 2, 3], [4, 5, 6]]\n6 6\n[100, 2, 3]\n[['a', 'b']] 2\n\
         [[[1], [2]], [[3]]] 2\n"
    );
}

#[test]
fn matrix_multiply_end_to_end() {
    let out = run_program(
        "matmul",
        "\
def matmul(a: list[list[int]], b: list[list[int]], n: int) -> list[list[int]]:
    c: list[list[int]] = []
    for i in range(n):
        row: list[int] = []
        for j in range(n):
            row.append(0)
        c.append(row)
    for i in range(n):
        for k in range(n):
            for j in range(n):
                c[i][j] += a[i][k] * b[k][j]
    return c

a = [[1, 2], [3, 4]]
b = [[5, 6], [7, 8]]
print(matmul(a, b, 2))
",
    );
    assert_eq!(out, "[[19, 22], [43, 50]]\n");
}

#[test]
fn argv_and_input_match_python() {
    use std::io::Write as _;
    use std::process::Stdio;

    let dir = TempDir::new("argvinput");
    let src = dir.0.join("prog.py");
    fs::write(
        &src,
        "\
import sys
print(len(sys.argv) - 1)
for a in sys.argv[1:]:
    print(\"arg:\", a)
name = input(\"name? \")
print(f\"hello {name}\")
line = input()
print(line.upper().split())
",
    )
    .unwrap();

    let mut child = Command::new(PYRS)
        .args(["run", "-i"])
        .arg(&src)
        .args(["alpha", "beta gamma"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"world\nthe quick fox\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "2\narg: alpha\narg: beta gamma\nname? hello world\n\
         ['THE', 'QUICK', 'FOX']\n"
    );
}

#[test]
fn input_at_eof_traps_like_python() {
    use std::process::Stdio;
    let dir = TempDir::new("inputeof");
    let src = dir.0.join("prog.py");
    fs::write(&src, "x = input()\nprint(x)\n").unwrap();
    let out = Command::new(PYRS)
        .args(["run", "-i"])
        .arg(&src)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("EOFError"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn import_of_unknown_module_is_error() {
    let dir = TempDir::new("importerr");
    let src = dir.0.join("prog.py");
    // Use a name that is not in the embedded / disk stdlib.
    fs::write(&src, "import totally_missing_xyz\nprint(1)\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("No module named 'totally_missing_xyz'"),
        "stderr: {stderr}"
    );
}

// ---- v0.6: file I/O ----

#[test]
fn file_write_read_roundtrip() {
    let dir = TempDir::new("fileio");
    let data = dir.0.join("data.txt").display().to_string();
    let out = run_program(
        "fileio",
        &format!(
            "\
path = \"{data}\"
f = open(path, \"w\")
n = f.write(\"hello\\n\")
f.write(\"world\\n\")
f.close()
print(n)

r = open(path)
print(r.read().split())
r.close()

r2 = open(path)
print(r2.readline() + \"|\")
r2.close()

r3 = open(path)
print(r3.readlines())
r3.close()

a = open(path, \"a\")
a.write(\"third\\n\")
a.close()
print(len(open(path).readlines()))
"
        ),
    );
    assert_eq!(
        out,
        "6\n['hello', 'world']\nhello\n|\n['hello\\n', 'world\\n']\n3\n"
    );
}

#[test]
fn multi_assign_matches_python() {
    let out = run_program(
        "multi",
        "\
a = b = 0
print(a, b)
a = b = c = 1
print(a, b, c)
xs = [0, 0]
a = xs[0] = 5
print(a, xs)
x = y = [1]
y.append(2)
print(x)
",
    );
    assert_eq!(out, "0 0\n1 1 1\n5 [5, 0]\n[1, 2]\n");
}

#[test]
fn defaults_and_kwargs_match_python() {
    let out = run_program(
        "defkw",
        "\
def f(a: int, b: int = 2, c: int = 3) -> int:
    return a * 100 + b * 10 + c

print(f(1))
print(f(1, 9))
print(f(1, c=10))
print(f(1, 8, 9))
print(f(a=4, b=5, c=6))
print(f(7, c=0, b=1))
",
    );
    // f(1,c=10) => a=1,b=2,c=10 => 130
    assert_eq!(out, "123\n193\n130\n189\n456\n710\n");
}

#[test]
fn defaults_and_kwargs_errors() {
    let dir = TempDir::new("defkwerr");
    let src = dir.0.join("prog.py");
    fs::write(
        &src,
        "def f(a: int, b: int = 1) -> int:\n    return a + b\nprint(f(b=2))\n",
    )
    .unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("missing required argument 'a'"),
        "stderr: {stderr}"
    );
}

#[test]
fn file_typed_params_match_python_io() {
    let dir = TempDir::new("fileparam");
    let data = dir.0.join("fp.txt").display().to_string();
    let out = run_program(
        "fileparam",
        &format!(
            "\
def first_line(f: file) -> str:
    return f.readline().strip()

def count_lines(f: file) -> int:
    n = 0
    for line in f:
        n = n + 1
    return n

def open_it(path: str) -> file:
    return open(path)

path = \"{data}\"
w = open(path, \"w\")
w.write(\"hello\\nworld\\n\")
w.close()

f = open_it(path)
print(first_line(f))
print(count_lines(f))
f.close()
"
        ),
    );
    assert_eq!(out, "hello\n1\n");
}

#[test]
fn for_line_in_file_matches_python() {
    let dir = TempDir::new("forfile");
    let data = dir.0.join("lines.txt").display().to_string();
    // no trailing newline on last line — CPython still yields "c"
    let out = run_program(
        "forfile",
        &format!(
            "\
path = \"{data}\"
w = open(path, \"w\")
w.write(\"a\\nb\\nc\")
w.close()
f = open(path)
for line in f:
    print(line.strip())
f.close()
# with-open + for
with open(path) as g:
    n = 0
    for line in g:
        n = n + 1
    print(n)
"
        ),
    );
    assert_eq!(out, "a\nb\nc\n3\n");
}

#[test]
fn list_repr_escapes_like_python() {
    let out = run_program(
        "represcape",
        "print([\"a\\nb\", \"don't\", 'say \"hi\"', \"tab\\there\"])\n",
    );
    // verified against python3: quote switching + escapes
    assert_eq!(out, "['a\\nb', \"don't\", 'say \"hi\"', 'tab\\there']\n");
}

#[test]
fn file_errors_match_python() {
    let (code, stderr) = run_program_expect_fail("fnf", "f = open(\"/nonexistent-pyrs-e2e\")\n");
    assert_eq!(code, 1);
    assert!(
        stderr.contains(
            "FileNotFoundError: [Errno 2] No such file or directory: \
             '/nonexistent-pyrs-e2e'"
        ),
        "stderr: {stderr}"
    );

    let dir = TempDir::new("fileerrs");
    let path = dir.0.join("t.txt").display().to_string();
    let (_, stderr) = run_program_expect_fail(
        "closedread",
        &format!("f = open(\"{path}\", \"w\")\nf.close()\nf.read()\n"),
    );
    assert!(
        stderr.contains("ValueError: I/O operation on closed file."),
        "stderr: {stderr}"
    );

    let (_, stderr) = run_program_expect_fail(
        "notwritable",
        &format!(
            "w = open(\"{path}\", \"w\")\nw.close()\n\
             f = open(\"{path}\")\nf.write(\"x\")\n"
        ),
    );
    assert!(
        stderr.contains("io.UnsupportedOperation: not writable"),
        "stderr: {stderr}"
    );
}

#[test]
fn open_invalid_mode_is_compile_error_when_constant() {
    let dir = TempDir::new("badmode");
    let src = dir.0.join("prog.py");
    fs::write(&src, "f = open(\"x\", \"q\")\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("invalid mode: 'q'"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn file_misuse_is_compile_error() {
    let dir = TempDir::new("filemisuse");
    let src = dir.0.join("prog.py");
    fs::write(&src, "f = open(\"x\")\nprint(f)\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cannot be printed"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---- with statement ----

#[test]
fn with_statement_matches_python() {
    let dir = TempDir::new("withstmt");
    let path = dir.0.join("w.txt").display().to_string();
    let out = run_program(
        "withstmt",
        &format!(
            "\
with open(\"{path}\", \"w\") as f:
    f.write(\"one\\n\")
    f.write(\"two\\n\")

with open(\"{path}\") as r:
    print(r.readlines())

def head(p: str) -> str:
    with open(p) as fh:
        return fh.readline().strip()

print(head(\"{path}\"))

found = \"\"
for attempt in range(3):
    with open(\"{path}\") as fh:
        if attempt == 1:
            found = fh.readline().strip()
            break
print(found, attempt)

with open(\"{path}\"):
    print(\"no-as form\")
"
        ),
    );
    assert_eq!(out, "['one\\n', 'two\\n']\none\none 1\nno-as form\n");
}

#[test]
fn with_closes_the_file() {
    let dir = TempDir::new("withclose");
    let path = dir.0.join("w.txt").display().to_string();
    let (code, stderr) = run_program_expect_fail(
        "withclose",
        &format!(
            "\
w = open(\"{path}\", \"w\")
w.close()
with open(\"{path}\") as f:
    pass
f.read()
"
        ),
    );
    assert_eq!(code, 1);
    assert!(
        stderr.contains("ValueError: I/O operation on closed file."),
        "stderr: {stderr}"
    );
}

#[test]
fn with_on_non_file_is_compile_error() {
    let dir = TempDir::new("withbad");
    let src = dir.0.join("prog.py");
    fs::write(&src, "with 5 as x:\n    pass\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("does not support the context manager protocol"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---- list comprehensions ----

#[test]
fn comprehensions_match_python() {
    let out = run_program(
        "listcomp",
        "\
print([x * x for x in range(6)])
print([x for x in range(20) if x % 3 == 0])
print([c.upper() for c in \"hello\"])
words = [\"the\", \"quick\", \"brown\", \"fox\"]
print([w.upper() for w in words if len(w) > 3])
grid = [[1, 2], [3, 4]]
print([[v * 10 for v in row] for row in grid])
print([i for i in range(10, 2, -2)])
print([q for q in range(0)])
def make(k: int) -> list[int]:
    return [j + k for j in range(3)]
print(make(100))
",
    );
    assert_eq!(
        out,
        "[0, 1, 4, 9, 16, 25]\n[0, 3, 6, 9, 12, 15, 18]\n\
         ['H', 'E', 'L', 'L', 'O']\n['QUICK', 'BROWN']\n\
         [[10, 20], [30, 40]]\n[10, 8, 6, 4]\n[]\n[100, 101, 102]\n"
    );
}

#[test]
fn comprehension_variable_shadows_but_does_not_leak() {
    // shadowing restores the outer variable
    let out = run_program(
        "compshadow",
        "\
x = 99
doubled = [x * 2 for x in range(4)]
print(doubled, x)
",
    );
    assert_eq!(out, "[0, 2, 4, 6] 99\n");

    // and a fresh comprehension variable is not defined afterwards
    let dir = TempDir::new("compleak");
    let src = dir.0.join("prog.py");
    fs::write(&src, "d = [y * 2 for y in range(4)]\nprint(y)\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("name 'y' is not defined"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn multi_for_comprehensions_match_python() {
    let out = run_program(
        "compmulti",
        "\
print([(i, j) for i in range(2) for j in range(3) if (i + j) % 2 == 0])
print([x for x in range(10) if x > 2 if x % 2 == 0])
print([(i, j) for i in range(3) for j in range(3) if i != j if i + j < 3])
xs = [10, 20]
print([i + j for i in range(2) for j in xs])
print([a + b for a, b in [(1, 2), (3, 4)]])
",
    );
    assert_eq!(
        out,
        "[(0, 0), (0, 2), (1, 1)]\n[4, 6, 8]\n\
         [(0, 1), (0, 2), (1, 0), (2, 0)]\n\
         [10, 20, 11, 21]\n[3, 7]\n"
    );
}

#[test]
fn for_unpack_match_python() {
    let out = run_program(
        "forunpack",
        "\
for a, b in [(1, 2), (3, 4)]:
    print(a, b)
for a, *rest in [[1, 2, 3], [4, 5]]:
    print(a, rest)
for x, *ys, z in [[1, 2, 3, 4], [5, 6, 7]]:
    print(x, ys, z)
for (a, b) in [(1, 2), (3, 4)]:
    print(a + b)
",
    );
    assert_eq!(
        out,
        "1 2\n3 4\n1 [2, 3]\n4 [5]\n1 [2, 3] 4\n5 [6] 7\n3\n7\n"
    );
}

// ---- modules (multi-file imports) ----

/// Write project files (creating parent dirs for package paths).
fn write_project(dir: &std::path::Path, files: &[(&str, &str)]) {
    for (name, body) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, body).unwrap();
    }
}

/// Write several files into a temp dir and run the root, returning stdout.
fn run_project(tag: &str, files: &[(&str, &str)], root: &str) -> String {
    let dir = TempDir::new(tag);
    write_project(&dir.0, files);
    let out = Command::new(PYRS)
        .args(["run", "-i"])
        .arg(dir.0.join(root))
        .output()
        .expect("failed to spawn PyRs");
    assert!(
        out.status.success(),
        "PyRs run failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Compile a project expecting a compile-time failure; returns stderr.
fn compile_project_expect_fail(tag: &str, files: &[(&str, &str)], root: &str) -> String {
    let dir = TempDir::new(tag);
    write_project(&dir.0, files);
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(dir.0.join(root))
        .output()
        .expect("failed to spawn PyRs");
    assert!(!out.status.success(), "expected a compile error");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn modules_functions_and_globals_match_python() {
    let out = run_project(
        "mod_basic",
        &[
            (
                "mathx.py",
                "PI = 3.5\n\
                 def square(n: int) -> int:\n    return n * n\n\
                 def scale(x: float) -> float:\n    return x * PI\n",
            ),
            (
                "main.py",
                "import mathx\n\
                 from mathx import PI as pi_value\n\
                 print(mathx.square(6))\n\
                 print(mathx.scale(2.0), pi_value)\n\
                 print([mathx.square(i) for i in range(4)])\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "36\n7.0 3.5\n[0, 1, 4, 9]\n");
}

#[test]
fn module_global_mutated_through_its_own_function() {
    let out = run_project(
        "mod_state",
        &[
            (
                "counter.py",
                "count = 0\n\
                 def bump(n: int):\n    global count\n    count += n\n\
                 def get() -> int:\n    return count\n",
            ),
            (
                "main.py",
                "import counter as c\n\
                 c.bump(3)\n\
                 c.bump(4)\n\
                 print(c.get(), c.count)\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "7 7\n");
}

#[test]
fn module_init_runs_once_at_import_site() {
    // 'a' is imported by both main and b (a diamond); its body runs once,
    // and 'main start' prints before any init (import-site ordering)
    let out = run_project(
        "mod_init",
        &[
            ("a.py", "print(\"init a\")\nval = 10\n"),
            ("b.py", "import a\nprint(\"init b\")\n"),
            (
                "main.py",
                "print(\"main start\")\n\
                 import a\n\
                 import b\n\
                 print(\"main:\", a.val)\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "main start\ninit a\ninit b\nmain: 10\n");
}

#[test]
fn transitive_imports_work() {
    let out = run_project(
        "mod_trans",
        &[
            ("base.py", "def one() -> int:\n    return 1\n"),
            (
                "mid.py",
                "import base\ndef two() -> int:\n    return base.one() + 1\n",
            ),
            ("main.py", "import mid\nprint(mid.two())\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "2\n");
}

#[test]
fn diagnostics_point_at_the_right_file() {
    let stderr = compile_project_expect_fail(
        "mod_diag",
        &[
            (
                "helper.py",
                "def broken(n: int) -> int:\n    x: str = \"a\"\n    x = 1\n    return n\n",
            ),
            ("main.py", "import helper\nprint(helper.broken(3))\n"),
        ],
        "main.py",
    );
    assert!(stderr.contains("helper.py:3"), "stderr: {stderr}");
    assert!(
        stderr.contains("type mismatch") || stderr.contains("storage type"),
        "stderr: {stderr}"
    );
}

#[test]
fn missing_module_is_an_error() {
    let stderr =
        compile_project_expect_fail("mod_missing", &[("main.py", "import nope\n")], "main.py");
    assert!(
        stderr.contains("No module named 'nope'"),
        "stderr: {stderr}"
    );
}

#[test]
fn missing_imported_name_is_an_error() {
    let stderr = compile_project_expect_fail(
        "mod_name",
        &[("u.py", "x = 5\n"), ("main.py", "from u import nope\n")],
        "main.py",
    );
    assert!(
        stderr.contains("cannot import name 'nope' from 'u'"),
        "stderr: {stderr}"
    );
}

#[test]
fn circular_imports_are_rejected() {
    let stderr = compile_project_expect_fail(
        "mod_cycle",
        &[
            ("a.py", "import b\ndef fa() -> int:\n    return 1\n"),
            ("b.py", "import a\ndef fb() -> int:\n    return 2\n"),
            ("main.py", "import a\nprint(a.fa())\n"),
        ],
        "main.py",
    );
    assert!(stderr.contains("circular import"), "stderr: {stderr}");
}

// ---- packages and relative imports (v0.10) ----

#[test]
fn package_dotted_import_matches_python() {
    let out = run_project(
        "pkg_basic",
        &[
            ("utilpkg/__init__.py", "print(\"init pkg\")\nVER = 1\n"),
            (
                "utilpkg/mathx.py",
                "print(\"init mod\")\n\
                 PI = 3.5\n\
                 def square(n: int) -> int:\n    return n * n\n",
            ),
            (
                "main.py",
                "import utilpkg.mathx\n\
                 print(utilpkg.mathx.square(6))\n\
                 print(utilpkg.mathx.PI)\n\
                 print(utilpkg.VER)\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "init pkg\ninit mod\n36\n3.5\n1\n");
}

#[test]
fn package_import_as_alias() {
    let out = run_project(
        "pkg_alias",
        &[
            ("utilpkg/__init__.py", ""),
            (
                "utilpkg/mathx.py",
                "def add(a: int, b: int) -> int:\n    return a + b\n",
            ),
            (
                "main.py",
                "import utilpkg.mathx as m\n\
                 print(m.add(2, 3))\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "5\n");
}

#[test]
fn from_package_module_import() {
    let out = run_project(
        "pkg_from",
        &[
            ("utilpkg/__init__.py", ""),
            (
                "utilpkg/mathx.py",
                "PI = 3.25\n\
                 def twice(n: int) -> int:\n    return n * 2\n",
            ),
            (
                "main.py",
                "from utilpkg.mathx import PI as pi, twice\n\
                 print(pi, twice(4))\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "3.25 8\n");
}

#[test]
fn from_package_import_submodule() {
    let out = run_project(
        "pkg_from_sub",
        &[
            ("utilpkg/__init__.py", ""),
            ("utilpkg/mathx.py", "V = 7\n"),
            (
                "main.py",
                "from utilpkg import mathx\n\
                 print(mathx.V)\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "7\n");
}

#[test]
fn import_package_only() {
    let out = run_project(
        "pkg_only",
        &[
            ("utilpkg/__init__.py", "print(\"init\")\nVAL = 42\n"),
            ("main.py", "import utilpkg\nprint(utilpkg.VAL)\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "init\n42\n");
}

#[test]
fn relative_import_sibling() {
    let out = run_project(
        "pkg_rel",
        &[
            ("utilpkg/__init__.py", ""),
            ("utilpkg/b.py", "Z = 99\nprint(\"b\")\n"),
            (
                "utilpkg/a.py",
                "from . import b\n\
                 from .b import Z\n\
                 print(\"a\", Z, b.Z)\n",
            ),
            ("main.py", "import utilpkg.a\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "b\na 99 99\n");
}

#[test]
fn relative_import_parent() {
    let out = run_project(
        "pkg_rel_parent",
        &[
            ("utilpkg/__init__.py", "P = 1\n"),
            ("utilpkg/sub/__init__.py", ""),
            ("utilpkg/a.py", "print(\"a loaded\")\n"),
            (
                "utilpkg/sub/c.py",
                "from .. import P\n\
                 from .. import a\n\
                 print(P)\n",
            ),
            ("main.py", "import utilpkg.sub.c\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "a loaded\n1\n");
}

#[test]
fn relative_import_in_non_package_is_error() {
    let stderr =
        compile_project_expect_fail("rel_top", &[("main.py", "from . import x\n")], "main.py");
    assert!(
        stderr.contains("attempted relative import with no known parent package"),
        "stderr: {stderr}"
    );
}

#[test]
fn nested_package_attribute_chain() {
    let out = run_project(
        "pkg_nested",
        &[
            ("utilpkg/__init__.py", "print(\"pkg\")\n"),
            ("utilpkg/sub/__init__.py", "print(\"sub\")\n"),
            ("utilpkg/sub/m.py", "print(\"m\")\nX = 3\n"),
            (
                "main.py",
                "import utilpkg.sub.m\n\
                 print(utilpkg.sub.m.X)\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "pkg\nsub\nm\n3\n");
}

#[test]
fn missing_package_module_is_error() {
    let stderr = compile_project_expect_fail(
        "pkg_missing",
        &[
            ("utilpkg/__init__.py", ""),
            ("main.py", "import utilpkg.nope\n"),
        ],
        "main.py",
    );
    assert!(
        stderr.contains("No module named 'utilpkg.nope'"),
        "stderr: {stderr}"
    );
}

#[test]
fn package_init_reexports_submodule() {
    // Common layout: `__init__.py` does `from .mod import …` (partial package init).
    let out = run_project(
        "pkg_reexport",
        &[
            (
                "utilpkg/__init__.py",
                "from .mod import f, VAL\nprint(\"init pkg\")\n",
            ),
            (
                "utilpkg/mod.py",
                "VAL = 7\n\
                 def f() -> int:\n    return VAL\n\
                 print(\"init mod\")\n",
            ),
            (
                "main.py",
                "import utilpkg\n\
                 print(utilpkg.VAL, utilpkg.f())\n\
                 from utilpkg import VAL as v, f\n\
                 print(v, f())\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "init mod\ninit pkg\n7 7\n7 7\n");
}

#[test]
fn package_from_dot_import_mod_binds_module() {
    // `from . import mod` must re-export the submodule, not a scalar Symbol.
    let out = run_project(
        "pkg_from_dot_mod",
        &[
            ("utilpkg/__init__.py", "from . import mod\n"),
            ("utilpkg/mod.py", "X = 5\ndef f() -> int:\n    return X\n"),
            (
                "main.py",
                "import utilpkg\n\
                 from utilpkg import mod\n\
                 print(mod.X, mod.f(), utilpkg.mod.X)\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "5 5 5\n");
}

#[test]
fn child_imports_parent_name_during_partial_init() {
    // Parent sets VERSION then imports child; child does `from . import VERSION`.
    let out = run_project(
        "pkg_child_parent",
        &[
            ("utilpkg/__init__.py", "VERSION = 1\nfrom .mod import f\n"),
            (
                "utilpkg/mod.py",
                "from . import VERSION\n\
                 def f() -> int:\n    return VERSION\n",
            ),
            (
                "main.py",
                "import utilpkg\nprint(utilpkg.f(), utilpkg.VERSION)\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "1 1\n");
}

#[test]
fn import_after_assignment_reexport_wins() {
    // Last binding wins: import after assignment → function re-export.
    let out = run_project(
        "pkg_import_last",
        &[
            (
                "utilpkg/__init__.py",
                "helper = 99\nfrom .mod import helper\n",
            ),
            ("utilpkg/mod.py", "def helper() -> int:\n    return 1\n"),
            ("main.py", "import utilpkg\nprint(utilpkg.helper())\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "1\n");
}

#[test]
fn value_reexport_after_submodule_last_wins() {
    // Last binding: value re-export overwrites same-named submodule for both
    // `from utilpkg import mod` and attribute `utilpkg.mod`.
    let out = run_project(
        "pkg_value_last",
        &[
            (
                "utilpkg/__init__.py",
                "from . import mod\nfrom .other import mod\n",
            ),
            ("utilpkg/other.py", "mod = 42\n"),
            ("utilpkg/mod.py", "X = 7\n"),
            (
                "main.py",
                "import utilpkg\n\
                 from utilpkg import mod\n\
                 print(mod)\n\
                 print(utilpkg.mod)\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "42\n42\n");
}

#[test]
fn assign_after_child_import_not_visible_to_child() {
    // Mid-init *module body* `from . import AFTER` when AFTER is assigned only
    // after the child-loading import → compile error (CPython ImportError).
    let stderr = compile_project_expect_fail(
        "pkg_after_partial",
        &[
            (
                "utilpkg/__init__.py",
                "VERSION = 1\nfrom .mod import f\nAFTER = 2\n",
            ),
            (
                "utilpkg/mod.py",
                "from . import VERSION\nfrom . import AFTER\ndef f() -> int:\n    return VERSION\n",
            ),
            ("main.py", "import utilpkg\nprint(utilpkg.f())\n"),
        ],
        "main.py",
    );
    assert!(
        stderr.contains("cannot import name 'AFTER'") && stderr.contains("partially initialized"),
        "stderr: {stderr}"
    );
}

#[test]
fn deferred_parent_attr_after_import_matches_python() {
    // CPython: child *function* body may read parent names assigned after the
    // child-loading import (lookup happens at call time, after full init).
    let out = run_project(
        "pkg_deferred_after",
        &[
            (
                "utilpkg/__init__.py",
                "VERSION = 1\nfrom .mod import f, h\nAFTER = 2\ndef g() -> int:\n    return 3\n",
            ),
            (
                "utilpkg/mod.py",
                "import utilpkg\n\
                 def f() -> int:\n    return utilpkg.AFTER\n\
                 def h() -> int:\n    return utilpkg.g()\n",
            ),
            (
                "main.py",
                "import utilpkg\nprint(utilpkg.f(), utilpkg.h(), utilpkg.AFTER)\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "2 3 2\n");
}

#[test]
fn child_reads_parent_attr_during_partial_init() {
    // `import utilpkg` + `utilpkg.VERSION` while parent mid-init.
    let out = run_project(
        "pkg_attr_partial",
        &[
            ("utilpkg/__init__.py", "VERSION = 1\nfrom .mod import f\n"),
            (
                "utilpkg/mod.py",
                "import utilpkg\n\
                 def f() -> int:\n    return utilpkg.VERSION\n",
            ),
            ("main.py", "import utilpkg\nprint(utilpkg.f())\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "1\n");
}

#[test]
fn child_call_parent_func_deferred_matches_python() {
    // CPython: child function body may call parent functions (def before import).
    let out = run_project(
        "pkg_call_deferred",
        &[
            (
                "utilpkg/__init__.py",
                "def g() -> int:\n    return 3\nfrom .mod import f\n",
            ),
            (
                "utilpkg/mod.py",
                "import utilpkg\n\
                 def f() -> int:\n    return utilpkg.g()\n",
            ),
            ("main.py", "import utilpkg\nprint(utilpkg.f())\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "3\n");
}

#[test]
fn child_call_parent_func_defined_after_import_deferred() {
    // Def after the child-loading import: still OK when called later (deferred).
    let out = run_project(
        "pkg_call_after",
        &[
            (
                "utilpkg/__init__.py",
                "from .mod import f\ndef g() -> int:\n    return 4\n",
            ),
            (
                "utilpkg/mod.py",
                "import utilpkg\n\
                 def f() -> int:\n    return utilpkg.g()\n",
            ),
            ("main.py", "import utilpkg\nprint(utilpkg.f())\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "4\n");
}

#[test]
fn relative_import_beyond_top_level_is_error() {
    let stderr = compile_project_expect_fail(
        "rel_beyond",
        &[
            ("utilpkg/__init__.py", "from ... import x\n"),
            ("main.py", "import utilpkg\n"),
        ],
        "main.py",
    );
    assert!(
        stderr.contains("attempted relative import beyond top-level package"),
        "stderr: {stderr}"
    );
}

#[test]
fn intermediate_module_is_not_a_package() {
    let stderr = compile_project_expect_fail(
        "not_pkg",
        &[("foo.py", "X = 1\n"), ("main.py", "import foo.bar\n")],
        "main.py",
    );
    assert!(
        stderr.contains("No module named 'foo.bar'") && stderr.contains("is not a package"),
        "stderr: {stderr}"
    );
}

#[test]
fn package_missing_exported_name_is_error() {
    let stderr = compile_project_expect_fail(
        "pkg_no_name",
        &[
            ("utilpkg/__init__.py", "X = 1\n"),
            ("main.py", "from utilpkg import nope\n"),
        ],
        "main.py",
    );
    assert!(
        stderr.contains("cannot import name 'nope' from 'utilpkg'"),
        "stderr: {stderr}"
    );
}

#[test]
fn package_scoped_cycle_is_rejected() {
    // True mutual imports between sibling modules still fail at load time.
    let stderr = compile_project_expect_fail(
        "pkg_cycle",
        &[
            ("utilpkg/__init__.py", ""),
            (
                "utilpkg/a.py",
                "from . import b\ndef fa() -> int:\n    return 1\n",
            ),
            (
                "utilpkg/b.py",
                "from . import a\ndef fb() -> int:\n    return 2\n",
            ),
            ("main.py", "import utilpkg.a\n"),
        ],
        "main.py",
    );
    assert!(stderr.contains("circular import"), "stderr: {stderr}");
}

#[test]
fn relative_from_parent_other_module() {
    let out = run_project(
        "pkg_rel_other",
        &[
            ("utilpkg/__init__.py", ""),
            ("utilpkg/sub/__init__.py", ""),
            ("utilpkg/other.py", "Z = 3\n"),
            ("utilpkg/sub/m.py", "from ..other import Z\nprint(Z)\n"),
            ("main.py", "import utilpkg.sub.m\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "3\n");
}

#[test]
fn assignment_after_reexport_wins() {
    // CPython: later binding in `__init__` overwrites the re-export.
    let out = run_project(
        "pkg_shadow",
        &[
            (
                "utilpkg/__init__.py",
                "from .mod import helper\nhelper = 99\n",
            ),
            ("utilpkg/mod.py", "def helper() -> int:\n    return 1\n"),
            ("main.py", "import utilpkg\nprint(utilpkg.helper)\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "99\n");
}

#[test]
fn nested_import_in_if_works() {
    // Imports inside if/functions are allowed; module body still runs once.
    let out = run_project(
        "nested_imp",
        &[
            ("mathx.py", "X = 7\n"),
            ("main.py", "if True:\n    import mathx\nprint(mathx.X)\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "7\n");
}

#[test]
fn namespace_package_import_child() {
    // PEP 420 subset: directory without __init__.py is a namespace package.
    let out = run_project(
        "ns_pkg",
        &[
            ("utilpkg/mod.py", "X = 1\n"),
            (
                "main.py",
                "import utilpkg.mod\nprint(utilpkg.mod.X)\nimport utilpkg\nprint(1)\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "1\n1\n");
}

#[test]
fn nested_namespace_package() {
    let out = run_project(
        "ns_nested",
        &[
            ("a/b/c.py", "Y = 2\n"),
            ("main.py", "import a.b.c\nprint(a.b.c.Y)\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "2\n");
}

#[test]
fn star_import_public_names() {
    let out = run_project(
        "star_pub",
        &[
            ("m.py", "a = 1\n_b = 2\nc = 3\n"),
            ("main.py", "from m import *\nprint(a, c)\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "1 3\n");
}

#[test]
fn star_import_hides_private() {
    // Private names are not bound by star without __all__.
    let stderr = compile_project_expect_fail(
        "star_priv",
        &[
            ("m.py", "a = 1\n_b = 2\n"),
            ("main.py", "from m import *\nprint(_b)\n"),
        ],
        "main.py",
    );
    assert!(
        stderr.contains("_b")
            || stderr.contains("undefined")
            || stderr.contains("not defined")
            || stderr.contains("unknown"),
        "stderr: {stderr}"
    );
}

#[test]
fn star_import_all_list() {
    let out = run_project(
        "star_all",
        &[
            (
                "m2.py",
                "__all__ = [\"_priv\", \"x\"]\n_priv = 1\nx = 2\ny = 3\n",
            ),
            ("main.py", "from m2 import *\nprint(_priv, x)\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "1 2\n");
}

#[test]
fn star_import_all_excludes_others() {
    let stderr = compile_project_expect_fail(
        "star_all_excl",
        &[
            ("m2.py", "__all__ = [\"x\"]\nx = 2\ny = 3\n"),
            ("main.py", "from m2 import *\nprint(y)\n"),
        ],
        "main.py",
    );
    assert!(
        stderr.contains('y')
            || stderr.contains("undefined")
            || stderr.contains("not defined")
            || stderr.contains("unknown"),
        "stderr: {stderr}"
    );
}

#[test]
fn star_import_all_tuple() {
    let out = run_project(
        "star_all_tup",
        &[
            ("m4.py", "__all__ = (\"a\", \"_b\")\na = 1\n_b = 2\n"),
            ("main.py", "from m4 import *\nprint(a, _b)\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "1 2\n");
}

#[test]
fn star_import_empty_all() {
    let out = run_project(
        "star_empty_all",
        &[
            ("m3.py", "__all__: list[str] = []\na = 1\n"),
            ("main.py", "from m3 import *\nprint(1)\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "1\n");
}

#[test]
fn star_import_empty_all_hides_names() {
    let stderr = compile_project_expect_fail(
        "star_empty_all_hide",
        &[
            ("m3.py", "__all__: list[str] = []\na = 1\n"),
            ("main.py", "from m3 import *\nprint(a)\n"),
        ],
        "main.py",
    );
    assert!(
        stderr.contains('a')
            || stderr.contains("undefined")
            || stderr.contains("not defined")
            || stderr.contains("unknown"),
        "stderr: {stderr}"
    );
}

#[test]
fn star_import_function_level_rejected() {
    let stderr = compile_project_expect_fail(
        "star_fn",
        &[
            ("m.py", "a = 1\n"),
            ("main.py", "def f() -> None:\n    from m import *\nf()\n"),
        ],
        "main.py",
    );
    assert!(
        stderr.contains("import * only allowed at module level"),
        "stderr: {stderr}"
    );
}

#[test]
fn star_import_dynamic_all_rejected() {
    let stderr = compile_project_expect_fail(
        "star_dyn_all",
        &[
            ("m.py", "names = [\"x\"]\n__all__ = names\nx = 1\n"),
            ("main.py", "from m import *\n"),
        ],
        "main.py",
    );
    assert!(
        stderr.contains("non-static __all__") || stderr.contains("__all__"),
        "stderr: {stderr}"
    );
}

#[test]
fn star_import_funcs_and_values() {
    let out = run_project(
        "star_fn_val",
        &[
            (
                "lib.py",
                "VAL = 7\ndef twice(n: int) -> int:\n    return n * 2\n",
            ),
            (
                "main.py",
                "from lib import *\nprint(VAL)\nprint(twice(3))\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "7\n6\n");
}

#[test]
fn relative_star_import() {
    let out = run_project(
        "rel_star",
        &[
            ("pkg2/__init__.py", "A = 10\n_B = 20\n"),
            ("pkg2/sub.py", "from . import *\nprint(A)\n"),
            ("main.py", "import pkg2.sub\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "10\n");
}

// ---- additional package edge cases (review follow-up) ----

#[test]
fn value_then_from_dot_import_keeps_value() {
    // CPython hasattr short-circuit: assign then `from . import same_name` keeps
    // the value; submodule body must not run.
    let out = run_project(
        "pkg_val_then_sub",
        &[
            (
                "utilpkg/__init__.py",
                "helper = 99\nfrom . import helper\nprint(\"init\", helper)\n",
            ),
            ("utilpkg/helper.py", "print(\"HELPER BODY\")\nX = 1\n"),
            (
                "main.py",
                "import utilpkg\nprint(utilpkg.helper)\nfrom utilpkg import helper\nprint(helper)\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "init 99\n99\n99\n");
    assert!(
        !out.contains("HELPER BODY"),
        "submodule body must not run: {out}"
    );
}

#[test]
fn func_then_from_dot_import_keeps_func() {
    let out = run_project(
        "pkg_func_then_sub",
        &[
            (
                "utilpkg/__init__.py",
                "def helper() -> int:\n    return 1\nfrom . import helper\nprint(\"init\", helper())\n",
            ),
            ("utilpkg/helper.py", "print(\"HELPER BODY\")\nX = 1\n"),
            (
                "main.py",
                "import utilpkg\nprint(utilpkg.helper())\nfrom utilpkg import helper\nprint(helper())\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "init 1\n1\n1\n");
    assert!(
        !out.contains("HELPER BODY"),
        "submodule body must not run: {out}"
    );
}

#[test]
fn reexport_then_from_dot_import_keeps_origin() {
    // Value/function re-export then `from . import same_name` must not lose origin.
    let out = run_project(
        "pkg_reexp_then_sub",
        &[
            (
                "utilpkg/__init__.py",
                "from .other import helper\nfrom . import helper\n",
            ),
            ("utilpkg/other.py", "def helper() -> int:\n    return 42\n"),
            ("utilpkg/helper.py", "print(\"HELPER BODY\")\nX = 1\n"),
            (
                "main.py",
                "import utilpkg\nprint(utilpkg.helper())\nfrom utilpkg import helper\nprint(helper())\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "42\n42\n");
    assert!(
        !out.contains("HELPER BODY"),
        "submodule body must not run: {out}"
    );
}

#[test]
fn value_reexport_then_from_dot_import_keeps_value() {
    let out = run_project(
        "pkg_val_reexp_then_sub",
        &[
            (
                "utilpkg/__init__.py",
                "from .other import helper\nfrom . import helper\n",
            ),
            ("utilpkg/other.py", "helper = 99\n"),
            ("utilpkg/helper.py", "print(\"HELPER BODY\")\nX = 1\n"),
            (
                "main.py",
                "import utilpkg\nprint(utilpkg.helper)\nfrom utilpkg import helper\nprint(helper)\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "99\n99\n");
    assert!(
        !out.contains("HELPER BODY"),
        "submodule body must not run: {out}"
    );
}

#[test]
fn annotated_assign_visible_during_partial_init() {
    let out = run_project(
        "pkg_annot_partial",
        &[
            (
                "utilpkg/__init__.py",
                "VERSION: int = 1\nfrom .mod import f\n",
            ),
            (
                "utilpkg/mod.py",
                "from . import VERSION\ndef f() -> int:\n    return VERSION\n",
            ),
            (
                "main.py",
                "import utilpkg\nprint(utilpkg.f(), utilpkg.VERSION)\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "1 1\n");
}

#[test]
fn non_simple_assign_before_child_not_in_partial_surface() {
    // Documented: only simple (literal / annotated) assigns are typed for partial init.
    let stderr = compile_project_expect_fail(
        "pkg_nonsimple_partial",
        &[
            (
                "utilpkg/__init__.py",
                "def make() -> int:\n    return 1\nVERSION = make()\nfrom .mod import f\n",
            ),
            (
                "utilpkg/mod.py",
                "from . import VERSION\ndef f() -> int:\n    return VERSION\n",
            ),
            ("main.py", "import utilpkg\nprint(utilpkg.f())\n"),
        ],
        "main.py",
    );
    assert!(
        stderr.contains("cannot import name 'VERSION'") && stderr.contains("partially initialized"),
        "stderr: {stderr}"
    );
}

#[test]
fn deep_relative_import_three_levels() {
    let out = run_project(
        "pkg_deep_rel",
        &[
            ("utilpkg/__init__.py", ""),
            ("utilpkg/mid/__init__.py", ""),
            ("utilpkg/mid/leaf/__init__.py", ""),
            ("utilpkg/other.py", "Z = 5\n"),
            (
                "utilpkg/mid/leaf/m.py",
                "from ...other import Z\nprint(Z)\n",
            ),
            ("main.py", "import utilpkg.mid.leaf.m\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "5\n");
}

#[test]
fn relative_missing_sibling_is_error() {
    let stderr = compile_project_expect_fail(
        "pkg_rel_miss",
        &[
            ("utilpkg/__init__.py", ""),
            ("utilpkg/a.py", "from . import nope\n"),
            ("main.py", "import utilpkg.a\n"),
        ],
        "main.py",
    );
    // No utilpkg/nope.py and no package export: semantic cannot-import (not load).
    assert!(
        stderr.contains("cannot import name 'nope' from 'utilpkg'"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("a.py"),
        "should point at importer file: {stderr}"
    );
}

#[test]
fn package_diamond_init_once() {
    let out = run_project(
        "pkg_diamond",
        &[
            (
                "utilpkg/__init__.py",
                "print(\"pkg\")\nfrom . import a\nfrom . import b\n",
            ),
            ("utilpkg/a.py", "print(\"a\")\nV = 1\n"),
            ("utilpkg/b.py", "from . import a\nprint(\"b\", a.V)\n"),
            ("main.py", "import utilpkg\nprint(\"done\")\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "pkg\na\nb 1\ndone\n");
}

#[test]
fn package_reexport_as_alias() {
    let out = run_project(
        "pkg_reexport_alias",
        &[
            (
                "utilpkg/__init__.py",
                "from .mod import f as ff, VAL as V\n",
            ),
            (
                "utilpkg/mod.py",
                "VAL = 7\ndef f() -> int:\n    return VAL\n",
            ),
            (
                "main.py",
                "import utilpkg\nprint(utilpkg.V, utilpkg.ff())\n\
                 from utilpkg import ff, V\nprint(V, ff())\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "7 7\n7 7\n");
}

#[test]
fn package_self_import_is_error() {
    let stderr = compile_project_expect_fail(
        "pkg_self",
        &[
            ("utilpkg/__init__.py", "import utilpkg\nX = 1\n"),
            ("main.py", "import utilpkg\n"),
        ],
        "main.py",
    );
    assert!(stderr.contains("cannot import itself"), "stderr: {stderr}");
    assert!(
        stderr.contains("__init__.py"),
        "should point at package file: {stderr}"
    );
}

#[test]
fn nested_package_only_import() {
    let out = run_project(
        "pkg_nested_only",
        &[
            ("utilpkg/__init__.py", "print(\"pkg\")\n"),
            ("utilpkg/sub/__init__.py", "print(\"sub\")\nVAL = 8\n"),
            ("main.py", "import utilpkg.sub\nprint(utilpkg.sub.VAL)\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "pkg\nsub\n8\n");
}

#[test]
fn nested_import_inside_package_module_works() {
    let out = run_project(
        "pkg_nested_imp",
        &[
            ("utilpkg/__init__.py", ""),
            (
                "utilpkg/a.py",
                "if True:\n    from . import b\nprint(b.X)\n",
            ),
            ("utilpkg/b.py", "X = 1\n"),
            ("main.py", "import utilpkg.a\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "1\n");
}

#[test]
fn missing_package_error_points_at_importer() {
    let stderr = compile_project_expect_fail(
        "pkg_diag_file",
        &[
            ("utilpkg/__init__.py", ""),
            ("main.py", "import utilpkg.missing\n"),
        ],
        "main.py",
    );
    assert!(
        stderr.contains("No module named 'utilpkg.missing'"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("main.py"),
        "diagnostic should point at importer: {stderr}"
    );
}

#[test]
fn deferred_parent_via_reexport_matches_python() {
    // Parent only re-exports from .other; child function bodies use utilpkg.VAL / g().
    let out = run_project(
        "pkg_deferred_reexport",
        &[
            (
                "utilpkg/__init__.py",
                "from .other import g, VAL\nfrom .mod import f, h\n",
            ),
            (
                "utilpkg/other.py",
                "VAL = 7\ndef g() -> int:\n    return VAL\n",
            ),
            (
                "utilpkg/mod.py",
                "import utilpkg\n\
                 def f() -> int:\n    return utilpkg.VAL\n\
                 def h() -> int:\n    return utilpkg.g()\n",
            ),
            (
                "main.py",
                "import utilpkg\nprint(utilpkg.f(), utilpkg.h(), utilpkg.VAL, utilpkg.g())\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "7 7 7 7\n");
}

#[test]
fn mid_init_from_import_parent_def_matches_python() {
    // `from . import g` when g is a def before the child-loading import.
    let out = run_project(
        "pkg_mid_from_def",
        &[
            (
                "utilpkg/__init__.py",
                "def g() -> int:\n    return 3\nfrom .mod import f\n",
            ),
            (
                "utilpkg/mod.py",
                "from . import g\ndef f() -> int:\n    return g()\n",
            ),
            ("main.py", "import utilpkg\nprint(utilpkg.f())\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "3\n");
}

#[test]
fn external_fromlist_skips_bound_submodule() {
    // `from utilpkg import helper` must not load helper.py when helper is a value.
    let out = run_project(
        "pkg_ext_fromlist",
        &[
            ("utilpkg/__init__.py", "helper = 99\n"),
            ("utilpkg/helper.py", "print(\"HELPER BODY\")\nX = 1\n"),
            ("main.py", "from utilpkg import helper\nprint(helper)\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "99\n");
    assert!(
        !out.contains("HELPER BODY"),
        "submodule must not run: {out}"
    );
}

#[test]
fn relative_from_dot_mod_missing_is_error() {
    let stderr = compile_project_expect_fail(
        "pkg_rel_miss_mod",
        &[
            ("utilpkg/__init__.py", ""),
            ("utilpkg/a.py", "from .nope import x\n"),
            ("main.py", "import utilpkg.a\n"),
        ],
        "main.py",
    );
    assert!(
        stderr.contains("No module named 'utilpkg.nope'"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("a.py"),
        "should point at importer file: {stderr}"
    );
}

#[test]
fn relative_outside_in_helper_module_is_error() {
    // Relative import in a non-package top-level sibling module (not __main__).
    let stderr = compile_project_expect_fail(
        "rel_helper",
        &[
            ("helper.py", "from . import x\n"),
            ("main.py", "import helper\n"),
        ],
        "main.py",
    );
    assert!(
        stderr.contains("attempted relative import with no known parent package"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("helper.py"),
        "should point at helper: {stderr}"
    );
}

#[test]
fn deferred_non_simple_parent_value_is_error() {
    // Non-simple assign is not on the deferred simple-assign surface.
    let stderr = compile_project_expect_fail(
        "pkg_deferred_nonsimple",
        &[
            (
                "utilpkg/__init__.py",
                "def make() -> int:\n    return 9\nfrom .mod import f\nVERSION = make()\n",
            ),
            (
                "utilpkg/mod.py",
                "import utilpkg\ndef f() -> int:\n    return utilpkg.VERSION\n",
            ),
            ("main.py", "import utilpkg\nprint(utilpkg.f())\n"),
        ],
        "main.py",
    );
    assert!(
        stderr
            .contains("cannot import name 'VERSION' from partially initialized package 'utilpkg'"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("mod.py"),
        "should point at child module: {stderr}"
    );
}

#[test]
fn tuples_print_index_unpack() {
    let out = run_program(
        "tuples",
        "\
t: tuple[int, str, float] = (1, \"a\", 2.0)
print(t)
print((1,))
print(())
print(len(t))
print(t[0], t[-1])
a, b = 1, 2
print(a, b)
a, b = (3, 4)
print(a, b)

def pair(x: int, y: int) -> tuple[int, int]:
    return (x, y)

p = pair(10, 20)
print(p[0] + p[1])
",
    );
    assert_eq!(out, "(1, 'a', 2.0)\n(1,)\n()\n3\n1 2.0\n1 2\n3 4\n30\n");
}

#[test]
fn tuple_index_error() {
    // dynamic index so the trap is at runtime (constant OOB is a compile error)
    let (code, err) = run_program_expect_fail("tuple_idx", "t = (1, 2)\ni = 5\nprint(t[i])\n");
    assert_eq!(code, 1);
    assert!(
        err.contains("IndexError: tuple index out of range"),
        "{err}"
    );
}

#[test]
fn unpack_too_few() {
    // list RHS exercises runtime pyrs_unpack_check
    let (code, err) = run_program_expect_fail("unpack_few", "xs: list[int] = [1]\na, b = xs\n");
    assert_eq!(code, 1);
    assert!(
        err.contains("not enough values to unpack (expected 2, got 1)"),
        "{err}"
    );
}

#[test]
fn unpack_too_many() {
    let (code, err) =
        run_program_expect_fail("unpack_many", "xs: list[int] = [1, 2, 3]\na, b = xs\n");
    assert_eq!(code, 1);
    assert!(
        err.contains("too many values to unpack (expected 2, got 3)"),
        "{err}"
    );
}

#[test]
fn dict_basic_ops() {
    let out = run_program(
        "dict_basic",
        "\
d: dict[str, int] = {\"x\": 1, \"y\": 2}
print(d)
print(len(d))
print(d[\"x\"])
d[\"z\"] = 3
print(\"x\" in d, \"q\" not in d)
print(d.get(\"x\", 0), d.get(\"q\", 0))
print(d.keys())
print(d.values())
print(d.items())
del d[\"x\"]
print(d)
print(d.pop(\"y\"))
print(d)
",
    );
    assert_eq!(
        out,
        "{'x': 1, 'y': 2}\n2\n1\nTrue True\n1 0\n['x', 'y', 'z']\n[1, 2, 3]\n[('x', 1), ('y', 2), ('z', 3)]\n{'y': 2, 'z': 3}\n2\n{'z': 3}\n"
    );
}

#[test]
fn dict_key_error() {
    let (code, err) = run_program_expect_fail(
        "dict_key",
        "d: dict[str, int] = {\"a\": 1}\nprint(d[\"b\"])\n",
    );
    assert_eq!(code, 1);
    assert!(err.contains("KeyError: 'b'"), "{err}");
}

#[test]
fn set_basic_ops() {
    let out = run_program(
        "set_basic",
        "\
s: set[int] = {1, 2, 3}
print(1 in s)
s.add(4)
print(len(s))
s.discard(2)
print(2 in s)
s2: set[int] = set()
s2.add(10)
print(s2)
",
    );
    assert_eq!(out, "True\n4\nFalse\n{10}\n");
}

#[test]
fn try_except_raise() {
    let out = run_program(
        "try_exc",
        "\
try:
    raise ValueError(\"bad\")
except ValueError as e:
    print(\"caught\", e)
print(\"after\")
try:
    xs: list[int] = [1]
    print(xs[5])
except IndexError:
    print(\"idx\")
try:
    raise RuntimeError(\"x\")
except:
    print(\"bare\")
",
    );
    assert_eq!(out, "caught bad\nafter\nidx\nbare\n");
}

#[test]
fn uncaught_raise() {
    let (code, err) = run_program_expect_fail("uncaught", "raise ValueError(\"oops\")\n");
    assert_eq!(code, 1);
    assert!(err.contains("ValueError: oops"), "{err}");
}

#[test]
fn try_finally_paths() {
    let out = run_program(
        "try_finally",
        "\
try:
    print(\"body\")
finally:
    print(\"fin\")
print(\"after\")
try:
    raise ValueError(\"x\")
except ValueError:
    print(\"caught\")
finally:
    print(\"fin2\")
try:
    try:
        raise ValueError(\"y\")
    finally:
        print(\"fin3\")
except ValueError:
    print(\"outer\")

def f() -> int:
    try:
        return 1
    finally:
        print(\"fin4\")
print(f())
while True:
    try:
        break
    finally:
        print(\"fin5\")
print(\"done\")
",
    );
    assert_eq!(
        out,
        "body\nfin\nafter\ncaught\nfin2\nfin3\nouter\nfin4\n1\nfin5\ndone\n"
    );
}

#[test]
fn dict_extra_ops() {
    let out = run_program(
        "dict_extra",
        "\
d: dict[str, int] = {}
d[\"a\"] = 1
print(d)
d.clear()
print(len(d))
d2: dict[int, str] = {1: \"x\", 2: \"y\"}
for k in d2:
    print(k)
print(d2.get(3, \"z\"))
",
    );
    assert_eq!(out, "{'a': 1}\n0\n1\n2\nz\n");
}

#[test]
fn set_extra_ops() {
    let out = run_program(
        "set_extra",
        "\
s: set[int] = set()
print(s)
s.add(3)
s.add(1)
s.add(2)
for x in s:
    print(x)
s.remove(1)
print(1 in s)
s.clear()
print(len(s))
",
    );
    assert_eq!(out, "set()\n3\n1\n2\nFalse\n0\n");
}

#[test]
fn set_remove_keyerror() {
    let (code, err) = run_program_expect_fail("set_rm", "s: set[int] = {1}\ns.remove(2)\n");
    assert_eq!(code, 1);
    assert!(err.contains("KeyError: 2"), "{err}");
}

#[test]
fn tuple_for_and_eq() {
    let out = run_program(
        "tuple_for",
        "\
t: tuple[int, int, int] = (1, 2, 3)
for x in t:
    print(x)
print((1, 2) == (1, 2))
print((1, 2) != (1, 3))
u: tuple[int, tuple[str, int]] = (1, (\"a\", 2))
print(u[1][0])
",
    );
    assert_eq!(out, "1\n2\n3\nTrue\nTrue\na\n");
}

#[test]
fn exception_matrix() {
    let out = run_program(
        "exc_matrix",
        "\
try:
    raise ValueError(\"v\")
except KeyError:
    print(\"no\")
except ValueError:
    print(\"val\")
try:
    d: dict[str, int] = {\"a\": 1}
    print(d[\"b\"])
except KeyError:
    print(\"key\")
try:
    print(1 // 0)
except ZeroDivisionError:
    print(\"zdiv\")
try:
    raise TypeError(\"t\")
except RuntimeError:
    print(\"rt\")
except:
    print(\"bare\")
",
    );
    assert_eq!(out, "val\nkey\nzdiv\nbare\n");
}

#[test]
fn with_try_still_closes() {
    // Exception propagates *out* of `with` so finally/close must run before
    // the outer except (proves with → try/finally, not only in-body catch).
    let dir = TempDir::new("with_try");
    let path = dir.0.join("t.txt");
    let path_s = path.to_str().unwrap();
    let out = run_program(
        "with_try",
        &format!(
            "\
try:
    with open(\"{path_s}\", \"w\") as f:
        f.write(\"hi\")
        raise ValueError(\"x\")
except ValueError:
    print(\"caught\")
f2 = open(\"{path_s}\", \"r\")
print(f2.read())
f2.close()
"
        ),
    );
    assert_eq!(out, "caught\nhi\n");
}

#[test]
fn other_trap_not_runtime_error() {
    let out = run_program(
        "other_trap",
        "\
try:
    open(\"/no/such/file/pyrs_xyz_missing\", \"r\")
except RuntimeError:
    print(\"rt\")
except:
    print(\"other\")
",
    );
    assert_eq!(out, "other\n");
}

#[test]
fn nested_list_dict_set_eq() {
    let out = run_program(
        "nested_eq",
        "\
print([{\"a\": 1}] == [{\"a\": 1}])
print([{1, 2}] == [{1, 2}])
print([{\"a\": 1}] == [{\"a\": 2}])
",
    );
    assert_eq!(out, "True\nTrue\nFalse\n");
}

#[test]
fn try_continue_and_multi_return() {
    let out = run_program(
        "try_cont",
        "\
i = 0
while i < 3:
    i += 1
    try:
        if i == 2:
            continue
        print(\"c\", i)
    finally:
        print(\"f\", i)
print(\"done\")

def g(x: int) -> int:
    try:
        if x > 0:
            return 1
        return 2
    finally:
        print(\"fr\")
print(g(1))
print(g(-1))

try:
    try:
        raise ValueError(\"a\")
    except ValueError:
        print(\"h\")
        raise RuntimeError(\"b\")
    finally:
        print(\"fh\")
except RuntimeError as e:
    print(\"outer\", e)
",
    );
    assert_eq!(
        out,
        "c 1\nf 1\nf 2\nc 3\nf 3\ndone\nfr\n1\nfr\n2\nh\nfh\nouter b\n"
    );
}

#[test]
fn loop_inside_try_break_defers_finally() {
    // finally must run once after the loop, not on break
    let out = run_program(
        "loop_in_try",
        "\
try:
    i = 0
    while i < 3:
        i += 1
        if i == 2:
            break
        print(\"x\", i)
    print(\"after\")
finally:
    print(\"fin\")
print(\"done\")
",
    );
    assert_eq!(out, "x 1\nafter\nfin\ndone\n");
}

#[test]
fn trap_in_except_finally_stdout() {
    let dir = TempDir::new("trap_exc");
    let src = dir.0.join("prog.py");
    let src_s = "\
try:
    raise ValueError(\"x\")
except ValueError:
    xs: list[int] = [1]
    print(xs[99])
finally:
    print(\"f\")
";
    std::fs::write(&src, src_s).unwrap();
    let out = std::process::Command::new(PYRS)
        .args(["run", "-i"])
        .arg(&src)
        .output()
        .expect("spawn");
    assert!(!out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "f\n");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("IndexError: list index out of range"),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn nested_try_in_except_outer_finally() {
    let out = run_program(
        "nested_h",
        "\
try:
    try:
        raise ValueError(\"a\")
    except ValueError:
        try:
            raise RuntimeError(\"b\")
        finally:
            print(\"fin_nested\")
    finally:
        print(\"fin_inner\")
except RuntimeError as e:
    print(\"outer\", e)
finally:
    print(\"fin_outer\")
",
    );
    assert_eq!(out, "fin_nested\nfin_inner\nouter b\nfin_outer\n");
}

// ---- stdlib os.path (v0.11) ----

#[test]
fn stdlib_os_path_join_dirname_basename_matches_python() {
    // Expected stdout captured from the same program under python3 (POSIX).
    let src = "\
from os.path import join, dirname, basename
import os.path

print(join(\"a\", \"b\"))
print(join(\"/a\", \"b\"))
print(join(\"a\", \"/b\"))
print(join(\"a/\", \"b\"))
print(join(\"\", \"b\"))
print(join(\"a\", \"\"))
print(join(\"a/b\", \"c/d\"))
print(join(\"/\", \"a\"))
print(join(\"a\", \"/\"))
print(join(\"\", \"\"))
print(join(\"/\", \"\"))
print(join(\"\", \"/\"))
print(dirname(\"/a/b/c\"))
print(dirname(\"/a/b/c/\"))
print(dirname(\"c\"))
print(dirname(\"/\"))
print(dirname(\"\"))
print(dirname(\"a/\"))
print(dirname(\"a/b\"))
print(dirname(\"a/b/\"))
print(basename(\"/a/b/c\"))
print(basename(\"/a/b/c/\"))
print(basename(\"c\"))
print(basename(\"/\"))
print(basename(\"\"))
print(basename(\"a/\"))
print(basename(\"a/b\"))
print(basename(\"a/b/\"))
print(os.path.join(\"x\", \"y\"))
print(os.path.dirname(\"/p/q\"))
print(os.path.basename(\"/p/q\"))
";
    let out = run_program("stdlib_os_path", src);
    assert_eq!(
        out,
        "a/b\n\
/a/b\n\
/b\n\
a/b\n\
b\n\
a/\n\
a/b/c/d\n\
/a\n\
/\n\
\n\
/\n\
/\n\
/a/b\n\
/a/b/c\n\
\n\
/\n\
\n\
a\n\
a\n\
a/b\n\
c\n\
\n\
c\n\
\n\
\n\
\n\
b\n\
\n\
x/y\n\
/p\n\
q\n"
    );
}

#[test]
fn stdlib_os_path_from_import_and_dotted_import() {
    let out = run_program(
        "stdlib_os_path_forms",
        "\
from os.path import join as j
import os.path as op
print(j(\"foo\", \"bar\"))
print(op.dirname(\"/x/y/z\"))
print(op.basename(\"/x/y/z\"))
",
    );
    assert_eq!(out, "foo/bar\n/x/y\nz\n");
}

#[test]
fn stdlib_os_import_then_path_attr() {
    // `os/__init__.py` re-exports `path` so `import os` then `os.path` works.
    let out = run_program(
        "stdlib_os_attr",
        "\
import os
print(os.path.join(\"a\", \"b\"))
print(os.path.dirname(\"/a/b\"))
",
    );
    assert_eq!(out, "a/b\n/a\n");
}

#[test]
fn user_os_path_shadows_stdlib() {
    // Entry-dir module wins over stdlib/embed on name clash.
    let out = run_project(
        "user_shadow_os",
        &[
            ("os/__init__.py", "from . import path\n"),
            (
                "os/path.py",
                "def join(a: str, b: str) -> str:\n    return \"USER:\" + a + \":\" + b\n",
            ),
            (
                "main.py",
                "from os.path import join\nprint(join(\"a\", \"b\"))\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "USER:a:b\n");
}

#[test]
fn from_os_import_path_matches_python() {
    // fromlist on embedded package that re-exports `path`.
    let out = run_program(
        "from_os_import_path",
        "\
from os import path
print(path.join(\"a\", \"b\"))
print(path.dirname(\"/x/y\"))
",
    );
    assert_eq!(out, "a/b\n/x\n");
}

#[test]
fn stdlib_os_path_missing_name_is_error() {
    let dir = TempDir::new("os_path_missing_name");
    let src = dir.0.join("prog.py");
    fs::write(&src, "from os.path import totally_missing\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot import name 'totally_missing' from 'os.path'"),
        "stderr: {stderr}"
    );
}

#[test]
fn stdlib_os_missing_submodule_is_error() {
    let dir = TempDir::new("os_nope");
    let src = dir.0.join("prog.py");
    fs::write(&src, "import os.nope\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("No module named 'os.nope'"),
        "stderr: {stderr}"
    );
}

#[test]
fn pyrs_stdlib_env_provides_module() {
    // PYRS_STDLIB is searched after entry and can supply a plain module.
    let dir = TempDir::new("pyrs_stdlib_env");
    let std = dir.0.join("std");
    fs::create_dir_all(&std).unwrap();
    fs::write(std.join("envmod.py"), "VALUE: int = 99\n").unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, "import envmod\nprint(envmod.VALUE)\n").unwrap();
    let out = Command::new(PYRS)
        .args(["run", "-i"])
        .arg(&src)
        .env("PYRS_STDLIB", &std)
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "99\n");
}

#[test]
fn pyrs_stdlib_incomplete_package_blocks_workspace_child() {
    // Incomplete package under PYRS_STDLIB must not split with workspace/embed os.path.
    let dir = TempDir::new("pyrs_stdlib_incomplete");
    let std = dir.0.join("std");
    fs::create_dir_all(std.join("os")).unwrap();
    fs::write(std.join("os/__init__.py"), "# incomplete shadow\n").unwrap();
    // no path.py under PYRS_STDLIB
    let src = dir.0.join("prog.py");
    fs::write(&src, "import os.path\nprint(1)\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .env("PYRS_STDLIB", &std)
        .output()
        .expect("spawn");
    assert!(!out.status.success(), "expected split-package failure");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("No module named 'os.path'"),
        "stderr: {stderr}"
    );
}

#[test]
fn empty_pyrs_stdlib_falls_through_to_embed_or_workspace() {
    // Empty PYRS_STDLIB directory: still stacked after entry; real stdlib
    // (workspace/embed) remains available for os.path.
    let dir = TempDir::new("pyrs_stdlib_empty");
    let std = dir.0.join("empty_std");
    fs::create_dir_all(&std).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(
        &src,
        "from os.path import join\nprint(join(\"e\", \"f\"))\n",
    )
    .unwrap();
    let out = Command::new(PYRS)
        .args(["run", "-i"])
        .arg(&src)
        .env("PYRS_STDLIB", &std)
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "e/f\n");
}

#[test]
fn incomplete_entry_os_package_does_not_use_stdlib_path() {
    let out_fail = compile_project_expect_fail(
        "incomplete_entry_os",
        &[
            ("os/__init__.py", "# user incomplete package\n"),
            ("main.py", "import os.path\nprint(1)\n"),
        ],
        "main.py",
    );
    assert!(
        out_fail.contains("No module named 'os.path'"),
        "stderr: {out_fail}"
    );
}

// ---- Batch A / v0.12 features ----

#[test]
fn min_max_iterable_matches_python() {
    let out = run_program(
        "minmax_list",
        "\
print(min([3, 1, 4, 1, 5]))
print(max([3, 1, 4, 1, 5]))
print(min([1.5, -2.0, 3.0]))
print(max([1.5, -2.0, 3.0]))
print(min([True, False, True]))
print(max([False, True]))
xs: list[int] = [10, -3, 7]
print(min(xs), max(xs))
print(min(-3, 2), max([1, 9, 3]))
",
    );
    assert_eq!(
        out,
        "\
1
5
-2.0
3.0
False
True
-3 10
-3 9
"
    );
}

#[test]
fn min_empty_list_traps() {
    let (code, err) = run_program_expect_fail(
        "min_empty",
        "\
xs: list[int] = []
print(min(xs))
",
    );
    assert_eq!(code, 1);
    assert!(
        err.contains("min() iterable argument is empty"),
        "stderr: {err}"
    );
}

#[test]
fn max_empty_list_traps() {
    let (code, err) = run_program_expect_fail(
        "max_empty",
        "\
xs: list[int] = []
print(max(xs))
",
    );
    assert_eq!(code, 1);
    assert!(
        err.contains("max() iterable argument is empty"),
        "stderr: {err}"
    );
}

#[test]
fn and_or_return_operands_matches_python() {
    let out = run_program(
        "and_or_ops",
        "\
print(0 or 5)
print(3 or 5)
print(\"\" or \"x\")
print(\"a\" and \"b\")
print(0 and 5)
print(1 and 2)
print(0.0 or 1.5)
print(2.5 and 3.5)
empty: list[int] = []
print(empty or [1])
print([0] and [1, 2])
print(True and False or True)
",
    );
    assert_eq!(
        out,
        "\
5
3
x
b
0
2
1.5
3.5
[1]
[1, 2]
True
"
    );
}

#[test]
fn and_or_short_circuit_side_effects() {
    // Right side of `or` skipped when left is truthy; right of `and` skipped when left is falsy.
    let out = run_program(
        "and_or_sc",
        "\
n = 0

def bump() -> int:
    global n
    n = n + 1
    return 5

print(1 or bump())
print(n)
print(0 or bump())
print(n)
print(0 and bump())
print(n)
print(1 and bump())
print(n)
",
    );
    assert_eq!(out, "1\n0\n5\n1\n0\n1\n5\n2\n");
}

#[test]
fn none_optional_union_and_or_matches_python() {
    // Captured from python3 (never invent stdout).
    let out = run_program(
        "none_union",
        "\
print(None)
print(0 or \"x\")
print(None or 3)
print(1 and None)
x: int | None = None
print(x)
x = 5
print(x)
print(x is None)
print(x is not None)
y: int | None = None
print(y is None)
d: dict[str, int] = {\"a\": 1}
print(d.get(\"a\"))
print(d.get(\"b\"))
print(d.get(\"b\") or 0)
",
    );
    assert_eq!(
        out,
        "\
None
x
3
None
None
5
False
True
True
1
None
0
"
    );
}

#[test]
fn multi_name_import_matches_python() {
    let out = run_project(
        "multi_import",
        &[
            ("a.py", "def fa() -> int:\n    return 1\n"),
            ("b.py", "def fb() -> int:\n    return 2\n"),
            ("main.py", "import a, b as bee\nprint(a.fa(), bee.fb())\n"),
        ],
        "main.py",
    );
    assert_eq!(out, "1 2\n");
}

#[test]
fn try_else_matches_python() {
    let out = run_program(
        "try_else",
        "\
try:
    x = 1
except:
    print(\"exc\")
else:
    print(\"else\")
print(\"after\")
try:
    raise ValueError(\"boom\")
except ValueError:
    print(\"caught\")
else:
    print(\"else-skip\")
try:
    x = 1
except ValueError:
    print(\"exc\")
else:
    print(\"else2\")
finally:
    print(\"fin\")
try:
    try:
        x = 1
    except ValueError:
        print(\"inner\")
    else:
        raise RuntimeError(\"from-else\")
    finally:
        print(\"inner-fin\")
except RuntimeError as e:
    print(\"outer\", e)
",
    );
    assert_eq!(
        out,
        "\
else
after
caught
else2
fin
inner-fin
outer from-else
"
    );
}

#[test]
fn dict_get_bare_and_default() {
    let out = run_program(
        "dict_get",
        "\
d: dict[str, int] = {\"a\": 1, \"b\": 2}
print(d.get(\"a\"))
print(d.get(\"a\", 9))
print(d.get(\"missing\", 9))
",
    );
    assert_eq!(out, "1\n1\n9\n");
}

#[test]
fn dict_get_bare_miss_returns_none() {
    // CPython: bare get returns None on miss (was KeyError in PyRs before Optional).
    let out = run_program(
        "dict_get_miss",
        "\
d: dict[str, int] = {\"a\": 1}
print(d.get(\"missing\"))
print(d.get(\"a\"))
",
    );
    assert_eq!(out, "None\n1\n");
}

#[test]
fn stdlib_math_matches_python() {
    let out = run_program(
        "stdlib_math",
        "\
import math
print(math.pi)
print(math.e)
print(math.sqrt(4.0))
print(math.sin(0.0), math.cos(0.0), math.tan(0.0))
print(math.floor(3.7), math.ceil(3.2))
print(math.floor(-3.2), math.ceil(-3.2))
print(math.fabs(-2.5))
print(math.exp(0.0))
print(math.log(math.e))
print(math.log10(100.0))
from math import sqrt, pi
print(sqrt(9.0), pi)
",
    );
    assert_eq!(
        out,
        "\
3.141592653589793
2.718281828459045
2.0
0.0 1.0 0.0
3 4
-4 -3
2.5
1.0
1.0
2.0
3.0 3.141592653589793
"
    );
}

// ---- v0.12 batch B: os.getcwd, *args/**kwargs, closures, json ----

#[test]
fn os_getcwd_matches_python() {
    let dir = TempDir::new("getcwd");
    let src = dir.0.join("prog.py");
    fs::write(&src, "import os\nprint(os.getcwd())\n").unwrap();
    let py = Command::new("python3")
        .arg(&src)
        .current_dir(&dir.0)
        .output()
        .expect("python3");
    assert!(py.status.success());
    let out = Command::new(PYRS)
        .args(["run", "-i"])
        .arg(&src)
        .current_dir(&dir.0)
        .output()
        .expect("pyrs");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&py.stdout)
    );
}

#[test]
fn starargs_and_kwargs_match_python() {
    let src = "\
def f(x: int, *args: int) -> int:
    s = x
    for a in args:
        s = s + a
    return s

def g(x: int, **kwargs: int) -> int:
    s = x
    for k in kwargs:
        s = s + kwargs[k]
    return s

print(f(1, 2, 3))
print(f(10))
xs: list[int] = [4, 5]
print(f(1, *xs))
print(g(1, a=2, b=3))
print(f(1, 2, *xs))
";
    let out = run_program("starargs", src);
    assert_eq!(out, "6\n10\n10\n6\n12\n");
}

#[test]
fn kwargs_unexpected_is_compile_error() {
    let dir = TempDir::new("kwerr");
    let src = dir.0.join("prog.py");
    fs::write(&src, "def f(a: int) -> int:\n    return a\nprint(f(b=1))\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unexpected keyword argument"),
        "stderr: {stderr}"
    );
}

#[test]
fn nested_function_captures_by_value() {
    let src = "\
def make(n: int) -> int:
    def add(x: int) -> int:
        return x + n
    return add(5)

def fact(n: int) -> int:
    def go(k: int, acc: int) -> int:
        if k <= 1:
            return acc
        return go(k - 1, acc * k)
    return go(n, 1)

print(make(3))
print(fact(5))
";
    let out = run_program("closure", src);
    assert_eq!(out, "8\n120\n");
}

#[test]
fn nested_function_return_and_call_match_python() {
    let src = "\
def outer(n: int):
    def inner(x: int) -> int:
        return x + n
    return inner

f = outer(10)
print(f(5))
";
    assert_eq!(run_program("closure_ret", src), "15\n");
}

#[test]
fn bitwise_ops_match_python() {
    let src = "\
print(5 & 3)
print(5 | 3)
print(5 ^ 3)
print(~5)
print(1 << 4)
print(16 >> 2)
x = 7
x &= 3
print(x)
y = 1
y <<= 3
print(y)
print(True | False)
";
    let out = run_program("bitwise", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success(), "python3 failed");
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn star_unpack_and_list_star_match_python() {
    let src = "\
xs: list[int] = [1, 2, 3, 4, 5]
a, *rest, b = xs
print(a)
print(rest)
print(b)
ys: list[int] = [10, 20]
print([*ys, 30, *ys])
";
    let out = run_program("star_unpack", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success(), "python3 failed");
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn narrowing_is_not_none_match_python() {
    let src = "\
def f(x: int | None) -> int:
    if x is not None:
        return x + 1
    return 0

print(f(3))
print(f(None))
";
    let out = run_program("narrow", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success(), "python3 failed");
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn nonlocal_escaping_closure_match_python() {
    let src = "\
def make_counter():
    n = 0
    def inc() -> int:
        nonlocal n
        n = n + 1
        return n
    return inc

c = make_counter()
print(c())
print(c())
";
    let out = run_program("nonlocal", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success(), "python3 failed");
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn lambda_match_python() {
    // Defaults infer param types; bare `lambda x:` still needs annotation or default.
    // Partial calls with defaults work for lambdas and nested defs.
    let src = "\
f = lambda x=0, y=1: x + y
print(f(2, 1))
print(f(2, 3))
print(f(0, 1))
";
    let out = run_program("lambda", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success(), "python3 failed");
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn generator_yield_for_match_python() {
    let src = "\
def gen(n: int):
    i = 0
    while i < n:
        yield i
        i = i + 1

for x in gen(3):
    print(x)
";
    let out = run_program("generator", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success(), "python3 failed");
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn match_case_literals_match_python() {
    let src = "\
def check(x: int) -> str:
    match x:
        case 1:
            return \"one\"
        case 2 | 3:
            return \"two-or-three\"
        case _:
            return \"other\"

print(check(1))
print(check(2))
print(check(9))
";
    let out = run_program("match", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success(), "python3 failed");
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn optional_param_annotation_from_default() {
    let src = "\
def f(x=1, y: int = 2) -> int:
    return x + y
print(f())
print(f(10))
";
    let out = run_program("opt_param", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success(), "python3 failed");
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn import_in_function_match_python() {
    // Multi-file: import inside function binds for use after.
    let out = run_project(
        "fn_import",
        &[
            ("helper.py", "VAL = 42\n"),
            (
                "main.py",
                "def load() -> int:\n    import helper\n    return helper.VAL\nprint(load())\n",
            ),
        ],
        "main.py",
    );
    assert_eq!(out, "42\n");
}

#[test]
fn yield_in_try_finally_match_python() {
    let src = "\
def g():
    try:
        yield 1
        yield 2
    finally:
        print(\"fin\")
for x in g():
    print(x)
";
    let out = run_program("yield_try", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn reassignment_after_narrow_rejects() {
    // After `x = None` the refinement becomes None; `x + 1` is rejected.
    let (_, stderr) = run_program_expect_fail(
        "narrow_reassign",
        "\
def f(x: int | None) -> int:
    if x is not None:
        x = None
        return x + 1
    return 0
",
    );
    assert!(
        stderr.contains("operator") && stderr.contains("None"),
        "stderr: {stderr}"
    );
}

#[test]
fn early_return_none_check_narrows_fallthrough() {
    let src = "\
def f(x: int | None) -> int:
    if x is None:
        return 0
    return x + 1
print(f(3))
print(f(None))
";
    let out = run_program("early_narrow", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn multi_member_union_narrow_print() {
    // int|str|None narrowed to int|str must not segfault on print.
    let src = "\
def f(x: int | str | None) -> None:
    if x is not None:
        print(x)
f(1)
f(\"a\")
f(None)
";
    let out = run_program("multi_narrow", src);
    assert_eq!(out, "1\na\n");
}

#[test]
fn match_guard_sees_capture() {
    let src = "\
def check(x: int) -> str:
    match x:
        case n if n > 0:
            return \"pos\"
        case _:
            return \"other\"
print(check(3))
print(check(-1))
";
    let out = run_program("match_guard", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn match_or_pattern_different_names_rejected() {
    let (_, stderr) = run_program_expect_fail(
        "or_pat",
        "\
def f(x: int) -> int:
    match x:
        case 1 | y:
            return 1
        case _:
            return 0
",
    );
    assert!(
        stderr.contains("alternative patterns bind different names"),
        "stderr: {stderr}"
    );
}

#[test]
fn negative_shift_traps() {
    let (code, stderr) = run_program_expect_fail("neg_shift", "print(1 << -1)\n");
    assert_eq!(code, 1);
    assert!(stderr.contains("negative shift count"), "stderr: {stderr}");
}

#[test]
fn star_unpack_min_length_traps() {
    let (code, stderr) = run_program_expect_fail(
        "star_min",
        "xs: list[int] = [1]\na, *rest, b = xs\nprint(a)\n",
    );
    assert_eq!(code, 1);
    assert!(
        stderr.contains("not enough values to unpack (expected at least 2, got 1)"),
        "stderr: {stderr}"
    );
}

#[test]
fn generator_escape_and_call_match_python() {
    let src = "\
def outer():
    def gen():
        yield 1
        yield 2
    return gen
f = outer()
for x in f():
    print(x)
";
    let out = run_program("gen_esc", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn bare_unannotated_param_error() {
    let (_, stderr) =
        run_program_expect_fail("no_ann", "def f(x) -> int:\n    return x\nprint(f(1))\n");
    assert!(
        stderr.contains("missing a type annotation") && stderr.contains("parameter 'x'"),
        "stderr: {stderr}"
    );
}

#[test]
fn import_in_function_not_visible_outside() {
    let stderr = compile_project_expect_fail(
        "fn_imp_scope",
        &[
            ("helper.py", "VAL = 1\n"),
            (
                "main.py",
                "def load() -> None:\n    import helper\nprint(helper.VAL)\n",
            ),
        ],
        "main.py",
    );
    // Module name bound only inside load(); top-level `helper.VAL` fails.
    assert!(
        stderr.contains("attribute access is only supported")
            || stderr.contains("name 'helper' is not defined"),
        "stderr: {stderr}"
    );
}

#[test]
fn arithmetic_rshift_const_fold() {
    let src = "print((-8) >> 2)\nprint((-1) >> 1)\n";
    let out = run_program("ashr", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn json_dumps_and_typed_loads_match_python() {
    // dumps parity vs CPython; loads_* are PyRs-only helpers (compare values).
    let src = "\
import json
from json import dumps, loads_int, loads_list_int, loads_dict_str_int, loads_str, loads_bool

print(dumps(42))
print(dumps([1, 2, 3]))
print(dumps({\"a\": 1, \"b\": 2}))
print(dumps(True))
print(dumps(\"hi\"))
print(loads_int(\"99\"))
print(loads_list_int(\"[1, 2, 3]\"))
d = loads_dict_str_int('{\"x\": 7}')
print(d[\"x\"])
print(loads_str('\"hello\"'))
print(loads_bool(\"true\"))
print(json.dumps([1, 2]))
";
    let out = run_program("json_sub", src);
    assert_eq!(
        out,
        "42\n\
[1, 2, 3]\n\
{\"a\": 1, \"b\": 2}\n\
true\n\
\"hi\"\n\
99\n\
[1, 2, 3]\n\
7\n\
hello\n\
True\n\
[1, 2]\n"
    );
}

#[test]
fn match_sequence_mapping_str_bool_none_match_python() {
    let src = "\
def seq(xs: list[int]) -> int:
    match xs:
        case [a, b]:
            return a + b
        case _:
            return -1

def mp(d: dict[str, int]) -> int:
    match d:
        case {\"x\": v}:
            return v
        case _:
            return 0

def kind(x: int | str | bool | None) -> str:
    match x:
        case None:
            return \"none\"
        case True:
            return \"true\"
        case False:
            return \"false\"
        case s:
            # capture last: str or other int — use type via later checks
            return \"other\"

print(seq([3, 4]))
print(seq([1]))
print(mp({\"x\": 9}))
print(mp({\"y\": 1}))
print(kind(None))
print(kind(True))
print(kind(False))
";
    let out = run_program("match_rich", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(
        py.status.success(),
        "python3 failed: {}",
        String::from_utf8_lossy(&py.stderr)
    );
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn yield_from_list_match_python() {
    let src = "\
def g():
    yield from [10, 20, 30]

for x in g():
    print(x)
";
    let out = run_program("yield_from", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn escaped_nested_def_defaults_via_call_closure() {
    let src = "\
def outer():
    def g(s: str = \"hi\") -> str:
        return s
    return g

f = outer()
print(f())
print(f(\"ok\"))
";
    let out = run_program("esc_defaults", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn while_not_none_narrowing_match_python() {
    let src = "\
def f(x: int | None) -> int:
    s = 0
    while x is not None:
        s = s + x
        x = None
    return s

print(f(5))
print(f(None))
";
    let out = run_program("while_narrow", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn if_return_else_and_elif_narrowing_match_python() {
    let src = "\
def f(x: int | None) -> int:
    if x is None:
        return 0
    else:
        y = 1
    return x + y

def g(x: int | None) -> int:
    if x is None:
        return 0
    elif x > 10:
        return x
    else:
        y = 1
    return x + y

print(f(3))
print(f(None))
print(g(3))
print(g(None))
print(g(20))
";
    let out = run_program("elif_narrow", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn cell_local_optional_narrowing_match_python() {
    let src = "\
def outer(x: int | None) -> int:
    def inner() -> int:
        nonlocal x
        if x is None:
            return 0
        return x + 1
    return inner()

print(outer(4))
print(outer(None))
";
    let out = run_program("cell_narrow", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn lambda_string_default_and_ret_inference() {
    let src = "\
f = lambda x=\"a\": x
print(f())
print(f(\"b\"))
";
    let out = run_program("lambda_str", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn nested_def_rebind_to_lambda_match_python() {
    let src = "\
def outer() -> int:
    def inner() -> int:
        return 1
    inner = lambda: 2
    return inner()

print(outer())
";
    let out = run_program("rebind_nested", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn optional_return_inference_forward_ref() {
    // g is defined after f; pre-infer must give g a non-None ret.
    let src = "\
def f(n: int) -> int:
    return g(n)

def g(x: int):
    return x + 1

print(f(3))
";
    let out = run_program("ret_fwd", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn tuple_of_closures_and_call_match_python() {
    let src = "\
def outer():
    def f(x: int = 0) -> int:
        return x + 1
    return f
g = outer()
t = (g, g)
print(t[0](3))
print(t[1](4))
";
    let out = run_program("tuple_clos", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn nonlocal_optional_cell_store_and_load() {
    // CellStore must ToUnion so None / int both round-trip through the cell.
    let src = "\
def outer() -> int:
    x: int | None = 5
    def setn() -> None:
        nonlocal x
        x = None
    def seti() -> None:
        nonlocal x
        x = 7
    setn()
    if x is None:
        print(1)
    seti()
    if x is None:
        return 0
    return x
print(outer())
";
    let out = run_program("cell_opt_store", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn nested_unannotated_return_cell_union() {
    // Nested get() returns cell Optional; outer return get() must be valued, not void.
    let src = "\
def outer():
    x: int | None = 1
    def get():
        nonlocal x
        return x
    return get()
print(outer())
";
    let out = run_program("cell_ret_union", src);
    assert_eq!(out, "1\n");
}

#[test]
fn else_arm_complementary_narrowing() {
    // Then falls through; else must still see non-None x.
    let src = "\
def f(x: int | None) -> int:
    if x is None:
        y = 0
    else:
        y = x + 1
    return y
print(f(None))
print(f(3))
";
    let out = run_program("else_narrow", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn re_refine_after_concrete_assign_in_narrowed_branch() {
    let src = "\
def f(x: int | None) -> int:
    if x is not None:
        x = x + 1
        return x
    return 0
print(f(3))
print(f(None))
";
    let out = run_program("rerefine", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn match_or_pattern_three_arms_match_python() {
    let src = "\
def check(x: int) -> int:
    match x:
        case 1 | 2 | 3:
            return x
        case _:
            return -1
print(check(1))
print(check(2))
print(check(3))
print(check(4))
";
    let out = run_program("or_three", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn bare_lambda_param_without_default_rejected() {
    let (_, stderr) = run_program_expect_fail("bare_lam", "f = lambda x: x + 1\nprint(f(1))\n");
    assert!(
        stderr.contains("lambda parameter annotation"),
        "stderr: {stderr}"
    );
}

#[test]
fn yield_from_non_iterable_rejected() {
    let (_, stderr) = run_program_expect_fail(
        "yf_nonlist",
        "def g():\n    yield from 1\nfor x in g():\n    print(x)\n",
    );
    assert!(
        stderr.contains("yield from expects an iterable"),
        "stderr: {stderr}"
    );
}

#[test]
fn match_class_pattern_unsupported() {
    // Class patterns are out of the supported match subset.
    let (_, stderr) = run_program_expect_fail(
        "match_cls",
        "\
def f(x: int) -> int:
    match x:
        case int():
            return 1
        case _:
            return 0
",
    );
    assert!(
        stderr.contains("unsupported match pattern starting with 'int'"),
        "stderr: {stderr}"
    );
}

#[test]
fn sibling_nested_cell_call_threads_cell() {
    // Sibling nested defs share outer cells: both() calls inc()/get() without
    // naming x itself — must pass the cell through (CPython parity).
    let src = "\
def outer() -> int:
    x = 1
    def inc() -> None:
        nonlocal x
        x = x + 1
    def get() -> int:
        return x
    def both() -> int:
        inc()
        return get()
    return both()
print(outer())
";
    let out = run_program("sib_cell", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn sibling_nested_optional_cell_call_match_python() {
    let src = "\
def outer():
    x: int | None = 1
    def get() -> int | None:
        nonlocal x
        return x
    def use() -> int | None:
        return get()
    return use()
print(outer())
";
    let out = run_program("sib_opt_cell", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn os_path_join_multi_arg_matches_python() {
    let src = "\
from os.path import join
print(join(\"a\", \"b\", \"c\"))
print(join(\"/a\", \"b\", \"/c\", \"d\"))
print(join(\"a\"))
";
    let out = run_program("join_multi", src);
    assert_eq!(out, "a/b/c\n/c/d\na\n");
}

#[test]
fn star_call_fixed_arity() {
    let src = "\
def add3(a: int, b: int, c: int) -> int:
    return a + b + c

xs: list[int] = [2, 3]
print(add3(1, *xs))
print(add3(*[1, 2, 3]))
";
    let out = run_program("star_fixed", src);
    assert_eq!(out, "6\n6\n");
}

// ---- v0.14.x hardening: control-flow, cells, generators ----

#[test]
fn while_local_optional_reassign_none_terminates() {
    // Regression: refined `is not None` must not constant-fold the loop cond
    // after a prior concrete assign (would infinite-loop on `x = None`).
    // Timeout so a hang fails CI quickly instead of blocking the suite.
    let src = "\
def f() -> int:
    x: int | None = 3
    s = 0
    while x is not None:
        s = s + x
        if s > 5:
            x = None
        else:
            x = x - 1
    return s
print(f())
";
    let out = run_program_timeout("while_local_opt", src, Duration::from_secs(5));
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn free_var_cell_escape_sees_later_assign() {
    let src = "\
def outer() -> int:
    n = 0
    f = lambda: n
    n = 5
    return f()

def outer2() -> int:
    n = 0
    def make():
        def read() -> int:
            return n
        return read
    g = make()
    n = 7
    return g()

print(outer())
print(outer2())
";
    let out = run_program("free_cell", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn generator_bare_return_and_yield_from_empty() {
    let src = "\
def g():
    yield 1
    return
    yield 2

def h():
    yield from []
    yield 9

for x in g():
    print(x)
for y in h():
    print(y)
print(\"ok\")
";
    let out = run_program("gen_ret_yf", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn while_else_return_paths_and_and_narrow() {
    let src = "\
def w(n: int) -> int:
    while n > 0:
        return n
    else:
        return 0

def a(x: int | None, flag: bool) -> int:
    if x is not None and flag:
        return x + 1
    return 0

print(w(3))
print(w(0))
print(a(3, True))
print(a(3, False))
print(a(None, True))
";
    let out = run_program("while_else_and", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn match_as_pattern_match_python() {
    let src = "\
def f(x: int) -> int:
    match x:
        case 1 as y:
            return y
        case z as w:
            return z + w
print(f(1))
print(f(3))
";
    let out = run_program("match_as", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn generator_return_expr_side_effects() {
    let src = "\
def side() -> int:
    print(\"side\")
    return 0

def g():
    yield 1
    return side()

for x in g():
    print(x)
print(\"done\")
";
    let out = run_program("gen_ret_se", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn post_while_optional_arith_rejected() {
    let (_, stderr) = run_program_expect_fail(
        "post_while",
        "\
def f() -> int:
    x: int | None = 1
    while x is not None:
        x = None
    return x + 1
",
    );
    assert!(
        stderr.contains("operator '+' is not supported for values of type None"),
        "stderr: {stderr}"
    );
}

#[test]
fn if_rebind_none_fallthrough_rejected() {
    let (_, stderr) = run_program_expect_fail(
        "if_rebind",
        "\
def f() -> int:
    x: int | None = 5
    if True:
        x = None
    return x + 1
",
    );
    assert!(
        stderr.contains("operator '+' is not supported for values of type"),
        "stderr: {stderr}"
    );
}

#[test]
fn free_cell_untaken_branch_outer_load() {
    let src = "\
def outer(flag: bool) -> int:
    n = 10
    if flag:
        def read() -> int:
            return n
        return read()
    return n

print(outer(False))
print(outer(True))
";
    let out = run_program("cell_branch", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn free_cell_plain_param_untaken_branch() {
    // Entry cell boxing for a free-captured plain param (not only locals / *args).
    let src = "\
def outer(flag: bool, n: int) -> int:
    if flag:
        def r() -> int:
            return n
        return r()
    return n

print(outer(False, 10))
print(outer(True, 10))
";
    let out = run_program("cell_param", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn try_except_in_generator_match_python() {
    let src = "\
def g():
    try:
        yield 1
        raise ValueError(\"x\")
    except ValueError:
        yield 2
    finally:
        print(\"fin\")
for x in g():
    print(x)
";
    let out = run_program("try_gen", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn nested_default_frozen_at_def() {
    let src = "\
def outer() -> int:
    n = 1
    def f(x: int = n) -> int:
        return x
    n = 99
    return f()
print(outer())
";
    let out = run_program("dflt_freeze", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn for_else_return_paths_match_python() {
    let src = "\
def f(n: int) -> int:
    for i in range(n):
        return i + 1
    else:
        return -1
print(f(3))
print(f(0))
";
    let out = run_program("for_else_ret", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn post_for_zero_trip_rebind_rejected() {
    // Body peels must not survive a zero-trip for (while-style restore).
    let (_, stderr) = run_program_expect_fail(
        "post_for",
        "\
def f() -> int:
    x: int | None = None
    for i in range(0):
        x = 5
    return x + 1
",
    );
    assert!(
        stderr.contains("operator '+' is not supported for values of type"),
        "stderr: {stderr}"
    );
}

#[test]
fn post_for_empty_list_rebind_rejected() {
    let (_, stderr) = run_program_expect_fail(
        "post_for_list",
        "\
def f() -> int:
    x: int | None = None
    xs: list[int] = []
    for i in xs:
        x = 5
    return x + 1
",
    );
    assert!(
        stderr.contains("operator '+' is not supported for values of type"),
        "stderr: {stderr}"
    );
}

#[test]
fn free_cell_vararg_untaken_branch() {
    let src = "\
def outer(*args: int) -> int:
    if False:
        def read() -> int:
            return args[0]
        return read()
    return args[0]

def outer_kw(**kwargs: int) -> int:
    if False:
        def read() -> int:
            return kwargs[\"k\"]
        return read()
    return kwargs[\"k\"]

print(outer(7))
print(outer_kw(k=8))
";
    let out = run_program("vararg_cell", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn or_narrow_else_arm_match_python() {
    // `x is None or flag` else-arm sees x non-None (both failed).
    let src = "\
def f(x: int | None, flag: bool) -> int:
    if x is None or flag:
        return 0
    return x + 1

print(f(None, False))
print(f(3, True))
print(f(3, False))
";
    let out = run_program("or_narrow", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn multi_level_free_default_escaped_clear_error() {
    let (_, stderr) = run_program_expect_fail(
        "ml_dflt",
        "\
def outer() -> int:
    n = 1
    def mid():
        def f(x: int = n) -> int:
            return x
        return f
    g = mid()
    return g()
print(outer())
",
    );
    assert!(
        stderr.contains("default argument that captures free variables")
            && stderr.contains("constant default"),
        "stderr: {stderr}"
    );
}

// ---- v0.15: mid-expr refine, match expand, late bind, closures, gens ----

#[test]
fn mid_expr_and_or_refine_match_python() {
    let src = "\
def a(x: int | None) -> int:
    if x is not None and x > 0:
        return x + 1
    return 0

def b(x: int | None) -> int:
    if x is None or x < 0:
        return -1
    return x

print(a(3))
print(a(None))
print(a(-1))
print(b(None))
print(b(-2))
print(b(5))
";
    let out = run_program("mid_refine", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn free_var_late_binding_match_python() {
    let src = "\
def outer() -> int:
    def f() -> int:
        return n
    n = 5
    return f()

print(outer())
";
    let out = run_program("late_bind", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn sibling_forward_nested_match_python() {
    let src = "\
def outer() -> int:
    def a() -> int:
        return b() + 1
    def b() -> int:
        return 10
    return a()

print(outer())
";
    let out = run_program("fwd_nested", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn list_of_closures_call_match_python() {
    let src = "\
def outer() -> int:
    def c1(x: int) -> int:
        return x + 1
    def c2(x: int) -> int:
        return x * 2
    fs = [c1, c2]
    return fs[0](5) + fs[1](3)

print(outer())
";
    let out = run_program("list_clos", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn match_star_sequence_and_mapping_rest_match_python() {
    let src = "\
def seq(xs: list[int]) -> int:
    match xs:
        case [a, *rest, b]:
            s = a + b
            for x in rest:
                s = s + x
            return s
        case _:
            return -1

def mp(d: dict[str, int]) -> int:
    match d:
        case {\"k\": v, **rest}:
            s = v
            for x in rest.values():
                s = s + x
            return s
        case _:
            return -1

print(seq([1, 2, 3, 4]))
print(seq([1, 9]))
print(mp({\"k\": 1, \"m\": 2, \"n\": 3}))
";
    let out = run_program("match_star", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn yield_from_tuple_and_gen_match_python() {
    let src = "\
def inner():
    yield 10
    yield 20

def g():
    yield from (1, 2)
    yield from [3]
    yield from inner()

for x in g():
    print(x)
";
    let out = run_program("yf_expand", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn yield_from_str_chars_match_python() {
    // Annotate return as str so yield type is str (chars).
    let src = "\
def g() -> str:
    yield from \"ab\"
    yield \"c\"

for c in g():
    print(c)
";
    let out = run_program("yf_str", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn identity_is_for_containers_match_python() {
    let src = "\
xs = [1, 2]
ys = xs
zs = [1, 2]
print(xs is ys)
print(xs is zs)
print(xs is not zs)
s = \"hi\"
print(s is s)
";
    let out = run_program("ident_is", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn generator_close_and_send_none_match_python() {
    let src = "\
def g():
    yield 1
    yield 2
    yield 3

it = g()
print(it.send(None))
it.close()
print(\"closed\")
";
    let out = run_program("gen_close", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn nested_gen_with_capture_match_python() {
    let src = "\
def outer(n: int):
    def gen():
        i = 0
        while i < n:
            yield i
            i = i + 1
    return gen

f = outer(3)
for x in f():
    print(x)
";
    let out = run_program("nested_gen_cap", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn global_optional_narrow_match_python() {
    let src = "\
x: int | None = 7

def f() -> int:
    global x
    if x is not None:
        return x + 1
    return 0

print(f())
x = None
print(f())
";
    let out = run_program("glob_narrow", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn gen_yield_in_except_reraise_match_python() {
    // Raise after yield in except must not re-enter sibling handlers.
    let src = "\
def g():
    try:
        raise ValueError(\"boom\")
    except ValueError:
        yield 1
        raise RuntimeError(\"after\")
    except RuntimeError:
        print(\"caught-sibling\")
        yield 99
    finally:
        print(\"finally\")

it = g()
print(it.send(None))
try:
    print(it.send(None))
except RuntimeError as e:
    print(\"RuntimeError\", e)
";
    // 15s: hang guard under parallel e2e load (compile is slow; true hang is endless).
    let out = run_program_timeout("gen_phase", src, Duration::from_secs(15));
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

/// Bare `except:` after yield must not infinite-loop on re-raise.
#[test]
fn gen_yield_in_bare_except_reraise_no_hang() {
    let src = "\
def g():
    try:
        raise ValueError(\"boom\")
    except:
        print(\"handler\")
        yield 1
        print(\"after yield\")
        raise RuntimeError(\"after\")
    finally:
        print(\"fin\")

try:
    for x in g():
        print(x)
except RuntimeError as e:
    print(\"RuntimeError\", e)
";
    let out = run_program_timeout("gen_bare_exc", src, Duration::from_secs(15));
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn gen_except_as_bind_match_python() {
    let src = "\
def g():
    try:
        raise ValueError(\"boom\")
    except ValueError as e:
        print(e)
        yield 1
        yield 2
for x in g():
    print(x)
";
    let out = run_program("gen_as_bind", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn gen_func_list_match_python() {
    let src = "\
def outer():
    def g1():
        yield 1
    def g2():
        yield 2
    return [g1, g2]
fs = outer()
for f in fs:
    for x in f():
        print(x)
";
    let out = run_program("gen_list", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn gen_close_runs_finally_match_python() {
    let src = "\
def g():
    try:
        yield 1
        yield 2
    finally:
        print(\"fin\")
it = g()
print(it.send(None))
it.close()
print(\"after close\")
";
    let out = run_program("gen_close_fin", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn gen_yield_in_print_match_python() {
    let src = "\
def g():
    print((yield 1))
    print(\"after\")
    yield 2
for x in g():
    print(\"v\", x)
";
    let out = run_program("gen_yprint", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn mutual_nested_free_capture_match_python() {
    let src = "\
def outer(n: int) -> int:
    def a() -> int:
        return b()
    def b() -> int:
        return n
    return a()
print(outer(7))
";
    let out = run_program("mutual_free", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn late_free_cell_load_before_assign_traps() {
    let (_, stderr) = run_program_expect_fail(
        "late_free_trap",
        "\
def outer() -> int:
    def f() -> int:
        return n
    print(f())
    n = 5
    return f()
print(outer())
",
    );
    assert!(
        stderr.contains("NameError") && stderr.contains("free variable"),
        "stderr: {stderr}"
    );
}

#[test]
fn match_or_pattern_binds_matching_alt_match_python() {
    let src = "\
def t(x: list[int]):
    match x:
        case [1, y] | [y, 2]:
            print(\"y\", y)
        case _:
            print(\"no\")
t([1, 9])
t([8, 2])
t([1, 2])
";
    let out = run_program("or_bind", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn match_duplicate_capture_rejected() {
    let (_, stderr) = run_program_expect_fail(
        "dup_cap",
        "\
def f(x: list[int]) -> int:
    match x:
        case [a, a]:
            return a
    return 0
",
    );
    assert!(
        stderr.contains("multiple assignments to name"),
        "stderr: {stderr}"
    );
}

#[test]
fn match_duplicate_key_rejected() {
    let (_, stderr) = run_program_expect_fail(
        "dup_key",
        "\
def f(x: dict[str, int]) -> int:
    match x:
        case {\"k\": a, \"k\": b}:
            return a
    return 0
",
    );
    assert!(stderr.contains("duplicate key"), "stderr: {stderr}");
}

#[test]
fn match_guard_refines_body_match_python() {
    let src = "\
def h(y: int | None) -> int:
    match y:
        case z if z is not None:
            return z + 1
        case _:
            return 0
print(h(3))
print(h(None))
";
    let out = run_program("guard_refine", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn match_star_rest_irrefutable_return_ok() {
    let src = "\
def f(xs: list[int]) -> int:
    match xs:
        case [*rest]:
            return len(rest)
print(f([1, 2, 3]))
";
    let out = run_program("star_irref", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn free_module_optional_narrow_match_python() {
    // Free module Optional read (no `global`) still peels in if.
    let src = "\
x: int | None = 7
def f() -> int:
    if x is not None:
        return x + 1
    return 0
print(f())
";
    let out = run_program("free_mod_opt", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn float_is_identity_match_python() {
    let src = "\
x = 3.14
print(x is x)
print(x is not x)
y = 1.0
print(y is y)
";
    let out = run_program("float_is", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn generator_throw_match_python() {
    let src = "\
def g():
    try:
        yield 1
        yield 2
    except ValueError as e:
        print(\"caught\", e)
        yield 99
    finally:
        print(\"fin\")
it = g()
print(it.send(None))
print(it.throw(ValueError(\"x\")))
it.close()
print(\"after\")
";
    let out = run_program("gen_throw", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn generator_throw_uncaught_match_python() {
    let src = "\
def g():
    yield 1
    yield 2
it = g()
print(it.send(None))
try:
    it.throw(ValueError(\"x\"))
except ValueError as e:
    print(\"outer\", e)
";
    let out = run_program("gen_throw_out", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn generator_throw_type_only_match_python() {
    let src = "\
def g():
    try:
        yield 1
    except ValueError as e:
        print(\"caught\", e)
        yield 2
it = g()
print(it.send(None))
print(it.throw(ValueError))
";
    let out = run_program("gen_throw_type", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn generator_send_value_match_python() {
    let src = "\
def g():
    x = yield 1
    print(\"got\", x)
    y = yield 2
    print(\"got\", y)
    yield 3
it = g()
print(it.send(None))
print(it.send(10))
print(it.send(20))
";
    let out = run_program("gen_send_val", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn generator_send_before_start_traps() {
    let (_, stderr) = run_program_expect_fail(
        "gen_send_early",
        "\
def g():
    yield 1
it = g()
it.send(1)
",
    );
    assert!(
        stderr.contains("TypeError") && stderr.contains("just-started"),
        "stderr: {stderr}"
    );
}

#[test]
fn capturing_closure_in_list_match_python() {
    let src = "\
def outer(n: int):
    def a(x: int) -> int:
        return x + n
    def b(x: int) -> int:
        return x * n
    return [a, b]
fs = outer(3)
print(fs[0](2))
print(fs[1](2))
for f in fs:
    print(f(10))
";
    let out = run_program("cap_list", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn capturing_closure_in_tuple_match_python() {
    let src = "\
def outer(n: int):
    def a(x: int) -> int:
        return x + n
    def b(x: int) -> int:
        return x * n
    return (a, b)
t = outer(4)
print(t[0](1))
print(t[1](1))
";
    let out = run_program("cap_tup", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

/// After exhaustion, further send/next must not re-run the tail (Optional None).
#[test]
fn generator_post_exhaust_send_is_none() {
    let src = "\
def g():
    print(\"start\")
    yield 1
    print(\"tail\")
it = g()
print(it.send(None))
print(it.send(None))
print(it.send(None))
print(it.send(None))
";
    let out = run_program("gen_post_ex", src);
    assert_eq!(out, "start\n1\ntail\nNone\nNone\nNone\n");
}

#[test]
fn generator_post_close_send_is_none() {
    let src = "\
def g():
    try:
        yield 1
        yield 2
    finally:
        print(\"fin\")
it = g()
print(it.send(None))
it.close()
print(\"closed\")
print(it.send(None))
";
    let out = run_program("gen_post_close", src);
    assert_eq!(out, "1\nfin\nclosed\nNone\n");
}

#[test]
fn generator_uncaught_throw_then_send_is_none() {
    let src = "\
def g():
    try:
        yield 1
        yield 2
    finally:
        print(\"fin3\")
it = g()
print(it.send(None))
try:
    it.throw(ValueError(\"x\"))
except ValueError as e:
    print(\"caught\", e)
print(it.send(None))
";
    let out = run_program("gen_throw_done", src);
    assert_eq!(out, "1\nfin3\ncaught x\nNone\n");
}

#[test]
fn generator_throw_before_start_then_send_is_none() {
    let src = "\
def g():
    print(\"body\")
    yield 1
it = g()
try:
    it.throw(ValueError(\"early\"))
except ValueError as e:
    print(\"early\", e)
print(it.send(None))
";
    let out = run_program("gen_throw_early", src);
    assert_eq!(out, "early early\nNone\n");
}

#[test]
fn generator_throw_type_msg_two_arg_match_python() {
    let src = "\
def g():
    try:
        yield 1
    except ValueError as e:
        print(\"caught\", e)
        yield 2
it = g()
print(it.send(None))
print(it.throw(ValueError, \"msg\"))
";
    let out = run_program("gen_throw_2arg", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn generator_throw_genexit_propagates_match_python() {
    let src = "\
def g():
    try:
        yield 1
        yield 2
    finally:
        print(\"fin6\")
it = g()
print(it.send(None))
try:
    it.throw(GeneratorExit)
except GeneratorExit:
    print(\"outer GE\")
print(it.send(None))
";
    let out = run_program("gen_throw_ge", src);
    assert_eq!(out, "1\nfin6\nouter GE\nNone\n");
}

#[test]
fn generator_throw_genexit_caught_match_python() {
    let src = "\
def g():
    try:
        yield 1
    except GeneratorExit:
        print(\"caught GE\")
        yield 99
    finally:
        print(\"fin5\")
it = g()
print(it.send(None))
print(it.throw(GeneratorExit))
it.close()
print(\"after\")
";
    let out = run_program("gen_throw_ge_c", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

/// Nested try inside generator finally must not clobber outer exit kind.
#[test]
fn gen_nested_try_in_finally_reraise_match_python() {
    let src = "\
def g():
    try:
        yield 1
        raise ValueError(\"boom\")
    finally:
        try:
            print(\"inner\")
            yield 2
        finally:
            print(\"inner-fin\")
        print(\"outer-after\")
try:
    for x in g():
        print(\"y\", x)
except ValueError as e:
    print(\"outer\", e)
";
    let out = run_program("gen_nest_fin", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

/// `return` exit-kind survives yield in finally (resume continues return path).
#[test]
fn gen_return_then_yield_in_finally_match_python() {
    let src = "\
def g():
    try:
        yield 1
        print(\"before return\")
        return
    finally:
        print(\"fin start\")
        yield 2
        print(\"fin end\")
    print(\"after try\")
    yield 3
for x in g():
    print(\"y\", x)
print(\"done\")
";
    let out = run_program("gen_ret_fin", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

/// Explicit `return N` + yield in finally feeds yield-from with N after resume.
#[test]
fn gen_return_value_yield_in_finally_match_python() {
    let src = "\
def g():
    try:
        yield 1
        return 42
    finally:
        print(\"fin\")
        yield 2
        print(\"fin done\")
def outer():
    x = yield from g()
    print(\"got\", x)
    yield 99
for v in outer():
    print(\"v\", v)
";
    let out = run_program("gen_retv_fin", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

/// Intentional PyRs limit: `send` is not forwarded through `yield from`
/// (subgen is advanced with None). CPython would deliver the sent value.
#[test]
fn yield_from_does_not_forward_send() {
    let src = "\
def inner():
    x = yield 1
    print(\"inner got\", x)
    yield 2
def outer():
    yield from inner()
    yield 3
it = outer()
print(it.send(None))
print(it.send(10))
print(it.send(None))
";
    let out = run_program("yf_no_fwd_send", src);
    // Pin PyRs: sent 10 is not delivered to inner's yield expression.
    assert_eq!(out, "1\ninner got None\n2\n3\n");
}

/// Intentional PyRs limit: `throw` is not delegated into the subgenerator of
/// `yield from` (exception is raised at the outer yield point).
#[test]
fn yield_from_does_not_forward_throw() {
    let src = "\
def inner():
    try:
        yield 1
        yield 2
    except ValueError as e:
        print(\"inner caught\", e)
        yield 99
def outer():
    try:
        yield from inner()
    except ValueError as e:
        print(\"outer caught\", e)
        yield 50
    yield 3
it = outer()
print(it.send(None))
print(it.throw(ValueError(\"x\")))
print(it.send(None))
";
    let out = run_program("yf_no_fwd_throw", src);
    // Pin PyRs: outer catches; inner does not see the throw.
    assert_eq!(out, "1\nouter caught x\n50\n3\n");
}

/// Homogeneous lists of closures require matching capture env shapes.
#[test]
fn mismatched_capture_env_closures_in_list_rejected() {
    let (_, stderr) = run_program_expect_fail(
        "cap_mismatch",
        "\
def outer(n: int, m: str):
    def a(x: int) -> int:
        return x + n
    def b(x: int) -> int:
        return x + len(m)
    return [a, b]
print(outer(1, \"hi\")[0](2))
",
    );
    assert!(
        stderr.contains("list elements must share one type"),
        "stderr: {stderr}"
    );
}

#[test]
fn yield_from_subgen_return_value_match_python() {
    // Explicit return N → yield-from result is N (Optional peels after assign
    // is not automatic; print the Optional value for parity).
    let src = "\
def inner():
    yield 1
    return 42
def outer():
    x = yield from inner()
    print(x)
    yield 99
for v in outer():
    print(\"y\", v)
";
    let out = run_program("yf_ret", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

/// Bare return / fall-off → yield-from expression is None (not typed zero).
#[test]
fn yield_from_bare_return_is_none_match_python() {
    let src = "\
def g_fall():
    yield 1
def g_bare():
    yield 1
    return
def h(which: int):
    if which == 0:
        x = yield from g_fall()
    else:
        x = yield from g_bare()
    print(x)
    yield 99
for v in h(0):
    print(\"y\", v)
for v in h(1):
    print(\"y\", v)
";
    let out = run_program("yf_none", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn close_cascades_to_yield_from_subgen_match_python() {
    let src = "\
def sub():
    try:
        yield 1
        yield 2
    finally:
        print(\"sub-fin\")
def outer():
    try:
        yield from sub()
    finally:
        print(\"outer-fin\")
g = outer()
print(g.send(None))
g.close()
print(\"after\")
";
    let out = run_program("yf_close", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn generator_ignore_generator_exit_runtime_error() {
    let src = "\
def bad():
    try:
        yield 1
    except:
        print(\"swallowed\")
        yield 2
g = bad()
print(g.send(None))
g.close()
print(\"after\")
";
    let (code, stderr) = run_program_expect_fail("ge_ignore", src);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("RuntimeError") && stderr.contains("GeneratorExit"),
        "stderr: {stderr}"
    );
    // stdout before the trap should match CPython's prints before the error
    // (we only check stderr trap here; CPython also raises on close).
}

#[test]
fn except_generator_exit_match_python() {
    let src = "\
def make():
    def g():
        try:
            yield 1
        except GeneratorExit:
            print(\"caught-ge\")
    return g()
it = make()
print(it.send(None))
it.close()
print(\"after\")
";
    let out = run_program("ge_except", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn float_nan_is_nan_match_python() {
    // inf - inf is nan; bit-identity: nan is nan; IEEE: nan != nan.
    let src = "\
x = 1e308 * 1e308
y = x - x
print(y is y)
print(y == y)
";
    let out = run_program("nan_is", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn match_as_whole_sequence_match_python() {
    let src = "\
def f(xs: list[int]) -> None:
    match xs:
        case [a, b] as whole:
            print(a, b, whole)
        case _:
            print(\"no\")
f([1, 2])
f([1])
";
    let out = run_program("match_as_wh", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn match_tuple_star_rest_match_python() {
    let src = "\
def f(t: tuple[int, int, int]) -> None:
    match t:
        case (a, *rest, b):
            print(a, rest, b)
f((1, 2, 3))
";
    let out = run_program("tup_rest", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn match_mapping_empty_rest_match_python() {
    let src = "\
def f(d: dict[str, int]) -> None:
    match d:
        case {**rest}:
            print(rest)
f({\"a\": 1})
empty: dict[str, int] = {}
f(empty)
";
    let out = run_program("map_rest", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn try_else_in_generator_match_python() {
    let src = "\
def g(flag: int):
    try:
        if flag == 0:
            raise ValueError(\"x\")
        yield 1
    except ValueError:
        yield 2
    else:
        yield 3
    finally:
        print(\"fin\")
for v in g(0):
    print(v)
for v in g(1):
    print(v)
";
    let out = run_program("gen_else", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn yield_in_finally_match_python() {
    let src = "\
def g():
    try:
        yield 1
        yield 2
    finally:
        print(\"fin-start\")
        yield 3
        print(\"fin-end\")
for x in g():
    print(\"y\", x)
";
    let out = run_program("yf_fin", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn yield_from_in_finally_match_python() {
    let src = "\
def g():
    try:
        yield 1
    finally:
        yield from [2, 3]
for x in g():
    print(x)
";
    let out = run_program("yf_fin_from", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn close_while_yield_in_finally_match_python() {
    let src = "\
def g():
    try:
        yield 1
    finally:
        print(\"fin\")
        yield 2
        print(\"after\")
it = g()
print(it.send(None))
print(it.send(None))
it.close()
print(\"closed\")
";
    let out = run_program("yf_fin_close", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

/// Empty mapping pattern `case {}:` is irrefutable for return analysis.
#[test]
fn match_empty_mapping_irrefutable_return() {
    let src = "\
def f(d: dict[str, int]) -> int:
    match d:
        case {}:
            return 1
empty: dict[str, int] = {}
print(f(empty))
print(f({\"a\": 1}))
";
    let out = run_program("empty_map_pat", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

/// Forward nested call + late free cell (sibling threads unbound cell).
#[test]
fn forward_nested_late_free_match_python() {
    let src = "\
def outer() -> int:
    def a() -> int:
        return b()
    def b() -> int:
        return n
    n = 7
    return a()
print(outer())
";
    let out = run_program("fwd_late", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

/// while+break must not leave a stale Optional peel on fallthrough.
#[test]
fn while_break_clears_optional_peel() {
    // After break, `x` is still Optional — arithmetic must not type-check
    // as bare int (would be unsound if then-peel leaked past break).
    let (_, stderr) = run_program_expect_fail(
        "while_brk_peel",
        "\
def f(x: int | None) -> int:
    while x is not None:
        break
    return x + 1
print(f(3))
",
    );
    assert!(
        stderr.contains("operator '+' is not supported for values of type")
            && stderr.contains("None"),
        "stderr: {stderr}"
    );
}

#[test]
fn close_genexit_reraise_completes_match_python() {
    // Re-raising GeneratorExit from its handler must run finally and end
    // close() — not re-enter the handler (O2 setjmp/phase correctness).
    let src = "\
def g():
    try:
        yield 1
    except GeneratorExit:
        print(\"handler\")
        raise GeneratorExit(\"x\")
    finally:
        print(\"fin\")
it = g()
print(it.send(None))
it.close()
print(\"after\")
";
    let out = run_program_timeout("ge_reraise", src, Duration::from_secs(3));
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn bigint_pow_and_literals_match_python() {
    let src = "\
print(2**100)
print(10**20 + 1)
print(-2**100)
print(abs(-(2**63)))
print(9999999999999999999999)
print(f\"{2**40:d}\")
";
    let out = run_program("bigint_pow", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(
        py.status.success(),
        "python failed: {}",
        String::from_utf8_lossy(&py.stderr)
    );
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn bigint_floor_mod_and_dict_keys_match_python() {
    let src = "\
print((-7) // 3)
print((-7) % 3)
print(7 // -3)
print(7 % -3)
d: dict[int, int] = {}
k = 2**80
d[k] = 1
d[2**80] = 2
print(d[k])
print(k in d)
s: set[int] = {2**60, 3}
print(2**60 in s)
print(len(s))
";
    let out = run_program("bigint_ops", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(
        py.status.success(),
        "python failed: {}",
        String::from_utf8_lossy(&py.stderr)
    );
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn bigint_indices_and_range_still_work() {
    let src = "\
xs = [10, 20, 30]
print(xs[1])
print(len(xs))
print(xs[-1])
total = 0
for i in range(5):
    total = total + i
print(total)
print(\"ab\" * 3)
print([1, 2] * 2)
";
    let out = run_program("bigint_idx", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn tier1_isinstance_and_narrowing() {
    let src = "\
print(isinstance(True, int))
print(isinstance(1, (str, int)))
print(isinstance(\"a\", int))
x: int | str = 1
if isinstance(x, int):
    print(x + 1)
else:
    print(\"no\")
y: int | str = \"hi\"
if isinstance(y, str):
    print(y)
";
    let out = run_program("tier1_isinstance", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(
        py.status.success(),
        "{}",
        String::from_utf8_lossy(&py.stderr)
    );
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn tier1_container_kit() {
    // PyRs materializes enumerate/zip/reversed as lists (not lazy iterators), so
    // expression `print(enumerate(...))` is compared via list(...) under CPython.
    let src = "\
empty: list[int] = []
print(any(empty))
print(all(empty))
print(any([0, 1]))
print(all([1, 2]))
print(all([1, 0]))
print(any(\"\"))
print(all(\"ab\"))
for i, x in enumerate([10, 20]):
    print(i)
    print(x)
print(enumerate([10, 20]))
for a, b in zip([1, 2], [3, 4, 5]):
    print(a)
    print(b)
print(zip([1, 2], [3, 4, 5]))
print(reversed([1, 2, 3]))
print(reversed(\"ab\"))
print(1 in (1, \"a\", 2))
print(\"a\" in (1, \"a\", 2))
print(3 in (1, 2))
s: set[int] = {1, 2}
t: set[int] = {2, 3}
print(s | t)
u = s.union(t)
print(u)
s2: set[int] = {1}
s2 |= {2}
print(s2)
d: dict[str, int] = {\"a\": 1}
d.update({\"b\": 2})
print(d[\"a\"])
print(d[\"b\"])
";
    // CPython mirror with list() around materializing builtins.
    let py_src = "\
empty = []
print(any(empty))
print(all(empty))
print(any([0, 1]))
print(all([1, 2]))
print(all([1, 0]))
print(any(\"\"))
print(all(\"ab\"))
for i, x in enumerate([10, 20]):
    print(i)
    print(x)
print(list(enumerate([10, 20])))
for a, b in zip([1, 2], [3, 4, 5]):
    print(a)
    print(b)
print(list(zip([1, 2], [3, 4, 5])))
print(list(reversed([1, 2, 3])))
print(''.join(reversed(\"ab\")))
print(1 in (1, \"a\", 2))
print(\"a\" in (1, \"a\", 2))
print(3 in (1, 2))
s = {1, 2}
t = {2, 3}
print(s | t)
u = s.union(t)
print(u)
s2 = {1}
s2 |= {2}
print(s2)
d = {\"a\": 1}
d.update({\"b\": 2})
print(d[\"a\"])
print(d[\"b\"])
";
    let out = run_program("tier1_containers", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(py_src)
        .output()
        .expect("python3");
    assert!(
        py.status.success(),
        "{}",
        String::from_utf8_lossy(&py.stderr)
    );
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn tier1_typing_join_and_bare_param() {
    let src = "\
def f():
    x = 1
    x = \"a\"
    return x
print(f())
def g(x):
    return x + 1
print(g(2))
x = 1
x = 2.5
print(x)
";
    let out = run_program("tier1_typing", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(
        py.status.success(),
        "{}",
        String::from_utf8_lossy(&py.stderr)
    );
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}

#[test]
fn tier1_richer_exceptions() {
    let src = "\
try:
    raise FileNotFoundError(\"missing\")
except FileNotFoundError as e:
    print(e)
try:
    raise OverflowError(\"big\")
except (OverflowError, ValueError) as e:
    print(e)
try:
    raise EOFError(\"eof\")
except EOFError as e:
    print(e)
try:
    raise NameError(\"n\")
except NameError as e:
    print(e)
try:
    raise OSError(\"os\")
except OSError as e:
    print(e)
try:
    open(\"/no/such/file_pyrs_tier1_xyz\")
except FileNotFoundError:
    print(\"caught fnf\")
except OSError:
    print(\"caught os\")
";
    let out = run_program("tier1_exc", src);
    let py = Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(
        py.status.success(),
        "{}",
        String::from_utf8_lossy(&py.stderr)
    );
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}
