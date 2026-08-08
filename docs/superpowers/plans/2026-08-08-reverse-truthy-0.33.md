# truthy reverse= (0.33) — shipped

**Theme:** CPython truthiness for `sorted` / `list.sort` `reverse=`.

**Ship:** `0.33.0`

## In scope (done)

1. **`reverse=` accepts any truthy value** (same rules as `if` conditions)
   - Const-fold: `True`/`1`/nonzero int/float/nonempty str → always reverse
   - Const-fold: `False`/`0`/`0.0`/`None`/empty str → never reverse
   - Runtime: `ToBool` into `ReverseMode::Cond`

2. **Unchanged**
   - Stable reverse-sort-reverse algorithm
   - Works with or without monomorphic `key=`
   - `min`/`max` still reject `reverse=` as unexpected

## Residuals

- Keyed sort materializes a never-freed auxiliary keys list of length `n`
- Multi-arg / list `min`/`max` for tuples (lexicographic)
- Class `__len__` via bare `key=len`

## Out of scope (unchanged)

- GC, multi-inheritance, open `__dict__`, new stdlib modules, full PEP 380
