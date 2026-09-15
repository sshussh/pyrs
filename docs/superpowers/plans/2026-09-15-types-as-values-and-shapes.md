# Types as values, and pay-per-use shapes

**Status: implemented in 0.150.0; `type(x) is C` added in 0.152.0.** Item F of
[the remaining dynamism brief](2026-09-10-remaining-dynamism-work.md).

## Types as values

A class name is a value: `k = C`, `k()`, `type(x)` on an instance,
`isinstance(x, k)`, classes in a dict registry, and printing a type as
`<class '__main__.Name'>`. `ir::Ty::Type` lowers to a type-id word.

Review found `type(x) is C` refused — the usual spelling of an exact-type
check — although `type(x) == C` worked. A type object is one per class, so
identity is the same question as equality and now lowers to the same
comparison. Exact-type semantics hold: a `Dog` is not `is` `Animal`, which is
what distinguishes it from `isinstance`.

## Shapes

`getattr`, `setattr` and `hasattr` on a class instance write through an
overflow dict, added **only** to classes the program reflects and their
subclasses. A known field name still uses its layout slot.

Pay-per-use was checked in the emitted IR, not asserted. In a program where
`Q` is reflected and `P` is not, `P` stays `{ i64, i64 }` and `Q` becomes
`{ i64, ptr, i64 }`; the pointer does not leak onto unreflected classes.

The overflow pointer is a new reference inside an object, so it is a collector
question. Review stored strings reachable **only** through overflow dicts on
300 objects, churned 20,000 allocations, and read them back: byte-identical
with CPython at -O0 and -O2, plainly and under `PYRS_GC_STRESS=1`. Instance
fields are scanned from offset 8, which covers the pointer.

## Limits

`type()` of a non-class value names itself as unimplemented. Attribute access on
a receiver that is not a class instance names the gap. Instance `repr` prints
`<A object>` where CPython prints `<__main__.A object at 0x…>`.
