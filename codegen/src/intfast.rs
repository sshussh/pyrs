//! Inline fast paths for tagged small-int arithmetic.
//!
//! Every `int` operation used to be an out-of-line call into `runtime.c`.
//! That is not just the cost of a call: the runtime is linked as a separate
//! object with no LTO, so LLVM must treat `pyrs_int_add` as an opaque,
//! arbitrarily-memory-clobbering, possibly-non-returning call. Loop-invariant
//! hoisting, redundant-load elimination and vectorization all stop dead at
//! every arithmetic operation. Measured: the same trial division runs 0.8x
//! CPython in `int` and 21.5x in `float`.
//!
//! Each operation below is emitted once per module as an `alwaysinline`
//! helper whose fast path is straight-line arithmetic on the tagged words and
//! whose cold edge is the same runtime call as before. Call sites in
//! [`crate::emit`] stay exactly one line, so no existing block or phi
//! assumption in the emitter changes.
//!
//! # The tagging contract
//!
//! `runtime.c` represents an `int` as `T(v) = (v << 1) | 1` for
//! `v` in `[-2^62, 2^62-1]`, and as a pointer to a GC-allocated `PyrsInt`
//! (LSB clear) otherwise. Two properties carry every proof here:
//!
//! * `T` is **injective and strictly increasing**, and its image
//!   `[-2^63+1, 2^63-1]` fits `i64` without wrapping. So signed comparison of
//!   raw tagged words is already the right answer for two smalls.
//! * `T(v)` has bit 0 set, so `a & b` has bit 0 set **iff both are small**.
//!
//! # Why the overflow checks are exact
//!
//! Writing `s` for the true mathematical result, the fast path computes
//! `2s + 1` in one `i64` operation and takes the signed-overflow flag as the
//! range test. That is exact rather than conservative, in both directions:
//!
//! * `2s + 1 <= 2^63-1  <=>  s <= 2^62-1 = SMALL_MAX`
//! * `2s + 1 >= -2^63    <=>  2s >= -2^63-1  <=>  s >= -2^62 - 1/2`, and `s`
//!   is an integer, so `<=> s >= -2^62 = SMALL_MIN`
//!
//! The half-integer on the lower bound is what makes it tight: `s = SMALL_MIN-1`
//! gives `2s+1 = -2^63-1`, which does overflow. So `!ovf` is precisely
//! "the result is a small int", with no slack at either end.
//!
//! # No `nsw`/`nuw`
//!
//! Every tagging `add`/`sub`/`shl` below is provably non-wrapping under its
//! guard, so the flags would be legal. They are deliberately absent: if a
//! guard is ever wrong, a poison flag converts a loud wrong answer into
//! undefined behaviour, which is this compiler's most serious defect class.
//! The optimizer recovers the same code from the guard's range facts anyway.

/// `T(SMALL_MIN)` = `-2^63 + 1`. Not spelled as an expression because it
/// appears in IR text.
const TAGGED_SMALL_MIN: &str = "-9223372036854775807";

/// Every helper this module can emit. The emitter records the ones it used
/// and [`define`] renders only those.
pub const HELPERS: &[&str] = &[
    "add", "sub", "mul", "floordiv", "mod", "lt", "le", "gt", "ge", "eq", "ne", "and", "or", "xor",
    "neg", "invert", "box", "unbox",
];

/// LLVM symbol for a helper. Dots keep it disjoint from `mangle`d user
/// functions, which are `pyrs_<python identifier>`.
pub fn symbol(op: &str) -> String {
    format!("@pyrs.int.{op}")
}

/// The `llvm.*.with.overflow` declarations the helpers need.
pub const INTRINSIC_DECLS: &str = concat!(
    "declare { i64, i1 } @llvm.sadd.with.overflow.i64(i64, i64)\n",
    "declare { i64, i1 } @llvm.ssub.with.overflow.i64(i64, i64)\n",
    "declare { i64, i1 } @llvm.smul.with.overflow.i64(i64, i64)\n",
);

/// Guard: both operands are tagged smalls.
///
/// `T(v)` always has bit 0 set and a `PyrsInt*` never does (the collector
/// allocates aligned), so bit 0 of `a & b` is exactly `is_small(a) &&
/// is_small(b)`. `trunc ... to i1` takes bit 0, which is what InstCombine
/// would canonicalize `and`+`icmp ne` into — emitted directly so the shape is
/// the same at `-O0`, where InstCombine does not run.
fn both_small() -> String {
    "  %both = and i64 %a, %b\n  %sm = trunc i64 %both to i1\n".to_string()
}

/// A binary helper whose fast value is computed speculatively in `entry`.
///
/// `sdiv`/`srem` cannot use this shape: LLVM will not speculate them past the
/// divisor check, so those get their own fast block in [`define`].
fn binary(op: &str, ret: &str, fast_body: &str, cond: &str, slow_body: &str) -> String {
    format!(
        "define internal {ret} {sym}(i64 %a, i64 %b) alwaysinline {{\n\
         entry:\n{fast_body}\
         \x20 br i1 {cond}, label %join, label %slow\n\
         slow:\n{slow_body}\
         \x20 br label %join\n\
         join:\n\
         \x20 %r = phi {ret} [ %f, %entry ], [ %s, %slow ]\n\
         \x20 ret {ret} %r\n\
         }}\n\n",
        sym = symbol(op)
    )
}

/// A comparison helper. The fast path is a signed compare of the **raw tagged
/// words** — no untagging at all, because `T` is strictly increasing over the
/// small range and never wraps.
fn compare(op: &str, pred: &str) -> String {
    let slow = if matches!(op, "eq" | "ne") {
        // pyrs_int_eq returns 0/1, not a three-way ordering.
        let test = if op == "eq" { "ne" } else { "eq" };
        format!("  %c = call i32 @pyrs_int_eq(i64 %a, i64 %b)\n  %s = icmp {test} i32 %c, 0\n")
    } else {
        format!("  %c = call i32 @pyrs_int_cmp(i64 %a, i64 %b)\n  %s = icmp {pred} i32 %c, 0\n")
    };
    binary(
        op,
        "i1",
        &format!("{}  %f = icmp {pred} i64 %a, %b\n", both_small()),
        "%sm",
        &slow,
    )
}

/// A helper whose fast path is an overflow-checked tagging operation.
///
/// `expr` names the intrinsic and its operands; the result of the intrinsic is
/// already `T(result)` unless `tag` says otherwise.
fn checked(op: &str, setup: &str, intrinsic: &str, args: &str, tag: Option<&str>) -> String {
    let value = match tag {
        None => "  %f = extractvalue { i64, i1 } %o, 0\n".to_string(),
        Some(_) => "  %p = extractvalue { i64, i1 } %o, 0\n  %f = or i64 %p, 1\n".to_string(),
    };
    let fast = format!(
        "{}{setup}  %o = call {{ i64, i1 }} @llvm.{intrinsic}.with.overflow.i64({args})\n\
         {value}  %ovf = extractvalue {{ i64, i1 }} %o, 1\n\
         \x20 %ok = xor i1 %ovf, true\n\
         \x20 %fast = and i1 %sm, %ok\n",
        both_small()
    );
    binary(
        op,
        "i64",
        &fast,
        "%fast",
        &format!("  %s = call i64 @pyrs_int_{op}(i64 %a, i64 %b)\n"),
    )
}

/// Shared tail of `//` and `%`: turn C's truncating `sdiv`/`srem` into
/// Python's floor semantics, then tag.
///
/// Mirrors `divmod_floor` in `runtime.c`: adjust when the remainder is
/// non-zero and its sign differs from the divisor's. Both arms are computed
/// unconditionally into a `select` rather than branched on, so the fast block
/// stays straight-line.
fn floor_adjust(adjusted: &str, plain: &str) -> String {
    format!(
        "  %rnz = icmp ne i64 %rt, 0\n\
         \x20 %rneg = icmp slt i64 %rt, 0\n\
         \x20 %bneg = icmp slt i64 %bv, 0\n\
         \x20 %ds = xor i1 %rneg, %bneg\n\
         \x20 %adj = and i1 %rnz, %ds\n\
         {adjusted}\
         \x20 %v = select i1 %adj, i64 %alt, i64 {plain}\n\
         \x20 %sh = shl i64 %v, 1\n\
         \x20 %f = or i64 %sh, 1\n"
    )
}

/// A division helper. `sdiv`/`srem` are UB on a zero divisor and LLVM will not
/// speculate them, so unlike every other operation these need their own block
/// dominated by the guard.
///
/// `guard` must define `cond`; `both_small` has already bound `%sm`, and
/// `%bnz` holds "the divisor is not zero".
fn division(op: &str, guard: &str, cond: &str, quick: &str) -> String {
    format!(
        "define internal i64 {sym}(i64 %a, i64 %b) alwaysinline {{\n\
         entry:\n\
         {both}\
         \x20 %bnz = icmp ne i64 %b, 1\n\
         {guard}\
         \x20 br i1 {cond}, label %quick, label %slow\n\
         quick:\n\
         \x20 %av = ashr i64 %a, 1\n\
         \x20 %bv = ashr i64 %b, 1\n\
         {quick}\
         \x20 br label %join\n\
         slow:\n\
         \x20 %s = call i64 @pyrs_int_{op}(i64 %a, i64 %b)\n\
         \x20 br label %join\n\
         join:\n\
         \x20 %r = phi i64 [ %f, %quick ], [ %s, %slow ]\n\
         \x20 ret i64 %r\n\
         }}\n\n",
        sym = symbol(op),
        both = both_small(),
    )
}

/// The IR text defining one helper.
///
/// # Panics
///
/// Panics if `op` is not in [`HELPERS`].
pub fn define(op: &str) -> String {
    match op {
        // T(av+bv) = 2(av+bv)+1 = a + (b-1). `b-1 = 2bv` never wraps, since
        // `b` is at most `2^63-1` and at least `-2^63+1`.
        "add" => checked(
            "add",
            "  %bm1 = sub i64 %b, 1\n",
            "sadd",
            "i64 %a, i64 %bm1",
            None,
        ),
        // T(av-bv) = a - (b-1), by the same algebra. The runtime's own
        // pyrs_int_sub is add(a, neg(b)) — two calls, one of which allocates
        // when b is SMALL_MIN.
        "sub" => checked(
            "sub",
            "  %bm1 = sub i64 %b, 1\n",
            "ssub",
            "i64 %a, i64 %bm1",
            None,
        ),
        // (a-1) * bv = 2*av*bv = 2p, so the overflow flag alone is again an
        // exact small-range test and the product needs only `| 1` to tag.
        // Untagging both operands instead would need an explicit bound check,
        // because a product can wrap back into range.
        "mul" => checked(
            "mul",
            "  %am1 = sub i64 %a, 1\n  %bv = ashr i64 %b, 1\n",
            "smul",
            "i64 %am1, i64 %bv",
            Some("tag"),
        ),

        // `%` can never leave the small range: |r| < |bv| <= 2^62 and r takes
        // the sign of bv. So the only guard beyond smallness is bv != 0, which
        // in tagged form is b != T(0) = 1.
        //
        // Zero divisors route to the runtime rather than trapping inline:
        // pyrs_int_mod already produces the exact ZeroDivisionError text, type
        // and catchability, and an inline trap would duplicate the message and
        // terminate the block.
        "mod" => division(
            "mod",
            "  %g0 = and i1 %sm, %bnz\n",
            "%g0",
            &format!(
                "  %rt = srem i64 %av, %bv\n{}",
                floor_adjust("  %alt = add i64 %rt, %bv\n", "%rt")
            ),
        ),

        // The quotient leaves the small range in exactly one case: av =
        // SMALL_MIN with bv = -1, giving 2^62 = SMALL_MAX+1. For |bv| >= 2 the
        // quotient is at most 2^61 in magnitude, and for |bv| = 1 the
        // remainder is zero so no adjustment applies. T(-1) is -1 exactly,
        // which makes the edge test two compares on the raw words.
        //
        // sdiv and srem are emitted adjacent with identical operands so
        // DivRemPairs and x86 ISel fuse them into a single idiv.
        "floordiv" => division(
            "floordiv",
            &format!(
                "  %amin = icmp eq i64 %a, {TAGGED_SMALL_MIN}\n\
                 \x20 %bneg1 = icmp eq i64 %b, -1\n\
                 \x20 %edge = and i1 %amin, %bneg1\n\
                 \x20 %noed = xor i1 %edge, true\n\
                 \x20 %g0 = and i1 %sm, %bnz\n\
                 \x20 %g1 = and i1 %g0, %noed\n"
            ),
            "%g1",
            &format!(
                "  %qt = sdiv i64 %av, %bv\n  %rt = srem i64 %av, %bv\n{}",
                floor_adjust("  %alt = sub i64 %qt, 1\n", "%qt")
            ),
        ),

        "lt" => compare("lt", "slt"),
        "le" => compare("le", "sle"),
        "gt" => compare("gt", "sgt"),
        "ge" => compare("ge", "sge"),
        "eq" => compare("eq", "eq"),
        "ne" => compare("ne", "ne"),

        // For any small v, bit 62 and bit 63 of its two's-complement word are
        // equal, and that property is closed under &, | and ^. So the result
        // is always small and needs no range check. Bit 0 is 1&1 = 1 and
        // 1|1 = 1, already correctly tagged; only xor clears it.
        //
        // For `and` the guard value is the result, so the fast path is one
        // instruction reused twice.
        "and" => binary(
            "and",
            "i64",
            "  %f = and i64 %a, %b\n  %sm = trunc i64 %f to i1\n",
            "%sm",
            "  %s = call i64 @pyrs_int_and(i64 %a, i64 %b)\n",
        ),
        "or" => binary(
            "or",
            "i64",
            &format!("{}  %f = or i64 %a, %b\n", both_small()),
            "%sm",
            "  %s = call i64 @pyrs_int_or(i64 %a, i64 %b)\n",
        ),
        "xor" => binary(
            "xor",
            "i64",
            &format!(
                "{}  %x = xor i64 %a, %b\n  %f = or i64 %x, 1\n",
                both_small()
            ),
            "%sm",
            "  %s = call i64 @pyrs_int_xor(i64 %a, i64 %b)\n",
        ),

        // T(-av) = -2av+1 = 2 - a. Overflow is set iff av = SMALL_MIN, which
        // is exactly pyrs_int_neg's own check: -(-2^62) = SMALL_MAX+1.
        // The guard is mandatory — a plain `sub i64 2, %a` would silently
        // compute -SMALL_MIN as SMALL_MIN.
        "neg" => unary(
            "neg",
            "  %o = call { i64, i1 } @llvm.ssub.with.overflow.i64(i64 2, i64 %a)\n\
             \x20 %f = extractvalue { i64, i1 } %o, 0\n\
             \x20 %ovf = extractvalue { i64, i1 } %o, 1\n\
             \x20 %ok = xor i1 %ovf, true\n\
             \x20 %fast = and i1 %sm, %ok\n",
            "%fast",
        ),
        // ~av = -av-1, so T(~av) = -(2av+1) = -a. Inverting maps the small
        // range onto itself bijectively, so this cannot overflow and needs no
        // check beyond smallness. The runtime's pyrs_int_invert is three
        // calls, one of which allocates.
        "invert" => unary("invert", "  %f = sub i64 0, %a\n", "%sm"),

        // 2v overflows iff v leaves the small range, by the same half-integer
        // algebra as add. The doubled value is even, so `| 1` tags it without
        // wrapping.
        "box" => format!(
            "define internal i64 {sym}(i64 %a) alwaysinline {{\n\
             entry:\n\
             \x20 %o = call {{ i64, i1 }} @llvm.sadd.with.overflow.i64(i64 %a, i64 %a)\n\
             \x20 %d = extractvalue {{ i64, i1 }} %o, 0\n\
             \x20 %ovf = extractvalue {{ i64, i1 }} %o, 1\n\
             \x20 %f = or i64 %d, 1\n\
             \x20 br i1 %ovf, label %slow, label %join\n\
             slow:\n\
             \x20 %s = call i64 @pyrs_int_from_i64(i64 %a)\n\
             \x20 br label %join\n\
             join:\n\
             \x20 %r = phi i64 [ %f, %entry ], [ %s, %slow ]\n\
             \x20 ret i64 %r\n\
             }}\n\n",
            sym = symbol("box")
        ),
        // The slow edge may raise OverflowError; the fast edge is pure.
        "unbox" => format!(
            "define internal i64 {sym}(i64 %a) alwaysinline {{\n\
             entry:\n\
             \x20 %lsb = and i64 %a, 1\n\
             \x20 %sm = icmp ne i64 %lsb, 0\n\
             \x20 %f = ashr i64 %a, 1\n\
             \x20 br i1 %sm, label %join, label %slow\n\
             slow:\n\
             \x20 %s = call i64 @pyrs_int_as_i64(i64 %a)\n\
             \x20 br label %join\n\
             join:\n\
             \x20 %r = phi i64 [ %f, %entry ], [ %s, %slow ]\n\
             \x20 ret i64 %r\n\
             }}\n\n",
            sym = symbol("unbox")
        ),

        other => unreachable!("no int fast-path helper named {other}"),
    }
}

/// A one-operand helper, shaped like [`binary`].
fn unary(op: &str, fast_body: &str, cond: &str) -> String {
    format!(
        "define internal i64 {sym}(i64 %a) alwaysinline {{\n\
         entry:\n\
         \x20 %lsb = and i64 %a, 1\n\
         \x20 %sm = icmp ne i64 %lsb, 0\n\
         {fast_body}\
         \x20 br i1 {cond}, label %join, label %slow\n\
         slow:\n\
         \x20 %s = call i64 @pyrs_int_{op}(i64 %a)\n\
         \x20 br label %join\n\
         join:\n\
         \x20 %r = phi i64 [ %f, %entry ], [ %s, %slow ]\n\
         \x20 ret i64 %r\n\
         }}\n\n",
        sym = symbol(op)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the module is that these are the operations the
    /// emitter may ask for; a typo in a call site should not silently fall
    /// back to a runtime call.
    #[test]
    fn every_helper_defines() {
        for op in HELPERS {
            let text = define(op);
            assert!(text.starts_with("define internal "), "{op}: {text}");
            assert!(text.contains("alwaysinline"), "{op} must inline at -O0");
            assert!(text.contains(&symbol(op)), "{op} names itself");
            assert!(text.contains("ret "), "{op} returns");
        }
    }

    /// Each helper must keep the runtime call on its cold edge: the fast path
    /// is an optimization, never a reimplementation of bignum semantics.
    #[test]
    fn every_helper_keeps_a_runtime_slow_path() {
        for op in HELPERS {
            let text = define(op);
            assert!(
                text.contains("call i64 @pyrs_int_") || text.contains("call i32 @pyrs_int_"),
                "{op} lost its slow path:\n{text}"
            );
        }
    }

    /// A poison flag on a tagging operation would turn a wrong guard into
    /// undefined behaviour instead of a wrong answer.
    #[test]
    fn no_poison_flags() {
        for op in HELPERS {
            let text = define(op);
            for flag in [" nsw ", " nuw ", " exact "] {
                assert!(!text.contains(flag), "{op} carries{flag}:\n{text}");
            }
        }
    }

    /// Comparisons work on the raw tagged words. If an untag ever appears
    /// here, the monotonicity argument has been broken.
    #[test]
    fn comparisons_do_not_untag() {
        for op in ["lt", "le", "gt", "ge", "eq", "ne"] {
            let text = define(op);
            assert!(!text.contains("ashr"), "{op} untags:\n{text}");
        }
    }

    /// Division is the one shape that needs its own block: sdiv/srem are UB on
    /// a zero divisor, so they must be dominated by the guard rather than
    /// speculated into the entry block.
    #[test]
    fn division_guards_the_divisor() {
        for op in ["mod", "floordiv"] {
            let text = define(op);
            assert!(
                text.contains("%bnz = icmp ne i64 %b, 1"),
                "{op} guards bv != 0"
            );
            let guard_at = text.find("br i1").expect("has a guard branch");
            let div_at = text.find("rem i64").expect("has a remainder");
            assert!(guard_at < div_at, "{op} speculates the division:\n{text}");
        }
    }

    /// `//` is the only operation whose result can leave the small range for
    /// in-range operands.
    #[test]
    fn floordiv_guards_the_overflow_edge() {
        let text = define("floordiv");
        assert!(
            text.contains(TAGGED_SMALL_MIN),
            "floordiv checks av == SMALL_MIN"
        );
        assert!(text.contains("%bneg1 = icmp eq i64 %b, -1"), "and bv == -1");
        // `%` cannot overflow, so it must not pay for the check.
        assert!(!define("mod").contains(TAGGED_SMALL_MIN));
    }
}
