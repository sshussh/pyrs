# Changelog

## 0.127.0 — A `try` no longer pins every local in the function to memory

**One `try` anywhere made every local in the function `volatile`**, which
defeats `mem2reg` for the whole body. A numeric kernel with a validity check
around it lost SSA promotion for its accumulators — the check was cold, the
loop was hot, and the loop paid.

C's setjmp rule (7.13.2.1) is narrower than that: it covers automatic objects
**changed between the `setjmp` and the `longjmp`**. One written only before the
`setjmp` keeps its value by the contract of `setjmp` itself, which is what
`returns_twice` makes LLVM honour. So the rule is now per *variable*, and only
a **store** inside a `try` disqualifies one — a read inside a `try` records
nothing, since an alloca with a volatile store is already left in memory and
marking the read too would only block CSE on it.

```python
def sim(n: int) -> float:
    x = 1.0; y = 2.0; z = 3.0
    vx = 0.1; vy = 0.2; vz = 0.3
    i = 0
    while i < n:
        vx = vx + x * 0.001   # ... six live locals, none touched by the try
        i += 1
    try:
        if x != x:
            raise ValueError("nan")
    except ValueError:
        return -1.0
    return x + y + z
```

20M iterations, best-of-5: **114 ms -> 66 ms (1.7x)**. The function body went
from 40 stack-referencing instructions to 11.

The corpus barely moves, and for a reason worth recording: `exceptions` (81 ms
-> 79 ms, within noise) puts its whole loop body *inside* the `try`, so its
locals genuinely are written between the `setjmp` and the `longjmp` and
correctly stay in memory. The gain is for the common real shape — a hot loop
beside error handling — not for code that raises in its inner loop.

### How the set is computed

By emitting the function twice. The first pass runs the **real** traversal with
its output discarded and records every local stored while inside a `try`; the
second emits for real. A parallel analysis over the statement enum would be
faster and would risk overlooking a statement kind, and being wrong here is a
silent miscompilation — a local reading back garbage in a handler, only under
optimization, only after a real raise. Emission is a small fraction of compile
time next to LLVM, so the second pass is not measurable.

`cli/tests/setjmp_locals.rs` is the differential check at -O0/-O2/-O3: a local
written inside the `try` and read in the handler, one written only before it,
nested tries, a handler that writes, a loop counter incremented outside the try
and read inside it, heap values live across a raise (under GC stress), and a
generator, whose frame storage already survives a resume and is unaffected.

## 0.126.0 — Integer arithmetic is inlined; the corpus goes from 3.2x to 9.5x

**Every `int` operation was an out-of-line call into the C runtime.** 0.125
measured the consequence and named the cause: `primes` ran at **0.8x CPython**
while the same trial division in floats ran at 21x. Arbitrary precision needs a
tagged representation with an overflow check, and the runtime is linked as a
separate object with no LTO, so LLVM saw an opaque call it had to assume could
clobber memory and never return.

Each operation now carries an inline fast path on the tagged words, emitted as
an `alwaysinline` helper per module, with the original `pyrs_int_*` call kept
on the cold edge for the bignum case. Covered: `+ - * // %`, all six
comparisons, `& | ^`, unary `-` and `~`, truthiness, and the boxing and
unboxing that sit on every `len()` and every subscript. `**` and the shifts
stay as runtime calls — their fast paths need shift-count range analysis for a
case that is rare in practice.

| benchmark | 0.125 | 0.126 |
|---|---:|---:|
| nbody | 18.8x | **48.2x** |
| mandelbrot | 21.5x | **37.0x** |
| pipeline | 10.4x | 15.0x |
| matmul | 5.4x | 13.1x |
| fib | 4.1x | 12.8x |
| **primes** | **0.8x** | **12.4x** |
| iteration | 7.0x | 9.9x |
| sort | 3.2x | 8.3x |
| listcomp | 1.6x | 6.3x |
| strings | 2.8x | 3.4x |
| exceptions | 0.9x | 1.1x |
| **total** | **3.2x** | **9.5x** |

Every benchmark in the corpus is now faster than CPython.

**The gain is larger than the calls it removed.** An opaque call in a loop also
stops LICM and GVN for everything around it, so the float benchmarks — which
never called the runtime for arithmetic, but did for their loop counters —
roughly doubled too. `lower_for_range` desugars every `for` loop into a `Lt`
plus an `Add`, so before this change every loop iteration of every PyRs program
made two opaque runtime calls.

`primes` fell from 457 ms to 31 ms. `perf` confirms the work is real and shows
where the last of it went: 669M instructions at 5.2 IPC, with LLVM narrowing
the 64-bit `srem` to a 32-bit `idiv` behind a range check once it could finally
see the division. `%` and `//` had no small-int fast path in the runtime at
all — every `n % d` did two `malloc`s, a general bignum divide and two `free`s.

### Why the overflow checks are exact

Tagging is `T(v) = 2v + 1`, so a fast path that computes `2s + 1` in one
operation can use the signed-overflow flag as its range test, and that test is
exact rather than conservative in both directions: `2s+1 >= -2^63` is
`s >= -2^62 - 1/2`, and `s` is an integer. Comparisons need no untagging at all
— `T` is strictly increasing and never wraps, so signed comparison of the raw
tagged words is already the answer. `~` maps the small range onto itself, so it
is a single `sub`. `%` can never leave the small range. `//` can, in exactly
one case: `SMALL_MIN // -1`.

The derivations, and the reason no `nsw`/`nuw` flag appears anywhere in the
tagging arithmetic, are in [codegen/src/intfast.rs](codegen/src/intfast.rs) and
[the plan](docs/superpowers/plans/2026-09-08-inline-int-arithmetic.md).

### How it is checked

`PYRS_INLINE_INT=0` reverts every site to the plain runtime call, so the same
program compiled both ways isolates the new code with no CPython semantics in
the way. `cli/tests/int_inline_parity.rs` requires the two to agree *and* both
to agree with CPython, at -O0/-O2/-O3, over 900 pairs crossing every boundary
of the representation and its immediate neighbours — so small x small,
small x heap, heap x small and heap x heap reach every guard — plus a seeded
sample by bit length. `compatibility/cases/int_boundary.py` runs the same cross
in the compatibility corpus; regenerate it with `scripts/gen_int_boundary.py`.

Division by zero routes to the runtime rather than trapping inline, so its
exception type, message and catchability are unchanged by construction.

## 0.125.0 — An unpack target binds directly; the benchmark table is re-measured

**`for a, b in zip(xs, ys)` allocated a heap tuple per element** and
destructured it on the next line. 0.119 removed the intermediate *list* and
recorded the per-element tuple as a deferred sub-step; benchmarking showed it
was the whole remaining cost. On a 1M-element zip: building the two lists took
19 ms, and the loop that paired them took a further **332 ms** against
CPython's 66 ms.

When the target's arity is known and matches the cursor's element — which is
exactly `zip` and `enumerate` — the components bind straight from the cursor
and no tuple is built. A single-name target still receives the whole tuple, and
a starred target keeps it too, since `*rest` consumes an unknown number of
elements and the arity match does not describe it.

The 1M-element zip loop went **351 ms → 33 ms**, from 1.6× slower than CPython
to 6.5× faster.

### The benchmark table was two months stale

The README recorded 25.4× overall and `fib(35)` at 25 ms. Measured now on a
quiet machine, best-of-5: 3.2× overall and `fib(35)` at 168 ms. The recorded
numbers date from **2026-07-08 (v0.5)**; tagged small-int and bignum
arithmetic landed **2026-07-15 (v0.17)**, a week later. They were measured
against a compiler whose `int` was a machine `i64`.

The table is replaced with what the current compiler does, and the headline
claim with it — "6–170× faster" becomes "float-heavy code runs 5–21× faster;
integer-heavy code is currently slower".

**One cause, measured rather than guessed.** Every `int` operation is an
out-of-line call into the runtime, because arbitrary precision needs a tagged
representation with an overflow check and the runtime is linked as a separate
object the optimizer cannot inline through. The `is_prime` inner loop makes six
such calls per iteration. Rewriting the same trial division in floats takes it
from 0.8× to 21×, which accounts for the entire gap.

Three benchmarks added for what 0.119–0.124 shipped: `iteration` (`zip` /
`enumerate`), `pipeline` (lazy `map`/`filter`) and `exceptions` (raise/catch
through a call boundary). Each is byte-checked against CPython before timing,
like the rest.

## 0.124.0 — `__name__`, `sys.exit`, and `print(file=...)`

```python
import sys

def main() -> None: ...

if __name__ == "__main__":     # the most common idiom in Python
    main()

print("failed", file=sys.stderr)
sys.exit(1)
```

None of that could be written before. `__name__` reported as an undefined
name, and `sys` was `argv` and nothing else — so a program had no way to
report failure on the right stream or with the right status.

**`__name__` has a compile-time answer**, which is why it is the one module
attribute that fits: the entry module is `"__main__"`, an imported one is its
dotted import name. It resolves after locals and globals, so a user binding of
the same name still shadows it — a test pins that.

**`sys.exit(code)` flushes and leaves.** CPython raises `SystemExit`, which a
bare `except` can catch; there is no exception object for it here, so this
exits directly and the guide says so. The flush is not incidental: stdout is
block-buffered when redirected, and exiting without one loses everything
printed. A 200-line probe covers exactly that.

**`print(file=sys.stderr)`** selects the stream. Only the two standard streams
are expressible — there is no file object behind them — so this is a
destination flag rather than a file value, and `file=f` for an `open()`ed file
is refused with a message naming `f.write(...)` instead.

The destination is set for the duration of one call rather than threaded
through every print routine: they all funnel into a single writer, and a
*capture* still wins, so `str()` and f-string interpolation of a value are
unaffected. Two tests pin that — one that the flag does not leak into the next
`print`, one that `str(xs)` is identical either way.

11 differential tests in `cli/tests/module_and_streams.rs`, comparing stdout,
stderr and exit status **separately** — a combined comparison would not test
which stream anything went to. Plus a `module-and-streams` compatibility probe.

One harness limitation found and recorded rather than worked around: the
compatibility runner classifies a non-zero *oracle* exit as `oracle_error`, so
a probe cannot assert a failing exit status. The probe exits 0 and the status
itself is covered in the Rust suite.

## 0.123.0 — Unsupported features are named, not merely refused

The guide has promised since it was written that "unsupported Python features
produce parse/semantic errors that name the feature". For the most common
ones they did not:

| Written | Reported before | Reported now |
|---|---|---|
| `if __name__ == "__main__":` | `name '__name__' is not defined` | names `__name__`, and points at the zero-parameter `main()` convention |
| `eval("1")` | `function 'eval' is not defined` | says PyRs is closed-world and points at `--compat` |
| `bytes([1])` | `function 'bytes' is not defined` | says `str` is UTF-8 text and there is no binary sequence type |
| `type(x)` | `function 'type' is not defined` | says classes are not first-class, use `isinstance` |
| `TypeVar("T")` | `function 'TypeVar' is not defined` | says generics have no meaning in a monomorphic subset |
| `raise X from Y` | `expected end of line after statement, found 'from'` | names exception chaining and why `__cause__` is absent |
| `async def f():` | `expected an expression, found 'async'` | names async/await and says it is not a compiler change alone |
| `import re` | `No module named 're'` | names what is missing and that `--compat` runs it |

`__name__` was the worst of them: the single most common idiom in Python,
reported as though the name were a typo.

Also covered: `exec`, `compile`, `__import__`, `getattr`/`setattr`/`hasattr`/
`delattr`/`vars`/`dir`, `globals`/`locals`, `callable`/`issubclass`,
`bytearray`/`memoryview`, `complex`, `frozenset`, `slice`, `id`/`hash`,
`iter`, `ParamSpec`, `NamedTuple`/`TypedDict`/`dataclass`, `__file__`/
`__package__`/`__doc__`, `__slots__`, `__dict__`, and every standard-library
package PyRs does not ship — `collections`, `itertools`, `functools`,
`datetime`, `random`, `pathlib`, `argparse`, `csv`, `logging`, `subprocess`,
`threading`, `asyncio`, `decimal`, `unittest` and their submodules.

**The half that matters more is that typos are still typos.** A table that
swallowed real misspellings would be a worse compiler, not a better one, so
five tests assert that `lenght(...)`, `xx`, `import nonexistent_thing` and
`x: itn` still get the plain "is not defined" message — and that a
missing-module typo is *not* advertised as a missing feature.

18 tests in `cli/tests/feature_diagnostics.rs`. The README claim 0.118 had to
soften is tightened back, because it is now true.

## 0.122.0 — `map` and `filter`

Neither existed; both are among the most-missed absent builtins. They are
small **now** because 0.119 built the cursor protocol they ride on — and they
are lazy for the same reason:

```python
any(map(f, infinite()))          # short-circuits, as CPython does
list(zip(filter(odd, xs), ys))   # a filtered element is skipped, not paired
```

**`map` is the inner cursor with its element transformed.** The exhaustion
shape, the step and the capacity all pass through, so `map` over a list stays
`Indexed` and allocation-free and `map` over a generator stays lazy. The
callable is resolved by the machinery `sorted(key=…)` already had, which
covers lambdas, nested and free functions, imported functions and the
builtins that take one argument.

**`filter` cannot be a guard around the loop body.** A cursor advances once
per iteration, and a skipped element must not be *paired* by an enclosing
`zip`. So the advance itself loops until it either finds a passing element or
exhausts the input, which keeps "an element appeared" meaning "an element the
consumer should see" and lets a filtered cursor compose like any other.
`filter(None, xs)` keeps the truthy elements.

**`any`/`all` needed their own route.** They short-circuit, so they cannot go
through the drain-to-a-list path the other eager builtins use. They now
normalise the cursor through the same `parts_to_advance` reconciliation `zip`
uses — necessary because stopping early means clearing a flag, and an
`Indexed` cursor has none: its condition is an index test.

Argument order is CPython's. The iterable has to be lowered first to type the
callable, so the *setup* order is what preserves `map(f(), it())` calling
`f()` first.

`map` takes one iterable, not several; that needs dynamic-arity tuples, the
same thing `zip(*rows)` waits on.

Eight differential tests added to `cli/tests/lazy_iteration.rs` (32 total) and
a `map-filter` compatibility probe under GC stress.

## 0.121.0 — Exception fidelity; release gate 2's list is empty

### An argument given is not an argument that is empty

```python
raise RuntimeError        # CPython: RuntimeError()   PyRs: RuntimeError()
raise RuntimeError("")    # CPython: RuntimeError('')  PyRs: RuntimeError()  ← wrong
```

The exception object stored only its message, and an empty message was
indistinguishable from no message. Recovering the difference means recording
it: `PyrsExc` gained an `nargs` field, and — because the information has to
survive from the source — `ast::StmtKind::Raise` and `ir::Stmt::Raise` took
`message: Option<Expr>`. The parser had been synthesizing an empty string for
`raise E` and `raise E()`, which is exactly where the distinction was lost.
Codegen now passes a null argument pointer for the no-argument form, which the
runtime already had to handle.

`repr`, `[e]` and `len(e.args)` all follow the count, so `raise E("")` has one
argument and `raise E` has none — as CPython does. `assert x` and
`assert x, ""` follow the same rule.

### Six exception types real code catches on

`AttributeError`, `NotImplementedError`, `ImportError`, `ModuleNotFoundError`,
`LookupError` and `ArithmeticError`, with their CPython hierarchy: `except
LookupError` catches index and key misses, `except ArithmeticError` catches
division by zero, `except RuntimeError` catches `NotImplementedError`, and
`except ImportError` catches `ModuleNotFoundError`. A test pins the opposite
direction too — `except IndexError` must not start catching `KeyError` just
because both are `LookupError`s.

### `e.args` shape: closed by decision, not by fix

`e.args` is a `list` where CPython has a tuple. Recorded as an explicit scope
decision under release gate 6, with the reasoning in
[docs/ROADMAP.md](docs/ROADMAP.md) so it can be revisited rather than
rediscovered: it is a display difference, the length and contents now match,
and closing it properly needs variable-length tuples — a type-system change
out of all proportion to the symptom. Printing a list as though it were a
tuple would put a lie in the type system to fix a print.

### Gate 2

With those, **the measured-defect table has no open row.** That is not the
same as the gate passing: it says "zero *known* silent miscompilations", and
the table is only as good as the probing behind it. New probes append.

12 differential tests in `cli/tests/exception_fidelity.rs` at `-O0`/`-O2`/`-O3`
and an `exception-fidelity` compatibility probe under GC stress.

## 0.120.0 — A generator expression evaluates its outermost iterable at creation

The second half of the paired defect closes, and release gate 2's list is
down to the two exception-display rows.

```python
g = (x for x in range(bound()))   # CPython calls bound() here
print("created")                  # PyRs called it on the first advance
```

**How it was fixed, and why it was smaller than expected.** The hoist that
makes the outermost iterable eager already existed: it lowers the iterable,
binds it to a temp, and passes it to the synthesized generator function as a
real argument. It fell back to leaving the iterable in the body only when
`lower_expr` failed — and by 0.119 exactly one iterable still took that
branch, `range(...)`, which is a loop form rather than a value.

The obvious fix is to make `range` a first-class value, which needs a reified
iterator object. The cheaper one is to notice that a `range` call has nothing
to evaluate *except its operands*: constructing it has no other side effect.
So the hoist gained a second form — when the iterable is not a value, hoist
its **operands** and rebuild the call inside the body from parameters:

```text
(x for x in range(lo(), hi()))
    setup:  t0 = lo(); t1 = hi()          # at creation, left to right
    body:   def .genexp(p0, p1): for x in range(p0, p1): yield x
    call:   .genexp(t0, t1)
```

The range itself stays lazy, so `(x for x in range(1000000000))` still costs
nothing — a test asserts that by taking three elements from it. Only the
outermost clause is eager; inner clauses are re-evaluated per outer element,
as CPython does.

[docs/GUIDE.md](docs/GUIDE.md) already stated this behaviour, so the
documentation was correct and the implementation was not. No doc change; the
claim is simply true now.

Six more differential tests in `cli/tests/lazy_iteration.rs` (24 total),
including the case nothing consumes — the only way to observe "at creation"
versus "on first advance" directly — and the `lazy-iteration` compatibility
probe covers both halves under GC stress.

## 0.119.0 — Lazy `zip` and `enumerate`, and the range operand order

Two measured defects close. Both were silent wrong answers in the supported
surface, which is release gate 2's outstanding list.

### `zip` no longer drains what it is about to discard

```python
print(list(zip(infinite(), [10])))     # hung; now [(0, 10)], as CPython
```

`zip` materialized **every** argument into a list before pairing them, so it
computed `min(len(...))` only after it had already tried to drain an infinite
input. Any finite generator's side effects also all ran up front, in the
wrong order.

**The algorithm.** Half the protocol already existed: comprehensions lowered
through `CompIterParts`, a compile-time cursor of `{cond, element, step,
kind}`, where `kind` is one of three advance shapes — `Indexed` (test, then
read: lists, strings, tuples, `range`), `ExhaustIf` (fetch, then discover
exhaustion: generators, files) and `StopTry` (call `__next__` inside a `try`:
user iterators). `for` loops had a parallel family of hand-written
`lower_for_*` functions, and the eager builtins had a third path that drained
to a list.

Composition needed one new operation, `parts_to_advance`: normalise any cursor
into *(the statements that try to produce an element, the test for whether one
appeared)*. The three kinds do not share that shape — `Indexed` tests before
producing while the other two produce and then discover — which is exactly why
they could not be composed before.

With that, `zip` is those pairs **nested inside each other**:

```text
<a's advance>
if <a produced>:
    e0 = <a's element>; <a's step>
    <b's advance>
    if <b produced>:
        e1 = <b's element>; <b's step>
    else: done = True
else: done = True
```

Component *k+1* is only advanced when component *k* produced, so an exhausted
input stops the ones after it from being touched at all — CPython's order, and
observable through side effects, so the tests assert pull *counts* rather than
just results. `enumerate` is the same cursor with a counter riding along on
its step.

`for`, comprehensions and the eager consumers (`list`, `sorted`, `sum`,
`min`/`max`, `any`/`all`, `str.join`) now share one protocol, so a composed
`zip` works in all of them. `for i, x in enumerate(xs)` no longer builds a
list or *N* tuples; `for a, b in zip(xs, ys)` no longer drains twice. Used as
a *value* (`it = zip(a, b)`) they still materialize — that needs iterators to
be first-class, which is the next milestone and also what closes the
generator-expression row.

### `range(a(), b())` called `b()` first

Found while writing the above. `stop` was bound to a temp before `start`, and
`lower_expr` leaves side effects inside the expression, so the assignment
order *was* the evaluation order. Present in both the `for` and comprehension
paths, invisible for the overwhelmingly common `range(n)`. Now start, stop,
step — CPython's left-to-right.

18 differential tests in `cli/tests/lazy_iteration.rs` at `-O0`/`-O2`/`-O3`,
each with a timeout, since a hung test process reports nothing. A new
`lazy-iteration` compatibility probe covers it under GC stress.

## 0.118.0 — Make the gates measure what they claim

Housekeeping before the correctness work, because every later milestone's
evidence rests on these.

**`make compatibility` ran `--group core`**, which excluded all six numpy and
pandas cases — the scientific/data workload family the product contract names.
The reported "native 66 pass / 0 known_gap" meant 22 synthetic cases × 3
optimization levels, with the only cases capable of producing a `known_gap`
never run. It now runs every group. A case whose packages are absent reports
as **`skipped` and is counted**, so a machine without numpy still passes the
gate while the gap stays visible:

```console
$ make compatibility
...
skipped          native: numpy-arrays (needs numpy)
{"compat": {"pass": 22, "skipped": 6}, "native": {"pass": 66, "skipped": 6}}
```

`skipped` is not a failure — a contributor without pandas must still be able
to run the gate — and never a pass. `make compatibility-science` runs that
group alone with `--require-all`, which fails rather than skipping. Five tests
in `compatibility/test_runner.py` pin both halves, including that a skip
cannot mask a regression.

**Three documentation claims were stale, not aspirational.** Each was checked
against the code rather than assumed:

- The roadmap said the example-parity gate "still uses command substitution,
  so the comparison is not byte-exact." It has not since 0.86 —
  `scripts/check_examples.py` compares raw stdout, stderr and exit status as
  bytes. Row closed.
- The guide said str methods use ASCII rules for case and whitespace. They
  have been Unicode 16.0.0 since 0.91. What genuinely remains ASCII-only is
  `int(s)` / `float(s)` whitespace stripping — confirmed against CPython,
  which accepts `int("\u00a042")` where PyRs raises.
- The README said every unsupported feature is "rejected at compile time,
  with a diagnostic naming the feature." Many are not: `eval(x)` reports
  `function 'eval' is not defined`, `raise X from Y` reports
  `expected end of line after statement, found 'from'`. The claim is now
  accurate; a later milestone makes the diagnostics match it.

Also fixed: the measured-defect table was split by a paragraph that orphaned
rows 8–23 so they did not render, and that paragraph read "Every row is now
closed" directly above four open rows. The 0.112 entry was listed twice.

## 0.117.0 — The cache ceiling is enforced by growth, not only by the clock

A bug in 0.111, found by looking at the cache after a day of work on this
tooling: **3.6 GiB against a 2 GiB ceiling.**

The opportunistic prune ran at most once per day. That check had found the
cache compliant at 06:02 with about 1 GiB; 46 minutes and ten thousand
entries later it held 3.6 GiB, and the next check was not due for another 23
hours. A time interval alone does not bound a cache — a test suite publishes
thousands of entries in an hour, and the interval was chosen to keep the walk
rare, not to keep the cache small.

Growth is now tracked alongside the clock: once an eighth of the limit has
been added since the last prune, the walk happens regardless of the time,
which bounds the overshoot to roughly 12% by construction rather than by
hoping builds are spread out. The daily check remains for a cache that grows
slowly. `PYRS_CACHE_LIMIT=0` still disables both.

A lost update to the growth counter between concurrent builders delays a
prune and never corrupts one, so it needs no locking.

Two tests: one that publishes eight programs against a small limit inside a
single day — which the previous version fails — and one that confirms
`PYRS_CACHE_LIMIT=0` prunes nothing.

## 0.116.0 — Closing the loop: a scaffolded test, build feedback, extension contracts

**`pyrs init` now scaffolds a test**, so `pyrs init myapp && cd myapp &&
pyrs test` passes without editing anything, the way `cargo new` then
`cargo test` does. A scaffold that cannot be tested was half a scaffold once
`pyrs test` existed.

The starter test deliberately imports nothing. The scaffolded entry calls
`main()` at import time — PyRs has no `__name__` yet, so there is no guard to
put it behind — and a scaffolded test that printed on every run would teach
the wrong shape. `--script` still produces a single file and no `tests/`.

**A build says what it did.** `pyrs build` was silent for 2.5 seconds and
then silent again for 1 millisecond, with nothing to distinguish them:

```console
$ pyrs build
  Finished target/app in 2.65s
$ pyrs build
  Finished target/app in 1ms (cached)
```

On **stderr**, so stdout stays clean for anything reading a build's output —
and because the compatibility harness classifies a build partly by its stdout
being empty. `--quiet` suppresses it. `pyrs run` stays silent: it stands in
for `python3`, and is compared against it byte for byte.

**`build-extension`'s rejection contract is now tested.** The extension
boundary hands compiled code to a CPython process that will call it with
objects PyRs did not create, and every rejection is a promise about what
cannot cross — but the only existing test exercised the *runtime* side, so
all fifteen entry-point rejections were unchecked. `cli/tests/extension_contracts.rs`
covers module names, module-level code, imports, classes, decorators and
defaults, private-only sources, object identity, unsupported calls, dynamic
constructs, output-overwrites-source, and the manifest's defaults and
overrides — with the complements, so a rejection that becomes total is caught
too.

That work turned up one thing worth recording rather than changing: `int`,
`float` and `str` are parsed as **conversions, not calls**, so they never
reach the call allow-list. That is consistent with `chr` being on it — a
kernel may already produce a string — but it is an asymmetry nothing was
checking, and now something is.

## 0.115.0 — `pyrs test`

```console
$ pyrs test
running 3 tests
test test_util::test_add ... ok
test test_util::test_add_negative ... ok
test test_util::test_broken ... FAILED

failures:
    test_util::test_broken
        one plus one is not three

test result: FAILED. 2 passed; 1 failed
```

**This is the one testing job that is unambiguously PyRs's.** pytest under
CPython already tests whether your logic is right, and does it better than
anything PyRs would build. What it cannot do is tell you whether the
*compiled* program agrees with it — which is exactly the failure mode of a
compiler for a Python subset. Running the same assertions through the
compiler closes that gap and nothing else does. The test files stay ordinary
Python, so both engines run them.

- Discovery follows pytest's conventions (`test_*.py`, `*_test.py`), across
  the declared import root and a `tests/` directory, in sorted order.
- A positional argument filters by test *or* module name; `--list` shows what
  would run.
- Tests taking parameters are **skipped, not rejected**. Fixtures are why a
  test takes arguments, PyRs cannot supply them, and failing the whole run
  over a file pytest handles fine would make `pyrs test` unusable beside it.
- Any exception fails its test, not just `AssertionError`, and a failure in
  one test does not stop the others.
- A project with no tests exits 0. Failing there would make the command
  unusable in CI from day one.

**The runner is a generated program, not a runtime feature.** PyRs is
closed-world with no reflection, so there is no way to enumerate test
functions at run time; the driver parses the modules it found, emits a
`__main__` that calls each one inside a `try`, and compiles that like any
other program. The generated source stays inside the documented subset — a
test run exercises the compiler on ordinary code rather than on a private
back door — and a unit test parses it to keep that true.

Results are written to a file rather than stdout. A test's own printing then
stays exactly what the user wrote, with no sentinel to collide with and
nothing to strip back out. Writes flush immediately, so a run that crashes
mid-suite leaves every result up to the crash on disk — and a short results
file is reported as `N not run` with a non-zero exit rather than as a pass.

## 0.114.0 — Machine-readable diagnostics, and the import graph

**`--message-format=json`** on `check`, `build` and `run`. An editor cannot
get a span out of prose, and until now prose was all PyRs produced — so no
editor integration was possible at all, whatever else the compiler got right.

```console
$ pyrs check -i prog.py --message-format json
{"level":"error","phase":"semantic","message":"type mismatch ...","file":"prog.py",
 "line":2,"column":10,"end_line":2,"end_column":13,"byte_start":20,"byte_end":23,
 "rendered":"error[semantic]: ...\n --> prog.py:2:10\n  |\n2 | y: int = 2.5\n  |          ^^^"}
```

One object per line, following `cargo --message-format=json`, carrying both
the machine fields and the `rendered` human text — so a tool can show exactly
what the terminal would have shown without reimplementing the renderer.

Getting there meant diagnostics keeping their structure until the moment they
are printed: `LoadError` was a rendered string, so lex, parse and import
failures had already lost their spans by the time the driver saw them. All
three phases now reach JSON with a real position. A failure with **no**
position — an unreadable file, a link step that failed — is still emitted as
JSON, because a tool's parser must not break on exactly the errors it did not
anticipate.

**`pyrs tree`** shows the import graph the resolver actually resolved:

```console
$ pyrs tree
__main__
├── app
├── app.util
│   ├── app (*)
│   └── app.shared
└── app.shared (*)

4 modules
```

PyRs is closed-world, so this is a *fact* rather than an estimate: it is
exactly the set of modules that will be compiled into the program. A module
reached twice is printed once and marked `(*)`, the way cargo does, since
repeating a shared subtree turns a diamond into an unreadable expansion.
`--paths` shows where each module was resolved from, and `--depth` limits
what is expanded without changing what is counted.

## 0.113.0 — `pyrs init` scaffolds the layout cargo and uv both produce

`pyrs init` wrote a flat `main.py` and one table. `cargo new` and `uv init`
both produce a `src/` layout, a `.gitignore`, a README, a pinned interpreter
and a repository — so a user starting from nothing had to assemble by hand
what every neighbouring tool hands them.

```console
$ pyrs init myapp
initialized project `myapp` at myapp
  myapp/pyproject.toml
  myapp/.python-version
  myapp/README.md
  myapp/.gitignore
  myapp/src/myapp/__init__.py
  myapp/src/myapp/main.py
```

- The **`src/` layout**, with `root = "src"` written alongside it — a src
  layout is only importable with a declared root, so scaffolding one without
  the other would produce a project that does not resolve.
- **`.gitignore`** carrying `/target`, so the first commit cannot contain
  build output. An existing `.gitignore` gets the one line appended and
  nothing else touched, and running `init` twice does not duplicate it.
- **`.python-version`** pinned to the CPython PyRs was built against.
  `requires-python` states the floor, but `.python-version` is what uv
  actually reads when it provisions the environment — writing only the first
  left uv free to pick 3.12 against PyRs's 3.14.
- **`git init`**, unless `--vcs none`, and never nested inside a repository
  that already exists.
- `--name` for a project name that is not the directory's. A package
  directory has to be a Python identifier, so `my-app` produces
  `src/my_app/`, the mapping uv uses.
- `--script` keeps the flat single-file layout for a single-file program.

**There is still no `pyrs new`,** and the split is by what is already there
rather than by which command was typed. A directory that already has a
`pyproject.toml` belongs to a project someone else created — `uv init`, most
likely — and gets exactly one table added, its existing entry point adopted
rather than a second one invented beside it, and nothing else written.

`[tool.pyrs] target` was added in 0.112; `init` now records it in
`.gitignore` as well.

## 0.112.0 — `build`, `clean`, `doctor`, completions

`pyrs compile` was not project-aware where `pyrs run` was, so the command
that produces the artifact ignored the manifest the command that runs it
obeys. It now takes the entry, import root and optimization level from
`[tool.pyrs]`, and `build` is an alias — `pyrs build` in a project should
mean what `cargo build` does.

- **`target/`**, configurable as `[tool.pyrs] target`. With no `-o`, a
  project builds to `target/NAME` instead of `./a.out` in whatever directory
  the command ran from. `NAME` comes from the entry's stem, or its package
  directory when that stem is `main` — so a `src/` layout builds to
  `target/demo`, not `target/main`.
- **`pyrs clean`** removes that directory and nothing else. The shared build
  cache stays `pyrs cache clean`: conflating them would mean clearing one
  project's outputs slowed down every build on the machine.
- **`pyrs doctor`** reports the C compiler, the resolved interpreter and
  whether its version matches the one PyRs targets, the cache and its size,
  and the project it would build. It resolves all of it through the same code
  the build runs, so the report cannot describe a different toolchain than
  the one used, and it exits non-zero when something is actually wrong —
  a report saying "no problems" about a project that cannot build would be
  worse than no report.
- **`pyrs completions bash|zsh|fish|elvish|powershell`.**

Argument errors now name the argument. clap was built without
`error-context`, so a mistyped flag produced `error: unexpected argument
found` with nothing to act on:

```console
$ pyrs check --inpt x.py
error: unexpected argument '--inpt' found

  tip: a similar argument exists: '--input'
```

That covers nested subcommands too, so `pyrs cache prun` suggests `prune`.

## 0.112.0 — `doctor`, `clean`, `build`, completions, and argument errors that name the argument

- **`pyrs doctor`** answers the question a *user* has, where `make doctor`
  answers a contributor's: can this binary compile my program, which
  interpreter will `--compat` use, how big is the cache, and what project am
  I in. It reads the same resolution code the build runs, so the report
  cannot describe a different toolchain than the one used, and it exits
  non-zero when it finds a problem — a report that says "no problems" about a
  project that cannot build would be worse than no report.

- **`pyrs build`** is `compile`, and both are now project-aware the way `run`
  already was. With no `-i` they build the manifest's entry through the
  declared import root; with no `-o` they write **`target/NAME`** instead of
  `./a.out` in whatever directory the command ran from. The directory is
  `[tool.pyrs] target`, defaulting to `target/` — Cargo's name for the same
  thing. Outside a project the old `a.out` default is unchanged.

- **`pyrs clean`** removes that directory, and only that directory. The
  machine-wide build cache stays `pyrs cache clean`: conflating the two would
  mean clearing one project's outputs slowed down every build on the system.

- **`pyrs completions bash|zsh|fish|elvish|powershell`**.

- **Argument errors name the argument.** `pyrs check --inpt x` reported
  `error: unexpected argument found` — no name, no hint. It now reads:

  ```console
  error: unexpected argument '--inpt' found

    tip: a similar argument exists: '--input'
  ```

  This comes from clap's `error-context` and `suggestions` features, which
  also cover nested subcommands, so `pyrs cache prun` suggests `prune`.

Two new dependencies, `clap_complete` and clap's `strsim`, both from the
argument parser already in use.

## 0.111.0 — Cache management, build flags, and a safe subcommand boundary

The build cache had no way to inspect it, no way to clean it, and no bound.
Measured on the development machine before this release: **998 MB across 3736
program entries**, accumulated in about a day of test runs, with "delete the
directory" as the only documented remedy — which also throws away the runtime
objects that make every build on the machine fast, in order to reclaim space
held by programs.

- **`pyrs cache dir | info | clean | prune`**, after `uv cache`. `info`
  reports entries and bytes per layer; `clean` empties them; `prune` takes
  `--older-than 7d` and `--max-size 2GiB`, applied in that order so both
  mean both. `--programs`/`--runtime` narrow any of them, and `--dry-run`
  reports without deleting.
- **Eviction is least-recently-used, not arbitrary.** Entries carry a `used`
  stamp refreshed on reuse (at most hourly, so a hot cache pays no write per
  hit), so the program rebuilt every day is the last one dropped rather than
  whichever the directory listing happened to yield first.
- **An opportunistic ceiling**, default 2 GiB, checked at most once a day and
  disabled with `PYRS_CACHE_LIMIT=0`. A cache that reaches a gigabyte in a
  day is not one a user can be expected to police by hand.
- `toolchain` entries are never pruned: 64 bytes each, and losing one costs
  two subprocesses on the next build.

**`PYRS_CFLAGS` and `PYRS_LDFLAGS` are honored and keyed**, closing the
limitation the previous release documented as accepted. Deliberately not
`CFLAGS`: that is a make convention, is routinely set machine-wide for
unrelated builds, and `cc` does not read it on its own, so adopting it would
change PyRs's output because of a setting aimed at something else.

**A mistyped subcommand is now an error, not a filename.** `pyrs script.py`
works by inserting `run` before any first argument that is not a subcommand,
and that list was a hand-maintained copy — so `pyrs comple -i x.py` reported
`failed to read comple`, a message about a file the user never named, and any
newly added subcommand would have done the same. The list is now derived from
the parser itself, and a near-miss suggests the real name:

```console
$ pyrs comple -i prog.py
error: unrecognized subcommand 'comple'

  tip: a similar subcommand exists: 'compile'
```

A file that exists always wins over a spelling guess, so a script named
`chec` still runs.

## 0.110.0 — Projects: `[tool.pyrs]`, `pyrs init`, and uv

Configuration goes in **`pyproject.toml`** under `[tool.pyrs]`, the table
Python tooling already agrees on. PyRs source is valid Python, so a PyRs
project is a Python project; a second config file would make PyRs a foreign
object in a Python repo.

- `entry`, `root`, `opt-level`, `execution`, `python`, and a
  `[tool.pyrs.extension]` table so `build-extension` stops retyping
  `--module` and `--python` on every call. Unknown keys are rejected, not
  ignored.
- `pyrs init [path]` adds the table to an existing `pyproject.toml`, or
  writes a minimal one. **There is no `pyrs new`** — project creation is
  `uv init`'s job, and PyRs contributes one table to what it produced.
- Discovery walks up to the nearest `pyproject.toml` containing
  `[tool.pyrs]`. One without that table belongs to another project; one that
  does not parse is reported rather than silently skipped. An explicit `-i`,
  `-c` or `-m` bypasses discovery, and flags override the manifest.
- `root` gives the resolver a declared import root, which is what makes a
  `src/` layout work — the layout `uv init` scaffolds.
- `execution = "compat"` is **declared, never inferred.** `--no-compat`
  overrides it, so a project can test whether its program has become natively
  compilable without editing the file.
- uv provides the CPython interpreter when present and is **never required**:
  `pyrs compile -i prog.py` still works with no uv, no virtual environment
  and no manifest. uv is consulted only for a project environment, since
  outside one it answers with a default that is not the project's choice.
- `pyrs check` reports the entry point, import root, execution mode and
  resolved interpreter, and warns when that interpreter's version differs
  from the one PyRs was built against — `uv init` defaults to 3.12 where PyRs
  targets 3.14, which would otherwise surface only when something Unicode- or
  compatibility-shaped disagreed.

Execution mode is not inferred from `dependencies`, deliberately: the
implication fails in both directions, and it would mean `uv add` silently
turning a native binary into an interpreted program. See
[docs/TOOLING.md](docs/TOOLING.md).

This adds the workspace's second external dependency, `toml`. The file is
shared with tools that parse TOML fully, so "almost TOML" would be
user-hostile in a way a private format would not.

## 0.109.0 — Build caching

A one-line program took **2.61 s** to build, of which **2.39 s** was
`cc -O2 -c runtime.c`. That is 92% of the floor, and it was paid again on
every invocation — including every `pyrs run` of a program that had not
changed.

- **Runtime objects** (`runtime.c`, `gc.c`, `unicode_data.c`) are compiled
  once and reused. A new program now builds in **84 ms** instead of 2.6 s.
- **Whole programs** are cached too, keyed on their inputs, so an unchanged
  `pyrs run` skips analysis, code generation and linking entirely: **11 ms**.
- `--no-cache` on `run` and `compile` reuses and publishes nothing.

The cache lives in `$XDG_CACHE_HOME/pyrs` (or `PYRS_CACHE_DIR`), not in the
project: the runtime objects depend only on the compiler and the embedded
sources, so every project on the machine wants the same ones, and an explicit
`pyrs run -i prog.py` with no project still hits.

A stale entry is a wrong answer that looks like a right one, so the keys cover
everything that can change the output bytes — the compiler's own fingerprint
(computed at build time over every workspace source, since hashing the 5 MB
executable per run would cost more than some hits save), the C toolchain's
identity, the optimization level, the target, and the content of every module
in the resolved import graph. Entries are checksum-verified before reuse and
published by atomic rename.

Two findings from testing, both of which would have made the cache quietly
useless or wrong:

- The runtime key is computed over **preprocessed** C, which is what makes a
  changed system header visible. Preprocessed output embeds the source path in
  its line markers, and the sources are written to a fresh temporary directory
  each run — so the key never repeated and the runtime cache never hit. Fixed
  by preprocessing with `-P`.
- Computing a key called `cc --version` and `cc -dumpmachine` on every run,
  including cache hits. The toolchain identity is now recorded under a cheap
  stamp of the compiler binary and recomputed only when that binary changes.


### Also in this release


The shortcomings recorded during the 0.90-0.108 series, addressed:

- **`KeyError` display vs storage.** `str(KeyError("k"))` was unquoted where
  CPython gives `'k'`, and the internal raise sites had the mirror-image bug:
  they stored the *pre-quoted* text, so `e.args[0]` for `d["z"]` came back as
  three characters rather than one — a wrong value, not just wrong text. The
  exception now stores `args[0]` raw plus the tag of that argument, so display
  applies CPython's `repr(args[0])` rule while storage stays the key itself.
  The tag is what the earlier reverted attempt lacked: without it, quoting at
  display could not tell a str key from an int one and regressed
  `s.remove(2)` to `KeyError: '2'`, which is now a test.
- **`ascii()` of a container** now works: the same rendering as `repr`, with
  non-ASCII escaped inside the elements.
- **An empty format spec on a container** (`f"{xs:}"`) was rejected. CPython
  treats `{x:}` as `str(x)` for every type; only a *non-empty* spec reaches
  `list.__format__` and raises, and that stays rejected.
- **Scattered indexing of a non-ASCII string** was O(n) per lookup: 118 ms
  against 2 ms for the same loop over ASCII, and 12 ms for CPython. The hot
  string now gets a lazily built sampled index — the byte offset of every 32nd
  code point — alongside the existing forward memo. The same benchmark is
  3 ms, sequential walks are unchanged, and ASCII still builds nothing.

Set iteration order is documented rather than changed, because it cannot be
matched: CPython's order for str elements varies between runs (randomized
string hashing), so there is no single order to target. `sorted()` is the
answer there, in CPython too.


Review fixes on the 0.90–0.108 series.

- **A missing tuple key crashed instead of raising `KeyError`.** All four miss
  paths (`d[k]`, `del d[k]`, `d.pop(k)`, `set.remove(k)`) formatted any
  non-string key with an integer-only routine, so a tuple key was read as a
  tagged bigint and died with `MemoryError`. They now share one renderer built
  on the 0.108 output sink, which produces CPython's `repr(key)` for every key
  type: a str quoted, an int bare, a tuple parenthesised.
- **f-string replacement fields were unescaped twice.** The lexer decoded the
  whole payload, including field source that the parser then re-lexes, so
  `f"{'\\n'}"` collapsed to a newline instead of the two characters `\` and
  `n`. Escapes are now decoded in the literal chunks only, for both the
  single- and triple-quoted forms.
- **`zip()` with no arguments** returned an error; CPython gives an empty
  iterator, so `list(zip())` is now `[]`.
- **Two stale compatibility expectations.** `pandas-group-join` still named
  the tuple-subscript parser diagnostic that 0.107 retired. Sweeping every
  `unsupported` case against the current compiler found a second one:
  `numpy-linalg` expected a generic parse error where the dedicated
  matrix-multiply diagnostic is now reported. Both now match reality, and the
  sweep is clean.
- **Lowercasing ignored the Final_Sigma rule.** `"ΟΣ".lower()` gave `"οσ"`
  where CPython gives `"ος"`: the tables are generated from single-character
  calls, and this mapping depends on the neighbours. The rule is now applied
  over the string — a sigma preceded by a cased character and not followed by
  one takes the final form — in `lower`, `capitalize`, `title` and
  `swapcase`. Two generated properties back it, `Cased` and `Case_Ignorable`.
  A differential sweep of 279 sigma contexts and 600 random mixed-script
  strings across all six case operations matches CPython; the sweep caught a
  seeding bug in `capitalize` that the reported examples did not.
- **`.format()` re-evaluated its arguments.** The argument expression was
  substituted into every field naming it, so `"{0} {0}".format(side())` ran
  `side()` twice and an argument no field named never ran at all. Arguments
  are now evaluated once each, in call order, into temporaries.
- **An exception inside a container rendered as `str`, not `repr`.**
  `print([e])` gave `[x]` where CPython gives `[ValueError('x')]`.
- **The hygiene gate** claimed to pin the Unicode tables to the interpreter
  that generated them but compared only the UCD version, which two CPython
  releases can share. It now also compares the `PYRS_UNIDATA_CPYTHON` stamp,
  to the minor version — a patch bump does not change casing, and pinning it
  would fail the gate on any other 3.14.x.

## 0.108.0 — `str()` and `repr()` of containers

`print([1, 2])` wrote `[1, 2]`, but `str([1, 2])`, `f"{xs}"` and `"%s" % xs`
were rejected with `str() cannot convert list[int] yet` — so the most ordinary
line in a Python program, `print(f"result: {xs}")`, could not be written, and
neither could a function that *returns* a rendered value.

- `str(x)` and `repr(x)` render `list`, `tuple`, `dict` and `set`, nested
  arbitrarily, with every element type `print` already handled.
- f-strings, `%` formatting and `str.format()` all route through the same
  `str()` lowering, so `f"{xs}"`, `f"{xs!r}"`, `"%s" % xs` and
  `"{}".format(xs)` work.
- `repr()` and `ascii()` are builtins now. They existed only as the f-string
  `!r` / `!a` conversions; the lowering is shared, so this binds a name to it.
- A format *spec* on a container (`f"{xs:>10}"`) stays rejected — CPython
  raises `TypeError: unsupported format string passed to list.__format__`, so
  this is the same rejection at compile time. `ascii()` of a container also
  stays rejected: unlike `repr` it would have to escape non-ASCII *inside* the
  elements, which the shared rendering does not do.

The formatting logic already existed and was already right; it just could not
be reached from anything but `print`, because the print routines wrote straight
to `stdout`. They now write through an output sink, and `str()` captures what
`print` would have emitted — so the two agree by construction rather than by
two implementations kept in step.

Inherited, not introduced: sets iterate in insertion order here and in hash
order in CPython, so `str({3, 1, 2})` differs exactly as `print({3, 1, 2})`
already did.

## 0.107.0 — Tuple dict and set keys

Dict keys and set elements were restricted to `int` and `str`, so the composite
key a transition table, a sparse grid or a two-argument memo wants had to be
flattened into a string by hand.

- `int`, `str`, and tuples whose elements are themselves hashable — nested
  arbitrarily — now work as dict keys and set elements everywhere: literals,
  subscripts, `in`, `get` / `pop` / `setdefault` / `del`, iteration, `dict()`,
  and both dict and set comprehensions.
- `d[i, j]` is `d[(i, j)]`, trailing comma included (`d[3,]` is a 1-tuple).
  This parse-level rejection had been carried since 0.87 and named the key
  restriction as its reason. A tuple subscript of a *list* is now the type
  error CPython also raises, rather than a parse error.
- `bool` keys stay rejected on purpose: CPython's `True == 1` would require a
  bool key to collide with an int one, which this subset does not model.
  Unhashable types are still rejected, now naming what is allowed —
  `int, str, or a tuple of those`.

Only hashing was ever missing. `slot_eq` already compared tuple slots
structurally, so equal tuples already compared equal; there was no way to reach
the right bucket. `hash_key` gained a `TAG_TUPLE` arm that folds the element
hashes and recurses. The hash is internal and never observed, so it needs to
agree with that existing equality and nothing else.

Found while testing: `str()` and f-strings cannot stringify any container
(`str((1, 2))`, `f"{xs}"`) even though `print` formats them. Recorded as a gap
in the roadmap.

## 0.106.0 — Class-body constants

Any assignment in a class body was rejected, so a class could not carry a
constant at all: enum-like values, limits, `PI`. The stated reason was that a
class attribute with a default would leave zeroed instance storage — true of an
instance *field* default, but not of a class constant, which is not an
instance field.

- `class C: LIMIT = 10` declares a constant, read as `C.LIMIT` and `self.LIMIT`
  (and inherited by subclasses, which may override it). Int, float, str, bool
  and negated numbers, with or without an annotation.
- An instance field of the same name shadows the constant, as in CPython.
- The value must be a **literal**: constants are substituted where they are
  read rather than stored, which is exact for something immutable and needs no
  storage or initialisation ordering. A computed value is rejected with that
  reason and a pointer to `__init__`.
- Assigning to a constant is rejected for the same reason — there is nothing to
  assign to. Previously `C.N = 2` reported `name 'C' is not defined`, which
  sent the reader after a missing binding.
- An unknown attribute on a class name now names the class, instead of
  reporting the class itself as undefined.

## 0.105.0 — n-ary `zip` and `enumerate(start)`

`zip` accepted exactly two arguments and `enumerate` only a keyword `start=`,
so `zip(a, b, c)` and `enumerate(xs, 1)` — both ordinary Python — were compile
errors.

- `zip` takes any number of iterables (one or more) and truncates to the
  shortest, producing a tuple of that arity.
- `enumerate` takes `start` positionally as well as by keyword, and rejects
  being given both.
- Both materialize their arguments the way the other eager builtins have since
  0.98, so `zip(range(3), "ab")`, `zip(gen(), xs)` and
  `enumerate(range(3), 10)` work too — previously all three took only lists,
  strs and homogeneous tuples.

Found by running realistic programs against CPython.

## 0.104.0 — `typing` imports and `Iterator[T]` annotations

Two gaps, one blocking the other.

- `from typing import ...` and `import typing` (and `collections.abc`) failed
  to **load**: `No module named 'typing'`. An ordinary typed Python file could
  not be compiled at all, however simple its contents. These are now
  annotation-only imports — they bind nothing at run time and run no module
  body.
- A generator could be created, iterated and passed to a builtin, but not to a
  user function: there was no way to annotate a generator parameter, and a
  `for` loop body says nothing about whether its subject is a list, a str or a
  generator. `Iterator[T]` and `Generator[T, None, None]` now annotate one, in
  parameters and return types, so a generator pipeline (`squares(evens(nums))`)
  compiles — and the same source still runs under CPython.
- A `-> Iterator[T]` return annotation names the generator type directly and is
  no longer wrapped a second time; the older `-> T` spelling still names the
  yield type.
- `Iterable[T]` and `Sequence[T]` are rejected with the reason: they cover a
  list as well as a generator, which are distinct types here, so there is
  nothing to resolve them to. The message names `list[T]` and `Iterator[T]`.
  `Generator[...]` with non-None send or return types, and
  `from typing import *`, are rejected too.

Found by running realistic programs against CPython: a generator-pipeline
script was the only one of five that did not compile.

## 0.103.0 — Annotated attribute assignment

`self.x: T = value` was rejected: the parser allowed an annotation only on a
bare name. That made an attribute whose initial value has no inferable type
unwritable — `self.xs = []` reported `'C' object has no attribute 'xs'` and
`self.d = {}` could not infer a dict type, so an empty list or dict attribute
could not be created at all.

- `self.x: T = value` in `__init__` declares the field's type, for scalars,
  containers, nested containers and unions (`self.opt: int | None = None`).
- The annotation is the field's declared type; an unannotated attribute still
  infers from its value exactly as before.
- An annotation that disagrees with its value is a type error. CPython does
  not check annotations at run time, but a typed compiler does, consistently
  with every other annotation here.
- An annotation on a subscript (`xs[0]: int = 5`) stays rejected: it is legal
  Python but has no effect there.

Found by running small realistic programs against CPython; a state-machine
script kept a `self.log: list[str] = []`. With this, all five programs in that
batch compile and match.

## 0.102.0 — Module-level containers are visible to functions

A module-level scalar could already be read from a function; a list, dict, set
or tuple could not, and reported `name 'X' is not defined`. A lookup table or
config dict at module scope is ordinary Python.

- Global storage types are seeded from container literals as well as scalars,
  including nested ones, so `edges = {"a": ["b"], ...}` is readable from a
  function. An element the seeder cannot type leaves that global unseeded,
  which is the safe direction: the name is simply not in scope, exactly as
  before.
- An empty `[]` nested inside a container now takes the surrounding element
  type instead of being rejected. `[["a"], []]` and `{"a": ["b"], "d": []}`
  are ordinary Python; the empty literal has no element type of its own and is
  provisionally `list[Any]`, and the runtime value — a length-zero list — is
  the same either way. `xs: list[str] = []` and `f([])` already worked, so
  this closes the nested case.
- Global containers stay shared state: mutating one from a function is visible
  outside, as in CPython.

Found by running small realistic programs against CPython. A graph-traversal
script needed the first item, and the second surfaced while fixing it.

## 0.101.0 — Tuple sort keys

`sorted(items, key=lambda p: (-p[1], p[0]))` is *the* way to sort by more than
one criterion in Python, and it was rejected: a `key=` function had to return
a bare scalar.

- `key=` may return a tuple or a list of orderable values, compared
  lexicographically. Works for `sorted`, `list.sort`, `min` and `max`, in both
  their iterable and multi-argument forms, with `reverse=`, and for a named
  function as well as a lambda.
- Tuples were already orderable everywhere else — `(1, 2) < (1, 3)`,
  `sorted(list_of_tuples)`, `min`/`max` of tuples — so the restriction sat
  only on the key path; it now uses the same `is_orderable_ty` rule as the
  rest of the compiler, and the same lexicographic lowering.
- A key type with no ordering at all is still rejected, and the message names
  what is accepted.

Found by running small realistic programs against CPython rather than by
probing constructs: three of four matched, and the fourth was a word-frequency
script that needed exactly this.

## 0.100.0 — `str.format()` and `%` formatting

Neither existed: `.format` was not in the str method table and `%` was
rejected as an operator on str. A large amount of ordinary Python could not be
compiled at all — f-strings covered new code, but rewriting an existing
codebase by hand is not a workaround.

- `"{} {}".format(a, b)`, `"{0} {1}"`, `"{name}"` and mixed positional and
  keyword forms, with the full spec mini-language (`{:.2f}`, `{:>8}`,
  `{:05d}`, fill and alignment) and the `!r` / `!s` / `!a` conversions.
- `"%d-%s" % (a, b)` and the bare-value form `"%s" % x`, with `%d %i %s %r %a
  %f %e %g %x %o %b`, the `-`, `+`, `0` and space flags, width, precision and
  `%%`.
- Both desugar into the `JoinedStr` parts f-strings already produce, so the
  mini-language comes from the code that already implements it and nothing new
  reaches the runtime.
- Argument-count and field-name mistakes are compile errors, where CPython
  raises `IndexError` / `KeyError` at run time: too few arguments, an
  out-of-range index, an unknown keyword, and `%` with too few or too many.
- The format string must be a literal, which is what makes the compile-time
  desugaring possible. A runtime one is rejected with that reason and a
  pointer to f-strings, rather than the previous generic "method not
  supported" / "operator not supported".
- A nested `{}` inside a format spec is rejected in `.format()`: it names an
  argument there and an expression in an f-string, and quietly picking one
  would be wrong.

## 0.99.0 — Bare `raise` (re-raise)

`except E: log(); raise` is the standard way to observe an error without
swallowing it, and it had no workaround: raising a *new* exception loses the
original type and message, which is the entire point of the idiom.

- A bare `raise` inside an `except` handler re-raises what that handler
  caught, preserving type and message — for builtin and user exception
  classes, out of functions and generators, and through `finally`.
- It picks the innermost enclosing handler, and survives other work in the
  handler body, including a nested `try` that raises and handles its own
  exception.
- A bare `raise` with no active handler is a compile error rather than
  CPython's runtime `RuntimeError: No active exception to re-raise` — the
  compiler can see there is nothing to re-raise. This covers a bare `raise` in
  a `try` body or a `finally` as well.

The handler prologue calls `pyrs_exc_clear()` before running its body, so the
pending exception is gone by the time the body executes. The exception object
is now captured immediately before that clear, and a bare `raise` re-raises
it through the existing `pyrs_raise_exc`.

Found and left out of scope: `str(KeyError("k"))` is `k` here and `'k'` in
CPython, whose `KeyError.__str__` is the repr of its argument. That is
unrelated to re-raising and affects `raise KeyError` generally.

## 0.98.0 — The eager builtins accept any iterable

`sorted`, `sum`, `max`, `min`, `set`, `list` and `str.join` took a list (and,
since 0.95, a generator) and rejected everything else — so `sorted(some_set)`,
`sorted(some_dict)`, `sum(range(n))` and `list(range(n))` were all compile
errors, even though `for x in` accepts every one of those.

- Those builtins now accept list, tuple, set, dict (its keys, as in CPython),
  str, range and generator. `key=` and `reverse=` work over all of them.
- `range` is materialized through the same comprehension path
  `[x for x in range(n)]` already used, because it is not a first-class value
  here and so cannot be lowered and then converted. `list(range(n))` and
  `sum(range(n))` build the list, which CPython does not; that is a memory
  cost on a very large range, not a wrong answer.
- `any` and `all` gained dict, and deliberately did *not* gain range: they
  short-circuit, and materializing would answer `all(range(10**9))` by
  building a billion elements where CPython returns False on the first one.
- `tuple()` is unchanged and still needs a fixed-arity tuple — materializing
  would hand it a list, which is exactly what it cannot accept.
- The `range` diagnostic no longer says it only works in a `for` loop, which
  had become false; it now names the places that do accept it.

## 0.97.0 — Lambda parameter inference

A lambda cannot carry annotations — the first `:` starts the body — so
requiring them made lambdas unusable, and `sorted(xs, key=lambda v: -v)`, the
idiom they exist for, was a compile error. Named functions already worked as
`key=`, so the whole gap was parameter typing.

- A `key=` lambda takes its parameter type from the element type of the
  iterable being sorted or scanned. That covers the case body inference cannot
  reach: `lambda s: len(s)` says nothing about `s`, but the consumer knows.
  Works for `sorted`, `list.sort`, `min` and `max`, with `reverse=`, and for
  any sortable return type.
- Other lambdas now get the same body-usage inference nested `def`s already
  had, so `f = lambda a: a + 1` works. Previously `lower_lambda` required an
  annotation up front and never reached that inference.
- Defaults, captures, multiple parameters and returning a lambda from a
  function all work.
- Still rejected: a lambda whose body constrains nothing about its parameter
  and that has no consumer to ask (`f = lambda x: len(x)`). Use a `def`, which
  can be annotated.
- The type hint is scoped to the lambda being lowered. It is keyed by the
  parameter's own name, so without scoping a `key=lambda s: ...` would leave
  `s` typed for any later parameter that happened to share the name.

## 0.96.0 — Generator expressions

`(elem for target in iter if cond)` was a parse error, which made the four
most common consuming idioms unavailable at once: `sum(x for x in xs)`,
`any(... for ...)`, `max(... for ...)` and `",".join(str(x) for x in xs)`.

- Generator expressions work as a parenthesized value and, per CPython, may
  drop their own parentheses when they are a call's sole argument. Multiple
  `for` clauses and multiple `if` filters are supported.
- They are genuinely lazy. The element expression runs on demand, `any` / `all`
  short-circuit through them, and the outermost iterable is evaluated once when
  the generator is created — the tests print from inside the producing code, so
  a comprehension-shaped desugaring would show a different trace even where the
  final answer agreed.
- The loop variable does not leak into the enclosing scope, and enclosing
  locals, parameters and module-level functions are all visible inside.
- Element types other than `int` are inferred, so `list(str(x) for x in xs)`
  and `",".join(s + "!" for s in ss)` work.
- Rejected with a diagnostic rather than a wrong answer: a bare generator
  expression alongside other call arguments (CPython requires parentheses
  there), and capturing a *module-level* variable — the closure cell would
  never be filled because the assignment writes a global. That last case
  previously failed at run time with a NameError, and did so for lambdas too;
  both now fail at compile time with guidance to move the code into a function
  or pass the value in.

## 0.95.0 — Generators as arguments to the eager builtins

A generator function could only be consumed by a `for` loop or a
comprehension. Every eager builtin rejected one, so `list(g())` — probably the
most common thing anyone does with a generator — was a compile error, and the
only way to get the values out was to write the loop by hand.

- `list`, `set`, `sorted`, `sum`, `max`, `min` and `str.join` accept a
  generator. These drain their argument anyway, so the generator is
  materialized first and side effects, order and result are identical to
  consuming it lazily.
- `any` and `all` accept a generator and **short-circuit**, stopping as soon
  as the answer is known. They cannot materialize first: a side-effecting or
  infinite generator would behave differently from CPython. (Over a list they
  still walk the whole sequence, where it is unobservable.)
- `tuple(gen)` is still rejected, because tuples are fixed-arity here; the
  existing diagnostic says so.
- Fixed alongside: an unannotated generator hard-coded its yield type to
  `int`, so `def g(): yield "a"` failed with a type mismatch at the yield and
  a `str`, `float` or `bool` generator could not be written at all without a
  return annotation. The yield type is now taken from the first `yield` of a
  literal or an annotated parameter, searched through `if` / `for` / `while` /
  `try` / `with` bodies, and the three signature-collection sites agree with
  the lowering site — a mismatch there gave a call site a different element
  type than the body produced.

## 0.94.0 — User-defined exception classes

`class E(Exception)` had no spelling: the exception type in `raise` / `except`
was resolved by the *parser* against a hardcoded list of builtins. Unlike most
open gaps there was no workaround — only falling back to a builtin type, which
loses the distinction the program is making.

- `class E(Exception): pass` and chains (`class B(A)`) are supported. A
  subclass is caught by any ancestor and by `except Exception`; a base is not
  caught by its subclass; unrelated user exceptions do not catch each other.
- `raise E`, `raise E()` and `raise E("msg")` all work, for user classes and
  builtins alike. These first two forms were previously rejected outright.
- An uncaught exception with no message prints just the type name, as CPython
  does: `raise ValueError()` reports `ValueError`, not `ValueError: `. The
  bound `e` and its message are empty in that case rather than repeating the
  type name.
- Builtin exceptions are unaffected, including the `OSError` family, and a
  tuple filter may mix user and builtin types.
- Exception-name resolution moved from the parser to the semantic phase, which
  is the only place that knows which classes exist. The parser no longer
  validates exception names against a fixed list.
- Deliberately rejected, each with a diagnostic that says why rather than a
  generic one: methods or fields on an exception class (it carries a tag and a
  name, with no instance layout); using an exception class as a value
  (`x = E("m")`) — the name exists, the use does not; subclassing
  `GeneratorExit`, which is BaseException-only in CPython so `except Exception`
  would miss the subclass; and a base declared after its subclass, which
  CPython rejects with NameError.

## 0.93.0 — Conditional expressions

`a if c else b` was a parse error. Unlike the other open gaps this one is not
an exotic corner: it is one of the most common expressions in Python, and
`x = "big" if n > 3 else "small"` had no spelling at all.

- Conditional expressions are supported everywhere an expression is:
  assignments, returns, call arguments and defaults, subscripts and slices,
  container literals, f-strings, `while` and `assert` conditions, comprehension
  elements, generators and closures.
- Only the selected branch is evaluated, so the guard idioms work:
  `1 // n if n else -1` does not divide by zero, and `xs[0] if xs else "empty"`
  does not raise. The condition is evaluated exactly once.
- Precedence and associativity match Python: `or`/`not` bind tighter
  (`0 or 2 if False else 9` is `9`), chains are right-associative, and the
  condition itself is an `or_test`, so CPython's rejection of
  `1 if 2 if 3 else 4 else 5` is reproduced rather than silently nested.
- A bare conditional is excluded from comprehension iterables and filters, as
  in CPython: the trailing `if` in `[x for x in a if b]` belongs to the
  comprehension. `[x for x in range(3) if 1 if True else 0]` is rejected with a
  diagnostic pointing at the `else`.
- Mixed numeric branches keep each branch's own type, so `1 if c else 2.5` is
  `1`, not `1.0` — the same rule 0.89 established for `[1, 2.5]`. Unrelated
  branch types (`1 if c else "s"`) become a union.
- Arithmetic and comparison on a mixed-numeric union remain unsupported, as
  they already were for list elements. That diagnostic now explains why and
  suggests two fixes that were checked to work — give the parts one type, or
  narrow with `isinstance`. `float(x)` on the union and annotating the target
  do not work and are no longer implied.

## 0.92.0 — String literal escapes and PEP 701 f-strings

Two gaps found while testing the Unicode milestones. Both were silent wrong
answers in the supported surface rather than missing features.

- String literals decode `\xNN`, `\uXXXX`, `\UXXXXXXXX`, one-to-three-digit
  octal (`\101`) and the control escapes `\a \b \f \v`. The lexer previously
  recognised only `\n \t \r \0 \\ \' \"`, so `"\x00"` survived as the four
  characters `\`, `x`, `0`, `0` and `len` reported `4`. Unknown escapes still
  survive verbatim, as in CPython.
- Malformed escapes are rejected with a specific message —
  `truncated \xXX escape`, `truncated \uXXXX escape`,
  `illegal Unicode character U+11FFFF` — instead of being mangled.
- Two forms CPython accepts are rejected deliberately, with a diagnostic
  rather than a wrong answer: a lone surrogate (`"\ud800"`), which has no
  UTF-8 form and so cannot be represented in a PyRs string, and `\N{NAME}`,
  which needs the Unicode name database the compiler does not carry.
  `chr(0xD800)` still produces the bytes at run time.
- f-string replacement fields accept string literals in either quote,
  including the quote delimiting the f-string: `f"{d["k"]}"` (PEP 701). The
  f-string token was a regex that stopped at the first unescaped quote; it is
  now a scanner tracking brace depth and nested literals. The parser's brace
  scan and its `!` / `:` split skip nested literals too, so `f"{'}'}"` and
  `f"{d[':']}"` are correct.
- An unterminated single-quoted f-string now reports
  `unterminated f-string literal` rather than `unexpected character`.

## 0.91.0 — Unicode case transforms and character classes

0.90 made string offsets code points; this makes character *properties*
Unicode too, closing the last rows of the measured-defect table. The tables in
`codegen/runtime/unicode_data.c` are generated by
`scripts/gen_unicode_tables.py`, which asks the installed CPython about every
code point rather than re-parsing the UCD — the same interpreter the
differential tests compare against. `make hygiene` fails if the stamped
Unicode version stops matching the running interpreter.

- `upper`, `lower`, `title`, `capitalize`, `swapcase` and `casefold` follow
  Unicode 16.0.0, including mappings that change length: `"ß".upper()` is now
  `"SS"`, `"ﬁ".upper()` is `"FI"`, and `"naïve café".upper()` is
  `"NAÏVE CAFÉ"`. Titlecase characters are handled, so `"ǅungla".title()`
  matches CPython.
- `isalpha`, `isdigit`, `isdecimal`, `isnumeric`, `isalnum`, `isspace`,
  `isupper`, `islower`, `istitle`, `isprintable` and `isidentifier` use
  Unicode categories and derived properties. `"é".isalpha()` is now `True`,
  `"²".isdigit()` is `True` while `"²".isdecimal()` is `False`, and
  identifiers accept `café` and `π`. `isascii` is now a header comparison.
- Whitespace-driven `strip`, `split` and `rsplit` use the Unicode whitespace
  set and scan by character, so a multi-byte character can no longer be split
  by a byte that happens to match.
- `repr` escapes by Unicode printability rather than byte range, both for
  `!r` and for strings printed inside a container: `repr("café")` is
  `'café'`, and a zero-width space is escaped.
- Table cost: 303 distinct records cover all 1.1M code points; 155 KiB of C
  that compiles in 40 ms, against the C runtime's existing 2.2 s.
- Still open, and stated rather than implied: no normalization or
  grapheme-cluster segmentation (`len` counts code points, so `"e" + U+0301`
  is 2); no locale-sensitive casing beyond CPython's default; lone-surrogate
  and encoding-error behavior is unspecified; indexing a non-ASCII string is
  O(n), not CPython's O(1).
- One suite change: the `while_local_optional_reassign_none_terminates`
  timeout went from 5s to 30s. Its budget has to cover a full `pyrs compile`
  (~2.4 s serially, dominated by building the C runtime) under a saturated
  parallel test run; it exists to catch an infinite loop, and the file's other
  timeout tests already used 15s.

## 0.90.0 — Unicode code point offsets

String offsets are Unicode code points, matching CPython. `PyrsStr` stays a
UTF-8 buffer and gains a cached code point count as its first header word, so
`len(s)` is O(1), codegen's `emit_len` is unchanged, and `print`, file I/O,
hashing, comparison and the CPython bridge keep operating on bytes. ASCII
strings are recognised as `cplen == len` and keep the existing paths.

- `len`, indexing, slicing (including a step), iteration, `list(str)` and
  `set(str)` count and select whole characters across 1-, 2-, 3- and 4-byte
  code points, combining sequences and an embedded NUL. `len("héllo")` is now
  `5`, `"héllo"[1]` is `é`, and `len("🐍")` is `1`.
- `find`/`rfind`/`index`/`rindex`/`count` return character offsets and accept
  character `start`/`end` bounds, as do `startswith`/`endswith`.
  `"héllo".find("l")` is now `2`.
- `split`/`rsplit`/`partition`/`rpartition`/`splitlines` and the `strip`
  family never split a character; `strip(chars)` compares whole code points
  instead of bytes, so it can no longer leave an invalid sequence behind.
- `center`/`ljust`/`rjust`/`zfill`/`expandtabs` and f-string format widths and
  precision measure characters, and a multi-byte fill character is written
  whole. `translate`/`maketrans` key on code point ordinals. `ord` is O(1).
- Fixed alongside: four string allocation sites hand-rolled the old header
  layout (exception messages, object reprs, generator `throw`), one of which
  corrupted the heap once the header changed.
- Indexing a non-ASCII string is O(n), not CPython's O(1). A one-entry
  sequential-access memo, invalidated on every collection, keeps `for c in s`
  and index loops linear. On this host a string-saturated ASCII workload costs
  5.3%; code that touches no strings is unaffected.
- Case transforms (`upper`, `lower`, `title`, …) and the `is*` predicates keep
  their documented ASCII-only behavior. `"ß".upper()` is still `ß` and
  `"é".isalpha()` is still `False`; the generated Unicode 16.0.0 tables are the
  next milestone. Offsets and properties are now separate, so no operation
  counts bytes while another counts characters.
- Not addressed, and unrelated to offsets: the lexer accepts no `\xNN` or
  `\uXXXX` escapes, and an f-string replacement field cannot contain nested
  quotes.

## 0.89.0 — Value fidelity: numeric literals and default `!=`

- Mixed-numeric list and tuple literals keep each element's own type instead of
  promoting to one: `[1, 2.5, 1]` prints `[1, 2.5, 1]`, not `[1.0, 2.5, 1.0]`.
  `join_elem_types` builds a union for mixed `(int, float)`, `(int, bool)` and
  `(float, bool)` pairs; homogeneous literals are unaffected and keep
  single-type storage. The default-argument inference path was joining with the
  scalar-assignment rule and now agrees.
- A class defining `__eq__` with no `__ne__` anywhere in its ancestry gets one
  synthesized, calling `self.__eq__` negated through the normal vtable. `a != b`
  where `a` is `Base`-typed but holds a `Child` defining `__ne__` now reaches
  `Child.__ne__`, keeping both its result and its side effects. Synthesis walks
  classes parent-first so an ancestor's explicit `__ne__` is never shadowed.
- Converting an already-typed `list[int]` to `list[float]` or to a union by
  assignment remains unsupported; only a literal's own elements are joined.
  Recorded as its own roadmap item rather than implied closed.

## 0.88.0 — Trustworthy validation gates

- The example parity gate fails when either process fails, instead of comparing
  only captured output.
- Integration tests build under `CARGO_TARGET_TMPDIR` (`target/tmp`, the path CI
  uploads) and retain their inputs when the thread is panicking, so a failing
  test leaves usable artifacts.
- `make hygiene` checks version agreement across 20 sites and resolves every
  relative documentation link, and its own failure paths are tested.
- `make asan` / `make ubsan` build the adapter, runtime and collector
  instrumented and run the extension boundary suite. The LLVM-generated kernel
  object is not instrumented; this is adapter and runtime coverage.

## 0.87.0 — String annotations and `__future__` imports

- Annotations written as string literals are accepted and resolved, including
  nested generics, unions and `Optional`, forward references to the enclosing
  class, and local variable annotations.
- `from __future__ import annotations` is accepted as a no-op, as are the other
  mandatory-in-Python-3 future features.
- Multidimensional slice syntax (`a[i, j]`) and matrix multiplication (`@`) are
  rejected with specific diagnostics naming what is missing, rather than a
  generic parse error.

## 0.86.0 — CPython interoperability and stranded correctness fixes

Native compilation is the default; the target workload family is scientific/data
Python, including NumPy and pandas through optional CPython compatibility
execution. Reaching 0.86.0 does not establish 1.0 readiness; see the
[roadmap](docs/ROADMAP.md) for the remaining gates.

- Borrowed list headers from the CPython bridge carry a non-owned capacity
  marker, so every runtime growth site raises `BufferError` instead of calling
  the libc allocator on exporter- or `PyMem`-owned memory. Previously a resize
  would free a foreign pointer, corrupt the heap and invalidate a buffer lease
  that still had to be released. Read-only enforcement no longer rests solely on
  the extension frontend's allowlist.

- Python-style script, `-c` and stdin invocation, with script arguments and
  native `sys.argv[0]` preserved on Unix. Existing compiler subcommands remain.
- Explicit `--compat` executes a whole script, command or `-m` package using
  CPython. Select an environment with `--python` or `PYRS_PYTHON`. This mode uses
  installed Python packages and preserves their semantics; it does not compile
  them or accelerate them. It is never an automatic retry of native execution.
- `pyrs check -i script.py` checks the native frontend/import graph without
  linking or executing code. Optimization levels outside 0–3 are rejected.
- Private, atomically created temporary build directories with automatic cleanup
  on normal completion and ordinary compiler errors.
- Exact mixed integer/float comparisons, including bigints, fractional values,
  NaN and infinities, and numeric equality through boxed list values.
- Correct bigint-to-float ties-to-even rounding and overflow, and float-to-int
  conversion at the positive small-integer boundary (`2**62`).
- Binding checks for conditionally assigned locals, including zero/False/None
  values and generator suspension. Locals modified inside `try` survive exception
  handling at optimized compilation levels. Module globals, deletion and static
  use-before-assignment diagnostics remain separate work.
- Preserve side effects and unbound reads in None identity comparisons and
  `print(sep=.../end=...)` expressions with a None result.
- Versioned compatibility probes and reports that distinguish native gaps from
  compatibility passes and compare streams, exit status and generated files.
- Experimental `build-extension` target: compile numerical function modules into
  CPython extensions on Linux. Native library analysis/emission is separate from
  the Python adapter and does not require or automatically call `main()`.
- Extension functions can return strings: native UTF-8 is copied into a Python
  string while the result remains rooted. Invalid UTF-8 raises UnicodeDecodeError
  with input buffers released normally. String arguments remain unsupported.
- Exact scalar boundary guards, arbitrary-size integer conversion, keyword
  binding, native exception translation and read-only 1D float64 buffer borrowing.
  Python lists of floats use temporary copies; NumPy/pandas buffers can pass
  without copying. Buffer leases are released on ordinary success and failure.
- Native extension boundary tests at O0/O2/O3 under GC stress, including lifetime,
  thread/reentrancy, symbol isolation, and scientific package checks. The
  `examples/interop` demo verifies results and measures full call overhead.
- The bridge remains an explicit numerical API; automatic mixed execution,
  general objects/arrays and a stable standalone library ABI remain planned
  work. See the [interoperability contract](docs/INTEROPERABILITY.md) for
  current restrictions.
- Known gaps recorded rather than claimed fixed: string length/indexing still
  operate on UTF-8 bytes, and mixed numeric list literals still promote ints to
  floats. Both are reported as `known_gap` by the compatibility probes.
- CI wiring for the compatibility probes is deferred: the required
  `scientific-compatibility` job would have made every run depend on installing
  NumPy/pandas from PyPI. `make ci` runs the probes locally in the meantime.

## Feature history before 0.86

PyRs had no changelog until 0.86. These records lived in the README's
language section, which had accreted one paragraph per milestone until
it was 77% of the file. They are kept here so the README can say what
PyRs *does* rather than when each piece arrived; the current behaviour
of every feature below is documented in [the guide](docs/GUIDE.md).

- **Also (v0.23+):** `@staticmethod` / `@classmethod` / read-only `@property`, bound methods as values, `__iter__`/`__next__` for-loops, `__len__`/`__bool__`, class `with` context managers, single free-function decorators, match class patterns.
- **v0.25 protocol completion:** `__exit__` suppress (truthy return swallows the body exception); exception path passes `__exit__(None, exc, None)` (type and traceback remain `None` — no exception type objects / traceback objects yet); builtin `next(it)` / `next(it, default)` for user iterators and generators; user-class `__contains__` for `in` / `not in`.
- **v0.26:** `sorted(xs, key=f)`, `min(xs, key=f)`, `max(xs, key=f)` with a monomorphic `key=` callable (`T →` sortable `int|float|bool|str`); desugared in semantic (no C comparator).
- **v0.27:** `list.sort(key=f)` in-place with the same monomorphic `key=` surface (shared desugar with `sorted`; `reverse=` still residual).
- **v0.28:** `sorted(..., reverse=bool)` and `list.sort(reverse=bool)` (stable reverse-sort-reverse; works with `key=`; `reverse=` must be `bool`).
- **v0.29:** multi-arg `min(a, b, c, …)` / `max(…)` (numeric fold with `bool`→`int`→`float` unify) and multi-arg with monomorphic `key=` (`min(a, b[, c…], key=f)`); positionals must share one type when `key=` is used.
- **v0.30:** `min`/`max` iterable `default=` (`min(xs, default=d)` / `min(xs, key=f, default=d)`); empty → default (result type is `join(elem, default)`); multi-arg form rejects `default=` like CPython.
- **v0.31:** bare builtins as monomorphic `key=` — `len`, `abs`, and casts `int`/`float`/`bool`/`str` on `sorted` / `list.sort` / `min` / `max` (IR ops, not first-class values); other builtins still need a wrapper.
- **v0.32:** lexicographic `min`/`max` for homogeneous `str` (multi-arg and `list[str]` without `key=`); numeric multi-arg/list path unchanged.
- **v0.33:** `sorted` / `list.sort` `reverse=` uses CPython truthiness (`reverse=1` / runtime int, not only `bool`); const-folds 0/1/True/False.
- **v0.34:** lexicographic tuple ordering (`<`/`<=`/`>`/`>=`), multi-arg and list `min`/`max` over orderable tuples, and `sorted`/`list.sort` for `list[tuple[…]]` (elements: int|float|bool|str or nested orderable tuples).
- **v0.35:** lexicographic list ordering (`[1,2] < [1,3]`), multi-arg and list `min`/`max` over orderable lists, and `sorted`/`list.sort` for nested `list[list[…]]` of orderable elements.
- **v0.36:** bare `key=len` on class instances that define `__len__` → `int` (`sorted` / `list.sort` / `min` / `max`).
- **v0.37:** `in` / `not in` for nested lists (`[1, 2] in [[1, 2], [3]]`), using the same recursive equality as `==` / `list.index` / `list.remove`.
- **v0.38:** `list.count(x)` with the same recursive equality (including nested lists).
- **v0.39:** `list.reverse()` in-place (statement only; `reversed(xs)` / `xs[::-1]` still allocate a copy).
- **v0.40:** `sum(xs, start)` / `sum(xs, start=s)` — numeric start (default 0 / 0.0); result type is `elem ⊔ start`.
- **v0.41:** `del xs[i]` for lists (negative indices; OOB → same `IndexError` as list assignment).
- **v0.42:** class `==` / `!=` — `__eq__` when defined (virtual), else pointer identity.
- **v0.43:** list slice assignment `xs[lo:hi:step] = ys` and `del xs[lo:hi]` (same-elem list RHS; extended slices require matching length).
- **v0.44:** set `==` / `!=`, subset operators `<` / `<=` / `>` / `>=`, and `issubset` / `issuperset` / `isdisjoint`.
- **v0.45:** `round(x)` / `round(x, ndigits)` — ties to even; one-arg yields `int`; two-arg keeps `int` or `float`.
- **v0.46:** `ord(s)` / `chr(n)` — Unicode code point of a one-character string, and the inverse (`chr` accepts `0 ..= 0x10FFFF`; `bool` → `int`). String `len`/index agree with them since v0.90.
- **v0.47:** integer literals `0x` / `0b` / `0o` (any case, PEP 515 underscores) convert to the same `int` as decimal; invalid prefixes are compile errors.
- **v0.48:** `print(..., sep=..., end=...)` — `sep`/`end` are `str` or `None` (`None` restores the defaults `" "` / `"\\n"`).
- **v0.49:** `hex(n)` / `bin(n)` / `oct(n)` — lowercase `0x` / `0b` / `0o` strings (`hex(-10)` is `'-0xa'`); `bool` → `int` like `chr`.
- **v0.50:** `dict.setdefault(k[, default])` — insert on miss and return the stored value; bare form requires a value type that includes `None`.
- **v0.51:** `divmod(a, b)` — `(a // b, a % b)` for int/bool/float (operands evaluated once; mixed numeric promotes like `//`).
- **v0.52:** `print(..., flush=...)` — CPython truthiness (`True`/`1` flush stdout after writing; `False`/`0`/`None` no-op); `file=` still residual.
- **v0.53:** `dict.popitem()` — LIFO last-inserted `(k, v)` pair; empty dict raises `KeyError: 'popitem(): dictionary is empty'`.
- **v0.54:** `str.removeprefix` / `str.removesuffix` — drop an exact prefix or suffix when present (empty affix is a no-op).
- **v0.55:** `str.partition` / `str.rpartition` — first/last split into `(head, sep, tail)`; empty separator is `ValueError`.
- **v0.56:** `pow(base, exp)` is `**`; `pow(base, exp, mod)` is modular exponentiation (ints; negative exp is modular inverse).
- **v0.57:** `str.rsplit` and optional `maxsplit` on `split`/`rsplit` (`None` sep is whitespace; `maxsplit < 0` is unlimited).
- **v0.58:** `int(s[, base])` and `float(s)` parse strings (CPython rules; ASCII whitespace; `int` bases 0 and 2..=36; `float` accepts `inf`/`nan`).
- **v0.59:** `str.index` and optional `start`/`end` on `find`/`index`/`rfind`/ `rindex` (CPython slice bounds; `None` allowed; miss is `-1` or ValueError).
- **v0.60:** `str.replace(old, new[, count])` — `count < 0` is unlimited; empty `old` inserts `new` between characters (capped by `count`).
- **v0.61:** `str.splitlines([keepends])` — CPython line boundaries (`\\n`/`\\r`/`\\r\\n`/`\\v`/`\\f`/C0 seps, UTF-8 U+0085/U+2028/U+2029); truthy `keepends` keeps the break; trailing break does not add `''`.
- **v0.62:** `str.count(sub[, start[, end]])` — same slice bounds as find; empty needle is `len(slice)+1`; start past `len` is 0.
- **v0.63:** `str.startswith`/`endswith` accept a tuple of strs and optional `start`/`end` (same slice bounds as find).
- **v0.64:** `str.capitalize` / `title` / `swapcase` (Unicode-aware since v0.91; title words break on cased characters and use the titlecase mapping, so `'` starts a new word like CPython).
- **v0.65:** `str.zfill` / `center` / `ljust` / `rjust` — pad to width (`zfill` keeps a leading `+`/`-`; fillchar is one character, counted in code points since v0.90; extra center pad matches CPython 3.14).
- **v0.66:** `str.isalnum` / `istitle` / `isascii` — ASCII predicates (empty `isalnum`/`istitle` are False; empty `isascii` is True).
- **v0.67:** `str.expandtabs([tabsize])` — tab stops (default 8); `\\n`/`\\r` reset the column; `tabsize <= 0` deletes tabs.
- **v0.68:** `str.strip` / `lstrip` / `rstrip` accept optional `chars` (`None` or omitted is Unicode whitespace since v0.91; empty `chars` is a no-op).
- **v0.69:** `str.isdecimal` / `isnumeric` / `isidentifier` / `isprintable` (Unicode 16.0.0 since v0.91: `"²"` is a digit but not a decimal, `café` and `π` are identifiers, empty is printable).
- **v0.70:** `tuple.count(x)` / `tuple.index(x)` — same tag+equality as `in` (homogeneous coerces; miss is `ValueError: tuple.index(x): x not in tuple`).
- **v0.71:** `list.index` / `tuple.index` accept optional `start`/`end` (CPython slice bounds; `None` is a type error; miss is the same ValueError).
- **v0.72:** `str.casefold()` — full Unicode case folding since v0.91, so `"ß".casefold()` is `"ss"` and matches `"SS".casefold()`.
- **v0.73:** `str.maketrans` / `str.translate` — 2-arg maps strings of equal character length; 3-arg also deletes; `translate` accepts `dict[int, int]` or `dict[int, int | None]` (code point ordinals since v0.90, as `ord` produces; replacements via `chr`).
- **v0.74:** `set.copy()` — shallow copy (independent of later add/remove).
- **v0.75:** `set.pop()` — remove and return an element (last-inserted); empty is `KeyError: 'pop from an empty set'`.
- **v0.76:** `set.intersection_update` / `difference_update` / `symmetric_difference_update` and `&=` / `-=` / `^=` (in-place, same element type; aliases see the mutation).
- **v0.77:** `dict.fromkeys(iterable[, value])` — keys from `list`/`set` of int or str, or a `str` (chars); omitted value is `None`.
- **v0.78:** class `<` / `<=` / `>` / `>=` — `__lt__` / `__le__` / `__gt__` / `__ge__` when defined on the left class (virtual, including inherited); no identity fallback.
- **v0.79:** `sorted` / `list.sort` / `min` / `max` of class instances that define `__lt__` (virtual, including inherited; CPython uses `<` only); empty iterable `min`/`max` matches the usual ValueError / `default=`.
- **v0.80:** reflected class ordering — `a < b` tries `b.__gt__(a)` when the left type has no `__lt__` (and the other swap pairs); subclass-first when the right type is a proper subclass and defines the reflected slot. No `NotImplemented` fallthrough. `sorted` / `min` / `max` accept `__gt__` as well as `__lt__`.
- **v0.81:** reflected class `==` / `!=` — `1 == P()` calls `P.__eq__(1)` when the left type has no `__eq__` and the left type is assignable to `other` (subclass-first when the right type is a proper subclass). Identity remains only when neither side provides a usable `__eq__`.
- **v0.82:** class `__getitem__` / `__setitem__` / `__delitem__` — `obj[k]`, `obj[k] = v`, `del obj[k]`, and `obj[k] += v` (virtual, including inherited). Slice syntax on a class is still residual.
- **v0.83:** class `!=` uses `__ne__` when present (virtual, inherited, reflected, subclass-first); otherwise it still inverts that receiver's `__eq__`. Comparison and class-membership operands are evaluated once in source order (needle before container).
- **v0.84:** user-iterator `for` / comprehensions treat `StopIteration` from `__next__` as the loop terminator only; body and target-binding exceptions propagate. List/set/dict comprehensions accept the same iterables as `for` (range, list, str, tuple, dict keys, set, file, generator, class `__iter__`).
- **v0.85:** `list[C]` `==` / `!=` / `in` / `index` / `count` / `remove` and tuple `==` / `!=` use class `__eq__` (virtual, inherited, identity fallback). List `!=` negates element `==`, not `__ne__`. Homogeneous `tuple[C, …]` `in` / `index` / `count` use the same protocol; mixed-tuple membership stays slot identity.
