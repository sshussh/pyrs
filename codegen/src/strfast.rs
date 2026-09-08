//! Inline fast path for string equality.
//!
//! `==` and `!=` on `str` went through `pyrs_str_cmp`, which computes a full
//! three-way lexicographic ordering — `memcmp` included — and then throws the
//! ordering away. It has no length short-circuit and no identity check, so two
//! one-character strings cost an opaque call plus a `memcmp` PLT call to
//! compare a single byte. In the `strings` benchmark that pair is 43% of the
//! runtime: it walks an 88k-character string sixty times, comparing each
//! character against five literals.
//!
//! Three facts decide most comparisons without leaving the caller:
//!
//! * **Same pointer, same string.** Only positively: a literal is a module
//!   global (`@.str.N`) while `s[i]` returns the runtime's interned singleton,
//!   so two equal one-character strings routinely have different addresses.
//!   Unequal pointers prove nothing and fall through.
//! * **Different byte length, different string.** `len` is the second header
//!   word, so this is one load each.
//! * **One byte each, compare the byte.** Which is the character-iteration
//!   case, and the one `memcmp` was being called for.
//!
//! Anything else is the same `pyrs_str_cmp` call as before. A null operand
//! also goes there, because `check_ref` is what turns a local read before
//! assignment into `UnboundLocalError`, and that must not be pre-empted by a
//! load here.
//!
//! Ordering comparisons (`<`, `<=`, `>`, `>=`) keep the call: they need the
//! full lexicographic answer, and no benchmark shape makes them hot.

/// `PyrsStr` is `{ i64 cplen; i64 len; char data[] }`, so the byte count is at
/// offset 8 and the bytes start at 16. `emit` hard-codes the same 16 when it
/// hands a literal's payload to `pyrs_die`.
const LEN_OFFSET: u32 = 8;
const DATA_OFFSET: u32 = 16;

/// Every helper this module can emit.
pub const HELPERS: &[&str] = &["eq", "ne", "index"];

/// LLVM symbol for a helper. Dots keep it disjoint from `mangle`d user
/// functions, which are `pyrs_<python identifier>`.
pub fn symbol(op: &str) -> String {
    format!("@pyrs.str.{op}")
}

/// The runtime's table of interned single-byte strings, as codegen sees it.
///
/// The C side statically initializes `PyrsSingleChar pyrs_single_chars[256]`
/// and `_Static_assert`s the 24-byte stride this type implies, so the two
/// stay in lockstep.
pub const SINGLE_CHARS_DECL: &str =
    "@pyrs_single_chars = external global [256 x { i64, i64, [2 x i8] }]\n";

/// Whether a helper needs [`SINGLE_CHARS_DECL`].
pub fn needs_single_chars(op: &str) -> bool {
    op == "index"
}

/// The IR text defining one helper.
///
/// # Panics
///
/// Panics if `op` is not in [`HELPERS`].
pub fn define(op: &str) -> String {
    if op == "index" {
        return define_index();
    }
    let (same, difflen, slow_pred) = match op {
        // Identical pointers are equal; different lengths are not.
        "eq" => ("true", "false", "eq"),
        "ne" => ("false", "true", "ne"),
        other => unreachable!("no str fast-path helper named {other}"),
    };
    // For `ne` the byte comparison is inverted along with everything else.
    let byte_cmp = if op == "eq" { "eq" } else { "ne" };
    format!(
        "define internal i1 {sym}(ptr %a, ptr %b) alwaysinline {{\n\
         entry:\n\
         \x20 %anull = icmp eq ptr %a, null\n\
         \x20 %bnull = icmp eq ptr %b, null\n\
         \x20 %null = or i1 %anull, %bnull\n\
         \x20 br i1 %null, label %slow, label %ident\n\
         ident:\n\
         \x20 %same = icmp eq ptr %a, %b\n\
         \x20 br i1 %same, label %join, label %lens\n\
         lens:\n\
         \x20 %ap = getelementptr inbounds i8, ptr %a, i64 {LEN_OFFSET}\n\
         \x20 %bp = getelementptr inbounds i8, ptr %b, i64 {LEN_OFFSET}\n\
         \x20 %la = load i64, ptr %ap\n\
         \x20 %lb = load i64, ptr %bp\n\
         \x20 %eqlen = icmp eq i64 %la, %lb\n\
         \x20 br i1 %eqlen, label %sized, label %join\n\
         sized:\n\
         \x20 %one = icmp eq i64 %la, 1\n\
         \x20 br i1 %one, label %byte, label %slow\n\
         byte:\n\
         \x20 %adp = getelementptr inbounds i8, ptr %a, i64 {DATA_OFFSET}\n\
         \x20 %bdp = getelementptr inbounds i8, ptr %b, i64 {DATA_OFFSET}\n\
         \x20 %ab = load i8, ptr %adp\n\
         \x20 %bb = load i8, ptr %bdp\n\
         \x20 %bres = icmp {byte_cmp} i8 %ab, %bb\n\
         \x20 br label %join\n\
         slow:\n\
         \x20 %c = call i32 @pyrs_str_cmp(ptr %a, ptr %b)\n\
         \x20 %s = icmp {slow_pred} i32 %c, 0\n\
         \x20 br label %join\n\
         join:\n\
         \x20 %r = phi i1 [ {same}, %ident ], [ {difflen}, %lens ], \
         [ %bres, %byte ], [ %s, %slow ]\n\
         \x20 ret i1 %r\n\
         }}\n\n",
        sym = symbol(op)
    )
}

/// `s[i]` when every code point is one byte.
///
/// `pyrs_str_index` is 41% of the `strings` benchmark after equality was
/// inlined: it walks an 88k-character string sixty times, and each character
/// costs an opaque call that re-derives what the caller already knows.
///
/// The fast path is the same test the runtime makes — `cplen == len` means one
/// byte per code point — followed by a bounds check and a load, ending in the
/// address of an interned entry. Nothing is allocated, so nothing can collect.
///
/// Out of range goes to the runtime rather than trapping here, so the
/// `IndexError` text lives in one place; so does a null receiver, whose
/// `check_ref` is what produces `UnboundLocalError`.
fn define_index() -> String {
    format!(
        "define internal ptr {sym}(ptr %s, i64 %i) alwaysinline {{\n\
         entry:\n\
         \x20 %null = icmp eq ptr %s, null\n\
         \x20 br i1 %null, label %slow, label %hdr\n\
         hdr:\n\
         \x20 %cpl = load i64, ptr %s\n\
         \x20 %lp = getelementptr inbounds i8, ptr %s, i64 {LEN_OFFSET}\n\
         \x20 %len = load i64, ptr %lp\n\
         \x20 %ascii = icmp eq i64 %cpl, %len\n\
         \x20 %neg = icmp slt i64 %i, 0\n\
         \x20 %plus = add i64 %i, %cpl\n\
         \x20 %adj = select i1 %neg, i64 %plus, i64 %i\n\
         \x20 %below = icmp slt i64 %adj, 0\n\
         \x20 %above = icmp sge i64 %adj, %cpl\n\
         \x20 %oob = or i1 %below, %above\n\
         \x20 %inrange = xor i1 %oob, true\n\
         \x20 %fast = and i1 %ascii, %inrange\n\
         \x20 br i1 %fast, label %pick, label %slow\n\
         pick:\n\
         \x20 %dp = getelementptr inbounds i8, ptr %s, i64 {DATA_OFFSET}\n\
         \x20 %bp = getelementptr inbounds i8, ptr %dp, i64 %adj\n\
         \x20 %b = load i8, ptr %bp\n\
         \x20 %bi = zext i8 %b to i64\n\
         \x20 %e = getelementptr inbounds [256 x {{ i64, i64, [2 x i8] }}], \
         ptr @pyrs_single_chars, i64 0, i64 %bi\n\
         \x20 br label %join\n\
         slow:\n\
         \x20 %r2 = call ptr @pyrs_str_index(ptr %s, i64 %i)\n\
         \x20 br label %join\n\
         join:\n\
         \x20 %r = phi ptr [ %e, %pick ], [ %r2, %slow ]\n\
         \x20 ret ptr %r\n\
         }}\n\n",
        sym = symbol("index")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_helper_defines() {
        for op in HELPERS {
            let text = define(op);
            assert!(text.starts_with("define internal "), "{op}: {text}");
            assert!(text.contains("alwaysinline"), "{op} must inline at -O0");
            assert!(text.contains(&symbol(op)), "{op} names itself");
        }
    }

    /// The fast path is an optimization; the runtime call is the definition of
    /// the answer for everything it does not decide.
    #[test]
    fn every_helper_keeps_the_runtime_slow_path() {
        for op in HELPERS {
            let text = define(op);
            assert!(
                text.contains("call i32 @pyrs_str_cmp(")
                    || text.contains("call ptr @pyrs_str_index("),
                "{op} lost its slow path:\n{text}"
            );
        }
    }

    /// A null operand must reach `check_ref`, which is what reports
    /// `UnboundLocalError`. Dereferencing it here would segfault instead.
    #[test]
    fn a_null_operand_goes_to_the_runtime() {
        for op in HELPERS {
            let text = define(op);
            let null_at = text.find("%null =").expect("null test");
            let load_at = text.find("load i64").expect("header load");
            assert!(null_at < load_at, "{op} loads before testing for null");
        }
    }

    /// The interned table is indexed directly, so its LLVM type has to match
    /// the C struct byte for byte. `runtime.c` `_Static_assert`s the same
    /// 24-byte stride this type implies.
    #[test]
    fn the_interned_table_layout_is_pinned() {
        assert!(SINGLE_CHARS_DECL.contains("[256 x { i64, i64, [2 x i8] }]"));
        assert!(define("index").contains("[256 x { i64, i64, [2 x i8] }]"));
        assert!(needs_single_chars("index"));
        assert!(!needs_single_chars("eq"));
    }

    /// Indexing must not trap on its own: the IndexError text belongs in one
    /// place, so out-of-range falls through to the runtime.
    #[test]
    fn out_of_range_indexing_defers_to_the_runtime() {
        let text = define("index");
        assert!(text.contains("%oob = or i1 %below, %above"));
        assert!(text.contains("br i1 %fast, label %pick, label %slow"));
        assert!(!text.contains("pyrs_die"), "index must not trap inline");
    }

    /// Pointer identity may only decide equality positively. If this ever
    /// grows an `else` that concludes inequality, two equal strings at
    /// different addresses — a literal and an interned character, which is the
    /// benchmark's exact shape — would start comparing unequal.
    #[test]
    fn pointer_identity_is_positive_only() {
        let text = define("eq");
        assert!(text.contains("br i1 %same, label %join, label %lens"));
        assert!(
            text.contains("%eqlen = icmp eq i64 %la, %lb"),
            "unequal pointers must still compare contents"
        );
    }
}
