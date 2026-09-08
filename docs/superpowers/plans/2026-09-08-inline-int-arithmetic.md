# Inline integer arithmetic: the algebra, and why each check is exact

**Status: implemented.** 0.126.

## The defect

0.125 re-measured the benchmark corpus and found `primes` at **0.8×** CPython
and `exceptions` at 0.9×, against `mandelbrot` at 21.5×. It named the cause but
did not fix it:

| same trial division | PyRs | python3 | |
|---|---:|---:|---|
| in `int` | 507 ms | 427 ms | 0.84× |
| in `float` | 47 ms | 1009 ms | 21.5× |

Floats emit `fadd`/`fmul`. Every `int` operation was `call i64 @pyrs_int_add`
into a separately compiled object with no LTO — so LLVM had to assume an
arbitrary memory clobber and a possible non-return at every arithmetic site.
That is worse than the call itself: it stops LICM from hoisting a list's length
load, stops GVN from folding the repeated null checks, and stops vectorization,
for the whole enclosing loop. `docs/PRIMITIVES.md` §5.1 already ranked this as
the anti-pattern; `docs/ROADMAP.md` already carried it as an open item.

## The representation

`runtime.c:1038` tags an int as

```text
T(v) = (v << 1) | 1        for v in [-2^62, 2^62-1]     (LSB set)
       pointer to PyrsInt  otherwise                    (LSB clear)
```

Two properties carry every proof below:

* **`T` is strictly increasing and never wraps.** Its image is
  `[-2^63+1, 2^63-1]`, which fits `i64` exactly.
* **`T(v)` has bit 0 set** and a GC pointer does not, so `a & b` has bit 0 set
  precisely when both operands are small.

## Why the overflow checks are exact, not conservative

The trick that makes this cheap: **do not untag, then check the result**.
Compute `2s + 1` directly in one `i64` operation and take the signed-overflow
flag *as* the range test.

Writing `s` for the true mathematical result:

```text
2s + 1 <=  2^63 - 1   <=>   s <= 2^62 - 1 = SMALL_MAX
2s + 1 >= -2^63       <=>   2s >= -2^63 - 1
                      <=>   s  >= -2^62 - 1/2
                      <=>   s  >= -2^62 = SMALL_MIN      (s is an integer)
```

The half-integer on the lower bound is what makes it **tight**: `s = SMALL_MIN-1`
gives `2s+1 = -2^63-1`, which does overflow. So `!ovf` means exactly "the result
is a small int", with no slack at either end — a stronger check than the
runtime's own `__int128` compute-then-compare, in one instruction.

Applied per operation:

| Op | Fast path | Why the guard is what it is |
|---|---|---|
| `a+b` | `sadd.with.overflow(a, b-1)` | `T(av+bv) = a + (b-1)`; `b-1 = 2bv` never wraps |
| `a-b` | `ssub.with.overflow(a, b-1)` | same algebra. The runtime's `pyrs_int_sub` was `add(a, neg(b))` — two calls, one allocating |
| `a*b` | `smul.with.overflow(a-1, b>>1)` | `2av · bv = 2p`, so the flag is again exact. Untagging *both* would need a separate bound test, because a product can wrap back into range |
| `< <= > >= == !=` | `icmp` on the **raw tagged words** | monotonicity. Zero untag instructions |
| `a % b` | `srem` + floor adjust | `\|r\| < \|bv\| <= 2^62`, so `%` can never leave the range. Only guard is `bv != 0` |
| `a // b` | `sdiv`+`srem` + floor adjust | one out-of-range case only: `SMALL_MIN // -1 = SMALL_MAX+1`. `T(-1)` is `-1` exactly, so the edge test is two compares on raw words |
| `& \| ^` | `and`/`or`/`xor` on raw words | bit 62 == bit 63 for every small, and that is closed under all three, so the result is always small. Bit 0 survives `&` and `\|`; only `^` needs `\| 1` |
| `-a` | `ssub.with.overflow(2, a)` | `T(-av) = 2 - a`. Overflow iff `av == SMALL_MIN`, matching `pyrs_int_neg` |
| `~a` | `sub i64 0, a` | `T(~av) = -a`. `~` is a **bijection of the small range onto itself**, so no check at all |
| `bool(a)` | `icmp ne i64 a, 1` | unguarded — see below |
| box / unbox | `sadd.with.overflow(v, v)` / `ashr` | same half-integer algebra |

Floor semantics come from C's truncating `sdiv`/`srem` the way `divmod_floor`
does it: adjust when the remainder is non-zero and its sign differs from the
divisor's. Both arms go into a `select` rather than a branch, so the fast block
stays straight-line.

### The one unguarded fast path

`bool(a)` compiles to a single `icmp ne i64 %a, 1` with **no smallness test**.
That is sound because `int_from_sign_limbs` (runtime.c:1080) is the *sole*
constructor of a heap `PyrsInt` — it holds the only `pyrs_gc_alloc(...,
PYRS_GC_BIGINT)` in the tree — and it returns `tag_small(0)` whenever
`nlimbs == 0 || sign == 0`. So no heap int is ever zero-valued, and no pointer
is ever `T(0) = 1`. The invariant is now load-bearing and is recorded where it
is established.

### No `nsw`/`nuw`

Every tagging `add`/`sub`/`shl` is provably non-wrapping under its guard, so
the poison flags would be *legal*. They are deliberately absent: if a guard is
ever wrong, a poison flag converts a loud wrong answer into undefined
behaviour — this compiler's most serious defect class. LLVM recovers the same
code from the guard's range facts anyway.

## Packaging: `alwaysinline` helpers, not open-coded IR

Each fast/slow sequence is emitted **once per module** as
`define internal ... alwaysinline`, and every call site emits a single `call`.

This is the decision that made the change safe rather than sprawling.
`emit_sum` and `emit_min_max_list` pre-reserve `%tN` names and hard-code phi
predecessors (`[ %acc_next, %sum.body ]`). Open-coding a guard would split the
block those phis name, and the same hazard exists anywhere else in the 6800-line
emitter that assumes an expression does not branch. With helper functions,
`emit_binary` stays exactly one line per operation and **no existing block or
phi assumption changes at all**.

It also keeps `-O0` honest: `buildO0DefaultPipeline` runs `AlwaysInlinerPass`,
so the bodies inline at every optimization level while the emitted text and
parse time stay flat. If a helper ever failed to inline it degrades to a
same-module call — rank 2 in `docs/PRIMITIVES.md` §5.1, not an opaque runtime
call.

## What it cost, and what it did not

**Zero lines** in `ir/`, `semantic/` and `codegen/runtime/`. No new IR node, no
new type, no runtime primitive. The slow edges are the same functions that were
called unconditionally before, so bignum semantics are untouched by
construction — and division by zero routes to the runtime rather than trapping
inline, so its exception type, message and catchability are unchanged for the
same reason.

## Result

| benchmark | 0.125 | 0.126 |
|---|---:|---:|
| nbody | 18.8× | 48.2× |
| mandelbrot | 21.5× | 37.0× |
| pipeline | 10.4× | 15.0× |
| matmul | 5.4× | 13.1× |
| fib | 4.1× | 12.8× |
| primes | **0.8×** | **12.4×** |
| iteration | 7.0× | 9.9× |
| sort | 3.2× | 8.3× |
| listcomp | 1.6× | 6.3× |
| strings | 2.8× | 3.4× |
| exceptions | 0.9× | 1.1× |
| **total** | **3.2×** | **9.5×** |

Every benchmark is now faster than CPython.

**The gain exceeds the calls removed**, which is the part worth remembering.
The float benchmarks never called the runtime for arithmetic — but they did for
their loop counters, and one opaque call is enough to stop optimization for the
whole loop. `nbody` and `mandelbrot` roughly doubled without a single float
instruction changing.

`primes` fell from 457 ms to 31 ms. `perf` shows 669M instructions at 5.2 IPC,
and the disassembly shows LLVM narrowing the 64-bit `srem` to a 32-bit `idiv`
behind a range check — an optimization it could not even attempt while the
division was behind a call. The runtime's `%` and `//` had no small-int fast
path *at all*: every `n % d` did two `malloc`s, a general bignum divide, and two
`free`s.

## How it is checked

The defect class here is a silent wrong answer at a boundary, so the checks are
layered loudest-first.

1. **The LLVM verifier, free.** The shim verifies before optimizing, so a
   malformed phi is a compile error with a message, never a wrong answer. One
   test compiles int arithmetic inside `sum`, `min`/`max`, a comprehension, a
   generator and a `try` for exactly this.
2. **Inline vs runtime.** `PYRS_INLINE_INT=0` reverts every site to the plain
   call. `cli/tests/int_inline_parity.rs` compiles the same program both ways
   and requires byte-identical output — a diff means the two disagree, with no
   CPython semantics in the way. The flag is read at emit time and is not in
   the cache key, so those compiles pass `--no-cache`.
3. **Against CPython**, at -O0/-O2/-O3, over 30 values crossed pairwise (900
   pairs × 15 operators). The set is each bound of the representation with both
   neighbours, so small × small, small × heap, heap × small and heap × heap
   reach every guard. Also in the compatibility corpus as
   `cases/int_boundary.py`, regenerated by `scripts/gen_int_boundary.py`.
4. **Seeded random**, sampled by *bit length* rather than magnitude so
   boundaries are hit far more often than uniform sampling manages.
5. **The benchmark corpus**, which already refuses to time a program whose
   output differs from CPython.

## Still open

- **`exceptions` at 1.1×** is the remaining outlier. Any function containing a
  `try` anywhere marks *every* local `volatile`, which defeats `mem2reg`
  function-wide. C's rule only requires it for locals modified between the
  `setjmp` and the `longjmp`.
- **`**`, `<<`, `>>`** are still runtime calls. Each needs shift-count range
  analysis for a case that is rare in practice.
- **`strings` at 3.4×** iterates with one `pyrs_str_index` call per character.
- **Allocation** is a per-object `calloc` on a global intrusive list, and every
  collection `qsort`s all live objects before marking.
- **The backend never sees the opt level.** `CodeGenOptLevel` is not passed to
  `createTargetMachine`, and the CPU is `"generic"` with no feature string.
