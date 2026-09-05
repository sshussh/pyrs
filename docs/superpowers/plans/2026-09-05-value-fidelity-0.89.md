# Value fidelity: numeric literals and default `!=` dispatch — 0.89.0

## Review and milestone choice

Two rows remained open in the measured-defect table after 0.86-0.88:

```
[1, 2.5, 1]                                CPython [1, 2.5, 1]      PyRs [1.0, 2.5, 1.0]
a != b, a: Base holding Child with __ne__  CPython Child.__ne__ ran  PyRs False
```

Both are silent wrong answers in the declared supported surface, which is
what release gate 2 tracks.

## Milestone contract

- Mixed-numeric list and tuple **literals** keep each element's own type
  as a union, matching CPython's per-element storage. A homogeneous
  literal is unaffected and keeps single-type storage; no benchmark
  regression is acceptable for that path.
- A class that defines `__eq__` but has no `__ne__` anywhere in its
  ancestry gets one synthesized, equivalent to
  `return not self.__eq__(other)`, dispatched through the existing
  virtual method table. This is CPython's own model: the default lives on
  `object`, above every user class.

## `!=` dispatch: why synthesis instead of dynamic slot lookup

`lower_instance_method_call` already dispatches virtually through the
existing vtable. The bug was narrower than "dispatch isn't virtual" -- the
*slot name* `resolve_class_comparison` picked (`__ne__` vs. negated
`__eq__`) was chosen from the *static* type of the comparison operands.
`Base`-typed code calling `a != b` where `a` holds a `Child` defining
`__ne__` therefore always emitted a call to the statically-resolved
slot, never `Child.__ne__`.

The fix does not touch dispatch at all. Instead, every class that would
have used the negated-`__eq__` fallback is given a *real* `__ne__` method
during class collection, so the existing static-slot-selection code always
finds a `__ne__` to call, and that call goes through the same virtual
dispatch every other method call uses. No new runtime mechanism, no
special-casing in `lower_class_compare`.

## The inheritance trap, found by the first version's own test suite

The obvious version -- synthesize `__ne__` wherever a class declares
`__eq__` -- is wrong. Given:

```python
class Base:
    def __ne__(self, other): ...
class Child(Base):
    def __eq__(self, other): ...
```

CPython resolves `Child() != Child()` to the *inherited* `Base.__ne__`.
Synthesizing a `__ne__` directly on `Child` would shadow it. This was
caught by `class_inequality.rs`'s existing
`inherited_ne_is_used_even_when_child_defines_eq` test, which failed
against the first implementation.

The fix visits classes in parent-first order (by inheritance depth, same
approach `finalize_class_layouts` already uses) and tracks which class ids
already provide a `__ne__`, including by inheritance. A class synthesizes
its own `__ne__` only when no ancestor -- including one synthesized
earlier in the same pass -- already supplies one. This requires running
after `resolve_class_bases`, so synthesis moved out of `collect_class_asts`
(which runs before bases exist) into a dedicated pass in `analyze_target`.

## Numeric literals: reusing the existing union machinery

`list[int | float]` already printed and compared correctly before this
change -- the union runtime representation was not the gap. The gap was
that `join_elem_types`, used when *inferring* an unannotated literal's
element type, collapsed `(Int, Float)` to `Float` (and `(Int, Bool)` to
`Int`) instead of building a union. That single function is now the fix:
mixed numeric pairs build a union via the same `flatten_union_members` /
`union_of` helpers used elsewhere, and homogeneous pairs are untouched.

A second, separate call site (`try_type_ast_expr`, used for inferring a
default argument's type from a list literal) delegated to `join_types`,
which is the *scalar-assignment* join rule ("storage type is the join of
all assignments") and correctly collapses numerics there -- collapsing is
right for `x = 1; x = 2.5` but wrong for list elements. That call site now
uses `join_elem_types` instead, so both literal-inference paths agree.

## Scope boundary found while testing

Two tests written against the fix failed for a reason outside its scope:
comparing a `list[int | float]` against an independently-typed
`list[float]`, and nesting `[[1, 2.5], [3, 4]]` where the two inner lists
have genuinely different element types. Both need general
`list[T1] -> list[T2]` re-coercion, which does not exist at all today --
confirmed by `fs: list[float] = xs` from `xs: list[int]` failing on
unmodified `main`. That is a separate, larger, pre-existing gap (recorded
in the roadmap workstreams) and not part of this milestone. The two tests
were narrowed to what the milestone actually claims: nested lists that
join to the *same* element type, and comparisons between two lists that
both independently infer the same union.

## Implementation plan

- [x] `join_elem_types`: mixed `(Int, Float)`, `(Int, Bool)`, `(Float,
      Bool)` build a union instead of promoting; homogeneous pairs
      unchanged.
- [x] `try_type_ast_expr`'s list-literal branch uses `join_elem_types`
      instead of `join_types`, for the same reason.
- [x] `synthesize_default_ne`: builds a `FuncDef` calling
      `self.__eq__(other)` negated, leaked to match how this file already
      handles synthesized type names.
- [x] `synthesize_default_ne_methods`: parent-first traversal tracking
      which class ids already provide (declared or inherited) `__ne__`;
      runs after `resolve_class_bases` in `analyze_target`.
- [x] Updated the one existing unit test that asserted the old collapsing
      behavior; added a companion test pinning homogeneous storage is
      unaffected.
- [x] 10 new differential tests: literal fidelity, indexing/iteration,
      homogeneous-unaffected, equality/membership, nested (same-type)
      lists, direct dispatch, dispatch through an overridden `__eq__`,
      the inheritance-shadowing case, side-effect-once, and three-level
      inheritance.
- [x] Bump the 7 crates, lockfile, README, SPECIFICATIONS and ROADMAP to
      0.89.0; document the `list[T1] -> list[T2]` gap explicitly rather
      than implying it is closed.

## The compatibility harness caught it too

`make ci` failed after the fix, and correctly: the probe manifest recorded
`mixed-list-types-gap` as an expected `mismatch` with the old wrong output
hardcoded (`native_stdout: "[1.0, 2.5, 1.0]\n"`), so the harness reported
`unexpected_pass` -- a real gap closing without the manifest being told.
This is exactly what an `unexpected_pass` classification exists to catch,
and it caught it on the first try. The case is renamed
`mixed-list-types-gap` -> `mixed-numeric-list` (file
`mixed_list_types.py` -> `mixed_numeric_list.py`) and its expectation
flips to `pass`, since a `-gap` name for a closed gap would be its own
kind of stale claim.

Separately, `make hygiene` briefly reported a false version mismatch
because its Makefile target had no dependency on `release`, so it could
check a `pyrs --version` built for the previous milestone. `hygiene` now
depends on `release`, the same way `examples` and `compatibility` already
did.

## Boundaries

No `NotImplemented` fallback for any dunder. No runtime-type slot
selection for other comparison operators -- this milestone fixes the one
measured case (`__eq__`/`__ne__`), not general dynamic dispatch. General
`list[T1] -> list[T2]` re-coercion (including into a union) is explicitly
out of scope and now tracked as its own roadmap item. Mixed non-numeric
list literals still require an explicit union annotation, unchanged.

## Acceptance

`[1, 2.5, 1]` prints and compares identically to CPython; homogeneous
literals are unaffected. `a != b` reaches an inherited or overridden
`__ne__`/`__eq__` through the real vtable in every configuration the new
tests cover, including the inheritance-shadowing case that would break
under the naive synthesis. Both measured-defect rows close.

## Validation (2026-09-05)

Toolchain: Rust 1.96.1, LLVM 22.1.8, CPython 3.14.7, GCC 16.2.1.

| Check | Result |
|-------|--------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test -p pyrs --test class_inequality` | 14 passed (inheritance-shadowing regression caught and fixed here) |
| `cargo test -p pyrs --test value_fidelity` | 10 passed |
| `cargo test --workspace` | 1077 passed, 0 failed, 0 ignored |
| `make examples` | 13/13 byte-exact |
| `make compatibility` | native 15 pass / 3 known_gap (0 unexpected_pass after the manifest update); compat 6 pass |
| `make hygiene` | versions agree at 0.89.0 across 20 sites; 52 links resolve |
| Release `pyrs --version` | `PyRs 0.89.0` |

## Note: an unrelated pre-existing performance observation

While benchmarking this change, `fib(35)` measured roughly 165ms locally
against the README's claimed 25ms. Bisecting confirmed this predates all
work in this and the three prior milestones -- `5b009a5` (the pre-0.86
baseline, `v0.82.0`) measures the same ~160ms on this machine, on a
program with no lists, classes, or unions. This is either a hardware/build
difference from wherever the README's numbers were measured, or a
pre-existing regression from further back; it is not caused by or fixed
by this milestone, and is out of scope for it. Recorded here so it is not
lost, and to explain why no benchmark-regression claim is made for M2
beyond "no additional regression."
