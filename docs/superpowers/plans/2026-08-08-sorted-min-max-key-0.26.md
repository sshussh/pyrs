# sorted / min / max with `key=` (0.26) — shipped

**Theme:** Keyword `key=` for ordering builtins via semantic desugar.

**Ship:** `0.26.0`

## In scope (done)

1. **`sorted(xs, key=f)`**
   - Keyword gate extended for `sorted` / `min` / `max` (alongside `enumerate(start=)`).
   - `key=` must be monomorphic `T → K` with `K` sortable (`int|float|bool|str`).
   - Callables: free functions (direct Call), nested functions / lambdas (CallClosure).
   - Desugar: copy list → evaluate keys once → stable insertion sort on key+value pairs.
   - `reverse=` residual with clear diagnostic.

2. **`min(xs, key=f)` / `max(xs, key=f)`**
   - Iterable form only; linear scan comparing `f(x)`.
   - Empty list → `ValueError: … iterable argument is empty` (CPython wording).
   - Two-arg numeric form rejects `key=` (use iterable form).

## Residuals

- `reverse=` on `sorted` only (`min`/`max` treat it as unexpected keyword, like CPython)
- `list.sort(key=)` / `list.sort(reverse=)` (named residual on method kwargs)
- Multi-arg `min(a, b, c[, key=…])`
- `min`/`max` `default=`
- Builtins as bare key values (`key=len`) — guided residual; use wrapper lambda/function
- Keyed `sorted` materializes a never-freed auxiliary keys list of length `n`

## Out of scope (unchanged)

- GC, multi-inheritance, open `__dict__`, new stdlib modules, full PEP 380
