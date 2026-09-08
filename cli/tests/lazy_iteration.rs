//! Lazy iteration: `zip` and `enumerate` advance their inputs in lockstep
//! instead of draining them into lists first.
//!
//! The defect this closes, recorded in the roadmap's measured-defect table:
//! `list(zip(infinite(), [1]))` returned `[(0, 1)]` in CPython and **did not
//! terminate** in PyRs, because `zip` materialized every argument before
//! pairing them and so never reached the shortest one.
//!
//! Side-effect *counts* are the real assertion here, not just the values.
//! An implementation that drains and then truncates produces the same list
//! while running the wrong number of iterations, so a test that only checked
//! the result would pass on the broken version.

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
            .join(format!("pyrs-lazy-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    (dir, src)
}

/// Differential check at every optimization level, against the CPython
/// oracle, with a timeout — a non-terminating `zip` is the failure this
/// suite exists for, and a hung test process reports nothing.
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
        let actual = Command::new("timeout")
            .arg("60")
            .arg(PYRS)
            .args(["run", "-O", opt, "-i"])
            .arg(&src)
            .output()
            .expect("failed to spawn PyRs");
        assert_ne!(
            actual.status.code(),
            Some(124),
            "{tag} at -O{opt} did not terminate — the input was drained eagerly"
        );
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

/// A generator that never ends and announces every element it produces.
const INFINITE: &str = "from typing import Iterator\n\n\n\
    def infinite() -> Iterator[int]:\n    \
    i: int = 0\n    \
    while True:\n        \
    print(\"pull\", i)\n        \
    i = i + 1\n        \
    yield i - 1\n\n\n";

/// A finite generator that announces every element, for counting pulls.
const COUNTED: &str = "from typing import Iterator\n\n\n\
    def counted(n: int, tag: str) -> Iterator[int]:\n    \
    i: int = 0\n    \
    while i < n:\n        \
    print(tag, i)\n        \
    i = i + 1\n        \
    yield i - 1\n\n\n";

// ---------------------------------------------------------------------------
// The recorded defect
// ---------------------------------------------------------------------------

#[test]
fn zip_stops_at_the_shortest_input_without_draining_the_others() {
    // The roadmap row. Two pulls from the infinite side: one paired, one
    // that discovers the short side is done.
    matches_python(
        "infinite-zip",
        &format!("{INFINITE}print(list(zip(infinite(), [10])))\n"),
    );
}

#[test]
fn an_infinite_zip_terminates_in_a_for_loop_too() {
    matches_python(
        "infinite-for",
        &format!("{INFINITE}for a, b in zip(infinite(), [10, 20]):\n    print(a, b)\n"),
    );
}

#[test]
fn the_infinite_input_may_come_second() {
    // Order matters: once the first input is exhausted the ones after it are
    // never advanced at all, so this pulls one fewer time than the reverse.
    matches_python(
        "infinite-second",
        &format!("{INFINITE}print(list(zip([10], infinite())))\n"),
    );
}

// ---------------------------------------------------------------------------
// Advance order and side-effect counts
// ---------------------------------------------------------------------------

#[test]
fn inputs_are_advanced_left_to_right_and_stop_where_cpython_stops() {
    // Prints interleave with the pairs, so this pins the order in which the
    // two generators are pulled as well as how many times.
    matches_python(
        "advance-order",
        &format!(
            "{COUNTED}for a, b in zip(counted(3, \"L\"), counted(5, \"R\")):\n    \
             print(\"pair\", a, b)\n"
        ),
    );
}

#[test]
fn an_exhausted_input_stops_the_ones_after_it_from_advancing() {
    // The left generator runs out first, so the right one must be pulled
    // exactly as many times as the left produced — not once more.
    matches_python(
        "no-overpull",
        &format!("{COUNTED}print(list(zip(counted(2, \"L\"), counted(9, \"R\"))))\n"),
    );
}

#[test]
fn zipping_one_generator_with_itself_interleaves_its_pulls() {
    matches_python(
        "self-zip",
        &format!("{COUNTED}g = counted(5, \"g\")\nprint(list(zip(g, g)))\n"),
    );
}

#[test]
fn zip_of_three_inputs_stops_at_the_first_exhausted() {
    matches_python(
        "three-way",
        &format!(
            "{COUNTED}print(list(zip(counted(4, \"a\"), counted(2, \"b\"), counted(9, \"c\"))))\n"
        ),
    );
}

// ---------------------------------------------------------------------------
// Composition
// ---------------------------------------------------------------------------

#[test]
fn enumerate_over_zip_composes_without_materializing_either() {
    matches_python(
        "enum-of-zip",
        &format!(
            "{INFINITE}for i, pair in enumerate(zip(infinite(), [10, 20])):\n    \
             print(i, pair)\n"
        ),
    );
}

#[test]
fn zip_over_enumerate_composes() {
    matches_python(
        "zip-of-enum",
        "print(list(zip(enumerate(\"abc\"), [1, 2])))\n",
    );
}

#[test]
fn every_iterable_kind_can_be_zipped() {
    // list, str, tuple, range, dict, set and a generator all reach the same
    // cursor protocol, so composition must work across the lot.
    matches_python(
        "kinds",
        &format!(
            "{COUNTED}d = {{\"k\": 1}}\ns = {{7}}\n\
             print(list(zip([1, 2], \"ab\", (3, 4), range(9), d, s, counted(2, \"g\"))))\n"
        ),
    );
}

#[test]
fn a_user_iterator_can_be_zipped_and_is_not_over_advanced() {
    // The `try/except StopIteration` shape nested inside another cursor's
    // advance — a combination that did not exist before composition.
    matches_python(
        "user-iter",
        "class Countdown:\n    \
         def __init__(self, n: int) -> None:\n        \
         self.n: int = n\n\n    \
         def __iter__(self) -> \"Countdown\":\n        \
         return self\n\n    \
         def __next__(self) -> int:\n        \
         if self.n <= 0:\n            \
         raise StopIteration\n        \
         self.n = self.n - 1\n        \
         print(\"next\", self.n)\n        \
         return self.n\n\n\n\
         print(list(zip(Countdown(4), [1, 2])))\n",
    );
}

// ---------------------------------------------------------------------------
// The loop machinery still works through the new nesting
// ---------------------------------------------------------------------------

#[test]
fn break_continue_and_else_survive_a_composed_cursor() {
    matches_python(
        "control-flow",
        "for a, b in zip([1, 2, 3, 4], [10, 20, 30, 40]):\n    \
         if a == 2:\n        \
         continue\n    \
         if a == 3:\n        \
         break\n    \
         print(a, b)\n\
         else:\n    \
         print(\"no break\")\n\n\
         for a, b in zip([1], [2]):\n    \
         print(a, b)\n\
         else:\n    \
         print(\"ran to the end\")\n",
    );
}

#[test]
fn break_out_of_a_zip_over_a_user_iterator_and_a_generator() {
    // Try inside If inside Try: the shape a `Fallible` cursor nested inside
    // another cursor's advance produces, with a loop `else` on top.
    matches_python(
        "break-nested",
        &format!(
            "{COUNTED}class Ticks:\n    \
             def __init__(self, n: int) -> None:\n        \
             self.n: int = n\n\n    \
             def __iter__(self) -> \"Ticks\":\n        \
             return self\n\n    \
             def __next__(self) -> int:\n        \
             if self.n <= 0:\n            \
             raise StopIteration\n        \
             self.n = self.n - 1\n        \
             return self.n\n\n\n\
             for a, b in zip(Ticks(9), counted(9, \"g\")):\n    \
             print(a, b)\n    \
             if b == 2:\n        \
             break\n\
             else:\n    \
             print(\"no break\")\n"
        ),
    );
}

#[test]
fn a_zip_inside_a_comprehension_is_lazy_too() {
    matches_python(
        "comprehension",
        &format!("{INFINITE}print([a + b for a, b in zip(infinite(), [10, 20])])\n"),
    );
}

#[test]
fn the_eager_consumers_take_the_lazy_path() {
    // sorted/sum/min/max/any/all/set/join all route through the same
    // materialization helper, which had to stop probing the eager lowering
    // first.
    matches_python(
        "consumers",
        &format!(
            "{COUNTED}print(sorted(zip([3, 1], \"ba\")))\n\
             print(any(a == b for a, b in zip([1, 2], [2, 2])))\n\
             print(all(a < b for a, b in zip([1, 2], [3, 4])))\n\
             print(sum(a for a, _ in zip(counted(3, \"s\"), [1, 2])))\n\
             print(list(enumerate(counted(2, \"e\"), 5)))\n"
        ),
    );
}

// ---------------------------------------------------------------------------
// Evaluation order of the iterables themselves
// ---------------------------------------------------------------------------

#[test]
fn zip_evaluates_its_arguments_left_to_right_at_creation() {
    matches_python(
        "arg-order",
        "def a() -> list[int]:\n    print(\"a\")\n    return [1]\n\n\n\
         def b() -> list[int]:\n    print(\"b\")\n    return [2]\n\n\n\
         print(list(zip(a(), b())))\n",
    );
}

#[test]
fn range_evaluates_its_operands_left_to_right() {
    // Previously `stop` was bound to a temp before `start`, so
    // `range(a(), b())` called `b()` first. Invisible for `range(n)`.
    matches_python(
        "range-order",
        "def a() -> int:\n    print(\"a\")\n    return 0\n\n\n\
         def b() -> int:\n    print(\"b\")\n    return 2\n\n\n\
         def c() -> int:\n    print(\"c\")\n    return 1\n\n\n\
         for i in range(a(), b(), c()):\n    print(i)\n\
         print([i for i in range(a(), b(), c())])\n",
    );
}

#[test]
fn enumerate_evaluates_the_iterable_before_the_start() {
    matches_python(
        "enum-order",
        "def seq() -> list[int]:\n    print(\"seq\")\n    return [1, 2]\n\n\n\
         def start() -> int:\n    print(\"start\")\n    return 10\n\n\n\
         print(list(enumerate(seq(), start())))\n",
    );
}
