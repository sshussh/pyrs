# Performance

What PyRs is fast at, what it is not fast at, how the numbers are produced,
and the specific changes that produced them.

## Benchmarks

`benchmarks/run.sh` compiles each program with `pyrs -O2`, verifies that its
output is byte identical to `python3`'s, and only then reports best of five
wall clock times. A benchmark whose output differs is a failure, not a slow
result.

| Benchmark | Workload | python3 | PyRs | Speedup |
|---|---|---:|---:|---:|
| nbody | float and list, 5 body gravity, 100k steps | 0.815s | 0.020s | 41.2x |
| mandelbrot | float math, 500x500 escape iterations | 0.604s | 0.018s | 33.6x |
| pipeline | lazy `map` and `filter` over 2M elements | 0.332s | 0.017s | 19.6x |
| strings | per character iteration, 2.6M comparisons | 0.373s | 0.023s | 16.3x |
| iteration | `zip` and `enumerate`, 2M paired steps | 0.340s | 0.022s | 15.7x |
| primes | integer loops, trial division to 300k | 0.430s | 0.031s | 14.0x |
| matmul | nested lists, 250x250 matrix multiply | 0.552s | 0.042s | 13.0x |
| fib | recursion, 30M calls (`fib(35)`) | 0.693s | 0.054s | 12.9x |
| listcomp | comprehensions, 3M element map and filter | 0.481s | 0.046s | 10.4x |
| sort | list indexing, bubble sort of 5000 | 0.664s | 0.079s | 8.4x |
| exceptions | 400k calls, 171k raise and catch round trips | 0.103s | 0.016s | 6.5x |
| objects | 400k small live objects, traced and swept | 0.106s | 0.111s | 1.0x |
| dicts | 1.2M dict and set operations, string keys | 0.248s | 0.332s | 0.7x |
| **total** | | 5.741s | 0.811s | 7.1x |

Measured on Linux with LLVM 22 and CPython 3.14, best of seven at a load of
roughly 3 of 16 cores. Absolute times move with machine load. The speedup
column is the comparable number, because both engines are timed in the same
run.

## The two at the bottom

Compute bound code is 6 to 41 times faster. The last two rows are the honest
exception and they are in the table for that reason: **allocation heavy code is
not yet faster than CPython.**

`objects` and `dicts` both build hundreds of thousands of short lived objects.
Every managed object is an individual `calloc` on one global intrusive list.
With `PYRS_GC=none` the `objects` benchmark runs in 47 ms against 111 ms with
the collector enabled, so the mutator is already twice CPython's speed and the
collector is the entire gap. [GC.md](GC.md) describes the size classed block
design that retires this.

Adding `dicts` is also why the aggregate reads 7.1x rather than the 11.6x of
the twelve benchmarks before it. A benchmark that exposes a weakness lowers the
average, and leaving it out would not have made the compiler faster.

## Why compiled code is fast

Three things, in order of how much they matter.

**Native code with real types.** An `int` addition on two statically typed
values is an add instruction and an overflow check, not a call. A list index is
a bounds check and a load. There is no interpreter dispatch loop, no boxed
integers for small values, and no attribute dictionary on an ordinary object.

**LLVM sees whole functions.** Because the hot primitives are emitted as inline
LLVM IR rather than calls into the C runtime, the optimizer can hoist,
vectorize and fold across them. This is worth more than the calls it removes,
which is a point the next section makes concrete.

**Types come from the program.** [INFERENCE.md](INFERENCE.md) describes how the
compiler recovers types from call sites rather than requiring annotations, so
ordinary Python code gets the fast path without being rewritten.

## How it got here

Four changes account for most of the current numbers. Each began as a
measurement rather than an intuition.

### Integer arithmetic was the original weak spot

Every `int` operation used to be an out of line call into the runtime, because
arbitrary precision needs a tagged representation with an overflow check, and
the runtime is a separate object the optimizer cannot inline through. The
`primes` benchmark ran at 0.8x, that is, slower than CPython.

Each operation now has an inline fast path on the tagged words. The gain
exceeded the calls that were removed, because one opaque call inside a loop
also blocks loop invariant hoisting for everything around it.

### The collector's mark phase was O(n log n) before it started

Marking answers the question "which object contains this address" for every
candidate word, so that lookup is the inner loop of the pass. The original
implementation sorted every live range and binary searched it, which cost a
sort before marking could begin and roughly twenty cache missing probes per
real pointer.

It was replaced by an envelope check (two compares reject any candidate outside
the lowest and highest address in the heap) followed by a granule keyed open
addressed table built in one linear pass. Ranges too wide to file that way, in
practice the large slot arrays owned by lists, go to a small sorted tier.

### Per operation calls that survived the first pass

Four separate cases, each found by profiling rather than by reading code:

* A `try` statement bound `@setjmp`, which on glibc saves the signal mask
  through a system call: 85 ns per call against 1.8 ns for `_setjmp`.
* A caught exception built an object that only a bound name or a bare `raise`
  ever reads.
* `==` on strings computed a full three way `memcmp` ordering to answer a yes
  or no question about two single characters.
* A dictionary rehashed its key from scratch on every lookup.

### Sorting with a key

`sorted(xs, key=f)` recomputed the key inside the comparator, which turns
O(n log n) comparisons into O(n log n) key evaluations. Keys are now computed
once per element and the sort orders by them.

## Dynamic code

Everything above concerns code whose types the compiler knows. Code it does not
know has its own performance story, told in [DYNAMISM.md](DYNAMISM.md), whose
short form is:

| Path | Relative cost |
|---|---|
| Statically typed operation | 1 |
| Profiled monomorphic dynamic operation | roughly 1, behind one tag compare |
| Polymorphic site, four types or fewer | a short chain of compares |
| Unprofiled dynamic operation | roughly 30, plus lost hoisting |

At 50 million iterations of a dynamic accumulation loop, the unprofiled kernel
takes 1.290s, the profiled build takes 0.039s, and the statically typed
equivalent takes 0.046s.

## Compile time

Builds are cached, keyed by source. An unchanged program recompiles nothing.

| | Cold | Cached |
|---|---:|---:|
| Unchanged program | 2610 ms | 11 ms |
| New program | 2610 ms | 84 ms |

Type checking alone, which is what `pyrs check` does, runs in roughly 16 to 19
ms for a typical single file program. Selecting a learned inference engine adds
about 2 ms for `ml` and about 5 ms for `dl`, and bypasses the cache.

## Reproducing any of this

```console
./benchmarks/run.sh          # the table above, best of RUNS (default 3)
make bench RUNS=7            # the same through make
make infer-eval              # the inference measurements
```

Every number in this document comes from one of those commands. None of them is
an estimate.
