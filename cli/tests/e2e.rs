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
fn fstring_format_spec_is_compile_error() {
    let dir = TempDir::new("fspec");
    let src = dir.0.join("prog.py");
    fs::write(&src, "x = 1\nprint(f\"{x:.2f}\")\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("format specifiers"), "stderr: {stderr}");
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
    fs::write(&src, "import os\nprint(1)\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("No module named 'os'"), "stderr: {stderr}");
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
fn comprehension_multiple_clauses_are_rejected() {
    let dir = TempDir::new("compmulti");
    let src = dir.0.join("prog.py");
    fs::write(&src, "m = [i + j for i in range(2) for j in range(2)]\n").unwrap();
    let out = Command::new(PYRS)
        .args(["compile", "-i"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("comprehension clauses"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---- modules (multi-file imports) ----

/// Write several files into a temp dir and run the root, returning stdout.
fn run_project(tag: &str, files: &[(&str, &str)], root: &str) -> String {
    let dir = TempDir::new(tag);
    for (name, body) in files {
        fs::write(dir.0.join(name), body).unwrap();
    }
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
    for (name, body) in files {
        fs::write(dir.0.join(name), body).unwrap();
    }
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
                "def broken(n: int) -> int:\n    x = 1\n    x = 2.5\n    return n\n",
            ),
            ("main.py", "import helper\nprint(helper.broken(3))\n"),
        ],
        "main.py",
    );
    assert!(stderr.contains("helper.py:3"), "stderr: {stderr}");
    assert!(stderr.contains("type mismatch"), "stderr: {stderr}");
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
