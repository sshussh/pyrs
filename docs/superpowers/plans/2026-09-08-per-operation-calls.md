# The calls that were left, and two that were not worth removing

**Status: implemented.** 0.131 (`_setjmp`, dead exception object, aggregate
loops, borrowed magnitudes), 0.132 (string equality and ASCII indexing, plus an
out-of-bounds write), 0.133 (dict and set hashing).

The second optimization pass. The first —
[inline int arithmetic](2026-09-08-inline-int-arithmetic.md) and
[the collector and backend](2026-09-08-collector-and-backend.md) — took the
corpus from 3.2× to about 10×. This one went after what a profile still showed
per operation, and it includes two changes that were implemented, measured, and
removed. Those are as much the result as the ones that stayed.

## 0.131 — a `try` was making a syscall

The emitted IR declared `@setjmp`. Naming an ELF symbol in IR bypasses glibc's
`#define setjmp(env) _setjmp(env)`, so it bound to `__sigsetjmp(env, 1)` — the
variant that saves the signal mask through `rt_sigprocmask`, and whose
`longjmp` pays a second syscall to restore it.

Measured directly against the two real ELF symbols, 400k calls each:

```
setjmp  (ELF, savemask=1):   33.99 ms    85.0 ns/call
_setjmp (ELF, savemask=0):    0.73 ms     1.8 ns/call
```

`exceptions.py` does 400k try entries and 171k longjmps, so ~33 ms of its 71 ms
was mask-saving alone. The IR now emits `@_setjmp`, and the runtime pairs it
with `_longjmp` so the save and restore forms match rather than relying on
glibc's `__mask_was_saved` flag to bridge them. **71 ms → 31 ms.**

Worth noting how this hid: the C runtime's own `setjmp` call goes through the
header macro and always got `_setjmp`, so a compiled binary imported *both*
symbols and only the generated half was paying.

### A caught exception built an object nobody read

`pyrs_exc_object()` ran at every matched handler entry, and a second time when
the handler bound a name. Each call GC-allocates twice — the object, and a
`PyrsStr` rebuilt from the formatted message. Its only consumers are a bound
name and a bare `raise`.

So `except ValueError:` paid for two allocations it discarded, 171,429 times,
and the garbage forced ~19 collections.

**The emitter now writes the call speculatively and splices the line back out
when the handler body never reads it.** The decision is made by *actual use* —
`Stmt::Reraise` sets a flag on the handler's stack entry — rather than by
scanning the body for a bare `raise`. A traversal over the statement enum could
overlook a kind; this cannot. Binding reuses that one object instead of
allocating a second. **31 ms → 14 ms**, and no collections at all.

### The last four raw int calls, and a malloc per comparison

`sum`, `min` and `max` build their own loops with hand-written phi predecessors
and predate 0.126, so they still emitted `pyrs_int_add` / `pyrs_int_cmp`
directly. The inline helpers are one `call` line by design, so those phis stay
valid and the fix is one line each.

And `int_read_mag` `xmalloc`'d a one-limb buffer **for a small operand**, so
every mixed-operand compare, add, multiply and divide did two malloc/free
pairs. `int_borrow_mag` writes that limb into a caller-supplied stack word.

Eleven call sites converted. **Two hand the magnitude onward to something that
takes ownership** and now copy first: `to_twos`, and the negative branch of
`pyrs_int_rshift`, where the transfer sits 25 lines below the read. `int_read_mag`
is deleted so no future caller picks up the allocating reader.

## 0.132 — strings, and an out-of-bounds write

### The bug, fixed first

`utf8_next` treats an invalid or truncated byte as a single latin-1 code point,
so `str_done_scan` counts it as one and a **non-UTF-8 file read satisfies
`STR_IS_ASCII`** (`cplen == len`). Indexing such a string asked to intern a byte
≥ 0x80 in a **128-entry** table: `single_char(0xE9)` wrote three fields about
2.5 KiB past the end.

The table is now 256 entries, which makes the answer *correct* rather than
merely in-bounds — the byte counts as one code point in the parent string and
yields a one-code-point string here, so slicing and re-joining still round-trip.
It is also statically initialized (via a macro-expanded initializer) and
exported, because codegen now indexes it directly.

This landed before the optimization, so the fast path is built on correct
ground.

### `==` computed an ordering to answer a yes/no question

`pyrs_str_cmp` has no length short-circuit and no identity check, so comparing
two one-character strings cost an opaque call plus a `memcmp` PLT call — 43% of
the benchmark — to produce a three-way ordering that was then discarded.

Three facts decide most comparisons inline:

| test | decides | why it is only this much |
|---|---|---|
| same pointer | equal | **positively only.** A literal is a module global (`@.str.N`); `s[i]` is the runtime's interned singleton. Equal one-character strings routinely differ in address, so an unequal pointer proves nothing |
| different `len` | not equal | byte count, which is not code-point count — fine for equality, useless for ordering |
| both `len == 1` | compare the byte | the character-iteration case, and the one `memcmp` was called for |

Anything else is the same call as before, including a null operand — `check_ref`
is what turns a local read before assignment into `UnboundLocalError`, and a
load here would segfault instead. **~100 ms → 25 ms.**

Ordering comparisons keep the call; they need the full lexicographic answer and
no benchmark shape makes them hot.

### `s[i]` re-derived what the caller already knew

That left `pyrs_str_index` at 41%. Its fast path is the same `cplen == len`
test the runtime makes, then a bounds check and a load, ending at the address of
an interned entry — no call, no allocation, and no lazy-init branch now that the
table is statically initialized. The IR names the table's layout as
`[256 x { i64, i64, [2 x i8] }]` and `runtime.c` `_Static_assert`s the same
24-byte stride, so the two cannot drift.

Out of range and non-ASCII fall through to the runtime, so the `IndexError` text
stays in one place. **25 ms → 22 ms.** `strings` went 3.2× → 16.3×.

## 0.133 — dicts, which nothing was measuring

Nothing in the corpus touched a dict, so `hash_key` recomputing FNV-1a
byte-at-a-time over the whole key on **every** lookup, insert and membership
test was invisible. `benchmarks/dicts.py` put it at **0.8×**, with `set_lookup`
at 45% and `hash_key` at 18%.

### One byte of hash, in the padding

A slot caches the **top byte** of its key's hash, so a probe landing on a
colliding full slot rejects it with one compare instead of a length check and a
`memcmp`.

One byte, not the whole hash, and that distinction is the whole result.
`DictSlot` is 25 bytes of payload padded to 32, so a `char` is free while a
`long long` takes it to 40 — a quarter more memory traffic on every probe.
Storing the full hash was implemented first and made the benchmark **slower**
(241 ms → 262 ms with the collector off); the byte version made it faster
(235 ms). `_Static_assert`s now pin both slot sizes so the tag cannot silently
stop being free. The top byte is cached because the low bits already chose the
bucket.

The trade is that a resize has no full hash to reuse and must rehash — amortized
O(1), against a cost paid on every probe.

### The finalizer is not optional

Hashing eight bytes per multiply instead of one was, on its own, a **6×
regression**: 314 ms → 1817 ms.

The bucket index is `h & mask`, so only the low bits matter — and the low bits
of a product depend only on the low bits of its inputs. Multiplying once per
byte stirs every byte into the low bits many times over; multiplying once per
word does not, so keys differing in their last characters all landed in one
bucket. A `fmix64` finalizer — two multiplies, three xor-shifts — makes every
input bit reach every output bit and fixes it.

**Result:** `set_lookup` 45% → 10%, `hash_key` 18% → 9%, benchmark
314 ms → 282 ms.

## Two changes that were implemented and removed

Both were in the plan. Neither survived measurement, and the negative results
are recorded in the roadmap so they are not re-attempted without a different
approach.

### Runtime attributes and list alias scopes

Fully implemented: `!alias.scope`/`!noalias` separating a container header from
its element buffer (sound — the header is `pyrs_gc_alloc`'d and `data` is a
separate `xmalloc`, and with no LTO the claim only has to be self-consistent
within the module), plus `nounwind` / `memory(read)` / `willreturn` on the nine
runtime functions that provably neither allocate nor trap.

The motivation was real and measured: disassembling `pyrs_sort` showed the
length and data-pointer loads **reloaded inside the innermost loop**, 14 header
loads across the function, even though the list is a loop-invariant parameter.

**Effect: none.** Every benchmark within noise, a purpose-built read-only float
loop within noise, and the header-load count unchanged at 14.

The reason is that **every loop carries a tagged-int counter** whose
`pyrs.int.add` cold edge calls `pyrs_int_add`, which can allocate a bignum and
so cannot be attributed. One such clobber stops LICM regardless of what the
other calls claim. `sort`'s profile also says the prize is one instruction in a
~50-instruction loop body — 99.86% of it is `pyrs_sort` itself, dominated by
guard chains and bounds checks.

The route that would pay is a machine-`i64` induction variable for `range` loops
with statically small bounds, which *removes* the clobber rather than describing
it. Attributing the allocating int family would need
`memory(read, inaccessiblemem: readwrite)`, which is a real claim about the
collector's access to list slots and not worth the silent-miscompilation surface
for that prize.

### Retaining the collector's scratch buffers

`ranges_push` is 15% of `objects`, and the array does restart at capacity 64 and
double its way to ~600k entries every pass, copying up to 14 MB on the way.

But that is ~56 reallocations against **2.4M pushes**. Removing the regrowth
measured at nothing: 104 ms vs 105 ms A/B. The 15% is the pushes themselves, one
per live object per collection, which only a different heap layout removes.
Retaining the buffers would also have pinned the high-water mark in resident
memory, against the bounded-memory gate.

## How these are checked

- `cli/tests/handler_object.rs` — the object is built exactly when something
  reads it: a plain handler builds none (asserted against the emitted IR), a
  binding builds one rather than two, a bare `raise` keeps it, and a bare
  `raise` under nested control flow still counts.
- `cli/tests/int_inline_parity.rs` — shifts and bitwise ops over the boundary
  set, which is the check for `int_borrow_mag`'s two ownership transfers, plus
  aggregates crossing the small boundary.
- `cli/tests/string_fast_paths.rs` — pairwise equality over a set chosen so each
  shortcut is both taken and refused, a character against a literal, strings of
  eight different provenances, indexing across ASCII and multi-byte, out-of-range
  indexing, and a non-UTF-8 file (PyRs's own round-trip, since CPython rejects
  the file and there is no oracle).
- `cli/tests/hash_tables.rs` — growth across many resizes, tombstone reuse,
  insertion order independent of the hash, key lengths 0–40 across the eight-byte
  blocking, keys differing only in length, and int and tuple keys as the control.

## Where it leaves the corpus

Compute-bound code is 6–41×. **`objects` at 1.0× and `dicts` at 0.7× are the
only places PyRs is still behind CPython**, and they are the same cause: every
managed object is an individual `calloc` on one global intrusive list. Turn the
collector off and `objects` runs in 47 ms against 111 ms — the mutator is
already twice CPython's speed and the collector is the whole gap.

With the two cheaper fixes above tried and measured at nothing, what is left is
the heap layout itself: size-classed blocks, aligned so a candidate's owner is a
mask and a divide rather than a table lookup, which also retires the range array
and the granule index.
