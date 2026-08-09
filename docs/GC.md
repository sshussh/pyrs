# Garbage collection

PyRs ships a tracing garbage collector for managed heap memory. The current
backend is a **single-threaded, nonmoving mark–sweep collector with
conservative native roots**. It is the first default collector for PyRs:
it reclaims unreachable objects and cycles while preserving the stable raw
addresses expected by generated LLVM and the C runtime.

This is an important baseline, not the final collector architecture. The
long-term target is a moving generational/Immix design once the compiler can
provide precise stack maps, safepoints, write barriers, and an FFI
pinning/handle protocol.

## How it works

Generated `main` supplies a native stack anchor and registers module-global
slots that can contain managed references. On Linux, initialization resolves
the real hardware stack pointer to its mapped stack boundary through
`/proc/self/maps`; this remains correct when a sanitizer relocates ordinary C
locals to a fake stack. The supplied anchor is the fallback on other
platforms. A collection then has three phases:

1. **Discover roots.** Registered global ranges are visited precisely by
   range. Machine words on the active native stack, explicitly copied
   callee-saved registers (x86-64 and AArch64), a supplemental `setjmp`
   snapshot, and the saved register images in active exception frames are
   treated conservatively as possible managed pointers. Generated functions
   containing `try` reserve the native frame-pointer register: on glibc
   x86-64 its `jmp_buf` representation is pointer-mangled, so allowing LLVM
   to keep a managed value there could otherwise hide that value until
   `longjmp` restored it.
2. **Mark.** The collector follows each managed object's pointer-capable
   fields. This includes container elements, `Any` and union boxes, class
   fields, closure captures and cells, suspended generator frames, and
   exception state. Layouts identify which regions can contain references;
   erased `i64` payload words within those regions are candidate-checked
   conservatively because they do not all retain source-level type metadata.
3. **Sweep.** Unmarked managed allocations are removed from the allocation
   registry and returned to the system allocator. Marked allocations remain
   at the same address.

Variable-sized objects can own separately allocated storage such as container
slot arrays and hash-table buffers. The collector records backing-owner
ranges for these allocations: a conservative interior pointer, including a
valid one-past pointer, keeps its managed owner alive, and sweeping the owner
releases the backing storage. This preserves derived pointers without treating
untyped buffers as independent language objects.

The nonmoving design is deliberate. PyRs values currently cross generated
LLVM, C runtime helpers, native stack slots, and `i64` payload slots as raw
pointers. Keeping object addresses stable makes collection safe without
quietly changing that ABI. Conservative root and erased-payload discovery can
retain an object when an unrelated bit pattern happens to resemble its
address; that delays reclamation but cannot free a live object.

Collections are stop-the-world because a PyRs executable currently executes
user code on one thread. There is no concurrent marker or mutator protocol.
Under the normal policy, a collection is requested after one threshold's
worth of managed allocation since the previous pass. The default threshold is
1 MiB.

## Diagnostic environment variables

The collector is automatic in normal programs. The following environment
variables select its policy and make behaviour observable and reproducible:

| variable | effect |
|----------|--------|
| `PYRS_GC=marksweep` | Select the mark–sweep backend. This is the default. |
| `PYRS_GC=none` | Disable collection while continuing to track managed allocations and statistics. `nogc` is an alias. This is a diagnostic comparison mode, not the default. |
| `PYRS_GC_THRESHOLD=<bytes>` | Request automatic collection after this many managed bytes have been allocated since the previous pass. The default is 1,048,576 bytes (1 MiB), and values below 1,024 are clamped to 1 KiB. |
| `PYRS_GC_STRESS=1` | Request a collection immediately before every managed allocation. This is intentionally slow and is meant for root/tracing validation, not benchmarks. |
| `PYRS_GC_STATS=1` | At normal process exit, write exactly one diagnostics line to `stderr`. Program output on `stdout` is unchanged. |

The statistics line contains these named integer fields:

```text
[pyrs-gc] collections=<n> allocated=<bytes> reclaimed=<bytes> live=<bytes> objects=<n>
```

`collections` is the number of completed mark–sweep passes. `allocated` and
`reclaimed` are cumulative accounted bytes; `live` is the currently tracked
byte count; and `objects` is the number of currently tracked managed
allocations. Accounted bytes include object payloads and owned backing stores
such as list slots, bigint limbs, tuple arrays, and dict/set tables. They do
not include collector headers, temporary native scratch buffers, or allocator
overhead, so they are diagnostics rather than process RSS or a source-level
payload-size guarantee. Tools should parse fields by name rather than depend
on field order.

For example:

```console
$ PYRS_GC=marksweep PYRS_GC_STRESS=1 PYRS_GC_STATS=1 ./program
```

Stress mode is the useful correctness signal: live values should continue to
behave normally while `collections` and `reclaimed` rise. Statistics alone do
not prove that every eligible object was reclaimed because conservative roots
may legitimately retain some garbage.

## Resources and finalizers

Garbage collection manages memory reachability; it is not a deterministic
resource-lifetime mechanism.

- Use `with open(...) as f:` or call `f.close()` explicitly. Sweeping an
  unreachable open file closes its runtime handle, but collection timing is
  nondeterministic and is not a substitute for deterministic cleanup.
- Call `generator.close()` when its `finally` blocks must run. Collecting an
  abandoned generator does not inject `GeneratorExit` or execute user code.
- PyRs does not currently expose `__del__`, weak-reference callbacks, or other
  user finalizers. Sweep order is not a language guarantee.

These rules avoid running arbitrary PyRs code from the collector and avoid
finalizer-order problems for cyclic graphs.

Native runtime helpers whose temporary malloc-backed state can still be live
when they raise register cleanup records on the active exception frame. The
runtime drains that LIFO before a `longjmp`, so a caught exception does not
bypass that native cleanup. This mechanism is for runtime implementation
resources only; it does not run user finalizers.

The runtime also exposes `pyrs_gc_collect()` to its C integration layer for an
explicit pass. It is not a PyRs source-language builtin; applications should
normally rely on the automatic threshold policy.

## Current limits and the GenImmix path

The current backend prioritizes safety and complete heap coverage over pause
time and locality:

- mark and sweep pauses scale with the managed heap;
- conservative native roots and erased payload slots may delay reclamation;
- nonmoving sweep can fragment the native allocator;
- there is no nursery, remembered set, incremental marking, compaction, or
  concurrent collection;
- collection is supported only by the single-threaded runtime model; and
- the fully validated conservative-root path is the Linux x86-64 release
  target. Linux AArch64 has explicit register capture too; non-Linux targets
  currently use the supplied stack anchor fallback and need target-specific
  validation before being treated as release-supported.

A moving generational/Immix successor must not simply move the objects managed
by this backend. It first needs precise compiler-emitted stack maps and
safepoints, typed global/root metadata, write barriers and remembered sets,
Immix block/line metadata, and an explicit policy for raw pointers passed to C
(handles or pinning). Those prerequisites let a nursery collect short-lived
objects cheaply while an Immix mature space provides locality and bounded
fragmentation. Until then, nonmoving mark–sweep is the sound default choice
for the current ABI and supported release target.

## Validation

The end-to-end suite forces collections while roots cross the boundaries most
likely to expose tracing bugs: native locals, module globals, nested
containers, `Any` boxes, exceptions, closure cells, and suspended generators.
A separate case creates unreachable list, dict, tuple/list, and class cycles
and requires the reclaimed counter to increase.

```console
$ cargo test -p pyrs --test e2e gc_stress_preserves_roots_across_runtime_boundaries -- --exact
$ cargo test -p pyrs --test e2e gc_stress_reclaims_unreachable_cycles -- --exact
$ cargo test -p pyrs --test e2e gc_threshold_triggers_without_stress_mode -- --exact
$ cargo test -p pyrs --test e2e gc_roots_survive_unoptimized_and_aggressively_optimized_code -- --exact
$ cargo test -p pyrs --test e2e gc_preserves_roots_restored_only_by_longjmp -- --exact
$ cargo test -p pyrs --test e2e try_controls_are_entry_hoisted_and_caught_exception_loops_stay_bounded -- --exact
$ cargo test -p pyrs --test e2e caught_json_errors_release_native_scratch_before_longjmp -- --exact
```

Run the stress tests in addition to ordinary tests. A normal build verifies
the common collection policy; stress mode verifies that every allocation
boundary preserves all required roots.
