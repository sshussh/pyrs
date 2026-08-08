# list.sort(key=) (0.27) — shipped

**Theme:** In-place `list.sort(key=)` reusing the v0.26 monomorphic key= desugar.

**Ship:** `0.27.0`

## In scope (done)

1. **`list.sort(key=f)`** (statement position)
   - Same monomorphic `T → K` callable surface as `sorted` / `min` / `max`.
   - Desugar: bind receiver once → evaluate keys once → stable insertion sort
     on value+key lists (shared helper with `sorted`).
   - Expr position still errors (`returns None`).
   - `reverse=` residual with `list.sort() keyword argument 'reverse=' is not supported yet`.

## Residuals

- `reverse=` on `sorted` and `list.sort` — **done in 0.28**
- Multi-arg `min(a, b, c[, key=…])`
- `min`/`max` `default=`
- Builtins as bare key values (`key=len`)
- Keyed sort materializes a never-freed auxiliary keys list of length `n`

## Out of scope (unchanged)

- GC, multi-inheritance, open `__dict__`, new stdlib modules, full PEP 380
