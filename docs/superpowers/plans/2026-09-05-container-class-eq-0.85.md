# Container class equality — 0.85.0

## Review and milestone choice

0.84.0 isolated `StopIteration` to `__next__` and shared iterables for
`for` and comprehensions. The highest-leverage remaining silent defect in
the class/container protocol is `list[C]` equality: `==`, `!=`, `in`,
`index`, `count`, and `remove` compare class elements by pointer identity
even when `C` defines `__eq__`. Tuple `==` / `!=` has the same gap.

This milestone desugars those operations to the existing class `==`
protocol (virtual, inherited, reflected, subclass-first, identity
fallback). Semantic lowering over existing `Block` / `While` / `Index` IR;
no runtime ABI change.

Unicode, int/float exactness, unbound scalar locals, `NotImplemented`,
and mixed-tuple membership remain later work.

## Implementation plan

- [x] Detect element types that need class `==` (class, nested list of
      those, tuple containing those).
- [x] `list[C] ==` / `!=` compares items with class `==` (lengths first;
      `!=` negates overall `==`, not element `__ne__`). Bind each list once.
- [x] `in` / `index` / `count` / `remove` use `item == needle` (CPython
      listobject). Same slice bounds as today's `index`. Needle still
      evaluates before the container.
- [x] Tuple `==` / `!=` with a class (or nested) element is pairwise
      `==` with short-circuit. Mixed-tuple `in` / `index` / `count` stay
      on slot identity.
- [x] Differential tests: direct/inherited/virtual `__eq__`, identity
      fallback, nested lists, `!=` vs `__ne__`, membership, index bounds,
      remove, tuple equality, side effects, exceptions, O0/O2/O3.
- [x] Update language documentation, crate/lockfile versions, and the
      roadmap. Next proposed milestone is 0.86.0.
- [x] Pass `make doctor` and the `make ci` equivalent; record actual
      validation results.

## Boundaries

`NotImplemented` fallthrough and runtime slot selection stay unsupported.
List/tuple ordering of class elements is still a compile error. Generator
exhaustion, Unicode, numeric exactness, and unbound scalars are separate.

## Acceptance

`[P(1)] == [P(1)]` is true when `P.__eq__` compares values. `list !=`
follows element `==`, not `__ne__`. `in` / `index` / `count` / `remove`
use the same protocol. Tuple pairs with class elements compare by `==`.
Documentation and `pyrs --version` report 0.85.0 without implying 1.0
readiness.

## Validation (2026-09-05)

Toolchain: Rust 1.96.1, LLVM 22.1.8, CPython 3.14.7, GCC 16.2.1.

| Check | Result |
|-------|--------|
| `make doctor` | All required tools available |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test -p pyrs --test container_class_eq` | 12 passed |
| `cargo test --workspace` | Passed (exit 0). 412 unit + 616 integration; 4 new semantic IR tests and 12 new differential tests |
| `make examples` | All 13 example entry points matched CPython |
| Release `pyrs --version` | `PyRs 0.85.0` |
