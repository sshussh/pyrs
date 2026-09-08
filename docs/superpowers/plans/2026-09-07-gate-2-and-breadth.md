# Closing gate 2, then widening: 0.118 – 0.124

**Status: implemented.** Seven milestones, taken in the order the roadmap's
own policy sets — "correctness rather than new capability sets the near-term
order" — and then breadth.

## What this closed

Release gate 2 is "zero unresolved known silent miscompilations", and the
measured-defect table is its stated outstanding list. It had four open rows.

| Row | Closed by |
|---|---|
| `list(zip(infinite(), [1]))` did not terminate | 0.119, lazy cursor composition |
| `(x for x in range(bound()))` evaluated `bound()` late | 0.120, hoisting the operands |
| `repr(RuntimeError(""))` printed `RuntimeError()` | 0.121, an argument-count field |
| `e.args` is a list, not a tuple | 0.121, **recorded scope decision** |

Plus one row *added* along the way: `range(a(), b())` evaluated `b()` first,
found while writing the cursor and confirmed against CPython before fixing.

**The table now has no open row.** That is not the gate passing — it says
zero *known* miscompilations, and the table is only as good as the probing
behind it.

## The order, and why

**0.118 first, and it is not a feature.** `make compatibility` ran
`--group core`, which excluded all six numpy/pandas cases — the workload
family the product contract names. "native 66 pass / 0 known_gap" meant the
only cases capable of producing a `known_gap` never ran. Every later
milestone's evidence rests on the gate, so the gate went first. Three
documentation claims turned out to be stale rather than aspirational, each
checked against the code rather than assumed.

**0.119 – 0.120: the iteration protocol.** Written up separately in
[2026-09-07-lazy-iteration.md](2026-09-07-lazy-iteration.md). The short
version: half the protocol already existed as `CompIterParts`, composition
needed exactly one new operation (`parts_to_advance`), and `zip` is those
pairs nested inside each other.

**0.121: exception fidelity.** The one place this needed an IR change, and
the reasoning is worth keeping. An argument that was *given* differs from one
that is empty, and the exception object stored only its message — so the
information was lost at the parser, which synthesized an empty string for
`raise E`. `ast::StmtKind::Raise` and `ir::Stmt::Raise` took
`message: Option<Expr>`, and codegen passes a null pointer for the
no-argument form, which the runtime already handled.

Six exception types came with it — `AttributeError`, `NotImplementedError`,
`ImportError`, `ModuleNotFoundError`, `LookupError`, `ArithmeticError` —
chosen because they are what real code *catches on*: `except LookupError`
around an index or key miss and `except ArithmeticError` around a division
are how the idiom is spelled. A test pins the opposite direction too, since a
hierarchy change invites over-catching.

**0.122: `map` and `filter`**, small only because 0.119 built what they ride
on. Two things were not obvious:

- **`filter` cannot be a guard around the loop body.** A cursor advances once
  per iteration, and a skipped element must not be *paired* by an enclosing
  `zip`. So the advance itself loops until it finds a passing element or
  exhausts the input, which keeps "an element appeared" meaning "an element
  the consumer should see".
- **`any`/`all` needed their own route.** They short-circuit, so they cannot
  use the drain-to-a-list path — and stopping early means clearing a flag,
  which an `Indexed` cursor does not have. They normalise through the same
  `parts_to_advance` reconciliation.

**0.123: the diagnostics the docs already promised.** The guide has always
said unsupported features "produce errors that name the feature"; for the
most common ones they did not. `__name__` was the worst: the most common
idiom in Python, reported as though it were a typo.

The half that matters more is that **typos are still typos**. A table that
swallowed real misspellings would be a worse compiler, so five tests assert
the plain message survives — including that a missing-module typo is not
advertised as a missing feature.

**0.124: `__name__`, `sys.exit`, `print(file=…)`.** `__name__` is the one
module attribute with a *compile-time* answer, which is why it fits where
`__file__` and `__package__` do not. `sys.exit` flushes before leaving —
stdout is block-buffered when redirected, and a missing flush loses
everything printed. The print destination is a flag set around one call
rather than a value threaded through every routine, because they all funnel
into one writer and a *capture* must still win, so `str()` is unaffected.

## Decisions recorded rather than deferred

**`e.args` as a tuple** is closed by explicit scope decision under gate 6, not
by a fix. It is a display difference — length and contents match — and
closing it properly needs variable-length tuples, a type-system change out of
proportion to the symptom. Printing a list as though it were a tuple would
put a lie in the type system to fix a print.

**`sys.exit` is not catchable.** CPython raises `SystemExit`; there is no
exception object for it here. Documented rather than approximated.

**The compatibility runner cannot express a failing exit status** — it
classifies a non-zero *oracle* exit as `oracle_error`. Found while adding the
`sys.exit` probe. The probe exits 0 and the status is covered in the Rust
suite; the harness limitation is recorded, not worked around.

## Still open

- **Iterators as values.** `it = zip(a, b)` still materializes. Closing it
  means reifying a cursor into the existing `PyrsGen` frame.
- `reversed`, `iter`, `StopIteration.value`, `yield from` send/throw.
- Dict and set iteration still materialize a key list once; truly lazy
  iteration needs a slot-walking runtime primitive.
- `map` over several iterables, which needs dynamic-arity tuples — the same
  thing `zip(*rows)` waits on.
- The remaining small syntax items: `f"{x=}"`, two-arg `super()`, stacked
  decorators, multiple context managers.

## A separate finding

The README's benchmark table does not reproduce on this machine — `fib(35)`
measured 160 ms against a recorded 25 ms, `nbody` 44 ms against 8 ms, at
best-of-5 with load 2.9 on 16 cores. CPython also differs (685 ms against a
recorded 1163 ms), so different hardware is the likely explanation. **It
predates all of this work**, verified by building the pre-change commit and
timing both binaries on the same machine — within noise. The numbers were not
re-baselined, because one machine is not enough to redo a published claim.
