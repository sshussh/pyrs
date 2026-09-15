# `pyrs profile`

**Status: implemented in 0.147.0.** Item C of
[the remaining dynamism brief](2026-09-10-remaining-dynamism-work.md).

## Why this was ahead of shapes and first-class types

Static inference recovers less than the original plan assumed: 12–14% of
parameter types from call-graph propagation at ~64% accuracy, with 61% of
parameters structural dead ends. A profile is ground truth for whatever actually
ran, so it was the higher-value input.

## Design

`pyrs profile <script>` runs an instrumented build that records, per dynamic
operator or method site, up to four concrete type tags and a hit count. Four,
because a 4-type cache covers 96.1% of executions across six measured
workloads, where a single type covers 79.5%.

The output is a checked-in text artifact:

```
pyrs-profile 1
compiler 0.151.1
source da5580b8e8452c0cfa268751aae4165299d26a9c2cad0e3ca43065536c88403f
site 0
  0 5
```

The header pins the compiler version and a digest of the source. `pyrs compile
--profile <file>` ignores a missing, unreadable, stale or mismatched profile
with a warning and degrades to the unprofiled emit — never to a wrong one.
`--profile` changes only the code emitted, never observable behaviour.

## Verification

`cli/tests/profile.rs` covers round-tripping, stale and mismatched profiles, and
byte-identical output with and without a profile. Review additionally trained a
profile on one input and ran the resulting binary on another (below).

## Limits

The profile records sites, not call contexts, so a site that is monomorphic in
one caller and polymorphic in another is recorded as polymorphic. A profile is
only as representative as the run that produced it; the guard in
[guarded specialization](2026-09-15-guarded-specialization.md) is what makes an
unrepresentative profile a performance event rather than a correctness one.
