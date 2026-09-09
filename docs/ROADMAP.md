# PyRs roadmap to 1.0

Status: implementation in progress. **Not a 1.0 release declaration.**
Reaching a particular minor version does not establish 1.0 readiness, and no
stable release or tag has been created.

This document is forward-looking: what is still missing, in what order, and
what shipping 1.0 would require. **What already shipped, and when, is in
[the changelog](../CHANGELOG.md)** -- this file used to carry a section per
milestone and a validation table per release, which grew to 70% of its
length and duplicated the changelog entry for the same work.

It is also the single roadmap. It absorbed the separate `ROADMAP-1.0.md`
delivery plan in 0.88, because the two had begun to contradict each other:
one listed exact int/float comparison as an open gap while the other
recorded it as fixed, and they disagreed about whether to bump minor
versions per milestone.

| Section | Purpose |
|---------|---------|
| [Current state](#current-state) | What exists now, and the latest gate result |
| [Confirmed remaining gaps](#confirmed-remaining-gaps) | Known missing features, found by probing |
| [Measured defects](#measured-defects-2026-09-05) | Wrong answers probed against CPython; the list for release gate 2 |
| [Product contract](#product-contract) | What 1.0 is meant to mean |
| [Workstreams](#workstreams) | Dependency-ordered work, with what is done |
| [Release gates](#10-release-gates) | Conditions for shipping 1.0 |

## Current state

PyRs is a native compiler for a statically typed Python subset, with
arbitrary-precision integers, typed containers, modules and packages,
closures, generators, closed-world classes with virtual methods, catchable
exceptions, Unicode 16.0.0 strings, project manifests, build caching, and a
default nonmoving mark-sweep collector. RiskSim exercises a real
multi-module command-line workload.

Every milestone runs the same gate before it lands, and the result is
recorded in [the changelog](../CHANGELOG.md). The most recent run:

| Check | Result after 0.138 |
|-------|--------------------|
| `make doctor` | All required tools available |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1711 passed; none failed or ignored |
| `make examples` | All 14 example entry points matched CPython |
| `make compatibility` | native 81 pass / 3 known_gap / 6 skipped; compat 28 pass / 6 skipped. The known gap is `mutable-defaults`, a recorded `mismatch`. The skips are the numpy and pandas cases, absent from this machine rather than excluded from the run |
| `pyrs --version` | `PyRs 0.138.1` |

Measured on Rust 1.96.1, LLVM 22.1.8, CPython 3.14.7, GCC 16.2.1. CI uses
Ubuntu 24.04, LLVM 18 and CPython 3.14. **These results do not establish
clean-machine release portability or complete Python compatibility** --
that is what the release gates below are for.

## Language surface, measured

A coverage audit on 2026-09-08 compiled 280 probes and diffed each against
CPython 3.14.7 on stdout, stderr and exit status. It is the input to the
breadth-first feature ordering, and it found three defects rather than three
missing features -- closed in 0.134, see
[the audit](superpowers/plans/2026-09-09-coverage-audit-and-tier-zero.md).

| Surface | PyRs | CPython |
|---|---:|---:|
| Keywords, full support | 30 | 35 |
| `list` / `dict` / `set` / `tuple` methods | 41 | 41 |
| `str` methods | 46 | 47 |
| Builtin functions | 41 | 69 |
| Exception types | 24 | 71 |
| Stdlib modules | 4 | 297 |

Syntax is not the gap and neither is the container library. What fails, fails
on a type rule or a missing literal form. Ranked by frequency over cost, the
open language items are: string prefixes (`r`, `R`, `u`, `F`, raw-f) and
adjacent string concatenation; `...`; keyword-only (`*,`) and positional-only
(`/`) parameters; module-level functions as values; arithmetic dunders; class
decorators and `@dataclass`; multiple context managers in one `with`; `{**d}`
in a dict display; `match`/`case` as identifiers (they are hard keywords here,
so `match = 5` is a syntax error); `del name` / `del obj.attr`; and
`raise X from Y`. `bytes` is excluded from that ordering and grouped with the
standard library, which it gates.

Two behaviours are recorded rather than fixed, both now in the README
divergence list:

- **Mutable defaults disagree with themselves.** A nested `def` and a lambda
  freeze each non-literal default once at definition time, as CPython does; a
  module-level `def` re-evaluates at every call. Closing it needs the default
  stored as a module global evaluated in `def` source order rather than as a
  frame temp, and the signature rewrite has to precede body lowering while the
  store lands in module init. Pinned as a `mismatch` in
  `compatibility/cases/mutable_defaults.py`, so the fix will fail the run as
  `unexpected_pass` until the record is updated.
- **An uncaught exception prints no traceback.** Type, message and exit status
  match CPython exactly; the `Traceback (most recent call last):` block and
  frame list are absent, so stderr never matches on a crash. Frame fidelity
  needs call-site line tracking.

## Confirmed remaining gaps

These findings remain open after the 0.83 scope. Passing the baseline did
not cover them.

| Area | Current gap | Required follow-up |
|------|-------------|--------------------|
| User iterator exception handling | Closed in 0.84: `StopIteration` is caught only around `__next__` | Keep generator `for` on Optional None unless that subset is deliberately changed |
| Iterable coverage | Closed in 0.84 for `for` and list/set/dict comprehensions | `any` / `all` / `enumerate` / `zip` / `reversed` still use a narrower set |
| Rich comparisons | Closed in 0.85 for `list[C]` `==`/`!=`/`in`/`index`/`count`/`remove`, tuple `==`/`!=`, and homogeneous `tuple[C, …]` `in`/`index`/`count`. Still: no `NotImplemented` fallback; slot choice uses static types; results are bool-coerced; mixed-tuple membership uses identity | Complete or explicitly bound the protocol contract before claiming general object compatibility |
| Text | Closed in 0.90 for offsets and in 0.91 for character properties: `len`, index, slice, iteration, search, split/strip, padding and format widths, `list(str)`/`set(str)` and `ord`/`chr` count code points; case transforms, `casefold`, the `is*` predicates, whitespace, line boundaries and `repr` escaping follow Unicode 16.0.0 tables generated from the CPython oracle. Still open: indexing a non-ASCII string is O(n) with a sequential-access memo, not CPython's O(1); lone-surrogate and encoding-error behavior is unspecified (a lone surrogate reaches the CPython bridge and is rejected there); no normalization or grapheme clusters; no `bytes`/`bytearray`, `.encode()` or non-UTF-8 codecs. Closed in 0.92: the lexer decodes `\xNN`/`\uXXXX`/`\UXXXXXXXX`/octal/control escapes, and f-string replacement fields accept nested quotes (PEP 701). A lone surrogate and `\N{NAME}` are rejected with a diagnostic rather than mis-decoded | Specify lone-surrogate and encoding-error behavior; `bytes` and codecs as the corpus requires |
| Numeric and binding semantics | Closed in 0.86 for mixed int/float comparison and conditionally assigned locals. Closed in 0.89: mixed-numeric list/tuple **literals** keep each element's own type instead of promoting to one. Still open: converting an already-typed `list[int]` into `list[float]`/a union by assignment (only a literal's own elements are joined); dynamic negative integer powers trap; module globals, deletion and static use-before-assignment diagnostics | Fix silent differences in the supported contract or narrow that contract explicitly with diagnostics |
| Generators and dynamism | `yield from` does not forward `send`/`throw`; generator exhaustion uses Optional None in several paths; `Any`, class attributes, inheritance, and class values remain restricted | Stabilize the intended subset and reject unsupported paths clearly; broader CPython dynamism is separate work |
| Memory confidence | Conservative roots can retain garbage; abandoned generators do not run user finalizers; collection statistics exclude native/allocator overhead | Continue stress and exception-path tests and measure process memory on sustained workloads; see [GC.md](GC.md) |
| Example parity gate | Closed in 0.86: `scripts/check_examples.py` compares raw stdout, stderr and exit status as bytes, builds and runs as separate steps, and its own failure paths are tested by `scripts/test_gates.py` | — |
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
| `len("héllo")` | `5` | `5` | **closed in 0.90** |
| `"héllo"[1]` | `é` | `é` | **closed in 0.90** |
| `"héllo".find("l")` | `2` | `2` | **closed in 0.90** |
| `len("🐍")` | `1` | `1` | **closed in 0.90** |
| `"naïve café".upper()` | `NAÏVE CAFÉ` | `NAÏVE CAFÉ` | **closed in 0.91** |
| `"ß".upper()` | `SS` | `SS` | **closed in 0.91** |
| `"é".isalpha()` | `True` | `True` | **closed in 0.91** |

| `2 ** 53 + 1 == 9007199254740992.0` | `False` | `False` | **closed in 0.86** |
| `[1, 2.5, 1]` | `[1, 2.5, 1]` | `[1, 2.5, 1]` | **closed in 0.89** |
| `a != b`, `a: Base` holding a `Child` defining `__ne__` | `Child.__ne__` runs | `Child.__ne__` runs | **closed in 0.89** |
| `def f(x: "Base")` | accepted | accepted | **closed in 0.87** |
| `str(KeyError("k"))` | `'k'` | `'k'` | **closed in review** |
| `e.args[0]` after `d["z"]` | `z` | `z` | **closed in review** |
| `ascii(["é"])` | `['\xe9']` | `['\xe9']` | **closed in review** |
| `f"{xs:}"` | `[1, 2]` | `[1, 2]` | **closed in review** |
| `e.args` display | `('z',)` | `['z']` | **scope decision, 0.121** — see below |
| `str((1, 2))`, `f"{[1, 2]}"` | `(1, 2)`, `[1, 2]` | `(1, 2)`, `[1, 2]` | **closed in 0.108** |
| `list(zip(infinite(), [1]))` | `[(0, 1)]` | `[(0, 1)]` | **closed in 0.119** |
| `(x for x in range(bound()))` | `bound()` at creation | `bound()` at creation | **closed in 0.120** |
| `"ΟΣ".lower()` | `ος` | `ος` | **closed in review** |
| `"{0} {0}".format(side())` | one call | one call | **closed in review** |
| `print([e])` for a caught `e` | `[ValueError('x')]` | `[ValueError('x')]` | **closed in review** |
| `repr(RuntimeError(""))` | `RuntimeError('')` | `RuntimeError('')` | **closed in 0.121** |
| `range(a(), b())` operand order | `a()` then `b()` | `a()` then `b()` | **closed in 0.119** |

All seven rows measured on 2026-09-05 are closed; the four still open were
appended by later review. The table is release gate 2's outstanding list, so
a row leaves it only by closing or by a recorded scope decision, and new
probes append to it.

`KeyError.__str__` is CPython's `repr(args[0])`, so a str key displays quoted
and an int key does not. This is now handled by the display-vs-storage
distinction the earlier attempt lacked: the exception stores `args[0]` **raw**
and carries the tag of that argument, so display can apply the repr rule while
`e.args[0]` stays the key itself. The earlier attempt failed because, without
the tag, quoting at display could not tell a str key from an int one and
regressed `s.remove(2)` to `KeyError: '2'`; that case is now a test.

Storing the pre-quoted form, which is what the internal sites used to do, was
also a wrong *value* and not merely wrong text: `e.args[0]` for `d["z"]` came
back as three characters rather than one.

### `e.args` shape: a recorded scope decision, not a fix

What remains is the shape of `args`, not its contents: PyRs models it as a
`list`, so it displays as `['z']` where CPython shows the tuple `('z',)`.
Tuples here are fixed-arity, and `args` is 0-or-1 elements decided at runtime,
so there is no tuple type to give it without variable-length tuples.

**Closed by decision in 0.121**, under release gate 6's allowance for "an
explicit owner-approved scope decision". The reasoning, so it can be revisited
rather than rediscovered:

- It is a *display* difference, not a wrong computation. `e.args[0]`,
  `len(e.args)`, iteration and indexing all match CPython, and 0.121 made the
  length match too — `raise E` gives 0 elements, `raise E("")` gives 1.
- Closing it properly needs variable-length tuples, a type-system change out
  of all proportion to the symptom, and one that would be driven by the
  workload corpus rather than by this row.
- The alternative — printing a list as though it were a tuple — would put a
  lie in the type system to fix a print.

Treated like set-iteration order below: documented, tested, and closed. It is
recorded in [the guide](GUIDE.md#9-differences-from-cpython) as a known
divergence, and reopens if variable-length tuples ever land.

**With this and `repr(RuntimeError(""))`, release gate 2's outstanding list is
empty.** That is not the same as the gate passing: the gate says "zero
*known* silent miscompilations", and the table is only as good as the probing
behind it. New probes append.

`str()` and f-string interpolation rejected every container until 0.108, even
though `print` formatted the same value correctly. The formatting logic was
right; it was unreachable, because the print routines wrote straight to
`stdout`. 0.108 routes them through a sink that `str()` can capture, so the
two now agree by construction. `ascii()` of a container now works — it is the same rendering with non-ASCII
escaped inside the elements, which the shared printer does under a flag. A
*non-empty* format spec on a container stays rejected, matching CPython's
`TypeError`; the **empty** spec (`f"{xs:}"`) was rejected too, which was
wrong, since CPython treats it as `str(x)` for every type.

`repr` cannot tell an exception raised with no argument from one raised with
an empty string: both store an empty message, so `RuntimeError()` and
`RuntimeError("")` render alike. CPython distinguishes them because it keeps
`args`. This is the same display-vs-storage distinction the KeyError row
needs, and it is now consistent between `repr(e)` and `[e]`, which is what
review raised.

Set iteration order is **not** a divergence that can be closed, and the
measurement says so plainly: CPython's own order for str elements is not
stable across runs, because string hashing is randomized per process. Three
consecutive runs of `print({'apple','banana','cherry','date'})` gave two
different orders. There is therefore no single CPython order to match, and a
differential test asserting one would be flaky against CPython itself. Int
sets are deterministic there (ints hash to themselves) and could be matched by
replicating CPython's table geometry, but doing so for ints alone would make
our order depend on element type while still not matching for strings.
Insertion order is kept, is documented, and programs needing deterministic
output should use `sorted(s)` — which CPython users need for the same reason.

Non-ASCII indexing is no longer O(n) per lookup. The one-entry memo made a
forward walk amortised O(1) but did nothing for scattered access, which
rescanned from the start: measured 118 ms against 2 ms for the same loop over
ASCII, and 12 ms for CPython. The hot string now also gets a sampled index —
the byte offset of every 32nd code point — so a lookup jumps to the nearest
sample and scans at most 32 code points. The same benchmark is now 3 ms, and
the sequential walk is unchanged.

Two divergences share one root cause: the eager builtins and generator
expressions **materialize their inputs** instead of advancing them lazily.
`zip` drains each argument into a list before pairing, so
`list(zip(infinite_generator(), [1]))` never terminates where CPython returns
one pair, and any finite generator's side effects all run up front. A
generator expression is the mirror image: CPython evaluates the *outermost*
iterable when the genexp is created — `(x for x in range(bound()))` calls
`bound()` right there — while PyRs leaves it in the synthesized body and calls
it on first iteration. Both were found by review on the 0.90-0.108 series and
confirmed against CPython.

Neither is a formatting detail that can be patched at the call site: closing
them means an iteration protocol that can advance heterogeneous inputs in
lockstep, which is the same machinery `itertools`-style laziness would need.
That is a milestone of its own rather than a fix, and it is tracked in
workstream C below.

**0.119 closed the first of the two.** The protocol turned out to already
exist in half the compiler: comprehensions lowered through `CompIterParts`,
a compile-time cursor of `{cond, element, step, kind}`, while `for` loops had
a parallel family of hand-written `lower_for_*` functions and the eager
builtins had a third path that drained to a list. Composition needed one new
operation — normalising a cursor into *(statements that try to produce an
element, test for whether one appeared)* — after which `zip` is those pairs
nested inside each other, so component *k+1* is only advanced when component
*k* produced. `for`, comprehensions and the eager consumers now share it.

**0.120 closed the second.** It needed less than expected. The hoist already
preferred the eager path and fell back only when `lower_expr` failed, and by
then `range(...)` was the only iterable still taking that branch. Rather than
making `range` a first-class value — which needs a reified iterator — the
hoist gained a second form: when the iterable is not a value, its *operands*
are hoisted and the form is rebuilt inside the synthesized body from
parameters. `(x for x in range(bound()))` now calls `bound()` at creation
while the range itself stays lazy, so `range(1000000000)` still costs
nothing.

Iterators as values remain open, and are what `it = zip(a, b)` needs.

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
      suspension are done; deleted bindings and static use-before-assignment
      diagnostics remain. Module-level containers became readable from
      functions in 0.102 (scalars already were).
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
- [x] Report the selected engine (0.110): `pyrs check` prints the entry
      point, import root, execution mode and the resolved interpreter with
      its source, decided before the call rather than after.
- [x] Project configuration (0.110): `[tool.pyrs]` in `pyproject.toml`,
      `pyrs init`, discovery, a declared import root for `src/` layouts, and
      declared -- never inferred -- compatibility mode. uv supplies the
      interpreter when present and is never required. Project *creation* is
      `uv init`'s job, so there is no `pyrs new`.
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

- [ ] **Lazy iteration protocol.** Half closed in 0.119: `zip` and
      `enumerate` compose as cursors and advance in lockstep wherever they are
      iterated or consumed. What remains is iterators as *values*, which is
      what the generator-expression row needs.
      Original statement: Advance iterables in lockstep instead of
      materializing them: `zip` must stop at the shortest input rather than
      draining each one first (today `zip(infinite(), [1])` hangs), and a
      generator expression must evaluate its outermost iterable at creation
      as CPython does. One protocol closes both.
- [ ] Runtime operations for `Any` and mixed containers; call-site inference
      for unannotated functions and lambdas. Partly closed in 0.136:
      `isinstance` narrows an `Any` for a single non-container pattern, so a
      guarded body uses the value directly. Still open: method calls on a bare
      `Any`, container patterns (no element type is recoverable from the tag)
      and multi-pattern peels (no member index exists in the box's tag space).
- [ ] Dynamic-length heterogeneous tuples; general hash/equality protocol;
      `float`/`bool`/`frozenset` keys; dict views. Dict *keys* and set
      elements deliberately stayed restricted when 0.137 let container
      *values* infer a union, because a key must be hashable.
- [ ] **Operators and conversions on a union value**, measured and documented
      in [GUIDE section 9](GUIDE.md#9-differences-from-cpython) as of 0.137.
      What works without narrowing: `print`, truthiness (`if x:` / `bool(x)`),
      `in` over a container of them, passing/returning one, and storing one as
      a dict value. What needs `isinstance` first: `str`/`repr`/f-string
      interpolation, every comparison and arithmetic operator, method calls,
      `len`, and `sorted(key=...)`. All of it needs the same thing — runtime
      dispatch on the union tag — so it is one piece of work with the `Any`
      runtime-operations item above, not several.
      `str(x)` is the cheapest and most conspicuous: `print(x)` already
      renders a union through the runtime's tag dispatch, and `ContainerRepr`
      has no union arm to capture the same output.
      Two related limits that are *not* bugs: a multi-member peel keeps
      storage (a subset's member indices do not match the original's, so
      rematerializing them would mis-tag the value), and a union is not a
      valid dict key or set element because a key must be hashable.
- [x] Tuple keys (0.107): dict keys and set elements may be tuples of
      hashable things, nested arbitrarily, which also unblocked the `a[i, j]`
      subscript rejected with a specific diagnostic since 0.87. `bool` stays
      out because `True == 1` would demand collision with an int key.
- [x] Positional-only and keyword-only rules, and signature binding for them
      (0.138): `/` and `*` markers with CPython's argument-passing rules,
      keyword arguments on methods (instance, static, class and virtual — they
      were refused for *any* method before, which made every
      `df.sort_values(by=...)`-shaped API unreachable), and module-level
      functions as first-class values including dispatch tables.
- [ ] Callable metadata, decorator factories, stacked decorators. Also: a
      container of functions with *differing* signatures, which needs a union
      of closure types and runtime dispatch through it; and functions with
      `*args`/`**kwargs` or defaults as values, which `Ty::Closure`'s fixed
      parameter list cannot represent.
- [ ] Class attributes, properties, descriptors, arithmetic and
      reflected/in-place dunders, `NotImplemented`, `__call__`, `__hash__`.
- [x] Default `!=` dispatches virtually (0.89): a class with `__eq__` but no
      `__ne__` anywhere in its ancestry gets one synthesized, calling
      `self.__eq__` through the normal vtable. `NotImplemented` fallback and
      runtime-type slot selection for *other* dunders remain open; slot
      choice elsewhere is still static.
- [ ] Decide multiple inheritance/MRO, class decorators/dataclasses, dynamic
      attributes and introspection from corpus requirements.
- [ ] General `list[T1]` -> `list[T2]` element-wise re-coercion (and into a
      union), needed for e.g. `fs: list[float] = xs` from `list[int]`, and
      for comparing/joining two independently-typed numeric lists. Mixed
      numeric list *literals* already keep exact per-element types (0.89), and
      0.136 made an *expected type* reach a literal at an index or attribute
      target, which covers construction. **The obstacle for values is aliasing,
      not layout**: every list slot is 8 bytes whatever the element type, but a
      `list[str]` slot holds the string pointer and a `list[Any]` slot a box
      wrapping it, so the conversion is an O(n) re-box into a fresh list — and
      after `f.cols["k"] = xs`, an `xs.append(...)` would no longer be visible
      through the frame. Any design has to answer that first.
- [ ] Complete iterator protocol: lazy `range`/`enumerate`/`zip`/`reversed`/
      `map`/`filter`, `iter`/`next` defaults, `StopIteration.value`,
      `yield from` send/throw, generator cleanup.
- [ ] Exception args as tuples, user exception classes, hierarchy, chaining,
      re-raise, traceback locations, multiple context managers. Partly closed
      in 0.121: the builtin hierarchy gained `AttributeError`,
      `NotImplementedError`, `ImportError`/`ModuleNotFoundError`,
      `LookupError` and `ArithmeticError`, and an exception now records how
      many arguments it was raised with. `args` as a *tuple* is a recorded
      scope decision, not open work.
- [x] Arithmetic, bitwise and unary dunders including reflected and in-place
      forms (0.135), which is what `@` actually needed: `a @ b` dispatches to
      `__matmul__` like any other operator, so an array type is a *library*
      rather than a compiler primitive. Still open: `NotImplemented` as a
      fallback value, and `__hash__` / `__call__`.
- [ ] A native array type covering shapes, strides, dtypes, views and
      zero-copy NumPy buffers. Reframed by 0.135 as a *performance and
      interop* item (see [interop gate 5](INTEROPERABILITY.md)) rather than a
      prerequisite for writing an array library, which pure PyRs can now
      express.

### D. Unicode, bytes and I/O

- [x] One documented representation (0.90): UTF-8 bytes with the code point
      count cached in the `PyrsStr` header, so `len` stays O(1) and codegen's
      `emit_len` is unchanged. `len`/index/slice/search/iteration agree on
      code points, including astral and combining characters and embedded
      NUL. ASCII is recognised as `cplen == len` and keeps the existing byte
      paths. Still open: a reproducible Unicode data version, and
      lone-surrogate and encoding-error behavior — a lone surrogate reaches
      the CPython bridge today and is rejected there as invalid UTF-8.
- [x] Unicode case transforms, predicates, whitespace, padding, translation
      and formatting (0.91). `scripts/gen_unicode_tables.py` asks CPython
      about every code point and emits `codegen/runtime/unicode_data.c`
      (Unicode 16.0.0, ~155 KiB, 40 ms to compile against `runtime.c`'s
      2.2 s), covering SpecialCasing so `"ß".upper()` is `"SS"`. `make
      hygiene` fails if the stamped Unicode version stops matching the
      running interpreter. Padding and formatting widths were already
      character-based from 0.90.
- [ ] `bytes`/`bytearray`/`memoryview` as the corpus requires; encoding,
      binary and text files, newline handling, seek/tell, resource cleanup.
- [ ] `sys` streams, `print(file=)`, environment, filesystem primitives,
      path-like values, process invocation. Partly closed in 0.124:
      `print(file=sys.stderr|sys.stdout)` selects the stream and `sys.exit`
      sets the status after flushing. There is still no file *object* behind
      the streams, so no other `file=` destination.

### E. Imports and libraries

- [ ] Module objects, `__name__`/`__file__`/`__package__`, initialization
      ordering, cycles, package `__main__`, relative imports, import errors. `__name__` closed in 0.124 —
      it has a compile-time answer (`__main__` for the entry module, the
      dotted import name otherwise), which the others do not.
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
- [x] Measured compile time and runtime; safely cache runtime objects keyed
      on runtime content, compiler, target and options (0.109). Whole
      programs are cached on the same terms, so an unchanged `pyrs run`
      compiles nothing: 2.6 s to 11 ms, and 84 ms for a new program against a
      warm runtime cache.
- [x] Bound and manage that cache (0.111): `pyrs cache dir/info/clean/prune`,
      least-recently-used eviction against `--older-than` and `--max-size`,
      and an opportunistic 2 GiB ceiling. Before this it grew without limit —
      998 MB in a day of test runs, with a deleted directory as the only
      remedy. `PYRS_CFLAGS`/`PYRS_LDFLAGS` are honored and keyed.
- [x] Command-line experience (0.112): `pyrs doctor`, `pyrs clean`, shell
      completions, `build` as a project-aware `compile` writing `target/NAME`,
      and argument errors that name the argument and suggest the real one.
- [x] Project scaffolding (0.113): `pyrs init` produces the `src/` layout,
      `.gitignore`, README, pinned `.python-version` and repository that
      `cargo new` and `uv init` both do, while a directory that already has a
      `pyproject.toml` still gets one table and nothing else.
- [x] Machine-readable diagnostics (0.114): `--message-format=json` on
      `check`, `build` and `run`, with lex, parse, import and semantic
      failures all keeping their spans through to the driver — the
      prerequisite for any editor integration. `pyrs tree` prints the
      resolved import graph.
- [x] `pyrs test` (0.115): pytest-convention discovery, compiled and run
      natively. pytest under CPython tests the logic; only a native runner
      tests whether the compiled program agrees, which is the failure mode a
      subset compiler actually has.
- [x] Close the loop (0.116): `pyrs init` scaffolds a test that passes, a
      build reports whether it did any work, and `build-extension`'s fifteen
      entry-point rejections are tested rather than merely written down.
- [x] Bound the cache for real (0.117): the 0.111 ceiling was checked once a
      day and nothing else, which let the cache reach 3.6 GiB against a 2 GiB
      limit in 46 minutes. The prune now also triggers on growth.
- [x] Make the gates measure what they claim (0.118): `make compatibility`
      runs every group, so the scientific/data cases the product contract
      names are counted as `skipped` rather than excluded by `--group core`.
- [x] Inline the small-int fast path (0.126). Every `int` operation was an
      out-of-line call the optimizer could not see through, which was the whole
      of the integer-benchmark gap. Each operation now has an inline fast path
      on the tagged words with the runtime call kept for the bignum edge:
      `primes` went 0.8x -> 12.4x and the corpus 3.2x -> 9.5x, with every
      benchmark now faster than CPython. LTO was considered and rejected — it
      needs matching clang for both halves, makes runtime objects
      non-cacheable, and asks LLVM to rediscover the fast path from opaque C.
      See [the plan](superpowers/plans/2026-09-08-inline-int-arithmetic.md).
- [x] Narrow the `volatile` local rule (0.127). A function with a `try`
      anywhere marked every local volatile, defeating `mem2reg` function-wide;
      C's rule only covers locals *changed* between the `setjmp` and the
      `longjmp`, so only a store inside a `try` now disqualifies one. A hot
      loop beside a cold validity check went 114ms -> 66ms. `exceptions` does
      not move, correctly: its loop body is inside the try.
- [x] Reduce the per-raise cost (0.131). The emitted IR named `@setjmp`, which
      bypasses glibc's `#define setjmp(env) _setjmp(env)` and bound to the
      signal-mask-saving entry point — 85ns a call against 1.8ns, plus a
      matching restore per longjmp. And `pyrs_exc_object()` ran at every
      handler entry, allocating twice for a result only a bound name or a bare
      `raise` reads. `exceptions` 1.3x -> 6.3x.
- [ ] ~~Attribute the runtime declarations and separate list-header from
      element aliasing.~~ **Tried and reverted, 2026-09-08.** Both were
      implemented: `!alias.scope`/`!noalias` separating a container header
      from its element buffer, and `nounwind` / `memory(read)` /
      `willreturn` on the nine runtime functions that provably neither
      allocate nor trap. Measured effect on every benchmark, and on a
      purpose-built read-only float loop: **none** — within noise, and the
      header-load count in the hot loop was unchanged. The reason is that
      every loop carries a tagged-int counter whose `pyrs.int.add` cold edge
      calls `pyrs_int_add`, which can allocate a bignum and so cannot be
      attributed; one such clobber is enough to stop LICM regardless of what
      the other calls claim. Attributing the allocating int family would need
      `memory(read, inaccessiblemem: readwrite)`, and `sort`'s profile says
      the prize is one instruction in a ~50-instruction loop body. Not worth
      the silent-miscompilation surface. The route that would pay is a
      machine-`i64` induction variable for `range` loops with statically small
      bounds, which removes the clobber rather than describing it.
- [ ] Reduce the remaining per-`try` cost. Each entry still `malloc`s a
      240-byte `PyrsExcFrame`, pushes it onto two intrusive lists, and stores
      four volatile control words; each raise `snprintf`s the message into a
      static buffer that the caught path immediately strips again. Measured
      2026-09-08.
- [x] Plumb `CodeGenOptLevel` and the target CPU (0.128). `-O` now reaches the
      backend, and `--target-cpu generic|native|<model>` (also `target-cpu` in
      the manifest) replaces the hardcoded baseline. `run`/`test` default to
      `native`, `compile`/`build-extension` to `generic`, and the resolved
      model and feature string join the program cache key so a shared cache
      cannot serve one machine another's instructions. Worth ~10% on a
      vectorizable float kernel and nothing on the scalar-bound corpus.
- [x] Stop paying a call per list element in the mark phase (0.129). A heap
      envelope rejects a non-pointer candidate in two compares, and contiguous
      slot runs are handed to the collector in bulk rather than one at a time.
      `listcomp` 6.3x -> 10.5x, `iteration` 9.9x -> 15.2x, `pipeline` 15.0x ->
      20.7x. `benchmarks/objects.py` added for what the collector governs.
- [x] Replace the range table (0.130). Marking filed every live range into a
      sorted array and binary searched it, costing an O(n log n) sort before
      marking could start; each range is now filed under every 256-byte
      granule it covers, in a table built in one linear pass, with a small
      sorted tier for ranges too wide to file. `objects` 109ms -> 61ms, 0.7x ->
      1.1x, and every benchmark in the corpus is now faster than CPython.
- [x] Measure and fix dict/set lookups (0.133). Nothing in the corpus touched a
      dict, so hashing a string key byte-at-a-time on every lookup was
      invisible; `benchmarks/dicts.py` put it at 0.8x. Slots now cache the top
      byte of the key's hash in existing padding — the full hash grew the slot
      and made it slower — and string keys hash eight bytes at a time behind a
      `fmix64` finalizer, without which a word-at-a-time hash collides
      catastrophically on a `h & mask` bucket index. 314ms -> 282ms.
- [ ] ~~Retain the collector's scratch buffers across passes.~~ **Tried and
      reverted, 2026-09-08.** `ranges_push` is 15% of `objects`, and the array
      does restart at capacity 64 and double its way to ~600k entries every
      pass — but that is ~56 reallocations against 2.4M pushes, so removing the
      regrowth measured at nothing (104ms vs 105ms A/B). The 15% is the pushes
      themselves, one per live object per collection, which only a different
      heap layout removes. Retaining the buffers would also have pinned the
      high-water mark in resident memory, against the bounded-memory gate.
- [ ] Replace the per-object `calloc` allocator. **The single largest remaining
      item, and the only place PyRs is still behind CPython.** Every managed
      object is an individual `calloc` on one global intrusive list, so the
      collector walks a cache-cold pointer chain twice per pass and pushes one
      range per live object. `objects` runs in 111ms with the collector and
      47ms without; `dicts` 332ms against 255ms. Two cheaper fixes have been
      tried and measured at nothing (scratch-buffer retention above; runtime
      attributes and alias scopes below), so what is left is the heap layout
      itself: size-classed blocks, aligned so a candidate's owner is a mask and
      a divide rather than a table lookup, which also retires the range array
      and the granule index. Measured 2026-09-08.
- [x] Reduce the per-character cost of string iteration (0.132). `==` computed
      a full three-way `memcmp` ordering to answer a yes/no question about two
      single characters, and `s[i]` was an opaque call per character. Both now
      decide inline — pointer identity, byte length, then the single byte for
      equality; `cplen == len` then a bounds check and a load for indexing.
      `strings` 3.2x -> 15.9x.
- [x] Fix an out-of-bounds write in `single_char` (0.132). A non-UTF-8 file
      read satisfies `STR_IS_ASCII`, because `utf8_next` counts an invalid byte
      as one latin-1 code point; indexing it interned a byte >= 0x80 in a
      128-entry table, writing about 2.5 KiB past the end. The table is now 256
      entries, so the byte round-trips instead.
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
