# PyRs roadmap to 1.0

Status: implementation in progress. **Not a 1.0 release declaration.**

PyRs has a substantial native compiler and runtime, but it is still a
statically typed Python subset. Version **0.88.0** makes the validation
gates trustworthy. The next milestone is **0.89.0**; reaching a particular
minor version does not establish 1.0 readiness, and no stable release or
tag has been created.

This is the single roadmap. It absorbed the separate `ROADMAP-1.0.md`
delivery plan in 0.88, because the two documents had begun to contradict
each other: one listed exact int/float comparison as an open gap while the
other recorded it as fixed, and they disagreed about whether to bump minor
versions per milestone. Milestone history and near-term gaps are below,
followed by the product contract, workstreams and release gates.

| Section | Purpose |
|---------|---------|
| [Measured defects](#measured-defects-2026-09-05) | Probed against CPython; the outstanding list for release gate 2 |
| [Product contract](#product-contract) | What 1.0 is meant to mean |
| [Workstreams](#workstreams) | Dependency-ordered work, with what is done |
| [Release gates](#10-release-gates) | Conditions for shipping 1.0 |

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
| `cargo test --workspace` | 1034 passed: 412 unit and 622 integration; none failed or ignored |
| `make examples` | All 13 example entry points matched CPython |
| `pyrs --version` | `PyRs 0.85.0` |

The 0.85 row previously reported 1028 tests. A re-run on the merged tree
counted 1034 (50 `#[test]` functions across the four new integration
files, plus 4 new semantic unit tests). The corrected figure is above.

After 0.86 on the same host:

| Check | Result after 0.86 |
|-------|-------------------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1051 passed; none failed or ignored |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 12 pass / 6 known_gap; compat 6 pass |
| `compatibility/test_extension.py` | 9 passed, 1 skipped (no NumPy/pandas in CPython 3.14) |
| `pyrs --version` | `PyRs 0.86.0` |

Details live in the [0.83 implementation checklist](superpowers/plans/2026-09-05-comparison-protocols-0.83.md),
the [0.84 implementation checklist](superpowers/plans/2026-09-05-iterator-exceptions-0.84.md),
the [0.85 implementation checklist](superpowers/plans/2026-09-05-container-class-eq-0.85.md),
and the [0.86 implementation checklist](superpowers/plans/2026-09-05-cpython-interop-0.86.md).

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
| Numeric and binding semantics | Closed in 0.86 for mixed int/float comparison (exact, including bigints, NaN, infinities and signed zero) and for conditionally assigned locals including generator suspension. Still open: mixed numeric list literals promote ints to floats; dynamic negative integer powers trap; module globals, deletion and static use-before-assignment diagnostics | Fix silent differences in the supported contract or narrow that contract explicitly with diagnostics |
| Generators and dynamism | `yield from` does not forward `send`/`throw`; generator exhaustion uses Optional None in several paths; `Any`, class attributes, inheritance, and class values remain restricted | Stabilize the intended subset and reject unsupported paths clearly; broader CPython dynamism is separate work |
| Memory confidence | Conservative roots can retain garbage; abandoned generators do not run user finalizers; collection statistics exclude native/allocator overhead | Continue stress and exception-path tests and measure process memory on sustained workloads; see [GC.md](GC.md) |
| Example parity gate | 0.86 makes the recipe fail when either process fails; it still uses command substitution, so trailing newlines are stripped and the comparison is not byte-exact | Compare actual bytes and test the gate's own failure paths |
| Borrowed CPython buffers | Closed in 0.86 for reallocation: growth sites raise `BufferError` rather than freeing exporter- or `PyMem`-owned memory. Direct element stores are still only prevented by the extension frontend's allowlist | Add a store-side check or a distinct immutable buffer type before the bridge is non-experimental |
| Failure artifacts | Closed in 0.88: integration tests build under `CARGO_TARGET_TMPDIR` (`target/tmp`, the path CI uploads) and retain their inputs when the thread is panicking | — |
| Release delivery | The compiler links system LLVM dynamically; archives have no clean-environment dependency check; manually selected release tags do not control archive version naming | State supported hosts/dependencies and verify extracted archives on clean hosts. Version agreement across crates, lockfile, docs and the binary is checked by `make hygiene` as of 0.88; tag-to-version agreement is not |
| Documentation checks | Closed in 0.88: `make hygiene` checks version agreement across 20 sites and resolves every relative documentation link, and its own failure paths are tested | Add tag/version agreement at release time |
| Sanitizers | Closed in 0.88 for reproducibility: `make asan` / `make ubsan` build the adapter, runtime and collector instrumented and run the extension boundary suite. The LLVM-generated kernel object is *not* instrumented, so this is adapter/runtime coverage | Instrument generated code, or state the boundary in release notes |

The 0.84 body-`StopIteration` distinction is: in
`try: for x in Counter(1): raise StopIteration("body")`, an enclosing
`except StopIteration` receives `"body"` and loop `else` does not run.
Ordinary `__next__` exhaustion still runs `else`.

## Measured defects, 2026-09-05

Probed directly against CPython 3.14 rather than taken from these tables.
Every row is a *silent* wrong answer in the supported surface, which is
why correctness rather than new capability sets the near-term order.

| Probe | CPython 3.14 | PyRs | Status |
|-------|--------------|------|--------|
| `len("héllo")` | `5` | `6` | open — Unicode milestone |
| `"héllo"[1]` | `é` | broken byte | open — Unicode milestone |
| `"naïve café".upper()` | `NAÏVE CAFÉ` | `NAïVE CAFé` | open — Unicode milestone |
| `"ß".upper()` | `SS` | `ß` | open — Unicode milestone |
| `len("🐍")` | `1` | `4` | open — Unicode milestone |
| `2 ** 53 + 1 == 9007199254740992.0` | `False` | `False` | **closed in 0.86** |
| `[1, 2.5, 1]` | `[1, 2.5, 1]` | `[1.0, 2.5, 1.0]` | open — value-fidelity milestone |
| `a != b`, `a: Base` holding a `Child` defining `__ne__` | `Child.__ne__` runs | negated `__eq__` | open — value-fidelity milestone |
| `def f(x: "Base")` | accepted | accepted | **closed in 0.87** |

## Product contract

Native execution by default, with an explicit opt-in CPython compatibility
mode. Native executables stay independent of CPython; compatibility mode's
Python dependency must be visible, and it is never an automatic retry after
native execution has begun. The target workload family is scientific/data
Python, including NumPy and pandas.

“A good percentage of Python use cases” is not a measurable claim. Before
declaring 1.0, freeze a named workload corpus with project versions,
commands, inputs, expected outputs, licenses and selection rationale.
Proposed acceptance: at least 100 representative scenarios from
independently maintained projects covering arrays, linear algebra,
reductions, missing values, indexing/broadcasting, dtypes, tabular I/O,
grouping, joins and time series, with at least 80% running unchanged in the
selected mode and native-only coverage reported separately. These are
proposed targets, not measured coverage. Synthetic probes and PyRs examples
are useful regression tests but are not independent projects.

**This corpus is not yet frozen.** Until it is, feature selection past the
milestones below is guesswork, and no native-coverage percentage or 1.0
readiness is claimed.

## Workstreams

Dependency-ordered. Each item needs implementation, differential tests,
documentation and the relevant gates.

### A. Evidence and compiler correctness (first)

- [x] Versioned compatibility probe manifest and runner recording CPython
      version, compiler version, optimization level, compile failures,
      timeouts, streams, exit status and file effects; native and
      compatibility results reported separately.
- [ ] Freeze the independent workload corpus with the owner.
- [x] Exact mixed integer/float comparisons, including bigints, NaN,
      infinities, signed zero and boxed/container equality.
- [ ] Definite assignment for every binding kind. Locals and generator
      suspension are done; module globals, deleted bindings and static
      use-before-assignment diagnostics remain.
- [ ] Preserve Python numeric types across assignments, annotations,
      heterogeneous literals, `min`/`max` and boolean operand selection.
- [ ] Audit evaluation order and once-only evaluation for calls, chained
      comparisons, augmented assignment, decorators, defaults, comprehensions.
- [ ] Audit int/float conversion rounding and overflow, dynamic negative
      powers, division, identity-sensitive membership, recursive equality.
- [ ] Fuzz lexer/parser/semantic and runtime boundaries; user input must
      never panic the compiler.

### B. Invocation and explicit compatibility mode

- [x] Python-style `pyrs script.py args`, `-c`, `-m`, stdin, script argv[0],
      working directory, exit status.
- [x] Explicit whole-program compatibility execution against a selected
      CPython environment, never an automatic retry.
- [ ] Report the selected engine. On Unix the native path `exec`s, so this
      needs deciding before the call rather than after.
- [ ] Define compiled compatibility artifacts: dependency discovery,
      relocatability, deployment layout. A Python launcher is not a native
      compilation and must never be labeled as one.
- [x] Specify and implement the initial numerical interop contract:
      ownership, retained GIL, exception translation, GC roots, guarded
      buffer layouts, blocked callbacks/reentrancy.
- [ ] General mixed execution: guarded specialization, preserved Python
      fallback, native-to-Python calls, callbacks, deployment.
- [ ] Native library initialization/export ABI and dependency manifests.

### C. Python values, typing and protocols

- [ ] Runtime operations for `Any` and mixed containers; call-site inference
      for unannotated functions and lambdas.
- [ ] Dynamic-length heterogeneous tuples; general hash/equality protocol;
      hashable tuple/float/bool/frozenset keys; dict views. Tuple keys also
      unblock `a[i, j]` subscripts, rejected with a specific diagnostic
      since 0.87.
- [ ] Callable metadata, signature binding, positional-only and keyword-only
      rules, `*args`/`**kwargs`, decorator factories, stacked decorators.
- [ ] Class attributes, properties, descriptors, arithmetic and
      reflected/in-place dunders, `NotImplemented`, `__call__`, `__hash__`.
- [ ] Runtime-type comparison slot selection. Slot *choice* is still static,
      so a `Base`-typed variable holding a `Child` that defines `__ne__`
      never reaches it.
- [ ] Decide multiple inheritance/MRO, class decorators/dataclasses, dynamic
      attributes and introspection from corpus requirements.
- [ ] Complete iterator protocol: lazy `range`/`enumerate`/`zip`/`reversed`/
      `map`/`filter`, `iter`/`next` defaults, `StopIteration.value`,
      `yield from` send/throw, generator cleanup.
- [ ] Exception args as tuples, user exception classes, hierarchy, chaining,
      re-raise, traceback locations, multiple context managers.
- [ ] A native array type, which `@` needs; rejected with a specific
      diagnostic since 0.87.

### D. Unicode, bytes and I/O

- [ ] One documented Python-compatible representation and a reproducible
      Unicode data version. `len`/index/slice/search/iteration must agree on
      code points, including astral and combining characters and embedded
      NUL. Specify lone-surrogate and encoding-error behavior.
- [ ] Unicode case transforms, predicates, whitespace, padding, translation
      and formatting. Avoid partial fixes mixing byte and character offsets.
- [ ] `bytes`/`bytearray`/`memoryview` as the corpus requires; encoding,
      binary and text files, newline handling, seek/tell, resource cleanup.
- [ ] `sys` streams, `print(file=)`, environment, filesystem primitives,
      path-like values, process invocation.

### E. Imports and libraries

- [ ] Module objects, `__name__`/`__file__`/`__package__`, initialization
      ordering, cycles, package `__main__`, relative imports, import errors.
- [ ] Standard-library inventory driven by workload failures. Enumerate each
      module's supported public API; an importable stub is not compatibility.
- [ ] Implement libraries in PyRs once their primitives exist; keep platform
      operations in the runtime. Reuse upstream tests with provenance.
- [ ] Dependency discovery from virtual environments and installed
      distributions; record unsupported C extensions as compatibility
      dependencies.

### F. Reliability, performance and distribution (continuous)

- [ ] GC stress, cycles, retained heap bounds, exception/generator roots and
      long-running workloads. A moving collector is optional; correct bounded
      memory behavior is the requirement.
- [x] Reproducible sanitizer runs for the adapter, runtime and collector.
- [ ] Measured compile time and runtime; safely cache runtime objects keyed
      on runtime content, compiler, target and options.
- [ ] Declare the supported host/target matrix (initially Linux x86-64) and
      exercise each claimed platform in CI.
- [ ] Reproducible release builds, checksums, install instructions,
      clean-machine smoke tests, stdlib relocation.
- [ ] Public compatibility matrix, migration guide, versioned language
      contract, support policy. Audit stale architecture docs against code.

## 1.0 release gates

1. Owner-confirmed workload definition and mode policy; the frozen corpus
   reaches the agreed native target with complete output, exit-status and
   file parity for passing cases.
2. Zero unresolved known silent miscompilations in the declared supported
   surface. Unsupported cases must reject before user code executes, or run
   under opted-in compatibility. **The measured-defect table above is this
   gate's outstanding list.**
3. `make ci`, compatibility regressions, optimization parity, sanitizers and
   GC stress, and clean-install tests pass on every claimed platform.
4. Representative long-running jobs have bounded memory and no reproducible
   corruption; benchmark claims include inputs, versions and measurements.
5. Native and compatibility dependency requirements are clear; mode
   selection, deployment, errors and cancellation have integration coverage.
6. Every workstream item has evidence or an explicit owner-approved scope
   decision. Update crate versions, changelog and release artifacts together
   only after these gates pass; tagging is a separate action.

## Semantic references

The installed CPython 3.14 oracle is the specification for differential
tests, alongside the Python 3.14
[execution model](https://docs.python.org/3.14/reference/executionmodel.html),
[built-in types](https://docs.python.org/3.14/library/stdtypes.html) and
[command-line contract](https://docs.python.org/3.14/using/cmdline.html).
Record version-sensitive diagnostic differences rather than hiding them
behind broad output normalization.

## Remaining phases

| Phase | Focus | Exit evidence |
|-------|-------|---------------|
| **Value fidelity, proposed next** | Heterogeneous numeric containers; runtime-type comparison slots; `NotImplemented`; mixed-tuple membership | The measured-defect rows for `[1, 2.5, 1]` and `__ne__` dispatch close; residual limits explicit |
| **Unicode contract** | One code-point representation, then search, then case/predicates | The five Unicode rows close; a pinned UCD version; the `strings` benchmark stays within an agreed budget |
| **Sustained workload validation** | GC, resource handling, compile cost, library-shaped programs | Repeatable memory/stress results; long-running programs with containers, cycles, exceptions, closures and generators stay correct |
| **1.0 release candidate** | Stable supported contract and reproducible release | Full gates pass for the exact candidate; no known silent defects in its supported contract; installation, diagnostics and upgrade expectations documented |

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
