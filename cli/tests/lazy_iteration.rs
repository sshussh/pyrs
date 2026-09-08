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

// ---------------------------------------------------------------------------
// A generator expression evaluates its outermost iterable at creation
// ---------------------------------------------------------------------------
//
// The second half of the recorded defect. CPython calls `bound()` when the
// genexp is *created*; PyRs left `range(bound())` inside the synthesized body
// and called it on first advance. The iterable is now hoisted either whole
// (when it is a value) or by its operands (when it is not, which today means
// only `range`).

#[test]
fn a_generator_expression_evaluates_range_operands_at_creation() {
    matches_python(
        "genexp-creation",
        "def bound() -> int:\n    print(\"bound\")\n    return 3\n\n\n\
         g = (x for x in range(bound()))\n\
         print(\"created\")\n\
         print(list(g))\n",
    );
}

#[test]
fn every_range_operand_is_evaluated_once_and_in_order() {
    matches_python(
        "genexp-range-operands",
        "def lo() -> int:\n    print(\"lo\")\n    return 1\n\n\n\
         def hi() -> int:\n    print(\"hi\")\n    return 6\n\n\n\
         def by() -> int:\n    print(\"by\")\n    return 2\n\n\n\
         g = (x for x in range(lo(), hi(), by()))\n\
         print(\"created\")\n\
         print(list(g))\n",
    );
}

#[test]
fn a_genexp_that_is_never_advanced_still_evaluated_its_iterable() {
    // The observable difference between "at creation" and "on first
    // advance": nothing consumes the generator at all.
    matches_python(
        "genexp-unused",
        "def bound() -> int:\n    print(\"bound\")\n    return 3\n\n\n\
         g = (x for x in range(bound()))\n\
         print(\"done\")\n",
    );
}

#[test]
fn a_value_iterable_is_still_hoisted_whole() {
    // The pre-existing path must keep working: one parameter, bound to the
    // whole iterable, evaluated once.
    matches_python(
        "genexp-value",
        "def seq() -> list[int]:\n    print(\"seq\")\n    return [1, 2, 3]\n\n\n\
         g = (x * 2 for x in seq())\n\
         print(\"created\")\n\
         print(list(g))\n",
    );
}

#[test]
fn a_nested_genexp_clause_is_still_evaluated_lazily() {
    // Only the *outermost* iterable is eager; inner clauses are re-evaluated
    // per outer element, as CPython does.
    matches_python(
        "genexp-inner",
        "def inner(n: int) -> list[int]:\n    print(\"inner\", n)\n    return [n, n]\n\n\n\
         g = (y for x in range(2) for y in inner(x))\n\
         print(\"created\")\n\
         print(list(g))\n",
    );
}

#[test]
fn a_genexp_over_range_is_still_lazy_in_its_elements() {
    // Hoisting the operands must not make the range itself materialize: this
    // would be a billion-element list if it did.
    matches_python(
        "genexp-lazy-elements",
        "g = (x for x in range(1000000000))\n\
         print(next(g), next(g), next(g))\n",
    );
}

// ---------------------------------------------------------------------------
// map and filter
// ---------------------------------------------------------------------------
//
// Neither existed. They are here rather than in a suite of their own because
// they are the same cursor protocol: `map` passes the inner cursor through
// with its element transformed, and `filter` loops inside the advance until
// it finds a passing element — which is what lets a filtered cursor be
// zipped without a skipped element being paired.

#[test]
fn map_applies_every_callable_form() {
    matches_python(
        "map-forms",
        "xs: list[int] = [1, 2, 3]\n\n\n\
         def double(n: int) -> int:\n    return n * 2\n\n\n\
         print(list(map(str, xs)))\n\
         print(list(map(double, xs)))\n\
         print(list(map(lambda n: n + 1, xs)))\n\
         print(list(map(abs, [-1, 2, -3])))\n",
    );
}

#[test]
fn filter_keeps_what_the_predicate_accepts() {
    matches_python(
        "filter-forms",
        "xs: list[int] = [1, 2, 3, 4, 5]\n\n\n\
         def odd(n: int) -> bool:\n    return n % 2 == 1\n\n\n\
         print(list(filter(odd, xs)))\n\
         print(list(filter(lambda n: n > 3, xs)))\n",
    );
}

#[test]
fn filter_none_keeps_the_truthy_elements() {
    matches_python(
        "filter-none",
        "print(list(filter(None, [1, 0, 2, 0, 3])))\n\
         print(list(filter(None, [\"\", \"a\", \"\", \"b\"])))\n\
         print(list(filter(None, [True, False, True])))\n",
    );
}

#[test]
fn a_filter_that_matches_nothing_or_everything_still_terminates() {
    // The advance loops until it finds a match; an empty result means it ran
    // to exhaustion, which is where an off-by-one would hang or over-read.
    matches_python(
        "filter-edges",
        "xs: list[int] = [1, 2, 3]\n\
         empty: list[int] = []\n\
         print(list(filter(lambda n: n > 100, xs)))\n\
         print(list(filter(lambda n: n > 0, xs)))\n\
         print(list(filter(None, empty)))\n\
         print(list(map(str, empty)))\n",
    );
}

#[test]
fn map_and_filter_stay_lazy_under_zip() {
    // The point of building them on the cursor: an infinite input must not be
    // drained, and a filtered element must not be paired.
    matches_python(
        "map-filter-lazy",
        &format!(
            "{INFINITE}def double(n: int) -> int:\n    return n * 2\n\n\n\
             def odd(n: int) -> bool:\n    return n % 2 == 1\n\n\n\
             print(list(zip(map(double, infinite()), [10])))\n\
             print(list(zip(filter(odd, infinite()), [10, 20])))\n"
        ),
    );
}

#[test]
fn map_and_filter_compose_with_each_other_and_the_consumers() {
    matches_python(
        "map-filter-compose",
        "xs: list[int] = [1, 2, 3, 4, 5]\n\n\n\
         def double(n: int) -> int:\n    return n * 2\n\n\n\
         def odd(n: int) -> bool:\n    return n % 2 == 1\n\n\n\
         for v in map(double, filter(odd, xs)):\n    print(v)\n\
         print(sorted(map(double, xs)))\n\
         print(sum(map(double, xs)))\n\
         print(any(map(odd, xs)), all(map(odd, xs)))\n\
         print([v for v in filter(odd, map(double, xs))])\n\
         print(list(enumerate(filter(odd, xs), 1)))\n",
    );
}

#[test]
fn map_evaluates_the_callable_before_the_iterable() {
    // CPython's left-to-right argument order. The iterable has to be lowered
    // first to type the callable, so the *setup* order is what preserves it.
    matches_python(
        "map-arg-order",
        "def pick() -> int:\n    print(\"pick\")\n    return 0\n\n\n\
         def seq() -> list[int]:\n    print(\"seq\")\n    return [1, 2]\n\n\n\
         print(list(map(lambda n: n + pick(), seq())))\n",
    );
}

#[test]
fn map_over_a_range_builds_no_intermediate_list() {
    // A billion-element range would not fit if the cursor materialized.
    matches_python(
        "map-lazy-range",
        "g = (v for v in map(lambda n: n * 2, range(1000000000)))\n\
         print(next(g), next(g), next(g))\n",
    );
}
