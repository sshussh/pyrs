# Getting a value into `Any`, and getting it back out

**Status: implemented in 0.136.** M2 of the library-enablement plan.

## Why this, and why it is small

`Any` already worked: a scalar dynamic box (`{ i32 print_tag, i64 payload }`),
concrete → `Any` boxing, `Any` → concrete with a runtime tag check that traps
on a mismatch. What did not work was the one shape that actually wants it — a
table whose columns hold different types.

The audit's failing case looked like a missing conversion:

```python
class Frame:
    def __init__(self) -> None:
        self.cols: dict[str, list[Any]] = {}
f.cols["name"] = ["a", "b"]
# error: type mismatch in item assignment: expected list[Any], found list[str]
```

It was not. **The identical literal at a *name* target always worked** —
`xs: list[Any] = ["a", "b"]` propagates the expected element type into the
literal and boxes each element at construction. `lower_assign` computed that
hint only for `AssignTarget::Name` and dropped it for an index or attribute
target, so the literal inferred `list[str]` on its own.

That distinction is the whole milestone. Fixing the *hint* is small and
correct. Fixing the *conversion* would have been neither.

## The hint

`probe_target_slot_ty` supplies the declared type of the slot a non-`Name`
target writes into: a list's element type, a dict's value type, a class
field's type. It walks the target's base **without lowering anything** — a
name, and attribute or index chains over one — and declines on anything else.

Declining is cheap and correct: the hint is an optimisation, and lowering the
base a second time to learn its type would duplicate side effects. A base with
a call in it (`get_frame().cols[k] = [...]`) simply does not get one.

Two properties matter:

- **It is exact.** The multi-assign `storage_hint` it sits beside comes from a
  `join_types` pre-pass and can be a loose union, which is why that one only
  ever steers *empty* literals. A declared element or field type is not a
  guess, so this one steers non-empty literals too — which is the case that
  was failing.
- **It reaches arguments as well.** `list.append` and `list.insert` did
  `lower_expr` then `coerce`; they now use `lower_arg_expr`, which already
  steered container literals for user function calls. So
  `rows.append(["a", 1])` into a `list[list[Any]]` boxes at construction.

## What is deliberately still refused

Assigning an already-typed `list[str]` into a `list[Any]` slot.

Nothing in the layout forbids it. Every list slot is 8 bytes for every element
type — `PyrsList` is `{ len, cap, data }` with no per-element tag; the tag is
passed in per operation from the static type — and the collector scans list
contents conservatively.

But the slot *contents* differ. A `list[str]` slot holds a `PyrsStr *`; a
`list[Any]` slot holds a `PyrsUnionBox *` wrapping it. So the conversion is an
O(n) re-box into a fresh list, and **that copy breaks Python aliasing**: after
`f.cols["k"] = xs`, an `xs.append(...)` would no longer be visible through the
frame. The two container escape hatches that already exist in `coerce` — the
empty-literal case and slice assignment — are precisely the two where the
runtime value is already identical and no copy is needed.

`docs/ROADMAP.md` recorded this item without that reason. It now says aliasing
rather than layout, because any future design has to answer aliasing first and
the slot width was never the obstacle.

## The narrowing

`isinstance(x, int)` on an `Any` already compiled to a runtime print-tag check.
It just did not *narrow*: `isinstance_pat_matches(Ty::Any, …)` is false, so the
then-arm got no refinement, `x` stayed `Any`, and every use inside the guard
needed an explicit `y: int = x`.

`isinstance_peel_member` now takes the tested type on the then-arm and keeps
`Any` on the else-arm — ruling out one tag says nothing about the rest — and
`apply_type_refinement` unwraps with `FromAny`.

**One pattern, and no containers.** `isinstance(x, list)` cannot peel to a
concrete `list[T]`: the element type is not recoverable from the tag. A
multi-pattern peel would need a union, whose member *indices* are per-site and
do not exist in the box's global tag space. Both decline and leave `Any` in
place, so the explicit restatement still works and nothing that compiled before
stops compiling.

`FromAny` re-checks the tag, which is redundant under the guard that produced
the refinement. Keeping it is deliberate: it is cheap, and it means a
refinement that is ever wrong traps rather than reinterpreting the payload as
the wrong type — the same reasoning as `nsw` being absent from the inline int
fast paths in 0.126.

This composes with what already exists. A conditional expression narrows an
`Any` (0.134's `IfExp` fix), an `and` chain narrows mid-expression, and a
complementary `else` arm refines fallthrough. `bool` narrows as an `int`,
matching CPython's subtype relation.

## Not in scope

Method calls on a bare `Any` (`'Any' has no method 'upper'`). That needs
runtime dispatch on the tag to a per-type method table, which is against the
closed-world model in a way the narrowing is not — narrowing resolves the type
*statically* at the guard and every call after it is an ordinary static call.

## How this is checked

`cli/tests/any_ergonomics.rs` — 9 differential tests at -O0/-O2/-O3 and under
`PYRS_GC_STRESS=1`, since boxing allocates one `PyrsUnionBox` per element and a
mis-rooted box is a use-after-free rather than a wrong number.

A frame with four columns of different types. The hint at attribute,
local-dict and `append`/`insert` targets, which are the shapes the probe walks.
A concrete `list[int]` slot still rejecting a `str` element, so the exact hint
does not loosen ordinary inference. The re-box staying refused, naming both
list types. Narrowing driving a column reduction, composing with the other
refinement positions, and `bool` as an `int`. And the declined shapes leaving
`Any` alone, so the explicit restatement still compiles.

Coverage went 146 to 148 of 206 probes, with two new probes pinning both halves.

## Next

M3, the callable surface: keyword-only and positional-only parameters, and
module-level functions as values. Both are needed before an API shape can be
committed to, and the second is subsumed by M4's monomorphizer for the
polymorphic case.
