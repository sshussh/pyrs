# Guarded specialization

**Status: implemented in 0.148.0.** Item D of
[the remaining dynamism brief](2026-09-10-remaining-dynamism-work.md).
Consumes [`pyrs profile`](2026-09-15-type-profile.md).

## Design

A site the profile saw with exactly one type tag compiles as

```
if (tag == expected) { the typed operation, inlined }
else                 { the pyrs_dyn_* kernel }
```

There is no deoptimization and no JIT: the slow path was compiled ahead of time
beside the fast one. A site the profile does not call monomorphic calls the
kernel directly, so code size grows only where a profile justifies it.

Specialized operations: int `+ - * // %`, float `+ - * /`, int comparison, and
int unary `+ - ~ abs`. Methods and `in` keep the kernel.

## The claim that mattered, and how it was checked

A specialization that changes behaviour is a bug. The dangerous case is a
profile that is *wrong* for the run: trained on ints, executed on strings. Review
profiled a function on integer arguments, compiled with that profile, and ran
the same binary with string arguments. The guard missed, the kernel took over,
and the output matched CPython — also under `PYRS_GC_STRESS=1`.

## Measurement

At 2M iterations the timings sit inside process-startup noise (a profiled
dynamic loop appeared *faster* than the typed one, which is the tell). At 50M
iterations of `total = total + i`, with both operands `object`:

| path | time |
|---|---:|
| `total: int`, typed | 0.046s |
| `object`, no profile | 1.290s |
| `object`, profiled | **0.039s** |

The profiled dynamic loop matches the typed loop within noise and is 33× faster
than the unprofiled kernel. 0.148.0's changelog reported 9× from the noisy size.

## Limits

Only single-type sites specialize. A 2–4 type polymorphic inline cache is the
natural next step — the profile already records up to four tags — and would
cover the gap between 79.5% and 96.1% of executions.
