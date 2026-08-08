# min / max default= (0.30) — shipped

**Theme:** Iterable-only `default=` for `min` / `max` (CPython empty-iterable path).

**Ship:** `0.30.0`

## In scope (done)

1. **`min(xs, default=d)` / `max(xs, default=d)`** (no key)
   - Empty list → return `d` (no ValueError).
   - Non-empty → same numeric MinList/MaxList path as before.
   - Result type is `join(elem, default)` (e.g. `default=None` → Optional).

2. **`min(xs, key=f, default=d)` / `max(..., key=f, default=d)`**
   - Empty → `d` without calling `key`.
   - Non-empty → existing monomorphic key scan.

3. **Rejected like CPython**
   - Multi-arg `min(a, b, default=…)` → `Cannot specify a default for min() with multiple positional arguments`.
   - `sorted` / `list.sort` → unexpected keyword `default`.
   - `reverse=` on min/max still unexpected.

## Residuals

- Builtins as bare key values (`key=len`) — **done in 0.31** (`len`/`abs`/casts)
- Keyed sort materializes a never-freed auxiliary keys list of length `n`
- Truthy non-bool `reverse=` on `sorted` / `list.sort`
- Multi-arg `min`/`max` without `key=` still numeric-only (no str/tuple lexicographic)

## Out of scope (unchanged)

- GC, multi-inheritance, open `__dict__`, new stdlib modules, full PEP 380
