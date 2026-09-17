# Performance

What PyRs is fast at, what it is not fast at, how the numbers are produced,
and the specific changes that produced them.

## Benchmarks

`benchmarks/run.sh` compiles each program with `pyrs -O2`, verifies that its
output is byte identical to `python3`'s, and only then reports the best wall
clock time of `RUNS` runs (3 by default). A benchmark whose output differs is a
failure, not a slow result.

| Benchmark | Workload | python3 | PyRs | Speedup |
|---|---|---:|---:|---:|
| nbody | float and list, 5 body gravity, 100k steps | 0.817s | 0.018s | 46.7x |
| mandelbrot | float math, 500x500 escape iterations | 0.606s | 0.017s | 36.1x |
| pipeline | lazy `map` and `filter` over 2M elements | 0.298s | 0.014s | 21.2x |
| strings | per character iteration, 2.6M comparisons | 0.368s | 0.022s | 16.8x |
| iteration | `zip` and `enumerate`, 2M paired steps | 0.309s | 0.019s | 16.3x |
| primes | integer loops, trial division to 300k | 0.428s | 0.031s | 13.9x |
| matmul | nested lists, 250x250 matrix multiply | 0.543s | 0.042s | 12.8x |
| fib | recursion, 30M calls (`fib(35)`) | 0.682s | 0.054s | 12.6x |
| listcomp | comprehensions, 3M element map and filter | 0.395s | 0.038s | 10.3x |
| sort | list indexing, bubble sort of 5000 | 0.655s | 0.077s | 8.5x |
| exceptions | 400k calls, 171k raise and catch round trips | 0.102s | 0.015s | 6.8x |
| objects | 400k small live objects, traced and swept | 0.079s | 0.068s | 1.2x |
| dicts | 1.2M dict and set operations, string keys | 0.242s | 0.272s | 0.9x |
| **total** | | 5.524s | 0.687s | **8.0x** |

Measured with `RUNS=5 ./benchmarks/run.sh` on Linux with LLVM 22 and CPython
3.14. Absolute times move with machine load, so the speedup column is the
comparable number: both engines are timed in the same run, minutes apart.

## The two at the bottom

Compute bound code is 7 to 47 times faster. The eleven benchmarks above
`objects` total 15.0x on their own; the aggregate reads 8.0x because the last
two are in the table, and they are in the table for a reason: **allocation
heavy code is where PyRs is weakest.**

`objects` and `dicts` both build hundreds of thousands of short lived objects,
and every managed object is an individual `calloc` on one global intrusive
list. The collector is the whole of that cost. On `objects`, best of five:

| | Time |
|---|---:|
| PyRs, collector on | 66 ms |
| PyRs, `PYRS_GC=none` | 36 ms |
| CPython | 78 ms |

The mutator is more than twice CPython's speed and the collector gives most of
it back. `dicts` is the one benchmark still slower than CPython at 0.9x.
[GC.md](GC.md) describes the size classed block design that retires this.

A benchmark that exposes a weakness lowers the average, and leaving it out
would not have made the compiler faster.

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

## Two changes that did not survive measurement

Both were implemented in full and then removed. They are recorded because the
reasoning that motivated them was sound, and someone will have it again.

**LLVM alias metadata on the runtime.** Disassembling `pyrs_sort` showed the
length and data pointer loads being reloaded inside the innermost loop, 14
header loads across the function, although the list is a loop invariant
parameter. The fix was `!alias.scope` and `!noalias` separating a container
header from its element buffer, plus `nounwind`, `memory(read)` and
`willreturn` on the nine runtime functions that provably neither allocate nor
trap.

The effect was nothing: every benchmark within noise, a purpose built read only
float loop within noise, and the header load count unchanged at 14. The reason
is that every loop carries a tagged integer counter whose cold edge calls
`pyrs_int_add`, which can allocate a bignum and therefore cannot be attributed.
One such clobber stops loop invariant code motion regardless of what the other
calls promise. The route that would pay is a machine `i64` induction variable
for `range` loops with statically small bounds, which removes the clobber
instead of describing it.

**Retaining the collector's scratch buffers.** `ranges_push` is 15% of the
`objects` benchmark, and the array does restart at capacity 64 and double its
way to roughly 600,000 entries every pass, copying up to 14 MB on the way. But
that is about 56 reallocations against 2.4 million pushes, and removing the
regrowth measured at nothing: 104 ms against 105 ms. The 15% is the pushes
themselves, one per live object per collection, which only a different heap
layout removes. Retaining the buffers would also have pinned the high water
mark in resident memory.

## Dynamic code

Everything above concerns code whose types the compiler knows. Code it does not
know has its own performance story, told in [DYNAMISM.md](DYNAMISM.md), whose
short form is:

| Path | Relative cost |
|---|---|
| Statically typed operation | 1 |
| Profiled monomorphic dynamic operation | roughly 1.2, behind one tag compare |
| Polymorphic site, four types or fewer | a short chain of compares |
| Unprofiled dynamic operation | roughly 7 on integer arithmetic, more when the kernel does more, plus lost hoisting |

At 50 million iterations of a dynamic accumulation loop, best of three, the
unprofiled kernel takes 0.235s, the profiled build takes 0.042s, and the
statically typed equivalent takes 0.035s.

## Compile time

Builds are cached, keyed by source. An unchanged program recompiles nothing.

| | Time |
|---|---:|
| Cold build, `--no-cache` | 2765 ms |
| Unchanged program, cached | 7 ms |
| New program, warm runtime cache | 80 ms |

Type checking alone, which is what `pyrs check` does, takes 6 ms on a trivial
file and a median of 16 ms across the 293 program corpus. Selecting a learned
inference engine adds about 2 ms for `ml` and about 5 ms for `dl` to that
median, and bypasses the build cache.

## Reproducing any of this

```console
RUNS=5 ./benchmarks/run.sh   # the table above
make bench RUNS=5            # the same through make
make infer-eval              # the inference measurements
```

Every number in this document comes from one of those commands. None of them is
an estimate.
