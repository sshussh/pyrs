# The first stdlib module written in PyRs

**Status: implemented in 0.139.** `stdlib/json.py`.

## The module was stubs

Every `loads_*` body was replaced by the compiler with a call into a JSON
parser written in C, and there was no dynamic `loads` at all. A caller had to
know the document's shape in advance and name it in the function:
`loads_dict_str_int`, `loads_list_bool`, and eleven more. Anything else — a
mixed object, a nested array, a field that might be null — had no spelling.

## Why it could be written now

`docs/PRIMITIVES.md` §9 freezes the stdlib "until the core language is far
enough along that libraries can be written in **pure PyRs**", and it named
this module as the test case:

> `json.loads` (dynamic) — **Later, pure PyRs** — Needs optional/union/`Any`
> or a value model — language first.

Four things landed since that was written, and together they are the value
model it was waiting for:

| | |
|---|---|
| `object` / `Any` as a return type | 0.138.2 |
| `isinstance` narrowing on `Any` | 0.136 |
| An expected type reaching a container slot | 0.136 |
| Closed-world classes with methods and user exceptions | earlier |

The parser needs no compiler special case. It is a class with methods, a
`ValueError` subclass, recursion, and `dict[str, object]` / `list[object]`
built element by element.

**The policy table now separates two things it had conflated.** *Growing* the
stdlib surface — new modules, new API — is still frozen. *Converting a stub to
a real PyRs body* is the exit criterion arriving, one module at a time, and is
encouraged. `json` is the first to make that move; `os.path` was always pure.

## The C parser is gone, not kept

Deleted: `ir::ExprKind::JsonLoads`, `JsonLoadsKind`, the emit arms, the
`declare` lines, `json_loads_kind` / `json_loads_ret` in semantic, and ~340
lines of `runtime.c`. Two JSON implementations in one project drift, and the
compiled one is now the only one.

`json_skip_ws`, `json_match` and `json_expect_end` went with it — they were
the parser's helpers, orphaned once it left. The `dumps` half of the runtime
stayed and its section comment now says so.

## Parity, measured

48 valid documents and 44 malformed ones, diffed against CPython's `json`:

- **48 of 48 values identical.** Scalars, nesting, whitespace between tokens,
  bignums beyond i64, astral escapes, surrogate pairs.
- **41 of 44 error messages byte-identical**, position included —
  `Expecting ',' delimiter: line 1 column 8 (char 7)`. `JSONDecodeError`
  subclasses `ValueError` as CPython's does, so `except ValueError` catches it.

Getting the last few required matching two behaviours that are not obvious
from the grammar:

- **CPython scans a *maximal valid* number and reports the rest as extra
  data.** `01` is the number `0` followed by junk; `1.` is `1` followed by
  `.`; `1e` is `1` followed by `e`. The scanner only consumes a `.` when a
  digit follows, and an `e` only when a digit (behind an optional sign)
  follows. Getting this wrong produces a plausible but different message for
  six inputs.
- **Trailing commas have their own message** — `Illegal trailing comma before
  end of object`, reported at the comma, not at what follows it.

`NaN`, `Infinity` and `-Infinity` are accepted, matching CPython's decoder
rather than the JSON grammar. `-Infinity` has to be checked *before* the
number branch, which would otherwise claim the `-`.

### The one deliberate difference

A **lone surrogate escape** — `"\ud800"` with no low half — is an error here
where CPython yields a lone surrogate. A PyRs `str` is well-formed UTF-8 and
has no way to hold one. A surrogate *pair* decodes to the astral code point it
denotes, which is the case that actually appears in real documents.

Nesting is capped at 200 so an adversarial document raises rather than
overflowing the native stack.

## Speed, and why both numbers belong in the record

```
20 x 2000-object document
  vs CPython running the same PyRs source   1.8x faster
  vs CPython's C json module                6.8x slower
```

The first number is what compiling this module buys. The second is what a C
extension buys, and it would be dishonest to omit: a pure-PyRs parser does not
beat one.

1.8× is low against the corpus's 7.1×, and the reason is worth knowing for the
next library: this workload is `Any` boxing — one `PyrsUnionBox` allocation
per parsed value — and hash-table inserts, not arithmetic. The optimisation
passes that took `nbody` to 41× do not reach it. If `json` needs to be faster,
the lever is the boxing, not the parser.

## `dumps` stays compiler-lowered

It dispatches on the **static** type of its argument, which is what makes
`dumps([1, 2, 3])` serialise a `list[int]`. A body written here could only
take `object`, and a concrete `list[int]` boxed into `object` carries a
different runtime tag (4) than the `list[object]` such a body would read it
back as (68) — so it would trap on exactly the calls that matter.

That was measured, not assumed, before deciding. The reason is written into
the module header and the PRIMITIVES table so the asymmetry does not read as
an oversight.

## How this is checked

`cli/tests/json_module.rs` — 9 tests at -O0/-O2/-O3 and under
`PYRS_GC_STRESS=1`. Values, escapes and the non-standard constants are
differential against CPython, and so are the **error messages**, which is the
half that would otherwise rot silently.

The typed helpers are asserted against recorded output instead: CPython's
`json` has no `loads_int`, so it cannot be their oracle. They also pin that a
`bool` is refused where an `int` is wanted, which Python's own subtyping makes
easy to get wrong.

`make coverage` gains a dynamic-`loads` probe and an error-parity probe;
stdlib coverage goes 3 to 5 of 14.

## What the module needed from the compiler

The first draft said two things Python would not. One was a missing feature.
The other was not, and finding that out was worth more.

**A call to a function that always raises terminates.** `def fail(m): raise
ValueError(m)` forced every caller to write an unreachable `return` after it,
to satisfy "every path through a value-returning function must return". The
analysis (`block_returns` / `stmt_returns`) works on IR, so the fix is a
pre-pass — `pre_register_no_return` walks the AST before anything is lowered
and records every function and method whose body always raises. Doing it on
the AST is what makes definition order irrelevant, the same reason
`pre_infer_free_func_rets` exists. A conditional raise does not count, so the
missing-return check keeps its teeth.

**Narrowing `object` to a container was tried and reverted**, and the reason
is the more useful half of this milestone. 0.136 had declined it, recording
that "no element type is recoverable from the tag". That reasoning only holds
for a *static union member*, so it looked like an omission — peel to the fully
dynamic container, let `FromAny`'s tag check catch a mismatch.

It regressed working code. `object` can hold a `list[int]` (tag 4) as well as
a `list[object]` (tag 68), `isinstance(v, list)` is true for both, and the
peel is applied at every *read* of the narrowed name. So this, which printed
fine before, started raising:

```python
xs: list[int] = [1, 2]
a: object = xs
if isinstance(a, list):
    print(a)          # TypeError: expected list[Any], got incompatible dynamic value
```

Caught by probing the change against a case the tests did not cover, not by
the tests. Reverted, and the annotation the module carries is now documented
for what it is: not a restatement the compiler could infer away, but the
program choosing between two runtime encodings that `isinstance` cannot
distinguish, checked at run time.

Fixing it properly means making `Any` hold one canonical container encoding,
which requires answering the aliasing question 0.137 recorded — converting a
`list[int]` to a `list[object]` is an O(n) re-box, and the copy breaks
aliasing.

The rule this milestone followed: when a library written in PyRs has to say
something Python would not, that is a compiler gap — **unless** the reason is
a representation the language genuinely has, in which case the code stays
explicit and the reason gets written down.

## Noticed while writing it

`from json import (a, b, c)` — a parenthesized import list — is not parsed.
Ordinary Python, common for long lists, and unrelated to this module. Recorded
rather than fixed here.
