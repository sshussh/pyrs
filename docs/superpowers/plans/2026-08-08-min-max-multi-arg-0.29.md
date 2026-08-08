# multi-arg min / max (0.29) — shipped

**Theme:** CPython multi-arg form for `min` / `max` with optional monomorphic `key=`.

**Ship:** `0.29.0`

## In scope (done)

1. **`min(a, b[, c…])` / `max(...)` without `key=`**
   - Fold binary `Min`/`Max` IR with `unify_numeric` (`bool` → `int` → `float`).
   - Classic two-arg path unchanged in surface (same IR fold).

2. **`min(a, b[, c…], key=f)` / `max(..., key=f)`**
   - Linear scan comparing monomorphic `key=` (same callable surface as iterable form).
   - Positionals must share one storage type; mixed types → compile error.
   - Equal keys keep the first argument (strict `<` / `>`).

3. **Still residual on min/max**
   - `default=` not supported yet.
   - `reverse=` unexpected (like CPython).
   - Bare builtins as `key=` values (`key=len`) need a wrapper.

## Residuals

- `min`/`max` `default=` — **done in 0.30**
- Builtins as bare key values (`key=len`)
- Keyed sort materializes a never-freed auxiliary keys list of length `n`
- Truthy non-bool `reverse=` on `sorted` / `list.sort`
- Multi-arg `min`/`max` without `key=` still numeric-only (no str/tuple lexicographic)

## Out of scope (unchanged)

- GC, multi-inheritance, open `__dict__`, new stdlib modules, full PEP 380
