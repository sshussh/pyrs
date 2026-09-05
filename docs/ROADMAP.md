# PyRs roadmap to 1.0

PyRs has a substantial native compiler and runtime, but it is still a
statically typed Python subset. Version **0.85.0** ships container equality
for class elements. The next milestone is **0.86.0**; reaching a particular
minor version does not establish 1.0 readiness.

## State reviewed on 2026-09-05

The starting revision was `5b009a5`, with all seven workspace crates and
the language label at 0.82.0. The existing implementation includes
arbitrary-precision integers, typed containers, modules and packages,
closures, generators, closed-world classes with virtual methods, catchable
exceptions, and a default nonmoving mark–sweep collector. RiskSim exercises
a real multi-module command-line workload.

The baseline validation passed:

| Check | Result before the 0.83 changes |
|-------|------------------------------|
| `make doctor` | All required tools available |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 980 passed: 408 unit and 572 integration tests; none failed or ignored |
| `make examples` | All 13 example entry points matched CPython under the existing comparison script |
| Release build | Passed |

This run used Rust 1.96.1, LLVM 22.1.8, CPython 3.14.7, and GCC 16.2.1.
The configured GitHub CI environment uses Ubuntu 24.04, LLVM 18, and
CPython 3.14. The local results do not establish clean-machine release
portability or complete Python compatibility.

After the 0.83 changes, the same host reported:

| Check | Result after 0.83 |
|-------|-------------------|
| `make doctor` | All required tools available |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 996 passed: 408 unit and 588 integration tests (16 new); none failed or ignored |
| `make examples` | All 13 example entry points matched CPython |
| `pyrs --version` | `PyRs 0.83.0` |

After 0.84 on the same host:

| Check | Result after 0.84 |
|-------|-------------------|
| `make doctor` | All required tools available |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | Passed; 14 new iterator-exception/comprehension tests |
| `make examples` | All 13 example entry points matched CPython |
| `pyrs --version` | `PyRs 0.84.0` |

After 0.85 on the same host:

| Check | Result after 0.85 |
|-------|-------------------|
| `make doctor` | All required tools available |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1028 passed: 412 unit and 616 integration (4 new semantic + 12 new container-class-eq); none failed or ignored |
| `make examples` | All 13 example entry points matched CPython |
| `pyrs --version` | `PyRs 0.85.0` |

Details live in the [0.83 implementation checklist](superpowers/plans/2026-09-05-comparison-protocols-0.83.md),
the [0.84 implementation checklist](superpowers/plans/2026-09-05-iterator-exceptions-0.84.md),
and the [0.85 implementation checklist](superpowers/plans/2026-09-05-container-class-eq-0.85.md).

## 0.83.0: comparison and membership correctness

Recent class protocols exposed observable semantic gaps: reflected
comparisons evaluated the right operand first, class membership evaluated
the container before the needle, and `!=` ignored explicit `__ne__`
methods. These can change output, mutate the wrong state, or raise the
wrong exception in otherwise supported programs.

The milestone contract is:

- `!=` selects a receiver's direct or inherited `__ne__` when present;
  it uses that receiver's negated `__eq__` only when `__ne__` is absent.
  Existing virtual dispatch, reflection, and statically known right
  subclass priority apply.
- Comparison operands are evaluated once, left to right, before invoking
  the selected method. Class `needle in container` and `not in` evaluate
  the needle first. Comparison chains preserve single evaluation of their
  shared operand and short-circuit behavior.
- Method results are still converted to booleans through truthiness.
  Existing identity fallback and method argument type checks remain.

This is semantic lowering over the existing typed IR. It does not add a
runtime ABI or standard library module. Acceptance requires differential
tests for direct, inherited, virtual, reflected, and subclass cases;
side effects and exceptions; chained comparisons; invalid method calls;
and O0/O2/O3, followed by the full local gate.

`NotImplemented` fallback and choosing comparison slots from runtime
operand types remain unsupported. Virtual overrides of an already selected
method continue to work; that does not make slot selection fully dynamic.

## 0.84.0: iterator exception boundaries and shared iterables

User-iterator `for` loops used to catch `StopIteration` around `__next__`
and the loop body, so a body `raise StopIteration(...)` looked like
normal exhaustion and could run `else`. Comprehensions only accepted
range/list/str, while `for` already accepted tuple, dict keys, set, file,
generator, and class `__iter__`.

The milestone contract is:

- User-iterator `for` and comprehensions catch `StopIteration` only around
  `__next__`. Body and target-binding exceptions propagate. Ordinary
  `__next__` exhaustion still runs loop `else` and does not leak into an
  enclosing `except StopIteration`.
- List, set, and dict comprehensions accept the same iterables as `for`.
- Generator `for` / comprehension exhaustion remains Optional None
  (documented subset). `any` / `all` / `enumerate` / `zip` / `reversed`
  are unchanged.

This is semantic lowering over existing `Try` / `While` IR. Acceptance
requires differential tests for body `StopIteration`, exhaustion/`else`,
non-StopIteration from `__next__`, break/continue/return/finally, nested
loops, virtual/`__iter__` iterator classes, and comprehension coverage of
tuple/dict/set/file/generator/user-iter, plus the full local gate.

## 0.85.0: container class equality

`list[C] == list[C]` compared elements by pointer identity even when `C`
defined `__eq__`, so `[P(1)] == [P(1)]` was false. The same identity
check fed `!=`, `in`, `index`, `count`, and `remove`, and tuple `==`.

The milestone contract is:

- List `==` compares lengths, then `left[i] == right[i]` through the
  existing class protocol (virtual, inherited, reflected, subclass-first,
  identity fallback). `!=` negates that result; it does not call `__ne__`.
- `in` / `index` / `count` / `remove` use `item == needle`. `index` keeps
  CPython slice bounds. The `in` needle is still evaluated first.
- Nested `list[list[C]]` uses the same recursive `==`. Tuple `==` / `!=`
  with a class (or nested) element is pairwise `==` with short-circuit.
- Same-object container elements skip `__eq__` (`RichCompareBool`);
  scalar `a == a` still calls it. Homogeneous `tuple[C, …]` `in` /
  `index` / `count` use class `==`. Mixed-tuple membership stays slot
  identity.

This is semantic lowering over existing `Block` / `While` / `Index` IR.
Acceptance requires differential tests for value equality, identity
fallback, inheritance/virtual overrides, `!=` vs `__ne__`, membership,
index bounds, remove, nested lists, tuple pairs, side effects, and
exceptions, plus O0/O2/O3 and the full local gate.

## Confirmed remaining gaps

These findings remain open after the 0.83 scope. Passing the baseline did
not cover them.

| Area | Current gap | Required follow-up |
|------|-------------|--------------------|
| User iterator exception handling | Closed in 0.84: `StopIteration` is caught only around `__next__` | Keep generator `for` on Optional None unless that subset is deliberately changed |
| Iterable coverage | Closed in 0.84 for `for` and list/set/dict comprehensions | `any` / `all` / `enumerate` / `zip` / `reversed` still use a narrower set |
| Rich comparisons | Closed in 0.85 for `list[C]` `==`/`!=`/`in`/`index`/`count`/`remove`, tuple `==`/`!=`, and homogeneous `tuple[C, …]` `in`/`index`/`count`. Still: no `NotImplemented` fallback; slot choice uses static types; results are bool-coerced; mixed-tuple membership uses identity | Complete or explicitly bound the protocol contract before claiming general object compatibility |
| Text | String length/index/slice use UTF-8 bytes, while `ord`/`chr` use Unicode code points; many methods use ASCII case and whitespace rules | Establish a consistent Unicode string contract and test multibyte, combining, whitespace, and case behavior |
| Numeric and binding semantics | int/float comparison loses precision beyond 2^53; some possibly unbound scalar locals read default values; dynamic negative integer powers trap | Fix silent differences in the supported contract or narrow that contract explicitly with diagnostics |
| Generators and dynamism | `yield from` does not forward `send`/`throw`; generator exhaustion uses Optional None in several paths; `Any`, class attributes, inheritance, and class values remain restricted | Stabilize the intended subset and reject unsupported paths clearly; broader CPython dynamism is separate work |
| Memory confidence | Conservative roots can retain garbage; abandoned generators do not run user finalizers; collection statistics exclude native/allocator overhead | Continue stress and exception-path tests and measure process memory on sustained workloads; see [GC.md](GC.md) |
| Example parity gate | `make examples` compares shell command-substitution output without checking both exit statuses and strips trailing newlines | Compare actual bytes, check process success, and test the gate's failure paths |
| Failure artifacts | CI uploads `target/tmp`, but integration tests use system temporary directories and delete them on drop | Preserve failing inputs/artifacts at the location CI uploads |
| Release delivery | The compiler links system LLVM dynamically; archives have no clean-environment dependency check; tag/crate/CLI agreement is unchecked; manually selected release tags do not control archive version naming | State supported hosts/dependencies, verify extracted archives on clean hosts, and enforce consistent version/tag metadata |
| Documentation checks | Hygiene verifies required files and basic workflow shape, without checking links or version agreement | Automate these checks; 0.83 repairs the observed active-document links but adds no new hygiene gate |

The 0.84 body-`StopIteration` distinction is: in
`try: for x in Counter(1): raise StopIteration("body")`, an enclosing
`except StopIteration` receives `"body"` and loop `else` does not run.
Ordinary `__next__` exhaustion still runs `else`.

## Proposed milestones and release gates

The 0.83, 0.84, and 0.85 scopes are implemented by their linked checklists.
The following phases are proposed follow-up work; later version numbers
should be assigned when each scope is reviewed.

| Phase | Focus | Exit evidence |
|-------|-------|---------------|
| **0.86.0, proposed next** | Unicode semantics, numeric/binding correctness, and remaining protocol bounds (`NotImplemented`, mixed-tuple membership) | A supported-feature matrix links behavior to differential tests; silent wrong behavior is removed from supported paths; residual limitations are explicit |
| **Reliable validation and delivery** | Strict parity checks, useful failure artifacts, version/link checks, and release dependency handling | Deliberately broken programs fail the parity gate; failure artifacts are retained; an extracted archive builds and runs a sample on each declared clean host |
| **Sustained workload validation** | GC, resource handling, compile cost, and library-shaped programs | Repeatable memory/stress results and benchmark baselines; long-running programs with containers, cycles, exceptions, closures, and generators stay correct; pure-PyRs library needs are documented |
| **1.0 release candidate** | Stable supported contract and reproducible release | Full gates pass for the exact candidate; no known silent correctness defects in its supported contract; installation, diagnostics, compatibility limits, and upgrade expectations are documented |

The nonmoving collector can support 1.0 if it meets the correctness and
workload gates. A moving generational/Immix collector remains a later
design requiring precise roots; its absence alone is not a reason to
replace a validated collector. Likewise, 1.0 should mean a reliable,
clearly specified product for real workloads, not an unsupported claim
that every CPython program works.

Keep the [primitives-first policy](PRIMITIVES.md): grow native operations
where representation, runtime contracts, or measured hot paths require
them, and add higher-level libraries in PyRs only when the language can
host them. Do not let new surface area bypass the correctness and release
gates above.
