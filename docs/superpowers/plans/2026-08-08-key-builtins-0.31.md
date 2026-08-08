# bare builtins as key= (0.31) — shipped

**Theme:** CPython-style `key=len` / `key=abs` / cast names without wrapper lambdas.

**Ship:** `0.31.0`

## In scope (done)

1. **`SortKey::Builtin`** for monomorphic key= on `sorted` / `list.sort` / `min` / `max`
   - `len` → `int` for str / list / tuple / dict / set elements
   - `abs` → `int`/`float` for bool/int/float elements
   - casts `int` / `float` / `bool` / `str` when `lower_cast` accepts the element type
   - Applied as IR ops (`Len`, `Abs`, casts) — not first-class values

2. **Still residual**
   - Other builtins (`sum`, `min`, …) need a wrapper
   - Class instances with `__len__` as `key=len` need a method wrapper
   - Truthy non-bool `reverse=`

## Residuals

- Keyed sort materializes a never-freed auxiliary keys list of length `n`
- Truthy non-bool `reverse=` on `sorted` / `list.sort`
- Multi-arg / list `min`/`max` str lexicographic — **done in 0.32** (tuple still residual)
- Class `__len__` via bare `key=len`

## Out of scope (unchanged)

- GC, multi-inheritance, open `__dict__`, new stdlib modules, full PEP 380
