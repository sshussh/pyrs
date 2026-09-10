//! Operators on a value whose type is not known until run time.
//!
//! Reading a dynamic value became free in 0.142.0, but operating on one was
//! still refused: `a + b`, `a < b`, `a == b` and `-a` on an `object` were all
//! compile errors. A generic kernel in the runtime dispatches on the tags the
//! values already carry, so those become a performance question rather than a
//! possibility question.
//!
//! CPython is the contract for both halves -- the result types *and* the error
//! messages, which differ in wording between arithmetic (`unsupported operand
//! type(s) for +`), sequence concatenation (`can only concatenate`) and
//! ordering (`not supported between instances of`).

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

fn temp_source(tag: &str, source: &str) -> TempDir {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-dynops-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    fs::write(dir.0.join("prog.py"), source).unwrap();
    dir
}

/// Differential against CPython at every optimization level and under
/// collection pressure. The kernel allocates a box per operand and a box for
/// its result, so it is a real allocation site.
fn matches_python(tag: &str, source: &str) {
    let dir = temp_source(tag, source);
    let expected = Command::new("python3")
        .arg("prog.py")
        .current_dir(&dir.0)
        .output()
        .expect("failed to spawn CPython");
    assert!(
        expected.status.success(),
        "CPython failed for {tag}: {}",
        String::from_utf8_lossy(&expected.stderr)
    );
    for opt in ["0", "2", "3"] {
        for stress in ["0", "1"] {
            let actual = Command::new(PYRS)
                .args(["run", "-O", opt, "-i", "prog.py"])
                .env("PYRS_GC_STRESS", stress)
                .current_dir(&dir.0)
                .output()
                .expect("failed to spawn PyRs");
            assert_eq!(
                String::from_utf8_lossy(&actual.stdout),
                String::from_utf8_lossy(&expected.stdout),
                "{tag}: stdout differs at -O{opt} (GC stress {stress})\nstderr: {}",
                String::from_utf8_lossy(&actual.stderr)
            );
            assert_eq!(
                actual.status.code(),
                expected.status.code(),
                "{tag}: exit status differs at -O{opt}"
            );
        }
    }
}

/// Both engines must fail with the same final stderr line. Only the last line
/// is compared: PyRs prints no traceback, a recorded divergence.
fn fails_like_python(tag: &str, source: &str) {
    let dir = temp_source(tag, source);
    let expected = Command::new("python3")
        .arg("prog.py")
        .current_dir(&dir.0)
        .output()
        .expect("failed to spawn CPython");
    assert!(
        !expected.status.success(),
        "{tag}: CPython was expected to fail"
    );
    let want = last_line(&expected.stderr);
    for opt in ["0", "2", "3"] {
        let actual = Command::new(PYRS)
            .args(["run", "-O", opt, "-i", "prog.py"])
            .current_dir(&dir.0)
            .output()
            .expect("failed to spawn PyRs");
        assert!(!actual.status.success(), "{tag}: PyRs succeeded at -O{opt}");
        assert_eq!(
            last_line(&actual.stderr),
            want,
            "{tag}: error differs at -O{opt}"
        );
    }
}

fn last_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .unwrap_or_default()
        .to_string()
}

const HEAD: &str = concat!(
    "a: object = 7\n",
    "b: object = 2\n",
    "f: object = 2.5\n",
    "s: object = \"ab\"\n",
    "t: object = True\n",
    "xs: object = [1, 2]\n",
    "tp: object = (1, 2)\n",
);

// ---------------------------------------------------------------------------
// results
// ---------------------------------------------------------------------------

#[test]
fn arithmetic_on_dynamic_numbers() {
    matches_python(
        "arith",
        &format!(
            "{HEAD}print(a + b, a - b, a * b, a / b, a // b, a % b, a ** b)\n\
             print(a + f, f * b, a / f, f - a, f // b, f % b, f ** b)\n\
             print(t + t, t * b, t - t)\n"
        ),
    );
}

#[test]
fn a_dynamic_operand_mixes_with_a_static_one() {
    // Only one side needs to be dynamic; the other is boxed for the call.
    matches_python(
        "mixed",
        &format!("{HEAD}print(a + 1, 1 + a, a * 2, 2 * a, a - 1, 1 - a, a / 2, 2 / a)\n"),
    );
}

#[test]
fn floored_division_keeps_pythons_signs() {
    // C truncates toward zero; Python floors. -7 // 2 is -4, and -7 % 2 is 1.
    matches_python(
        "floored",
        "for pair in [(-7, 2), (7, -2), (-7, -2), (7, 2)]:\n\
         \x20   x: object = pair[0]\n\
         \x20   y: object = pair[1]\n\
         \x20   print(x // y, x % y)\n",
    );
}

#[test]
fn a_negative_exponent_gives_a_float() {
    matches_python(
        "negpow",
        "a: object = 2\nb: object = -1\nprint(a ** b)\nc: object = 2.0\nprint(c ** b)\n",
    );
}

#[test]
fn sequences_concatenate_and_repeat() {
    matches_python(
        "sequences",
        &format!(
            "{HEAD}print(s + s, s * b, b * s, s * t)\n\
             print(xs + xs, xs * b, b * xs)\n\
             print(tp + tp, tp * b, b * tp)\n"
        ),
    );
}

#[test]
fn set_algebra_on_dynamic_sets() {
    matches_python(
        "sets",
        // Only the length is checked. `sorted()` and `in` over a dynamic
        // container are separate gaps, and set printing order is unstable.
        "p: object = {1, 2, 3}\nq: object = {2}\nd: object = p - q\n\
         e: object = p - p\nprint(len(d), len(e))\n",
    );
}

#[test]
fn comparisons_on_dynamic_values() {
    matches_python(
        "compare",
        &format!(
            "{HEAD}print(a < b, a <= b, a > b, a >= b, a == b, a != b)\n\
             print(a < f, f < a, a == a, s < s, s == s)\n\
             print(xs < [1, 3], tp < (1, 3), xs == [1, 2])\n\
             print(t < b, t == 1)\n"
        ),
    );
}

#[test]
fn equality_across_kinds_never_raises() {
    // `==` returns False for mismatched types where `<` would raise.
    matches_python(
        "eq-kinds",
        "vals: list[object] = [1, \"a\", 2.5, True, None, [1], (1,)]\n\
         i: int = 0\n\
         while i < len(vals):\n\
         \x20   j: int = 0\n\
         \x20   while j < len(vals):\n\
         \x20       x: object = vals[i]\n\
         \x20       y: object = vals[j]\n\
         \x20       print(i, j, x == y, x != y)\n\
         \x20       j += 1\n\
         \x20   i += 1\n",
    );
}

#[test]
fn mixed_int_and_float_compare_exactly() {
    matches_python(
        "exact-cmp",
        "a: object = 1\nb: object = 1.0\nprint(a == b, a < b, a <= b)\n\
         big: object = 10 ** 30\nd: object = 1e30\nprint(big == d, big > d)\n",
    );
}

#[test]
fn bigints_survive_the_kernel() {
    matches_python(
        "bigint",
        "a: object = 10 ** 30\nb: object = 3\nprint(a * b, a // b, a % b, a + b, -a)\n",
    );
}

#[test]
fn unary_operators_on_dynamic_values() {
    matches_python(
        "unary",
        "a: object = 7\nf: object = -2.5\nt: object = True\n\
         print(-a, +a, ~a, abs(a))\nprint(-f, +f, abs(f))\nprint(-t, ~t)\n",
    );
}

// ---------------------------------------------------------------------------
// errors -- CPython's wording, which differs by operator family
// ---------------------------------------------------------------------------

#[test]
fn arithmetic_on_mismatched_kinds_raises() {
    fails_like_python(
        "arith-err",
        "a: object = 1\nb: object = \"x\"\nprint(a + b)\n",
    );
}

#[test]
fn concatenating_a_sequence_with_a_non_sequence_uses_its_own_wording() {
    // `can only concatenate str (not "int") to str`, not the `unsupported
    // operand type(s)` form.
    fails_like_python(
        "concat-err-str",
        "a: object = \"x\"\nb: object = 1\nprint(a + b)\n",
    );
    fails_like_python(
        "concat-err-list",
        "a: object = [1]\nb: object = 1\nprint(a + b)\n",
    );
}

#[test]
fn ordering_mismatched_kinds_raises() {
    fails_like_python(
        "order-err",
        "a: object = 1\nb: object = \"x\"\nprint(a < b)\n",
    );
    fails_like_python(
        "order-none",
        "a: object = None\nb: object = 1\nprint(a < b)\n",
    );
}

#[test]
fn dividing_a_dynamic_value_by_zero_raises() {
    for (tag, expr) in [
        ("div0", "a / b"),
        ("floordiv0", "a // b"),
        ("mod0", "a % b"),
    ] {
        fails_like_python(
            tag,
            &format!("a: object = 1\nb: object = 0\nprint({expr})\n"),
        );
    }
    fails_like_python(
        "zero-neg-pow",
        "a: object = 0\nb: object = -1\nprint(a ** b)\n",
    );
}

#[test]
fn unary_operators_reject_the_kinds_python_rejects() {
    fails_like_python("neg-str", "a: object = \"x\"\nprint(-a)\n");
    fails_like_python("invert-float", "a: object = 1.5\nprint(~a)\n");
    fails_like_python("abs-list", "a: object = [1]\nprint(abs(a))\n");
}

// ---------------------------------------------------------------------------
// the one gap, named rather than mistranslated
// ---------------------------------------------------------------------------

/// printf-style `%` on a dynamic str is not implemented. CPython succeeds
/// here, so the runtime must say what is missing rather than raise a TypeError
/// CPython would never raise.
#[test]
fn printf_formatting_on_a_dynamic_str_says_it_is_unsupported() {
    let dir = temp_source(
        "str-percent",
        "a: object = \"x=%d\"\nb: object = 5\nprint(a % b)\n",
    );
    let out = Command::new(PYRS)
        .args(["run", "-i", "prog.py"])
        .current_dir(&dir.0)
        .output()
        .expect("failed to spawn PyRs");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("printf-style") && stderr.contains("not supported yet"),
        "the gap must name itself, not raise a TypeError CPython would not: {stderr}"
    );
    assert!(
        !stderr.contains("unsupported operand type(s)"),
        "must not claim a TypeError CPython does not raise: {stderr}"
    );
}
