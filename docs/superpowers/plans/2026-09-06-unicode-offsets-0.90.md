# Unicode code point offsets — 0.90.0

## Review and milestone choice

Five rows in the measured-defect table were Unicode. Probed against CPython
3.14.7 on the 0.89.0 release binary before any change:

```
len("héllo")            CPython 5              PyRs 6
"héllo"[1]              CPython é              PyRs a broken byte
"héllo"[1:3]            CPython él             PyRs é
for c in "héllo"        CPython h é l l o      PyRs h + 2 fragments + l l o
"héllo".find("l")       CPython 2              PyRs 3
len("🐍")               CPython 1              PyRs 4
"naïve café".upper()    CPython NAÏVE CAFÉ     PyRs NAïVE CAFé
"ß".upper()             CPython SS             PyRs ß
"é".isalpha()           CPython True           PyRs False
```

`PyrsStr` is a UTF-8 byte buffer and every operation except `ord`, `chr` and
`ascii` treated it as a plain byte array. Those three already decode real
code points through `utf8_next`, which nothing else called. The
representation was never wrong; the operations disagreed with it.

The two M2 rows (`[1, 2.5, 1]` and virtual `__ne__`) were already closed in
0.89 and were re-verified byte-exact against CPython before this milestone
started, rather than taken from the table.

## Why this splits at a property boundary, not an offset one

The roadmap warns against "partial fixes mixing byte and character offsets".
That rules out splitting by method. It does *not* rule out splitting offsets
from properties: the last three rows above are questions about Unicode
character *properties* (which characters are letters, how they upper-case),
answered by data tables, while the first six are questions about *offsets*.

0.90 makes every offset a code point. Case transforms and the `is*`
predicates keep their already-documented ASCII-only behavior, so at no point
does one operation count bytes while another counts characters. 0.91 replaces
the ASCII property helpers wholesale with generated Unicode 16.0.0 tables.

## Representation: UTF-8 with a cached count

The alternative was a PEP 393-style kind-tagged buffer (O(1) indexing, as
CPython guarantees). It was rejected because it converts at every boundary:
`print`'s raw `fwrite`, file I/O, the CPython bridge, literals, and every
cross-kind concat. Keeping UTF-8 leaves all of those untouched.

```c
typedef struct {
    long long cplen;   /* code points -- what len() returns */
    long long len;     /* UTF-8 bytes */
    char data[];
} PyrsStr;
#define STR_IS_ASCII(s) ((s)->cplen == (s)->len)
```

Two decisions carry most of the leverage:

- **`cplen` is first.** Codegen's `emit_len` blindly loads the first `i64` of
  every sized object (str/list/tuple/dict/set), so putting the Python answer
  there means `len()` needed *no codegen change at all*.
- **The byte count keeps the name `len`.** Nearly every existing `->len` on a
  string means bytes — `memcmp`, `memcpy`, `fwrite`, the substring searches,
  the bridge's `PyUnicode_DecodeUTF8`. Naming the new field `cplen` instead
  of renaming the old one shrank the diff from "touch every string function"
  to "touch the ones that actually deal in offsets".

The ASCII fast path needs no flag: every code point being one byte *is*
ASCII, so `cplen == len` is the test.

## Making 35 producers auditable

`str_alloc` had 35 callers. Rather than hand-audit each, `str_alloc` now sets
`cplen = -1` and every producer must finish with `str_done_ascii` (ASCII by
construction: `str(int)`, `str(float)`, `str(bool)`, `ascii()`, numeric
format padding), `str_done_cplen` (the count is arithmetic: concat, repeat,
slice, `removeprefix`, padding), or `str_done_scan` (bytes of unknown
provenance: file reads, `input()`, `translate`, JSON). A missed site makes
`len()` return `-1` — loud and immediate, never a silent miscount. A script
over the file confirms every site finishes.

## What the tests found that the plan did not

- **Four more allocation sites bypassed `str_alloc` entirely**, hand-rolling
  `pyrs_gc_alloc(sizeof(long long) + n + 1, PYRS_GC_STRING)` for exception
  messages and object reprs, plus one `(const char *)msg + 8` in generator
  `throw()`. These surfaced as two e2e failures, one of them
  `malloc(): invalid size` — a heap corruption, because the allocation was
  sized for the old header. All five now route through `str_alloc` /
  `str_from_utf8`, so the layout lives in exactly one place.
- **`list(str)` and `set(str)` had their own byte loops** in
  `pyrs_list_from_str` / `pyrs_set_from_str`, independent of the `for`
  lowering. `list("héllo")` returned six elements, two of them fragments.
- **Two compile-time checks counted bytes**: `str.maketrans("é", "z")` was
  rejected as unequal length, and the `center`/`ljust` fill-character check
  rejected any non-ASCII fill. Both now use `chars().count()`.
- **`partition` mixed both conventions in one expression** — it took a code
  point index from `find()` and fed it straight to byte slicing. This is
  precisely the failure mode the roadmap warns about, and it is why the
  milestone is scoped by offset rather than by method.

## Iteration and the sequential-access memo

`for c in s` desugars to `while i < len(s): c = s[i]`, which is *correct*
under code point semantics but would be O(n²) on non-ASCII text. Rather than
add new IR, `str_byte_of_cp` carries a one-entry thread-local memo of the
last (string, code point index, byte offset) triple, so a forward walk
resumes instead of rescanning. This also fixes explicit
`for i in range(len(s)): s[i]` loops, which no IR change would have caught.

The memo keys on an object address, and the collector is nonmoving but does
free objects whose addresses can be reused. `pyrs_str_cache_invalidate` is
therefore declared in `gc.h` beside the other runtime.c callbacks and called
once per collection from `gc_collect_inner`, before anything can be swept.

## The extension test encoded the old defect

`compatibility/extension_kernels.py` built deliberately invalid UTF-8 as
`"é"[0]` — the first *byte* of a two-byte character — and the bridge test
asserted `UnicodeDecodeError`. Indexing no longer splits a character, so the
premise is gone. The kernel now uses `chr(0xD800)`: a lone surrogate is
what still produces bytes the bridge must refuse, and it exercises the same
export- and root-release cleanup path. `chr`/`ord` were added to the
extension frontend's allowlist so the kernel can express it; losing coverage
of a memory-safety path was the worse trade.

The compatibility manifest pinned `unicode-length-gap` to the old wrong
output (`native_stdout: "8\n"`), so `make compatibility` reported
`unexpected_pass` on the first run after the fix — the classification working
exactly as it did for 0.89's numeric fix. The case is renamed
`unicode-length-gap` → `unicode-text` (file `unicode_length.py` →
`unicode_text.py`), broadened to cover indexing, slicing, search, splitting,
padding and `ord`/`chr`, and its expectation flips to `pass`.

## Implementation

- [x] `PyrsStr` gains `cplen` as its first word; `STR_IS_ASCII` derived from it.
- [x] `str_alloc` / `str_done_ascii` / `str_done_cplen` / `str_done_scan` /
      `str_from_utf8`; all 35 producers finish, verified by script.
- [x] `str_byte_of_cp` / `str_cp_of_byte` / `str_cp_between`, with the memo
      and its GC invalidation hook.
- [x] Offsets: index, slice (including strided), `find` family bounds and
      results, `count`, affix bounds, `strip(chars)`, `str_just`, `zfill`,
      `expandtabs`, `format_pad` widths and precision, `partition`,
      `translate`/`maketrans`, `list(str)`, `set(str)`, `ord`.
- [x] Codegen: literal constants become `{ i64 cplen, i64 len, [n x i8] }`,
      and the four hardcoded `i64 8` payload offsets become `i64 16`.
- [x] Semantic: `maketrans` and fill-character checks count characters.
- [x] 24 differential tests in `cli/tests/unicode_text.rs`, at O0/O2/O3.
- [x] Manifest rename and expectation flip; extension kernel repointed.
- [x] Bump the 7 crates, lockfile, README, SPECIFICATIONS and ROADMAP; backfill
      the 0.87–0.89 changelog entries, which had gone unwritten.

## Boundaries

No Unicode case or property data — that is 0.91, and `"ß".upper()`,
`"naïve café".upper()` and `"é".isalpha()` stay wrong until it lands. No
normalization or grapheme clusters. No `bytes`/`bytearray`, no `.encode()`,
no encodings other than UTF-8. Indexing a non-ASCII string is O(n), not
CPython's O(1); the memo makes sequential access amortised O(1) but random
access is a documented divergence. Two pre-existing lexer gaps were found and
left alone as unrelated: `\xNN`/`\uXXXX` escapes are not recognised, and an
f-string replacement field cannot contain nested quotes (PEP 701). A
non-ASCII fill character in a *numeric* format spec is still parsed as a
byte.

## Validation (2026-09-06)

Toolchain: Rust 1.96.1, LLVM 22.1.8, CPython 3.14.7, GCC 16.2.1.

| Check | Result |
|-------|--------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test -p pyrs --test unicode_text` | 24 passed |
| `cargo test --workspace` | 1101 passed, 0 failed, 0 ignored |
| `make examples` | 13/13 byte-exact |
| `make compatibility` | native 18 pass / 0 known_gap; compat 6 pass |
| Release `pyrs --version` | `PyRs 0.90.0` |

Performance, A/B against a `1fffee2` worktree build on the same host, best
of 7:

| Workload | 0.89.0 | 0.90.0 | Ratio |
|----------|--------|--------|-------|
| ASCII string saturation (index, count, find, split, join, upper, slice) | 141.0 ms | 148.5 ms | 1.053 |
| `fib(30)`, no strings | 15.7 ms | 15.4 ms | 0.979 |

The 5.3% is the extra header word's cache cost plus the ASCII branch in
indexing, on a loop that does nothing but string work. It is a real cost, not
noise, and it is the price of the representation choice; the alternative
(kind-tagged buffers) would have paid at every I/O and bridge boundary
instead. Non-string code is unaffected.
