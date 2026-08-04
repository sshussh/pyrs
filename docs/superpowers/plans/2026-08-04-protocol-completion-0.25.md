# Protocol Completion (0.25) — shipped

**Theme:** Closed-world protocol completion for library-shaped programs.

**Ship:** `0.25.0`

## In scope (done)

1. **Class `with` / `__exit__` suppress**
   - Success: `__exit__(None, None, None)`
   - Exception: `__exit__(None, exc, None)`; truthy return suppresses
   - Residual: type and traceback args always `None` (no first-class exception types / traceback)
   - IR: `Stmt::RaiseExc` + runtime `pyrs_raise_exc`

2. **Builtin `next(it)` / `next(it, default)`**
   - User iterators (`__next__`) and generators
   - Exhaustion without default → `StopIteration`

3. **User-class `__contains__`** for `in` / `not in`

## Deferred (still residual)

- `sorted` / `min` / `max` with `key=` (and `reverse=`)
- Full CPython `__exit__` type/traceback objects
- Multi-item `with`
- GC / multi-inherit / new stdlib modules
