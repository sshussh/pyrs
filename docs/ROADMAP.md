# PyRs roadmap to 1.0

PyRs has a substantial native compiler and runtime, but it is still a
statically typed Python subset. Version **0.83.0** ships class comparison
and membership correctness: independent `__ne__` methods and source-order
operand evaluation. The next milestone is **0.84.0**; reaching a particular
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

Details live in the [0.83 implementation checklist](superpowers/plans/2026-09-05-comparison-protocols-0.83.md).

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

## Confirmed remaining gaps

These findings remain open after the 0.83 scope. Passing the baseline did
not cover them.

| Area | Current gap | Required follow-up |
|------|-------------|--------------------|
| User iterator exception handling | A `for` loop over a class catches `StopIteration` from the whole loop body, so a body exception can be mistaken for normal exhaustion and run `else` | Catch exhaustion only around `__next__`; propagate body and target-binding exceptions |
| Iterable coverage | Comprehension lowering accepts range/list/str, while ordinary `for` supports additional tuple/dict/set/generator/user-iterator paths | Define a common supported iterable contract and add parity tests for each consumer |
| Rich comparisons | No `NotImplemented` fallback; slot choice uses static types; results are bool-coerced; `list[C]` equality uses element identity even when `C` defines `__eq__` | Complete or explicitly bound the protocol contract before claiming general object compatibility |
| Text | String length/index/slice use UTF-8 bytes, while `ord`/`chr` use Unicode code points; many methods use ASCII case and whitespace rules | Establish a consistent Unicode string contract and test multibyte, combining, whitespace, and case behavior |
| Numeric and binding semantics | int/float comparison loses precision beyond 2^53; some possibly unbound scalar locals read default values; dynamic negative integer powers trap | Fix silent differences in the supported contract or narrow that contract explicitly with diagnostics |
| Generators and dynamism | `yield from` does not forward `send`/`throw`; generator exhaustion uses Optional None in several paths; `Any`, class attributes, inheritance, and class values remain restricted | Stabilize the intended subset and reject unsupported paths clearly; broader CPython dynamism is separate work |
| Memory confidence | Conservative roots can retain garbage; abandoned generators do not run user finalizers; collection statistics exclude native/allocator overhead | Continue stress and exception-path tests and measure process memory on sustained workloads; see [GC.md](GC.md) |
| Example parity gate | `make examples` compares shell command-substitution output without checking both exit statuses and strips trailing newlines | Compare actual bytes, check process success, and test the gate's failure paths |
| Failure artifacts | CI uploads `target/tmp`, but integration tests use system temporary directories and delete them on drop | Preserve failing inputs/artifacts at the location CI uploads |
| Release delivery | The compiler links system LLVM dynamically; archives have no clean-environment dependency check; tag/crate/CLI agreement is unchecked; manually selected release tags do not control archive version naming | State supported hosts/dependencies, verify extracted archives on clean hosts, and enforce consistent version/tag metadata |
| Documentation checks | Hygiene verifies required files and basic workflow shape, without checking links or version agreement | Automate these checks; 0.83 repairs the observed active-document links but adds no new hygiene gate |

The iterator bug has a minimal behavioral distinction: in
`try: for x in Counter(): raise StopIteration("body")`, an enclosing
`except StopIteration` should receive `"body"`. With a loop `else`, the
reviewed implementation instead prints the `else` branch as though
`Counter.__next__` had exhausted normally. This is a correctness defect,
not a deliberate subset rule.

## Proposed milestones and release gates

The 0.83 scope is implemented by its linked checklist.
The following phases are proposed follow-up work; later version numbers
should be assigned when each scope is reviewed.

| Phase | Focus | Exit evidence |
|-------|-------|---------------|
| **0.84.0, proposed next** | Iterator exception boundaries and a documented iterable support matrix | `StopIteration` from body/target binding propagates; ordinary exhaustion alone runs loop `else`; break/continue/return/finally and nested-loop differential tests pass |
| **Core compatibility** | Unicode semantics, numeric/binding correctness, and consistent class/container protocols | A supported-feature matrix links behavior to differential tests; silent wrong behavior is removed from supported paths; residual limitations are explicit |
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
