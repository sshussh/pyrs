# The collector, the backend, and one blanket rule

**Status: implemented.** 0.127 (volatile locals), 0.128 (backend knobs),
0.129 (bulk slot visiting), 0.130 (granule index).

Companion to [inline int arithmetic](2026-09-08-inline-int-arithmetic.md),
which was the same pass's first and largest milestone. That one closed the
integer gap; these four closed everything else the first pass found.

## 0.127 — one `try` pinned every local in the function

`emit_function` set `volatile_locals = try_depth > 0`, and every load and store
of every local in the function then carried `volatile`. One `try` anywhere —
including a cold validity check after a hot loop — defeated `mem2reg` for the
whole body.

C's setjmp rule (7.13.2.1) is narrower: it covers automatic objects **changed
between the `setjmp` and the `longjmp`**. One written only before the `setjmp`
keeps its value by the contract of `setjmp` itself, which `returns_twice` is
what makes LLVM honour. So the rule is per *variable*, and only a **store**
inside a `try` disqualifies one. A read records nothing — an alloca with a
volatile store is already left in memory, and marking the read too would only
block CSE on it.

### The set is computed by emitting the function twice

The first pass runs the **real** traversal with its output discarded and
records every local stored while `self.tries` is non-empty; the second emits
for real.

A parallel analysis over the statement enum would be faster and would risk
overlooking a statement kind — and being wrong here is a silent
miscompilation: a local reading back garbage in a handler, only under
optimization, only after a real raise. Emission is a small fraction of compile
time next to LLVM, so the second pass does not show up.

**Measured:** a hot float loop with six live locals beside a cold `try`, 20M
iterations: **114 ms → 66 ms**, and the function body went from 40
stack-referencing instructions to 11. The `exceptions` benchmark does *not*
move, correctly — its whole loop body is inside the `try`, so its locals
genuinely are written between the `setjmp` and the `longjmp`.

## 0.128 — two knobs the shim never passed

`createTargetMachine` never received the optimization level, so instruction
selection, scheduling and register allocation always ran at `Default`: `-O3`
never reached the backend and `-O0` never got a fast one.

And the CPU was `"generic"` with an empty feature string — baseline x86-64-v1,
no AVX2, BMI2 or FMA. New `--target-cpu generic|native|<model>`, and
`target-cpu` under `[tool.pyrs]`.

**The default depends on what the binary is for.** `run` and `test` default to
`native`: they build for this machine and throw the binary away. `compile` and
`build-extension` default to `generic`, because the artifact may be moved and a
binary built for the wrong host faults with an illegal instruction rather than
a diagnostic. The **resolved** model and feature string — not the request —
joins the program cache key, because two hosts both asking for `native` resolve
it differently.

**Worth ~10% on a vectorizable float kernel and nothing on this corpus**, which
is scalar-dependency-bound. Recorded as measured rather than as a headline.

## 0.129 — the mark phase paid a call per list element

Tracing a `list[int]` offered every element to the collector one at a time,
through an indirect call into `mark_candidate`, which then binary-searched the
range table to decide whether the value was a pointer. It never is: a tagged
small int is odd and tiny, and a `float` bit-cast into a slot is enormous.
`listcomp` has **four live objects** and spent 49% of its runtime there.

Two changes, neither touching what is reachable:

- **A heap envelope.** The range table records the lowest and highest address
  it covers; a candidate outside them is rejected by two compares.
- **Bulk slot visiting.** `pyrs_gc_trace_object` gained a second visitor for a
  contiguous run of slots, so a list or tuple is handed over once rather than
  per element. The envelope bounds hoist out of that loop and the per-candidate
  call becomes direct rather than a function pointer into another translation
  unit.

Dicts and sets keep the per-slot form: their slots are strided, not contiguous.

**Measured:** pipeline 15.0× → 20.7×, iteration 9.9× → 15.2×, listcomp
6.3× → 10.5×.

This is also where `benchmarks/objects.py` was added — 400k small live objects.
The rest of the corpus builds a handful of very large objects and so never
exercised allocation or sweeping at all. It went in at **0.7×** rather than
being left out.

## 0.130 — the collector sorted its heap on every pass

`PYRS_GC=none` was the measurement that mattered: `objects` ran in **36 ms
without the collector and 109 ms with it**, over just four passes. So two
thirds of it was collection, and the mutator was already twice CPython's speed.

Conservative marking has to answer "which object contains this address" for
every candidate word. It did that by pushing every live range into an array,
`qsort`ing it, and binary searching — an O(n log n) sort before marking could
start, plus ~20 cache-missing probes for every real pointer.

### The granule index

Each range is now filed under **every 256-byte granule it covers**, in an
open-addressed table built in one linear pass. A lookup hashes the candidate's
granule and scans to the first empty slot: O(1) expected, one or two cache
lines, no sort at all.

A slot is `{ uint32 tag, uint32 range }` — the low bits of the granule plus the
range index *plus one*, so a zeroed slot reads as empty. Entries for a granule
are inserted from its hash position onward, so scanning to the first empty slot
sees all of them.

Ranges too wide to file that way — the handful of large `data` buffers a
program's lists own — go to a small sorted tier that keeps the old search.

**Marking became more inclusive, not less.** It retains every range containing
the candidate rather than only the nearest by start, which subsumes the old
special case for an address that is simultaneously one allocation's
one-past-the-end and the next one's start.

**Measured:** `objects` 109 ms → 61 ms, 0.7× → 1.1×, and with `exceptions`
reaching 1.4× every benchmark in the corpus became faster than CPython.

Nothing about allocation, object lifetime, sweeping or ownership changed. The
per-object `calloc` is still there; the evidence said the sort and the search
were the cost.

## How these are checked

- `cli/tests/setjmp_locals.rs` — differential at -O0/-O2/-O3: a local written
  inside the `try` and read in the handler, one written only before it, nested
  tries, a handler that writes, a loop counter incremented outside and read
  inside, heap values live across a raise under GC stress, and a generator
  (whose frame storage already survives a resume and is unaffected).
- `cli/tests/collector_index.rs` — both index tiers against CPython under
  `PYRS_GC_STRESS=1` and tiny thresholds: many small objects, large owned
  buffers mixed with small ones, sizes straddling granule boundaries, nested
  containers traced through both tiers, dict and set tables, and values held
  only by a generator frame or a local live across a raise.
- `cli/tests/build_cache.rs` — `generic` and `native` must not share a program
  cache entry, and the same request must hit.

## Still open

The per-object `calloc`. Every managed object is an individual allocation on
one global intrusive list, so the collector walks a cache-cold pointer chain
twice per pass and pushes one range per live object. See
[the second pass](2026-09-08-per-operation-calls.md) for the two cheaper fixes
that were tried against it and measured at nothing.
