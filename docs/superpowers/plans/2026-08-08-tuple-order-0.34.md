# lexicographic tuple order / min / sort (0.34) — shipped

**Theme:** CPython-style tuple ordering and kit that depends on it.

**Ship:** `0.34.0`

## In scope (done)

1. **`pyrs_tuple_cmp`** — lexicographic element order (int/float/bool/str/nested tuple)
2. **Operators** `tuple < <= > >=` when both sides share an orderable tuple type
3. **`min`/`max`** multi-arg and list form over homogeneous orderable tuples
4. **`sorted` / `list.sort`** without `key=` for `list[tuple[…]]` (via list_sort tag 5)

## Residuals

- Keyed sort materializes a never-freed auxiliary keys list of length `n`
- Class `__len__` via bare `key=len`
- List lexicographic ordering (`[1,2] < [1,3]`) — **done in 0.35**

## Out of scope (unchanged)

- GC, multi-inheritance, open `__dict__`, new stdlib modules, full PEP 380
