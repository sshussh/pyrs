# Class comparison and membership correctness — 0.83.0 — shipped

## Review and milestone choice

The clean starting revision is `5b009a5` (language and all seven crates
at 0.82.0). PyRs already has modules, closures, generators, closed-world
classes, container protocols, arbitrary-precision integers, a nonmoving
collector, and a real CLI example (RiskSim). CI covers formatting, clippy,
workspace tests, example parity, and several LLVM optimization levels.

The recent comparison features leave two observable gaps: reflected
comparisons bind the right operand before the left, and class membership
binds the container before the needle. Side effects and exceptions can
therefore change program behavior. Also, `!=` always negates `__eq__`, even
when the class explicitly defines `__ne__`.

This milestone closes those related protocol gaps before expanding the
standard library or the object model. It uses semantic lowering and the
existing typed method-call/block IR; no runtime ABI change is needed.

## Implementation plan

- [x] Resolve `!=` to inherited or direct `__ne__`, retaining virtual
  dispatch, reflected dispatch, and statically known subclass priority.
  Only fall back to negated `__eq__` when that receiver has no `__ne__`.
- [x] Bind comparison operands once in source order before selecting the
  method receiver. Preserve identity fallback and current type diagnostics.
- [x] Bind class membership needle before container, once each, preserving
  the method result's truth conversion and `not in` inversion.
- [x] Compare runtime behavior against CPython for direct, reflected,
  inherited, and virtual inequality; independent equality/inequality;
  operand side effects, mutations, exceptions, and chained comparisons.
  Exercise the changed paths at O0/O2/O3 and check invalid method calls.
- [x] Update language documentation, all crate/lockfile versions, and the
  roadmap with explicit remaining readiness work.
- [x] Pass `make doctor` and the `make ci` equivalent; record actual
  validation results.

## Boundaries

Comparison results remain booleans in this typed subset. `NotImplemented`
fallthrough, runtime-type-based changes to comparison-slot selection,
new standard library modules, broader iterable support, and release
packaging changes are separate work. Existing virtual overrides of the
selected method continue to work. A non-class/class comparison without a
compatible equality/inequality protocol remains a compile error.

## Acceptance

The new differential regressions and existing suite pass; a `__ne__` body
can disagree with `__eq__`; reflection never reorders operand effects;
a left operand exception prevents the right operand from running; a
comparison chain evaluates its middle operand once and short-circuits.
Documentation and `pyrs --version` report 0.83.0 without implying 1.0
readiness.

## Validation (2026-09-05)

Toolchain: Rust 1.96.1, LLVM 22.1.8, CPython 3.14.7, GCC 16.2.1.

| Check | Result |
|-------|--------|
| `make doctor` | All required tools available |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test -p pyrs --test class_inequality --test protocol_order` | 16 passed (12 inequality + 4 evaluation-order) |
| `cargo test --workspace` | e2e 572 passed; class_inequality 12; protocol_order 4; other crates' unit tests passed. One semantic unit test (`chained_comparison_lowers_to_let_and`) failed because chains now bind the first operand in an outer `Let`; updated and `cargo test -p semantic --lib` then passed 248/248. Combined: 996 tests, none ignored. |
| `make examples` | All 13 example entry points matched CPython |
| Release `pyrs --version` | `PyRs 0.83.0` |

`make ci` is `fmt-check` + `clippy` + `cargo test --workspace` + `make examples`. Those pieces passed after the unit-test IR assertion update; the full `make ci` target was not re-run as a single recipe after that one-line test fix.
