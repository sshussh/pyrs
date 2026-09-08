//! The inline small-int fast paths agree with the runtime's bignum path.
//!
//! `codegen/src/intfast.rs` emits an inline fast path for every common `int`
//! operation, with the original `pyrs_int_*` call kept on the cold edge. The
//! fast path is an optimization, so its only contract is that it produces the
//! *same answer* as the call it replaced — including at the boundaries of the
//! tagged representation, where a wrong overflow check would be a silent
//! miscompilation rather than a crash.
//!
//! Two independent checks, deliberately:
//!
//! * `PYRS_INLINE_INT=0` reverts every site to the plain runtime call, so
//!   compiling the same program both ways isolates the new code completely,
//!   with no CPython semantics in the way. A diff here means the inline path
//!   and the runtime disagree — nothing else.
//! * The same program is then diffed against CPython, which is what actually
//!   defines the answer.
//!
//! The escape hatch is read at emit time and is **not** part of the compile
//! cache key, so every compile here passes `--no-cache`. Without that a cached
//! binary from the other setting would silently defeat the comparison.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const PYRS: &str = env!("CARGO_BIN_EXE_pyrs");

/// Boundaries of the tagged representation: `(v << 1) | 1` is a small int for
/// `v` in `+-2**62`, and a heap pointer outside it. Each bound appears with
/// both neighbours so a fast path that is off by one at either end is caught,
/// and the out-of-range entries are heap bignums, so crossing this list
/// pairwise reaches small x small, small x heap, heap x small and heap x heap.
const VALUES: &str = "\
vals: list[int] = [
    0, 1, -1, 2, -2, 3, -3, 7, -7, 97, -97,
    2147483648, -2147483648, 2147483647, -2147483649, 4294967296,
    2305843009213693952, -2305843009213693952,
    4611686018427387903, 4611686018427387902, 4611686018427387904,
    -4611686018427387904, -4611686018427387903, -4611686018427387905,
    9223372036854775807, -9223372036854775808, 9223372036854775808,
    18446744073709551616, 1000000000000000000000000000000,
    -1000000000000000000000000000000,
]
";

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
        Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-intfast-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    (dir, src)
}

/// Run `src` with the inline paths on or off, at one optimization level.
fn run(src: &Path, opt: &str, inline: bool) -> String {
    let output = Command::new(PYRS)
        .args(["run", "--no-cache", "-O", opt, "-i"])
        .arg(src)
        .env("PYRS_INLINE_INT", if inline { "1" } else { "0" })
        .output()
        .expect("failed to spawn PyRs");
    assert!(
        output.status.success(),
        "PyRs failed at -O{opt} (inline={inline}):\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Compile `source` both ways at every optimization level, require the two to
/// agree, and require both to agree with CPython.
fn parity(tag: &str, body: &str) {
    let source = format!("{VALUES}\nn = len(vals)\n{body}");
    let (_dir, src) = write_prog(tag, &source);

    let oracle = Command::new("python3")
        .arg(&src)
        .output()
        .expect("failed to spawn CPython");
    assert!(
        oracle.status.success(),
        "CPython failed for {tag}: {}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    let expected = String::from_utf8_lossy(&oracle.stdout);

    for opt in ["0", "2", "3"] {
        let inlined = run(&src, opt, true);
        let called = run(&src, opt, false);
        assert_eq!(
            inlined, called,
            "{tag} at -O{opt}: the inline fast path and the runtime disagree"
        );
        assert_eq!(inlined, expected, "{tag} at -O{opt} differs from CPython");
    }
}

#[test]
fn add_sub_mul_across_the_small_boundary() {
    parity(
        "arith",
        "for i in range(n):\n\
         \x20   for j in range(n):\n\
         \x20       print(vals[i] + vals[j], vals[i] - vals[j], vals[i] * vals[j])\n",
    );
}

/// Tagging is strictly increasing, so the fast path compares the raw tagged
/// words. If that ever stops holding, ordering breaks near the bounds first.
#[test]
fn comparisons_order_the_same_as_the_runtime() {
    parity(
        "cmp",
        "for i in range(n):\n\
         \x20   for j in range(n):\n\
         \x20       a = vals[i]\n\
         \x20       b = vals[j]\n\
         \x20       print(a < b, a <= b, a > b, a >= b, a == b, a != b)\n",
    );
}

/// The one operation whose result can leave the small range for in-range
/// operands: `SMALL_MIN // -1` is `SMALL_MAX + 1`.
#[test]
fn floordiv_and_mod_including_the_overflow_edge() {
    parity(
        "divmod",
        "for i in range(n):\n\
         \x20   for j in range(n):\n\
         \x20       if vals[j] != 0:\n\
         \x20           print(vals[i] // vals[j], vals[i] % vals[j])\n\
         print(-4611686018427387904 // -1, -4611686018427387904 % -1)\n\
         print(-4611686018427387904 // 1, -4611686018427387904 % 1)\n",
    );
}

/// `&`, `|` and `^` need no range check because bit 62 equals bit 63 for every
/// small and that property is closed under all three. Negative operands are
/// where that argument would fail if it were wrong.
#[test]
fn bitwise_ops_stay_in_range_for_negatives() {
    parity(
        "bitwise",
        "for i in range(n):\n\
         \x20   for j in range(n):\n\
         \x20       print(vals[i] & vals[j], vals[i] | vals[j], vals[i] ^ vals[j])\n",
    );
}

/// `-SMALL_MIN` is the sole overflow case for negation, and `~` maps the small
/// range onto itself so it must never take the slow path for a small.
#[test]
fn unary_neg_and_invert_at_the_bounds() {
    parity(
        "unary",
        "for i in range(n):\n\
         \x20   print(-vals[i], ~vals[i], - -vals[i], ~ ~vals[i])\n",
    );
}

/// Truthiness is inlined *unguarded*, on the invariant that a heap `PyrsInt`
/// is never zero-valued — `int_from_sign_limbs` returns a tagged small for
/// zero. If that invariant ever breaks, a bignum starts reading as falsy.
#[test]
fn truthiness_is_total_over_smalls_and_bignums() {
    parity(
        "truth",
        "for i in range(n):\n\
         \x20   a = vals[i]\n\
         \x20   if a:\n\
         \x20       print(a, 'truthy')\n\
         \x20   else:\n\
         \x20       print(a, 'falsy')\n\
         \x20   print(a and 1, a or 2, not a)\n",
    );
}

/// A zero divisor routes to the runtime rather than trapping inline, so the
/// exception type, message and catchability must be exactly what they were.
#[test]
fn zero_division_keeps_its_type_and_message() {
    parity(
        "zerodiv",
        "for i in range(n):\n\
         \x20   try:\n\
         \x20       print(vals[i] // 0)\n\
         \x20   except ZeroDivisionError as e:\n\
         \x20       print('floordiv', e)\n\
         \x20   try:\n\
         \x20       print(vals[i] % 0)\n\
         \x20   except ArithmeticError as e:\n\
         \x20       print('mod', e)\n",
    );
}

/// Boxing and unboxing sit on every `len()` and every subscript, so they are
/// reached by ordinary indexing rather than by arithmetic.
#[test]
fn boxing_round_trips_through_indexing() {
    parity(
        "box",
        "for i in range(n):\n\
         \x20   print(len(vals), vals[i], vals[-1], vals[i % n])\n",
    );
}

/// Shifts and bitwise ops over the same boundary set.
///
/// These are not inlined — they are the check for `int_borrow_mag`, which
/// replaced a reader that `xmalloc`'d a one-limb buffer even for a small
/// operand. Two of its eleven call sites hand the borrowed magnitude onward
/// to something that takes ownership (`to_twos`, and the negative branch of
/// `pyrs_int_rshift` where the transfer sits 25 lines below the read), so
/// they must copy first. Getting that wrong is a double free or a
/// use-after-free, which is exactly what a bignum shift sweep surfaces.
#[test]
fn shifts_and_bitwise_across_the_small_boundary() {
    parity(
        "shifts",
        "for i in range(n):\n\
         \x20   a = vals[i]\n\
         \x20   print(a, ~a, -a, a >> 1, a >> 64, a >> 200)\n\
         \x20   for j in range(n):\n\
         \x20       b = vals[j]\n\
         \x20       print(a & b, a | b, a ^ b)\n\
         \x20       if 0 <= b and b < 300:\n\
         \x20           print(a << b, a >> b)\n",
    );
}

/// `sum`, `min` and `max` build their own loops with hand-written phis, so
/// they were the last sites still emitting raw `pyrs_int_add` / `pyrs_int_cmp`
/// after 0.126 routed every other site through the inline helpers. The
/// accumulator has to cross the small boundary correctly.
#[test]
fn aggregates_cross_the_small_boundary() {
    parity(
        "aggregate",
        "print(sum(vals), min(vals), max(vals))\n\
         acc: list[int] = []\n\
         for i in range(n):\n\
         \x20   acc.append(vals[i])\n\
         \x20   print(sum(acc), min(acc), max(acc))\n\
         \x20   for j in range(n):\n\
         \x20       print(min(vals[i], vals[j]), max(vals[i], vals[j]))\n",
    );
}

/// The fast paths must not change what the emitter does with block structure.
/// `sum` and `min`/`max` build loop phis with hard-coded predecessors, and a
/// generator keeps its locals in a heap frame; all three would produce invalid
/// IR if an int operation started splitting blocks. LLVM verifies the module
/// before optimizing, so this fails loudly at compile time rather than by
/// giving a wrong answer.
#[test]
fn arithmetic_inside_phi_heavy_constructs_still_verifies() {
    let source = "\
xs: list[int] = [3, 1, 4, 1, 5, 9, 2, 6]

def counter(limit: int):
    i = 0
    while i < limit:
        yield i * i + 1
        i += 1

total = sum(xs)
print(total, min(xs), max(xs), sum(x * 2 - 1 for x in xs))
print([x % 3 for x in xs if x // 2 > 0])
print(list(counter(5)))

acc = 0
for v in counter(4):
    try:
        acc += v % 2
    except ZeroDivisionError:
        acc -= 1
print(acc)
";
    let (_dir, src) = write_prog("phi", source);
    let oracle = Command::new("python3").arg(&src).output().unwrap();
    assert!(oracle.status.success());
    for opt in ["0", "2", "3"] {
        let inlined = run(&src, opt, true);
        assert_eq!(
            inlined,
            run(&src, opt, false),
            "phi-heavy constructs differ between inline and runtime at -O{opt}"
        );
        assert_eq!(inlined, String::from_utf8_lossy(&oracle.stdout));
    }
}

/// A seeded sample by *bit length* rather than by magnitude, so the boundaries
/// are hit far more often than uniform sampling would manage. This is the
/// check for cases nobody thought to enumerate.
#[test]
fn randomized_by_bit_length() {
    // A small LCG rather than a dev-dependency: the sequence only has to be
    // fixed and reproducible, not statistically good.
    let mut state: u64 = 0x2026_0908_dead_beef;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        state >> 11
    };
    let mut lits = Vec::new();
    for _ in 0..60 {
        let bits = (next() % 71) as u32;
        let magnitude: u128 = if bits == 0 {
            0
        } else {
            let hi = if bits > 64 { next() as u128 } else { 0 };
            let v = (hi << 64) | next() as u128;
            v & ((1u128 << bits) - 1)
        };
        let sign = if next() % 2 == 0 { "-" } else { "" };
        lits.push(format!("{sign}{magnitude}"));
    }
    let source = format!(
        "vals: list[int] = [{}]\nn = len(vals)\n\
         for i in range(n):\n\
         \x20   for j in range(n):\n\
         \x20       a = vals[i]\n\
         \x20       b = vals[j]\n\
         \x20       print(a + b, a - b, a * b, a & b, a | b, a ^ b)\n\
         \x20       print(a < b, a <= b, a == b, -a, ~a)\n\
         \x20       if b != 0:\n\
         \x20           print(a // b, a % b)\n",
        lits.join(", ")
    );
    let (_dir, src) = write_prog("random", &source);
    let oracle = Command::new("python3").arg(&src).output().unwrap();
    assert!(
        oracle.status.success(),
        "CPython failed: {}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    let inlined = run(&src, "2", true);
    assert_eq!(
        inlined,
        run(&src, "2", false),
        "inline and runtime disagree on the random sample"
    );
    assert_eq!(inlined, String::from_utf8_lossy(&oracle.stdout));
}
