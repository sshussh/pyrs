# bare key=len for class __len__ (0.36) — shipped

**Theme:** CPython-style `key=len` on user classes that define `__len__`.

**Ship:** `0.36.0`

## In scope (done)

1. **`key=len` on `list[C]`** when `C` defines `__len__(self) -> int`
   - Resolve accepts `Class` with `resolve_method(..., "__len__")`
   - Call site uses virtual `lower_instance_method_call` (same as builtin `len(obj)`)
   - Works for `sorted` / `list.sort` / `min` / `max`

2. **Unchanged**
   - Containers still use `ExprKind::Len`
   - Classes without `__len__` still error at compile time

## Residuals

- Keyed sort materializes a never-freed auxiliary keys list of length `n` (GC)
- Other bare builtins as keys (beyond len/abs/casts) still need wrappers
- Dict/set ordering still unsupported (like CPython)

## Out of scope (unchanged)

- GC, multi-inheritance, open `__dict__`, new stdlib modules, full PEP 380
