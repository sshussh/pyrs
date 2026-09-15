# `sorted()` on a dynamic value

**Status: implemented in 0.145.0; rebuilt in 0.152.0.** Item A of
[the remaining dynamism brief](2026-09-10-remaining-dynamism-work.md).

## What shipped first

`sorted(v)` on an `object` was a compile error. 0.145.0 added
`pyrs_dyn_sorted`, which takes the iterable as a tag/payload pair, materialises
a `list[Any]` of boxed elements, and orders it with `pyrs_dyn_compare`. It
covers a dynamic list, tuple, str, set and dict (keys), `reverse=`, and CPython's
`'int' object is not iterable`. `key=` names itself as unimplemented.

## What review found

The ordering was an insertion sort, chosen for stability. It was correct and
quadratic:

| n | PyRs (insertion) | CPython |
|---|---:|---:|
| 1,000 | 0.003s | 0.010s |
| 4,000 | 0.020s | 0.011s |
| 16,000 | **0.273s** | 0.012s |

Stability did not require it — merge sort is stable too.

## The rebuild, and why it is not just a merge sort

CPython's comparison *order* is observable. When two elements cannot be ordered,
the TypeError names their types in the order the comparison met them:
`sorted([1, 'a'])` and `sorted(['a', 1])` raise different messages, and a list
of two homogeneous halves raises only when the halves merge. An arbitrary stable
sort would change which pair is compared first.

So the rebuild follows CPython's timsort structure: `count_run` (the 3.13+
version that reverses equal sub-runs inside a descending run), binary insertion
up to `minrun`, and the powersort merge order. Galloping is omitted; it changes
how many comparisons a merge makes, never its result, and a merge's first
comparison — the later run's head against the earlier run's — is the one
CPython makes first.

| n | PyRs (timsort) | CPython |
|---|---:|---:|
| 16,000 | 0.003s | 0.012s |
| 100,000 | 0.023s | 0.027s |
| 1,000,000 | 0.247s | 0.227s |

## Two memory-safety decisions

- **The merge buffer is a GC-managed list.** An element held only in the copy
  must stay visible to the collector, and a raising comparison must not leak a
  malloc'd buffer.
- **It is deliberately not a scoped GC root.** `pyrs_die` unwinds to an
  enclosing `except` without popping scoped roots, which would leave the root
  chain pointing into the dead frame. The conservative stack and register scan
  already covers the live locals.

## Verification

260 randomised inputs — every length class from 0 to 257, including runs,
reversed runs, equal-heavy lists, 80 cases that raise, and homogeneous halves
that meet only in a merge — are byte-identical with CPython plainly and under
`PYRS_GC_STRESS=1`, each raise caught and followed by collector churn. Tests
in `cli/tests/dynamic_operators.rs` pin the error order, stability through
merges, and a one-second bound on 100,000 elements.

## Limits

`key=` on a dynamic value still names itself as unimplemented. A mixed-type
error that only arises *mid-merge*, after some comparisons in that merge
succeeded, could name its operands in a different order than CPython, because
galloping visits elements in a different order; no builtin type pair produces
that shape outside nested containers.
