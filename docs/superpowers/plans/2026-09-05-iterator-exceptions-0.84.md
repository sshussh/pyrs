# Iterator exception boundaries and shared iterables — 0.84.0

## Review and milestone choice

0.83.0 closed class comparison and membership correctness. The remaining
highest-leverage defect was user-iterator `for` loops catching
`StopIteration` around the whole loop body, plus comprehensions accepting
only range/list/str.

This milestone isolates exhaustion to `__next__` and gives list/set/dict
comprehensions the same iterable set as `for`. Semantic lowering over
existing `Try` / `While` IR; no runtime ABI change.

## Implementation plan

- [x] Catch `StopIteration` only around user-iterator `__next__`; bind
      and body run in `Try` `orelse` so they propagate.
- [x] Extend comprehension lowering to tuple, dict keys, set, file,
      generator, and class `__iter__`, reusing the same StopIteration
      isolation for user iterators.
- [x] Differential tests: body `StopIteration` vs `else`, exhaustion
      without leak, non-StopIteration from `__next__`, break/continue/
      return/finally, nested loops, virtual iterator classes, unpack
      bind errors, comprehension matrix, file iteration, compile error
      for a non-iterable class. StopIteration cases at O0/O2/O3.
- [x] Update language documentation, crate/lockfile versions, and the
      roadmap. Next proposed milestone is 0.85.0.
- [x] Pass `make doctor` and the `make ci` equivalent; record actual
      validation results.

## Boundaries

Generator `for` / comprehension exhaustion stays Optional None.
`any` / `all` / `enumerate` / `zip` / `reversed` stay on their existing
iterable set. No Unicode, `NotImplemented`, `yield from` send/throw, or
release-packaging work.

## Acceptance

Body `StopIteration` reaches an enclosing handler and skips loop `else`.
Ordinary `__next__` exhaustion runs `else`. Comprehensions accept the same
iterables as `for`. Documentation and `pyrs --version` report 0.84.0
without implying 1.0 readiness.

## Validation (2026-09-05)

Toolchain: Rust 1.96.1, LLVM 22.1.8, CPython 3.14.7, GCC 16.2.1.

| Check | Result |
|-------|--------|
| `make doctor` | All required tools available |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test -p pyrs --test iterator_exceptions` | 14 passed |
| `cargo test --workspace` | Passed (exit 0). One e2e (`while_local_optional_reassign_none_terminates`) hit a 5s compile/run timeout under a loaded first suite run and passed in isolation and on the second full suite. |
| `make examples` | All 13 example entry points matched CPython |
| Release `pyrs --version` | `PyRs 0.84.0` |
