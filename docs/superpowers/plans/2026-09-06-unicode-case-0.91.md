# Unicode case and character classes — 0.91.0

## Review and milestone choice

0.90 made every string *offset* a code point and deliberately stopped there,
leaving character *properties* on the documented ASCII-only contract. The
three remaining measured-defect rows are all property questions:

```
"naïve café".upper()    CPython NAÏVE CAFÉ    PyRs NAïVE CAFé
"ß".upper()             CPython SS            PyRs ß
"é".isalpha()           CPython True          PyRs False
```

That boundary was chosen so nothing ever mixed byte and character offsets in
between — the failure mode the roadmap warns about. It also means this
milestone is a self-contained table change: no offset logic moves.

## Tables generated from the oracle, not from the UCD

`scripts/gen_unicode_tables.py` walks `range(0x110000)` and asks the
installed CPython — `str.upper/lower/title/casefold` and `str.is*` — rather
than re-parsing `UnicodeData.txt` and friends.

The roadmap already names the installed CPython 3.14 as the specification for
differential tests. Deriving the tables from it gives agreement with that
specification *by construction*: the differential suite cannot disagree with
the oracle about a per-character mapping, only about the string-level
algorithms built on top. It also needs no network in CI, and it covers
SpecialCasing — `"ß".upper() == "SS"` — which `unicodedata` does not expose
but `str.upper()` does.

The cost is that the tables are pinned to whatever interpreter generated
them. `make hygiene` therefore compares the stamped `PYRS_UNIDATA_VERSION`
against the running interpreter's `unicodedata.unidata_version` and fails on
a mismatch, with its own two failure-path tests in `scripts/test_gates.py`
(wrong version, missing file).

## Table shape

Per code point: a 12-bit property mask, plus four case mappings
(upper/lower/title/fold). Single-character mappings are stored as a **delta**
from the code point, which is what makes whole alphabets collapse onto one
record — every ASCII letter and every Cyrillic letter shares `-32` and `+32`.
Multi-character mappings (up to three code points) index a shared flat array.

A two-stage index with 128-entry deduplicated blocks then maps a code point
to a record.

| | |
|---|---|
| Distinct records for all 1,114,112 code points | 303 |
| Stage-1 entries / stage-2 entries | 8,704 / 37,760 |
| Multi-character expansion entries | 367 |
| `unicode_data.c` | 155 KiB |
| Compile cost at `-O2` | 40 ms, against `runtime.c`'s 2.2 s |

Per-compile cost is not a consideration, which is why the tables ship as
ordinary C compiled with every program rather than needing a prebuilt object.

## String-level algorithms

Only the per-character data comes from the tables; the string-level rules are
CPython's, implemented against them:

- `title()` and `istitle()` track `previous_is_cased`, where "cased" is
  upper, lower **or** titlecase, and start a word with the *titlecase*
  mapping rather than the uppercase one — so `"ǅungla".title()` is right.
- `isupper()` / `islower()` treat a titlecase character (Lt) as cased but as
  neither upper nor lower, so `"ǅ".isupper()` and `"ǅ".islower()` are both
  False while `"Ǆ".isupper()` is True.
- `capitalize()` title-cases the first character and lower-cases the rest.
- `repr()` escapes by Unicode printability rather than byte range, so
  `repr("café")` is `'café'` and a zero-width space becomes `​`. The
  same rule had to be applied a second time in `print_str_repr`, the separate
  path used when a string appears inside a printed container.

Because a case mapping can change length, the transforms build through a
growable `StrBuf` rather than writing into a same-size buffer, and 0.90's
code point bookkeeping carries the new count out.

## Whitespace

`is_py_space` (six ASCII characters, tested per byte) is replaced by a
table-backed `cp_is_space`, and the `strip` / `split_ws` / `rsplit_ws` scans
now advance by whole UTF-8 sequences in both directions. Scanning per byte
would have been not just incomplete but unsafe here: a multi-byte character
whose trailing byte happened to match could be split. `splitlines` already
recognised the full Unicode boundary set and is unchanged.

## What testing turned up

- **The suite's one timeout test became flaky.** `while_local_optional_
  reassign_none_terminates` budgets 5s, which has to cover a whole
  `pyrs compile`. That is ~2.4s serially — dominated by `runtime.c`'s 2.2s,
  not by the 40ms of new tables — and tips over 5s under a saturated
  parallel run. The test exists to catch an *infinite* loop, so its budget is
  now 30s, matching the three other timeout tests in the file. The underlying
  point stands and is already a roadmap item: building the C runtime on every
  user compile is the dominant compile cost.
- `str.maketrans` and the fill-character check had **compile-time** length
  checks counting bytes; those were fixed in 0.90 once non-ASCII arguments
  became expressible.

## Implementation

- [x] `scripts/gen_unicode_tables.py`, output committed as
      `codegen/runtime/unicode_data.{c,h}`, wired into `codegen/src/lib.rs`
      and written and compiled by both `cli/src/main.rs` and
      `cli/src/extension.rs`.
- [x] `upper`/`lower`/`casefold`/`capitalize`/`title`/`swapcase` through a
      growable buffer; `xrealloc` added alongside `xmalloc`.
- [x] All eleven `is*` predicates plus `isascii` as `cplen == len`.
- [x] Unicode whitespace for `strip`/`split`/`rsplit`, scanning by character.
- [x] `repr` and `print_str_repr` escape by printability.
- [x] `make hygiene` checks the stamped Unicode version, with failure-path
      tests.
- [x] 13 further differential tests (37 total in `cli/tests/unicode_text.rs`).
- [x] Version bump, README, SPECIFICATIONS, ROADMAP, GUIDE, CHANGELOG.

## Boundaries

No normalization (NFC/NFD/NFKC/NFKD) and no grapheme-cluster segmentation —
`len` counts code points, as CPython does, so `"e" + U+0301` is 2. No
locale-sensitive casing beyond CPython's default (`"İ".lower()` matches
CPython's two-code-point result, not the Turkish-locale one). No
`bytes`/`bytearray`/`memoryview`, no `.encode()`, no codecs other than UTF-8.
Lone-surrogate behavior is still unspecified: `chr(0xD800)` produces bytes
that the CPython bridge rejects. Indexing a non-ASCII string remains O(n).
The lexer still accepts no `\xNN`/`\uXXXX` escapes and no nested quotes in an
f-string replacement field.

## Validation (2026-09-06)

Toolchain: Rust 1.96.1, LLVM 22.1.8, CPython 3.14.7, GCC 16.2.1,
Unicode 16.0.0.

| Check | Result |
|-------|--------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test -p pyrs --test unicode_text` | 37 passed |
| `cargo test --workspace` | 1114 passed, 0 failed, 0 ignored |
| `make examples` | 13/13 byte-exact |
| `make compatibility` | native 18 pass / 0 known_gap; compat 6 pass |
| `make hygiene` | 0.91.0 across 20 sites; Unicode 16.0.0 matches; 104 links resolve |
| Release `pyrs --version` | `PyRs 0.91.0` |

Every row in the 2026-09-05 measured-defect table is now closed.
