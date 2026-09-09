# The rest of the module, and the operation it was waiting for

**Status: implemented in 0.140.** `stdlib/json.py` is now the whole module.

## What 0.139 left behind, and why that reason expired

0.139 wrote `loads` in PyRs and kept `dumps` in C. The reason was recorded
carefully, and it was correct at the time:

> It dispatches on the **static** type of its argument, which is what makes
> `dumps([1, 2, 3])` serialise a `list[int]`. A body written here could only
> take `object`, and a concrete `list[int]` boxed into `object` carries a
> different runtime tag (4) than the `list[object]` such a body would read it
> back as (68) — so it would trap on exactly the calls that matter.

The governing rule from that milestone says what to do with a reason like
that: *when a library written in PyRs has to say something Python would not,
that is a compiler gap — unless the reason is a representation the language
genuinely has.* The two encodings are a real representation. But "you cannot
read a container whose element encoding you do not know" is not a consequence
of having two encodings; it is a missing operation. So this was a gap.

## The route that was already tried, and the one taken instead

0.139 tried **narrowing** — peel `object` to `list[Any]` under
`isinstance(v, list)` and let the tag check catch a mismatch — and reverted
it. `isinstance(v, list)` is true for tag 4 and tag 68 alike, and the peel
applies at every *read* of the narrowed name, so this stopped working:

```python
xs: list[int] = [1, 2]
a: object = xs
if isinstance(a, list):
    print(a)          # TypeError: expected list[Any], got incompatible dynamic value
```

The other route needs no peel: read the value **through the tag it already
carries**. `any_elem_tag` recovers a list's element encoding from its
container tag (`4 + 8 * elem`), and `any_from_slot` boxes a raw slot back into
an `Any` using it — or returns the slot untouched when the element encoding is
already `Any`. Nothing is copied, so nothing about aliasing changes.

| Operation | Runtime helper | Accepts |
|---|---|---|
| `len(v)` | `pyrs_any_len` | list, tuple, dict, set, str |
| `v[i]` | `pyrs_any_list_get` | list, tuple |
| `v[k]` | `pyrs_any_dict_get` | dict, `str` key |
| `v[k]` | `pyrs_any_dict_get_any` | dict, dynamic key |
| `v.keys()` | `pyrs_any_dict_keys` | dict with `str` keys |
| `for x in v` | `pyrs_any_iter_get` | list, tuple, dict, str |

`len` is the cheapest of them and reads no tag at all beyond the container
check: every sized object keeps its count in its first `i64`, which is why
`cplen` leads `PyrsStr`.

**Iteration is a distinct node from indexing** (`AnyIterGet`, not `Index`),
because `d[0]` on a dict looks up the key `0` rather than the first entry.
Python's `for` over a dict yields keys; its `[]` looks up. Collapsing them
would have been wrong in a way that only shows on an int-keyed dict.

**A tuple carries a tag per slot.** So a heterogeneous tuple reads back
exactly typed — `(1, "a", True, 2.5)` through an `object` gives an int, a str,
a bool and a float — which a list, holding one tag for the whole container,
could only match by boxing every element.

## What the library gained

`dumps` is now `def dumps(value: object) -> str` over a recursive `_dump`.
Four CPython behaviours the C version could not express now work, all for the
same reason: it dispatched on a static type, and these values have none.

- `dumps(None)`
- a tuple serialises as an array
- a non-`str` key is coerced — `{1: "a"}` → `{"1": "a"}`
- anything that came back from `loads`

Escaping is CPython's `ensure_ascii=True` default, written out in PyRs:
`"` and `\`, short forms for `\b \f \n \r \t`, every other control character
and **all non-ASCII** as `\uXXXX`, with a surrogate pair above the BMP. Floats
go through `str`, which already matches CPython byte for byte — `1e+30`,
`1e-07`, `-0.0` — so the encoder does no float formatting of its own.

### The one message that differs

CPython: `Object of type Point is not JSON serializable`. Here: `Object of
this type is not JSON serializable`. Naming the type needs
`type(x).__name__`, and `type()` is refused on purpose — classes are not
first-class values in a closed-world model, where dispatch is a `switch` on a
type id. The exception type and the behaviour match; only the noun is absent.
This is the second deliberate difference in the module, alongside the lone
surrogate escape.

## Two segfaults in the helpers, found by using them

`pyrs_any_dict_keys` returns a `list[str]`. Handed a dict keyed by `int` it
pushed the raw key slots into that list, and the first `print` read an integer
as a `PyrsStr *`. Reachable from four lines of ordinary source:

```python
d: dict[int, str] = {1: "a"}
v: object = d
print(v.keys())
```

Both `.keys()` and the `str`-keyed subscript now check the dict's key tag and
raise `TypeError`. The fix for the *caller* is `pyrs_any_dict_get_any`:
iterating a dict yields correctly tagged keys, and a dynamic key can index a
dynamic dict, so `dumps` walks an int-keyed dict without `.keys()` ever
appearing. The safe operation and the general operation landed together.

Worth noting how it was found: not by the tests, and not by review, but by
writing a library that uses the operation on a shape the tests did not cover —
the same way 0.139's narrowing regression surfaced. A helper that takes a tag
and returns a pointer has no type system holding it; only a caller does.

## Speed, both numbers

```
20 x 2000-object document, serialised
  vs CPython running the same PyRs source   1.9x faster
  vs CPython's C json module                3.8x slower
```

Almost exactly `loads`'s shape (1.8x / 6.8x), and the profile says why: 30% of
cycles are in `pyrs_gc_collect`. The workload is boxing and allocation, not
arithmetic, so the passes that take `nbody` to 41x do not reach it. Replacing
a C serialiser with PyRs is a real speed loss against the C encoder, bought
for one implementation instead of two, and for the four behaviours above.

**If `json` needs to be faster, the lever is allocation, not the encoder** —
the same conclusion 0.139 reached from the other half of the module. Two
independent measurements now point at the same thing, which makes it the next
performance question worth answering rather than a note.

## How this is checked

- `cli/tests/dynamic_containers.rs` — 8 tests for the operations themselves:
  `len`, indexing, tuples, iteration, dynamic keys, `.keys()` and its refusal,
  a recursive walk over every shape, and the raising cases.
- `cli/tests/json_module.rs` — 13, up from 9. Four new `dumps` tests cover
  every value kind, a concretely-typed container reaching it through `object`,
  the escaping, and tuples and non-`str` keys.

Both run differentially against CPython at -O0/-O2/-O3 and under
`PYRS_GC_STRESS=1`, which is not optional here: every read of a dynamic
element allocates a box, so a mis-rooted one is a use-after-free.

`examples/risksim` serialises a `dict[str, float]` through `json.dumps` and is
byte-checked by `make examples`; it is the regression gate that a concretely
typed container still reaches the encoder.

## A diagnostic that was pointing the wrong way

`isinstance(v, list or tuple)` is **accepted by CPython** and silently tests
only `list`: `or` yields its first truthy operand, and a type object is always
truthy. PyRs rejected it already — correctly, since evaluating it would need
first-class type objects — but with the general message, "not a variable or
expression". That reads as *this language cannot test multiple types*, which
is false; the tuple form `isinstance(v, (list, tuple))` works and matches
CPython.

The message now names the trap and prints the fix with the caller's own type
names in it. `cli/tests/feature_diagnostics.rs` asserts both halves: that the
message names the tuple form, and that the spelling it suggests actually
compiles and prints `True` — a suggestion nothing checks is how a diagnostic
starts lying.

Worth separating the two things. Rejecting the `or` form is a *divergence from
CPython that is deliberate*, because accepting it would mean reproducing a
bug. Pointing at the wrong fix was the defect.

## Noticed while writing it

- `dict` keys may only be `int`, `str` or a tuple of those. So the `bool`,
  `float` and `None` arms of the key coercion are correct but unreachable
  today. They are written out anyway, because they are what CPython does and
  the restriction is not `json`'s.
- Tuples cannot hold `None` (`tuple elements cannot be None`), which limits
  how heterogeneous a tuple can be before it reaches `dumps`.
