# Operators on a dynamic value

**Status: implemented in 0.143.0.** Tier 2 of
[the dynamism plan](2026-09-10-dynamism-at-native-speed.md), whose D2 this
closes for operators. Tier 1 is
[the register-pair representation](2026-09-10-unbox-any.md).

## Why this had to come second

Tier 1 made reading a dynamic value free. It did nothing for *using* one:

```python
a: object = 7
b: object = 2
print(a + b)     # error: operator '+' is not supported for values of type Any
```

Eleven probes against 0.142.0 mapped the frontier. Working without narrowing:
`print`, `bool`, `str`, `repr`, `len`, `in` (as the element), f-string
interpolation, indexing, iteration, `.keys()`, `isinstance` narrowing. Refused:
every arithmetic operator, every comparison, every unary operator, `abs()`, and
method calls.

The ordering mattered for a reason the plan stated up front and the literature
confirmed: **a kernel over a boxed representation would have made every one of
these compile and every one of them run at roughly CPython's speed.** The only
published study that fed predicted types to an AOT compiler reports 1.0× — no
speedup at all — on the kernel where its prediction missed. Correctness without
a fast representation buys nothing.

## The shape

Three runtime entry points, dispatching on the tags the operands already carry:

```c
long long pyrs_dyn_binop(long long a, long long b, int op);
int       pyrs_dyn_compare(long long a, long long b, int op);
long long pyrs_dyn_unary(long long a, int op);
```

An op code per family rather than a function per operator, so the emit side is
three `declare`s and the table lives in one place in C. The discriminants are
shared with `ir::DynBinOp` / `DynCmpOp` / `DynUnOp` and must not be reordered.

Four design choices worth recording:

**The tag space is collapsed before switching.** A list's tag is `4 + 8*elem`
and a class's is `13 + 8*id`, so the raw tag space is sparse and unbounded.
`dyn_kind` maps it to a dense ten-value enum first, which is what lets the
dispatch be a jump table.

**The kernel calls the typed helpers rather than reimplementing arithmetic.**
`pyrs_int_add`, `pyrs_str_concat`, `pyrs_list_repeat`, `pyrs_int_cmp`,
`pyrs_int_float_cmp`, `pyrs_list_cmp`, `pyrs_set_diff` and the rest already
exist and are already tested. Reimplementing bigint arithmetic or Unicode-aware
comparison inside the kernel would have been a second implementation of
something the runtime already knows, which is the mistake
`read_write_parity.rs` exists to document.

**Promotion lives in two helpers, not in seven operators.** `dyn_as_int` and
`dyn_as_double` are written once; every operator asks them. `bool` being an int
falls out of `dyn_as_int` rather than being special-cased per operator, so
`True + True == 2` and `True * "ab" == "ab"` need no separate arms.

**Equality shares the table with ordering and differs in one place.** `==` and
`!=` must never raise; `<` and friends must. Both compute an `order` and a
`have` flag, and only the `!have` branch differs — equality returns
False/True, ordering calls `dyn_cmp_error`.

## CPython is the contract, including three error wordings

The results were derived by running CPython over every pair of
{int, float, bool, str, list, tuple, dict, set, None} rather than from memory.
That is also how the messages were captured, and it caught something worth
knowing: what looks like one failure has three different wordings.

```
TypeError: unsupported operand type(s) for +: 'int' and 'str'
TypeError: can only concatenate str (not "int") to str
TypeError: '<' not supported between instances of 'int' and 'str'
```

The second applies when the **left** operand is a sequence, and only to `+`.
Zero division has its own two: `division by zero` for `/`, `//` and `%` alike,
and `zero to a negative power` for `0 ** -1`.

Preserved because they are easy to get wrong: Python's floored division, so
`-7 // 2` is -4 and `-7 % 2` is 1 where C truncates; a negative exponent giving
a float; `1 == 1.0` being true; a repeat count of zero or negative giving an
empty sequence; and an exact int/float comparison, so a bigint against a double
does not lose the answer to rounding.

## The one gap, named rather than mistranslated

`"x=%d" % 5` on a dynamic str needs printf-style formatting, which this kernel
does not implement. CPython **succeeds** here, so falling through to the
operator table would have raised

```
TypeError: unsupported operand type(s) for %: 'str' and 'int'
```

— telling the user their valid program is invalid. It says what is missing
instead:

```
NotImplementedError: printf-style '%' formatting on a dynamic str is not
supported yet; use an f-string, or narrow with isinstance first
```

That is the governing rule from 0.139 applied: when a compiler gap would make
the compiler say something Python would not, the gap gets named and written
down.

## The ABI decision, and why the first one was wrong

The kernel's first version took boxed slots, matching every other `pyrs_any_*`
entry point, which all take one word. That is three GC allocations per
operation -- both operands and the result -- and it measured **2.5x slower than
CPython**:

| 5M-iteration `total = total + i` | time | allocated |
|---|---:|---:|
| `total: int` | 0.0043s | 0 B |
| `total: object`, read only | 0.0043s | 0 B |
| `total: object`, dynamic `+` -- **boxed ABI** | 0.444s | **240 MB** |
| `total: object`, dynamic `+` -- **pair ABI** | **0.024s** | 16 B |
| CPython | 0.178s | — |

That version was correct and passed every test. It also gave back everything
tier 1 had won, on exactly the code the tier exists to serve. The invariant
from the plan -- *the cost of a dynamic feature is paid by the code that uses
it, and by nothing else* -- was satisfied, but the cost was too high to be
worth paying, and a kernel slower than CPython is the one outcome the whole
plan says loses.

The fix is to stop crossing the ABI as a box. Operands arrive as their own
tag and payload; the result payload is the return value and the result tag is
written through an `int *`, which is a stack slot allocated once per function
in the entry block rather than a heap allocation per operation. The 16 bytes
that remain are the single box `print` needs at the end.

The general lesson is that the C ABI's one-word convention was a habit, not a
constraint. Every other `pyrs_any_*` function inherited it from a time when a
dynamic value *was* one word. After tier 1 it no longer is, and the entry
points added since should not pretend otherwise.

## The bug that appeared twice

Both crashes in this work were the same mistake, and the second one is the
lesson.

```c
int both_int = dyn_as_int(lt, lp, &li) && dyn_as_int(rt, rp, &ri);
```

`&&` short-circuits. When the left operand is a `str`, the right conversion
never runs, so `ri` keeps its initial 0 — and **0 is not a tagged small
integer**, because tagging sets the low bit, so small zero is `1`. The repeat
path then read 0 as a heap `PyrsInt*` and dereferenced address 0.

`"ab" * 2` segfaulted. I fixed it in `pyrs_dyn_binop`, and the identical bug
was still sitting in `pyrs_dyn_compare`, where the mixed int/float arms read
whichever side is the integer: `2.5 < 7` crashed while `7 < 2.5` worked,
because the working direction happened to convert the left operand first.

Both conversions now run unconditionally, with the reason in a comment, and the
pattern is grepped for. The general lesson: after fixing a short-circuit bug,
search for the shape rather than the instance.

Neither crash was found by a unit test. Both were found by diffing a sheet of
25 operator results against CPython, and the second only after bisecting a
file whose parts all passed individually.

## Verification

- `make ci` — **1,800 tests**, fmt, clippy `-D warnings`, hygiene, 15 examples
  byte-identical to CPython, compatibility unchanged at 81 native / 28 compat
- `cli/tests/dynamic_operators.rs` — 17 tests. Results are diffed against
  CPython at -O0/-O2/-O3 and again under `PYRS_GC_STRESS=1`, since the kernel
  boxes both operands and its result and so is a real allocation site. Failures
  compare the final stderr line, PyRs printing no traceback. One test pins the
  named gap and asserts it does **not** claim a TypeError CPython would not
  raise.
- The equality test walks all 49 ordered pairs of
  {int, str, float, bool, None, list, tuple} to hold the line that `==` never
  raises where `<` does.

## What is still refused

- **Method calls on a dynamic value** — `a.upper()` is `'Any' has no method
  'upper'`. This needs a name-to-dispatch table, not an operator table, and is
  the larger remaining half of D2.
- **`in` over a dynamic container** — `1 in d` where `d` is dynamic. `in` works
  in the other direction, with a dynamic element in a typed container.
- **`sorted()` of a dynamic value** — needs the iterable protocol on `Any`.
- **`str % args`** — above.
