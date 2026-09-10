# What is left, and how to build it

**Status: implementation brief.** A handoff spec for the remaining dynamism
work. Tiers 1 and 2 are done — see
[the register-pair representation](2026-09-10-unbox-any.md) (0.142.0) and
[the operator and method kernels](2026-09-10-dynamic-operators.md)
(0.143.0, 0.144.0). The design rationale is in
[the dynamism plan](2026-09-10-dynamism-at-native-speed.md); the evidence
behind the ordering is in
[the type-prediction study](2026-09-10-type-prediction-research.md).

Read section 1 before writing any code. It is the part that is expensive to
rediscover.

---

## 1. House rules for this codebase

### Build and test

```bash
make ci                                    # the gate: fmt, clippy -D warnings,
                                           # tests, hygiene, examples, compatibility
cargo test -p pyrs --test <file>           # one integration test file
cargo test -p semantic                     # one crate's unit tests
```

Note the crate in `cli/` is named **`pyrs`**, not `cli`. The other six match
their directories.

**Three traps that cost real time here:**

1. **`codegen/runtime/runtime.c` is embedded with `include_str!`.** Editing it
   and running `cargo build` does *not* pick up the change — the build script
   does not see it as a dependency. Run `touch codegen/build.rs` first, always.
2. **`pyrs compile` caches by program source, not by compiler version.** After
   any compiler change, use `--no-cache` or you will measure the previous
   binary. A "Finished in 1ms (cached)" line is the tell.
3. **C changes surface at *link* time, not `cargo build` time.** `cargo build`
   succeeding proves nothing about `runtime.c`. Compile an actual `.py` program.

### Representation contract — the important one

A dynamic value (`ir::Ty::Any`) is an LLVM `{ i32 print_tag, i64 payload }`
**in registers**. It is boxed at exactly two places, both one word wide:
a container slot, and older C entry points that take `long long`.

> **Any new runtime entry point must take pairs, not boxes.**

The convention, established in 0.143.0 and 0.144.0: operands arrive as
separate `int tag, long long payload` arguments; a result returns its payload
and writes its tag through an `int *out_tag`. The out-pointer is `%.dyn.tag`,
one `alloca` per function emitted in the entry block
(`codegen/src/emit.rs`, in the function prologue beside the try pool).

This is not a style preference. The first version of the operator kernel took
boxed slots and measured **2.5× slower than CPython** — three GC allocations
per operation. The pair ABI made the same loop 18× faster and allocation-free.
A correct kernel that is slower than CPython fails the entire point of the
project.

`pyrs_dyn_binop`, `pyrs_dyn_compare`, `pyrs_dyn_unary`, `pyrs_dyn_method` and
`pyrs_dyn_contains` are the models. Copy their shape.

### CPython is the oracle, and you derive it rather than recall it

Do not write expected outputs or error messages from memory. Generate them:

```bash
python3 -c "
try: (1).nope()
except Exception as e: print(f'{type(e).__name__}: {e}')"
```

CPython's messages are inconsistent in ways nobody remembers correctly — `+`
alone has two wordings depending on whether the left operand is a sequence,
and arity errors have four different forms. Every message in the existing
kernels was captured this way.

### The error contract has two failure directions

When a dynamic operation cannot proceed, there are two very different cases
and getting them backwards is the worst outcome in this codebase:

| Case | Must produce |
|---|---|
| CPython also rejects this | CPython's exact error, verbatim |
| CPython **accepts** it, we have not implemented it | a message naming the gap |

Reporting a `TypeError` for the second tells a user their valid program is
invalid. That is why `pyrs_dyn_method` carries CPython's full method-name list
per type: it is a list of *what exists*, not of what works.

### Testing convention

Every behavioural change gets a differential test in `cli/tests/`:
run both engines at `-O0`, `-O2`, `-O3` **and** under `PYRS_GC_STRESS=1`, and
compare stdout and exit status. Copy the harness from
`cli/tests/dynamic_operators.rs` (`matches_python`, `fails_like_python`).
Failure tests compare only the last stderr line — PyRs prints no traceback,
which is a recorded divergence.

Use a unique workspace tag per test; two tests sharing one alias the same temp
directory and flake.

### Two bug shapes that have each appeared twice

- **Short-circuit in a conversion pair.** `dyn_as_int(l..) && dyn_as_int(r..)`
  skips the right conversion when the left fails, leaving `0` — which is *not*
  a tagged small integer (tagging sets the low bit), so it gets dereferenced as
  a heap pointer. This segfaulted twice, in two different functions. After
  fixing one, grep for the shape.
- **Statement vs expression lowering.** A method call in statement position
  (`xs.append(1)`) goes through a different path in `semantic/src/lib.rs` from
  one in expression position (`y = xs.count(1)`). Wiring only one leaves the
  other failing. The same is true of augmented assignment.

### Milestone checklist

Version bump across 19 hygiene-checked sites, `CHANGELOG.md` entry,
`docs/ROADMAP.md` flip, a plan doc here, `docs/GUIDE.md` if the user-visible
surface moved, and `make ci` green. Commit split: `ir` → parser → semantic →
codegen/runtime → tests → docs. Commit subjects are lowercase, describe the
*behaviour* not the mechanism, and never mention tooling or authorship.

---

## 2. Item A — `sorted()` on a dynamic value

**Status: implemented in 0.145.0.**

**Size: small.** Currently refused at `semantic/src/lib.rs`, in the `sorted()`
lowering, with a diagnostic that names the real reason.

Sorting needs an ordering for each pair of elements, and `pyrs_dyn_compare`
already provides exactly that. The work:

1. Materialize the dynamic value into a `PyrsList` of boxed elements (the
   iteration path already exists — `pyrs_any_iter_get`).
2. Sort with a comparator that calls `pyrs_dyn_compare(..., PYRS_DYN_LT)`.
3. Return `list[Any]`.

**Acceptance:** `sorted(v)` for a dynamic list of ints, of strs, and of floats
matches CPython; a mixed list raises CPython's `'<' not supported between
instances of 'int' and 'str'`; `sorted()` of a dynamic dict yields its keys;
`reverse=` works. Differential tests as above.

## 3. Item B — `str % args` on a dynamic value

**Status: implemented in 0.146.0.**

**Size: small–medium.** Currently raises `NotImplementedError` naming itself,
from `pyrs_dyn_binop`'s `PYRS_DYN_MOD` path.

printf-style formatting: `%s %d %f %r %x %o %e %g %%`, width, precision, flags
(`-`, `+`, `0`, space), and a tuple right-hand side as well as a scalar.

**Do not invent the semantics.** Enumerate them from CPython first, including
the error cases (`not enough arguments for format string`, `not all arguments
converted during string formatting`, `%d format: a real number is required, not
str`).

**Acceptance:** a table of at least 30 format strings diffed against CPython,
plus every error message above.

## 4. Item C — `pyrs profile`, profile-guided type feedback

**Status: implemented in 0.147.0.**

**Size: medium. Recommended next after A and B, ahead of D3/D4.**

This is the highest-value remaining item and the plan under-ranked it. The
verified measurements: call-graph propagation alone recovers only 12–14% of
parameter types at ~64% accuracy, and 61% of parameters are structural dead
ends. Static inference cannot close the gap on its own, and a profile is
*ground truth* for whatever actually ran.

Design:

- `pyrs profile <script.py>` runs an instrumented build that records, per
  polymorphic site, the concrete type tags observed and a hit count.
- Output is a checked-in text artifact, hashed and pinned — follow Android's
  PGO policy, *"profiles should be collected offline and checked in alongside
  the code to ensure reproducible builds."*
- `pyrs compile --profile <file>` consumes it. A missing or stale profile must
  degrade to today's behaviour, never to a wrong one.
- Record the compiler version in the profile and ignore a mismatched one.

**Acceptance:** a profile round-trips; a stale profile is ignored with a
warning rather than trusted; `--profile` never changes observable behaviour,
only the code emitted. Note that this milestone ships *no* speedup by itself —
it is the input to Item D.

Useful prior art: measured monomorphism on real workloads is 86.6% of argument
sites but only 79.5% weighted by execution, ranging 62–98% by workload; a
4-type cache covers 96.1% of executions. Those numbers argue for recording up
to four types per site, not one.

## 5. Item D — guarded specialization

**Status: implemented in 0.148.0.**

**Size: medium–large. Needs Item C.**

Each polymorphic site becomes:

```
if (tag == expected) { fast, monomorphic, inlined }
else                 { the existing pyrs_dyn_* kernel }
```

The generic kernel already exists and is already correct, which is what makes
this an optimization rather than a feature. There is no deoptimization and no
JIT: the slow path was compiled AOT beside the fast one.

Emit the guarded pair only where a profile or the analysis says a site is
monomorphic; everywhere else, call the kernel directly and keep the code small.

**Acceptance:** a benchmark showing a profiled dynamic loop approaching the
statically typed one; `make ci` green with and without a profile; and — the
real gate — **byte-identical output with and without `--profile`.** A
specialization that changes behaviour is a bug, not an optimization.

## 6. Item E — the inference engine

**Status: implemented in 0.149.0.**

**Size: large.** Three specific, locatable defects:

1. **`try_infer_param_from_body` (`semantic/src/lib.rs`) returns `Option<Ty>`
   and fails on conflict.** For a genuinely polymorphic parameter, conflict is
   the *answer* — the site wants "observed: {int, str}", which is a
   specialization plan. Needs a parallel observed-set path, not a change to the
   existing contract.
2. **No program-level fixpoint.** `analyze_target` is explicit phase sequencing
   (pass 0 to pass 3) with a return-type patch-back loop that only works
   because methods are lowered before free functions. Call-site-driven
   inference needs a worklist over the call graph, iterated to a fixpoint with
   a named cap — `resolve_params_with_body_infer` already caps at 8 rounds and
   is the precedent.
3. **No typeshed lookup.** Third-party and stdlib signatures are ground truth
   available by table lookup rather than inference. Cheaper and more accurate
   than any predictor for the library boundary.

Do **not** build a learned type predictor before this lands. Measured against
today's weak baseline any predictor looks essential; measured after items C and
E, the residue may not justify one. That ordering trap is the single most
important conclusion of the research document.

## 7. Item F — shapes, and types as values

**Status: implemented in 0.150.0.**

**Size: large.** These are the plan's D3 and D4 and add *reach* rather than
speed. Deliberately after C/D/E, which are the performance argument.

- **Shapes** — `getattr`/`setattr`, attributes added at run time,
  `__getattr__`. Pay-per-use is the constraint: a lazily allocated overflow
  dict only on classes whole-program analysis cannot prove closed, so a class
  nobody touches keeps today's layout and today's speed exactly. Whole-program
  IR makes "no `setattr` anywhere targets this class" decidable rather than a
  guess.
- **Types as values** — `type(x)`, `k = C`, factory functions. Largely giving
  `ir::ClassInfo` a runtime representation; it already holds the name, parent,
  fields and method table.

## 8. Out of scope, permanently

`eval` / `exec` of arbitrary strings. There is no interpreter in an AOT binary,
and the existing diagnostic says so correctly. Do not soften it.

## 9. Reviewer's checklist

For whoever reviews this work:

- [ ] Does every new runtime entry point take pairs rather than boxes?
- [ ] `PYRS_GC_STATS=1` on a hot loop — is it allocation-free where it should be?
- [ ] Was every error message derived from CPython rather than written by hand?
- [ ] For each refusal: does CPython also refuse? If not, does the message name
      the gap instead of borrowing a CPython error?
- [ ] Do differential tests run at -O0/-O2/-O3 **and** under `PYRS_GC_STRESS=1`?
- [ ] Any `&&` between two conversion calls that both write out-parameters?
- [ ] Both statement and expression lowering paths, where both exist?
- [ ] `make ci` green, and the measurement that motivated the work re-run?
