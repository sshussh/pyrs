# Lazy iteration: one cursor protocol, and what it closed

**Status: implemented.** 0.118 (gates), 0.119 (lazy `zip`/`enumerate`, range
operand order), 0.120 (generator-expression creation-time evaluation).

## The two defects

Both sat in the roadmap's measured-defect table, which is release gate 2's
outstanding list. Both were silent wrong answers inside the declared
supported surface.

| Probe | CPython 3.14 | PyRs before |
|---|---|---|
| `list(zip(infinite(), [1]))` | `[(0, 1)]` | **did not terminate** |
| `(x for x in range(bound()))` | `bound()` at creation | `bound()` at first advance |

The roadmap recorded them as sharing one root cause — eager materialization —
and estimated "a milestone of its own rather than a fix." That was right about
the first and wrong about the second.

## What was already there

Reading before writing turned up the thing that decided the whole design:
**half the protocol already existed.**

Comprehensions lowered through `CompIterParts`, a *compile-time* cursor:

```rust
struct CompIterParts {
    cond: ir::Expr,        // loop condition
    element: ir::Expr,     // this iteration's value
    step: Vec<ir::Stmt>,   // runs after the body
    cap: Option<ir::Expr>, // exact length, when knowable
    kind: CompIterKind,    // how exhaustion is discovered
}
```

`kind` is where the iterables differ, and there are exactly three shapes:

| Kind | Discovers exhaustion by | Iterables |
|---|---|---|
| `Indexed` | testing `cond` **before** producing | list, str, tuple, `range`, dict keys, set |
| `ExhaustIf` | fetching, then checking for a sentinel | generators, files |
| `StopTry` | calling `__next__` inside a `try` | user classes with `__iter__` |

There is no runtime iterator object anywhere in this. A cursor is a bundle of
statements and expressions that the *consumer* splices into its own loop, so
it is monomorphised per source expression by construction and costs nothing.

The problem was that this protocol had two rivals. `for` loops had a parallel
family of hand-written `lower_for_indexed` / `_range` / `_generator` / `_file`
/ `_user_iter` functions, and the eager builtins had a third path that drained
everything to a list through `materialize_iterable_arg`. `zip` used the third:
it materialized **every** argument, then computed `min(len(...))` — so it
tried to drain an infinite input before it could ever learn that another input
was shorter.

## The algorithm

Composition needs a shape the three kinds do not share. `Indexed` tests before
producing; the other two produce and then discover. One new function
reconciles them:

```rust
/// (statements that try to produce an element, test for whether one appeared)
fn parts_to_advance(parts: &CompIterParts) -> (Vec<ir::Stmt>, ir::Expr)
```

- `Indexed` → `(nothing, cond)`
- `ExhaustIf` → `(prelude + "if exhausted: more = False", more)`
- `StopTry` → `(the try/except, more)`

With every cursor in that form, **`zip` is those pairs nested inside each
other**:

```text
setup:  <a's setup>; <b's setup>; more = True; done = False

body:   <a's advance>
        if <a produced>:
            e0 = <a's element>; <a's step>
            <b's advance>                 ← only reached if a produced
            if <b produced>:
                e1 = <b's element>; <b's step>
            else: done = True
        else: done = True
        if done: more = False else: <payload sees (e0, e1)>
```

The nesting *is* the semantics. Component *k+1* is only advanced when
component *k* produced, so an exhausted input stops everything after it from
being touched — which is CPython's order, and is observable through side
effects. That is why the tests assert pull **counts**, not just results: an
implementation that drains and then truncates returns the same list while
running the wrong number of iterations.

The composed cursor is itself an `ExhaustIf`, so it composes again:
`enumerate(zip(...))` needs no special case. `enumerate` is the inner cursor
with a counter added to its `step` and a tuple as its `element` — it keeps the
inner kind, so `enumerate` over a list stays `Indexed` and allocation-free.

`zip()` with no arguments is a cursor that never produces, rather than a
special case: `cond` is a flag that the advance immediately clears.

Two flags rather than one (`more` and `done`) because the IR has no boolean
negation, and testing a flag the advance sets is clearer than threading an
inverted condition through the nesting.

### Wiring it up

`comp_kind_body(kind, payload)` — extracted from `wrap_comp_level` — turns a
cursor into loop-body statements. `lower_for_parts` then emits a `for` loop
from a cursor using the existing `push_loop_with_else`, so `break`, `continue`,
`else` and the type-refinement lifecycle stay in one place. Three consumers now
share the protocol:

- `for` loops (`lower_for`)
- comprehensions (`lower_comp_iter`, unchanged)
- the eager builtins (`materialize_iterable_arg`, `list()`)

The last needed one ordering change worth naming: it probed `lower_expr`
first, and a combinator *can* be lowered as a value — into a materialized
list. Probing first would always find the eager path and never reach the lazy
one, so combinators are taken **before** the probe, not after it.

### The generator-expression half

Smaller than the roadmap estimated. The hoist that makes the outermost
iterable eager already existed and already worked; it fell back to leaving the
iterable in the body only when `lower_expr` failed, and after 0.119 exactly one
iterable still took that branch: `range(...)`, a loop form rather than a value.

Making `range` first-class needs a reified iterator object. But a `range` call
has nothing to evaluate *except its operands* — constructing it has no other
side effect — so the hoist gained a second form. When the iterable is not a
value, hoist its **operands** and rebuild the form inside the synthesized body
from parameters:

```text
(x for x in range(lo(), hi()))
    setup:  t0 = lo(); t1 = hi()      # at creation, left to right
    body:   def .genexp(p0, p1): for x in range(p0, p1): yield x
    call:   .genexp(t0, t1)
```

The range stays lazy, so `(x for x in range(1000000000))` still costs nothing.

## What it cost, and what it did not

**Zero lines** in `ir/`, `codegen/src/emit.rs` and `codegen/runtime/runtime.c`.
That was the design constraint: a step needing a new `Ty`, a new `ExprKind` or
a runtime primitive would have left the plan.

Fast paths are unchanged by construction — an `Indexed` cursor emits the same
`While` shape it always did. Verified two ways: the benchmark corpus was
compiled with the before and after binaries and timed on the same machine
(within noise, e.g. `primes` 506 → 502 ms), and the emitted IR for
`for i, x in enumerate(xs)` / `for a, b in zip(xs, ys)` contains no
`pyrs_list_new` beyond the source literals.

Where it changed, it got cheaper: `enumerate` no longer builds a list and *N*
tuples, `zip` no longer drains twice.

## Found while doing it

`range` bound `stop` to a temp **before** `start`, and `lower_expr` leaves
side effects inside the expression, so the assignment order *was* the
evaluation order — `range(a(), b())` called `b()` first. Present in both the
`for` and comprehension paths, invisible for the overwhelmingly common
`range(n)`. Confirmed against CPython before fixing, and recorded as a new
measured-defect row rather than folded in silently.

## Still open

- **Iterators as values.** `it = zip(a, b)` still materializes. Closing it
  means reifying a cursor into the existing `PyrsGen` frame — which already
  has a GC kind, tracing, `send`/`throw`/`close` and an `Iterator[T]`
  annotation — and is what `map`/`filter`/`iter` would ride on.
- **Dict and set iteration** still materialize a key list once. Truly lazy
  iteration needs a slot-walking runtime primitive, which would have broken
  the zero-runtime-lines constraint.
- **`reversed`** is untouched, and `reversed("ab")` still returns a `str`.
- The **tuple per element** for `zip`/`enumerate` remains. Binding an unpack
  target's components directly would remove it.
