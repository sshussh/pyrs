# The inference engine

**Status: implemented in 0.149.0; corrected and extended in 0.152.0.** Item E of
[the remaining dynamism brief](2026-09-10-remaining-dynamism-work.md).

## What shipped first

The brief named three defects. 0.149.0 addressed each:

1. **Inference failed on conflict.** A parallel observed-set path records every
   type a bare parameter's body shows; more than one is `Any` — a specialization
   plan rather than an error.
2. **No program-level fixpoint.** A worklist over the call graph, capped at 8
   rounds, seeds parameters from call-site argument types.
3. **No typeshed lookup.** A table of library parameter types.

## What review found

- **The polymorphic claim held only for type-neutral bodies.** The test used
  `identity(x): return x`. The worklist only visits parameters the body could
  not type at all, so `def show(x): return x + x` — where body inference
  commits to `int` for `+` — still rejected `show("ab")`.
- **"Typeshed" was a 17-entry hand-written table**, and it was wrong where it
  guessed: `abs` seeded as `int` though it takes `SupportsAbs[_T]`, and
  `math.floor` / `math.ceil` as `float` though both have a
  `_SupportsFloor[_T]` overload.
- **"A learned predictor is not justified" was asserted, not measured.**

## Call-site conflicts widen a body-inferred parameter

A second pass runs after the worklist and before return types are inferred. An
unannotated parameter that some call site passes an incompatible type becomes
`Any`, and the return type follows. It propagates, since a dynamic caller passes
dynamic arguments on.

It is narrow in both directions. A call site that `coerce` accepts statically —
a numeric promotion, an empty literal, a subclass into its base — is not a
conflict, so agreeing call sites keep an unboxed signature (checked in the IR:
`define i64 @pyrs_step(i64 %p.x)`). An annotated parameter never widens; an
annotation is a contract.

Methods are covered. The receiver's class has to be known before any expression
is typed, so a flow-insensitive scan finds names bound to `ClassName(...)`,
`self` names the enclosing class, method bodies are visited as callers, and
inherited methods resolve through the class chain.

## Typeshed, for real

`scripts/gen_typeshed_seeds.py` reads the stubs and narrows each annotation to
what PyRs can represent, following the precedent of the Unicode tables:
generated from the oracle, checked in, stamped with its source commit, and
provenance-checked by `make hygiene`.

| Annotation | Treatment |
|---|---|
| protocol, unconstrained TypeVar, generic | **poisons** the parameter: an open set, left unseeded |
| `bytes`, `PathLike`, `complex` | dropped from a union: `str \| bytes` seeds `str` |
| constrained TypeVar | the union of its constraints: `AnyStr` is `str` |
| an overload admitting no PyRs type | abstains rather than disagreeing |

The surface is read from PyRs itself, so the table only names callable
functions. One member is excluded by evidence: `open` admits a file descriptor,
and PyRs refuses `open(3)`. The result is 27 functions from typeshed `76b8c9f8`;
`abs`, `math.floor` and `math.ceil` now seed nothing.

## Annotated field assignments

`self.x: float = x` writes the parameter's type beside it, and body inference
ignored the annotation. It is now evidence.

A constructor's call site (`Circle(2.5)` typing `Circle.__init__`'s `r`) was
tried and **left out**. Class layouts are finalized in pass 0, before call sites
are visited, so a field assigned from a parameter typed that late is missing
from the layout — the actionable "parameter 'r' is missing a type annotation"
became the false "'Circle' object has no attribute 'r'". A test pins the honest
diagnostic.

## The residue, measured

The 23 programs in `examples/` and `compatibility/cases/` that carry annotations
and already match CPython, with every parameter and return annotation stripped:

| Compiler | Compile | Also match CPython |
|---|---:|---:|
| before the inference work (`db9a8a5`) | 7/23 (30%) | — |
| after | **17/23 (74%)** | **17/23** |

No stripped program that compiles produces different output — the inference is
not buying compilation with wrong answers.

The six that remain, classified by what each would need:

| Program | Failing parameter | Evidence that exists |
|---|---|---|
| `vectors.py` | `o` in `__add__` | the operator site `a + b`, and `o.x` naming one class's field |
| `class_constants.py` | `v` in `check` | the method call site, and `v < self.LIMIT` against a typed constant |
| `annotated_attributes.py` | `v` in `add` | `self.xs.append(v)` into a `list[int]` field |
| `generator_expressions.py` | `v` in `trace` | a call site inside a generator expression |
| `typing_annotations.py` | `src` in `evens` | the iterator passed at its call site |
| `cli.py` | `default` | `default=None` with `self.default: object` — a None default inferred as `None` |

`vectors.py` and `class_constants.py` each had an earlier gap closed by the
field-annotation rule; the count did not move because a program compiles
all-or-nothing.

## Is a learned predictor justified?

**No — and the reason is now measured rather than asserted.** Every remaining
failure has its answer written in the source: an operator site, a method call
site, a typed field, a comprehension's element, a constant. None is a case where
the information is absent and would have to be guessed. They need static rules
— most of them the call-site resolution for methods, operators and constructors
that the layout ordering currently blocks — not a model. A predictor would be
competing against ground truth that is already reachable.

That conclusion is scoped to this corpus, which was written for PyRs. Unannotated
third-party code at a library boundary is the case a predictor would earn its
keep, and the measurement that would show it has not been run.

## Next

The largest lever is **finalizing class layouts after parameter refinement**. It
unblocks constructor call sites, and with them most of the residue above.
