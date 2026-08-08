# sorted / list.sort reverse= (0.28) — shipped

**Theme:** Stable `reverse=` for sorting builtins/methods.

**Ship:** `0.28.0`

## In scope (done)

1. **`sorted(xs, reverse=bool)`** and **`list.sort(reverse=bool)`**
   - Works with or without monomorphic `key=`.
   - CPython-stable reverse-sort-reverse (reverse list → ascending sort → reverse).
   - `reverse=` must be `bool` (`True`/`False` or runtime bool local); truthy int residual.
   - Const folds: `True` always reverses, `False` is a no-op.

2. **min/max** still reject `reverse=` as unexpected (CPython has no such kwarg).

## Residuals

- Multi-arg `min(a, b, c[, key=…])` — **done in 0.29**
- `min`/`max` `default=`
- Builtins as bare key values (`key=len`)
- Keyed sort materializes a never-freed auxiliary keys list of length `n`
- Truthy non-bool `reverse=` (CPython accepts; PyRs requires `bool`)

## Out of scope (unchanged)

- GC, multi-inheritance, open `__dict__`, new stdlib modules, full PEP 380
