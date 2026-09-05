# PyRs 1.0 delivery plan

Status: implementation in progress; **not a 1.0 release declaration**.
Baseline: repository 0.82.0 (`5b009a5`), reviewed 2026-09-05.

## Product contract

The owner selected **native execution by default, with an optional CPython
compatibility mode**. Native executables remain independent of CPython.
Compatibility mode is explicit and its Python dependency must be visible.
The owner selected **scientific/data workloads, including NumPy and pandas**.
Initial implementation targets native numerical Python and explicit whole-program
CPython execution for the scientific package ecosystem. This does not yet provide
native calls into NumPy, pandas, or their compiled extensions.

“A good percentage of Python use cases” is not yet a measurable claim. Before
declaring 1.0, freeze a named workload corpus with project versions, commands,
inputs, expected outputs, licenses, and selection rationale. Proposed acceptance:
at least 100 representative scenarios from independently maintained scientific
projects, covering arrays, linear algebra, reductions, missing values,
indexing/broadcasting, dtypes, tabular I/O, grouping, joins and time series.
At least 80% should run unchanged in the selected mode; report native-only
coverage separately and agree its target after measuring the corpus. These are
proposed release targets, not measured current coverage. Synthetic probes and PyRs examples are
valuable regression tests but do not count as independent projects.

## Architecture review

The seven Rust crates form a clean pipeline: lexer → syntax-only AST → semantic
analysis and desugaring → typed IR → LLVM text → C++ object-emission shim → native
link with the embedded C runtime and collector. The module loader resolves a
closed import graph; the semantic stage specializes storage and calls. Keep type
decisions in semantic analysis, not code generation.

Existing strengths include arbitrary-precision integers, flow narrowing, union
and limited Any values, closures/cells, generator frames, exception handling,
single-inheritance virtual methods, containers, package imports, and conservative
mark–sweep collection. The existing end-to-end suite has 572 tests at baseline.

The main blockers are semantic breadth, not missing spelling variants of methods:
numeric promotion changes values; string operations are largely byte/ASCII based;
some unassigned locals read as zero; Any has limited operations; dictionary keys
and container layouts are restricted; classes and imports are closed-world;
generator exhaustion and exception objects differ from Python. Only a small
stdlib is embedded. The driver recompiles the C runtime for each program and
currently exposes only compiler-oriented subcommands.

## Workstreams and dependency order

Each checked item requires implementation, differential tests, documentation, and
the relevant quality gates. Milestones describe coherent work, not promises that
one small feature earns a release. Stay on 0.y.z until every release gate passes.

### A. Evidence and compiler correctness (first)

- [ ] Add a versioned compatibility probe manifest and runner that records
  CPython version, source, compiler version, optimization level, compile failures,
  timeouts, stdout, stderr, exit status, and file effects. Report native and
  compatibility results separately; fail on regressions and unexpected passes.
- [ ] Freeze the independent workload corpus with the owner. Inventory actual
  dependency/import/syntax failures before prioritizing new stdlib modules.
- [ ] Exact mixed integer/float comparisons, including bigints, NaN, infinities,
  signed zero, fractional values, and boxed/container equality.
- [ ] Definite assignment/runtime binding checks for every local representation,
  including generator suspension; module/global and deleted bindings follow.
- [ ] Preserve Python numeric types/values across assignments, annotations,
  heterogeneous literals, min/max and boolean operand selection. Keep optimized
  homogeneous storage only where behavior is preserved.
- [ ] Audit evaluation order and once-only evaluation for calls, chained
  comparisons, augmented assignment, decorators, defaults and comprehensions.
- [ ] Audit integer/float conversion rounding and overflow, dynamic negative
  powers, division, identity-sensitive membership, and recursive equality.
- [ ] Run accepted features across O0/O2/O3 and GC stress; minimize every
  wrong-result or crash into a regression test. Fuzz lexer/parser/semantic and
  runtime container/numeric boundaries; user input must not panic the compiler.

### B. Invocation and explicit compatibility mode (alongside A)

- [ ] Python-style `pyrs script.py args`, `-c`, `-m`, stdin, `--`, script argv[0],
  source metadata, working directory/import path, exit status and signals.
- [ ] Explicit whole-program compatibility execution using the selected CPython
  environment, including installed scientific packages. Native compilation remains
  the default. Never retry a program after native execution has begun. Preserve
  stdin/stdout/stderr/args, exit status, environment, imports and signals.
- [ ] Keep code generation/linker/toolchain failures actionable; they are not
  reasons to silently change execution engines. Report the selected engine.
- [ ] Define compiled compatibility artifacts: dependency/version discovery,
  relocatability, deployment layout and diagnostics. A Python launcher is not a
  standalone native compilation and must never be labeled as one.
- [ ] Decide whether workload evidence justifies finer-grained Python interop.
  If so, specify object ownership, GIL, exception translation, GC boundaries,
  callbacks and extension ABI before implementing an embedding bridge.
- [ ] Semantic-only `check`, clear unsupported-feature diagnostics, validated
  optimization options, secure temporary directories, interrupted-run cleanup.

### C. Python values, typing and protocols (A prerequisites)

- [ ] Runtime operations for Any and mixed containers, plus call-site inference
  and specialization for ordinary unannotated functions and lambdas.
- [ ] Dynamic-length heterogeneous tuples; general hash/equality protocol and
  hashable tuple/float/bool/frozenset keys; Python dict views and mutation checks.
- [ ] First-class callable metadata, signature binding, positional-only and
  keyword-only rules, defaults, arbitrary *args/**kwargs, bound/closure keywords,
  decorator factories and stacked decorators.
- [ ] Class attributes, writable/deletable properties, descriptors, arithmetic
  and reflected/in-place dunders, NotImplemented, __call__, __hash__, slicing.
- [ ] Decide multiple inheritance/MRO, class decorators/dataclasses, dynamic
  attributes, type objects and introspection from corpus requirements. Each is
  either native-supported or an explicitly classified compatibility dependency.
- [ ] Complete iterator protocol: lazy range/enumerate/zip/reversed/map/filter,
  iter/next with defaults, generator StopIteration.value, yield-from send/throw,
  generator methods, and cleanup semantics.
- [ ] Exception args as tuples, user exception classes, hierarchy, chaining,
  re-raise, traceback source locations, multiple context managers and exit args.
- [ ] Syntax/metadata used by the corpus: source encodings and literal escapes,
  Unicode identifiers, bytes, f-string debug/grouping, annotations and __doc__.

### D. Unicode, bytes and I/O (before native library expansion)

- [ ] Choose and document one Python-compatible Unicode representation and a
  reproducible Unicode data version. len/index/slice/search/count/iteration must
  agree on code points, including astral characters, combining characters and
  embedded NUL. Specify lone-surrogate behavior and encoding errors.
- [ ] Unicode case transforms, predicates, whitespace, padding, translation and
  formatting. Avoid inconsistent partial fixes that mix byte and character offsets.
- [ ] bytes/bytearray/memoryview as required by the corpus; encoding/decoding,
  binary and text files, newline handling, seek/tell, errors and resource cleanup.
- [ ] sys streams, print(file=), environment/argv, filesystem primitives,
  path-like values, process invocation and exit handling.

### E. Imports and libraries (C/D prerequisites)

- [ ] Module objects, __name__/__file__/__package__, initialization ordering,
  cycles, package __main__, relative imports, search roots and import errors.
- [ ] Standard-library inventory driven by workload failures. Initial candidates:
  sys/os/pathlib, json loads/dumps with dynamic values, re, argparse, collections,
  itertools/functools, math/statistics/random, datetime/time, csv, io/tempfile,
  shutil/glob, subprocess and typing metadata. Explicitly enumerate each module's
  supported public API; an importable stub does not count as compatibility.
- [ ] Implement libraries in PyRs after their required primitives exist; keep
  platform and primitive operations in the runtime. Reuse upstream library tests
  with provenance/license information and pinned versions where practical.
- [ ] Dependency discovery from virtual environments/installed distributions,
  package resources and metadata. Record unsupported C extensions as compatibility
  dependencies. NumPy/pandas or web frameworks require their own integration suites.

### F. Reliability, performance and distribution (continuous)

- [ ] GC stress, cycles, retained heap bounds, exception/generator roots,
  sanitizer runs and long-running workloads. A moving collector is optional;
  correct bounded memory behavior is the release requirement.
- [ ] Measured compile time and runtime; cache runtime objects safely using
  runtime content, compiler/target/options as the cache key. Verify cache races,
  invalidation, interrupted writes and reproducibility.
- [ ] Declare supported host/target matrix (initially Linux x86-64); CI must
  exercise each claimed platform, supported LLVM and CPython compatibility version.
- [ ] Reproducible release builds, checksums, install/uninstall instructions,
  clean-machine smoke tests, stdlib relocation and compatibility dependency checks.
- [ ] Public compatibility matrix, migration guide, troubleshooting, versioned
  language contract, release notes and support policy. Audit stale architecture
  docs against code, especially integer ABI and inference descriptions.

## 1.0 release gates

1. Owner-confirmed workload definition and mode policy; frozen corpus reaches the
   agreed native target, with complete output/exit/file parity for passing cases.
2. Zero unresolved known silent miscompilations in the declared supported surface;
   unsupported cases reject before user code executes or use opted-in compatibility.
3. `make ci`, compatibility regressions, optimization parity, sanitizers/GC stress,
   and clean-install tests pass on every claimed platform.
4. Representative long-running jobs have bounded memory and no reproducible
   runtime corruption; benchmark claims include inputs, versions and measurements.
5. Native and compatibility dependency requirements are clear; mode selection,
   deployment, errors, cancellation and recovery have integration coverage.
6. All workstream requirements have evidence or an explicit owner-approved scope
   decision. Update every crate/version, changelog and release artifact together
   only after these gates pass. Tagging/publishing is a separate release action.

## Semantic references

Use the installed CPython 3.14 oracle for differential tests and the Python 3.14
[execution model](https://docs.python.org/3.14/reference/executionmodel.html),
[built-in types](https://docs.python.org/3.14/library/stdtypes.html), and
[command-line contract](https://docs.python.org/3.14/using/cmdline.html) as the
specification. Record version-sensitive diagnostic differences instead of hiding
them with broad output normalization.

## Implementation ledger

- 2026-09-05: architecture/limitations review and this dependency-ordered plan;
  baseline workspace suite started with Rust 1.96.1 and CPython 3.14.7.
- Native-default / optional compatibility mode confirmed by the owner.
- Scientific/data priority (NumPy and pandas) confirmed by the owner. Independent
  workload selection remains open; no native coverage percentage or
  1.0 readiness is claimed.
