# PyRs roadmap to 1.0

Status: implementation in progress. **Not a 1.0 release declaration.**

PyRs has a substantial native compiler and runtime, but it is still a
statically typed Python subset. Versions **0.90.0** through **0.92.0** make
strings Unicode: offsets are code points, case transforms and character
classes follow Unicode 16.0.0, and string literals accept the full escape
set. **0.93.0** adds conditional expressions, **0.94.0** user-defined
exception classes, **0.95.0** generators as arguments to the eager builtins
**0.96.0** generator expressions, **0.97.0** lambda parameter inference and
**0.98.0** iterable coverage for the eager builtins, **0.99.0** bare `raise`
**0.100.0** `str.format()` / `%` formatting, **0.101.0** tuple sort keys and
**0.102.0** module-level containers, **0.103.0** annotated attributes and
**0.104.0** `typing` imports with `Iterator[T]`, **0.105.0** n-ary `zip` with
`enumerate(start)`, **0.106.0** class-body constants, **0.107.0** tuple
dict/set keys, **0.108.0** `str()` / `repr()` of containers and **0.109.0**
build caching. The next milestone is **0.110.0**; reaching a particular minor version does not
establish 1.0 readiness, and no stable release or tag has been created.

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

After 0.109 on the same host:

| Check | Result after 0.109 |
|-------|-------------------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1378 passed; none failed or ignored (18 new) |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 66 pass / 0 known_gap; compat 22 pass |
| `pyrs --version` | `PyRs 0.109.0` |

After 0.108 on the same host:

| Check | Result after 0.108 |
|-------|-------------------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1348 passed; none failed or ignored (21 new) |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 66 pass / 0 known_gap; compat 22 pass |
| `pyrs --version` | `PyRs 0.108.0` |

After 0.107 on the same host:

| Check | Result after 0.107 |
|-------|-------------------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1327 passed; none failed or ignored (24 new) |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 63 pass / 0 known_gap; compat 21 pass |
| `pyrs --version` | `PyRs 0.107.0` |

After 0.106 on the same host:

| Check | Result after 0.106 |
|-------|-------------------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1303 passed; none failed or ignored (12 new) |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 60 pass / 0 known_gap; compat 20 pass |
| `pyrs --version` | `PyRs 0.106.0` |

After 0.105 on the same host:

| Check | Result after 0.105 |
|-------|-------------------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1291 passed; none failed or ignored (10 new) |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 57 pass / 0 known_gap; compat 19 pass |
| `pyrs --version` | `PyRs 0.105.0` |

After 0.104 on the same host:

| Check | Result after 0.104 |
|-------|-------------------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1281 passed; none failed or ignored (11 new) |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 54 pass / 0 known_gap; compat 18 pass |
| `pyrs --version` | `PyRs 0.104.0` |

After 0.103 on the same host:

| Check | Result after 0.103 |
|-------|-------------------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1270 passed; none failed or ignored (9 new) |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 51 pass / 0 known_gap; compat 17 pass |
| `pyrs --version` | `PyRs 0.103.0` |

After 0.102 on the same host:

| Check | Result after 0.102 |
|-------|-------------------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1261 passed; none failed or ignored (8 new) |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 48 pass / 0 known_gap; compat 16 pass |
| `pyrs --version` | `PyRs 0.102.0` |

After 0.101 on the same host:

| Check | Result after 0.101 |
|-------|-------------------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1253 passed; none failed or ignored (10 new) |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 45 pass / 0 known_gap; compat 15 pass |
| `pyrs --version` | `PyRs 0.101.0` |

After 0.100 on the same host:

| Check | Result after 0.100 |
|-------|-------------------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1243 passed; none failed or ignored (12 new) |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 42 pass / 0 known_gap; compat 14 pass |
| `pyrs --version` | `PyRs 0.100.0` |

After 0.99 on the same host:

| Check | Result after 0.99 |
|-------|-------------------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1231 passed; none failed or ignored (12 new) |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 39 pass / 0 known_gap; compat 13 pass |
| `pyrs --version` | `PyRs 0.99.0` |

After 0.98 on the same host:

| Check | Result after 0.98 |
|-------|-------------------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1219 passed; none failed or ignored (11 new) |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 36 pass / 0 known_gap; compat 12 pass |
| `pyrs --version` | `PyRs 0.98.0` |

After 0.97 on the same host:

| Check | Result after 0.97 |
|-------|-------------------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1208 passed; none failed or ignored (10 new) |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 33 pass / 0 known_gap; compat 11 pass |
| `pyrs --version` | `PyRs 0.97.0` |

After 0.96 on the same host:

| Check | Result after 0.96 |
|-------|-------------------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1198 passed; none failed or ignored (15 new) |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 30 pass / 0 known_gap; compat 10 pass |
| `pyrs --version` | `PyRs 0.96.0` |

After 0.95 on the same host:

| Check | Result after 0.95 |
|-------|-------------------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1183 passed; none failed or ignored (14 new) |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 27 pass / 0 known_gap; compat 9 pass |
| `pyrs --version` | `PyRs 0.95.0` |

After 0.94 on the same host:

| Check | Result after 0.94 |
|-------|-------------------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1169 passed; none failed or ignored (21 new) |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 24 pass / 0 known_gap; compat 8 pass |
| `pyrs --version` | `PyRs 0.94.0` |

After 0.93 on the same host:

| Check | Result after 0.93 |
|-------|-------------------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1148 passed; none failed or ignored (18 new) |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 21 pass / 0 known_gap; compat 7 pass |
| `pyrs --version` | `PyRs 0.93.0` |

After 0.92 on the same host:

| Check | Result after 0.92 |
|-------|-------------------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1130 passed; none failed or ignored |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 18 pass / 0 known_gap; compat 6 pass |
| `pyrs --version` | `PyRs 0.92.0` |

After 0.91 on the same host:

| Check | Result after 0.91 |
|-------|-------------------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1114 passed; none failed or ignored (37 Unicode tests) |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 18 pass / 0 known_gap; compat 6 pass |
| `pyrs --version` | `PyRs 0.91.0` |

After 0.90 on the same host:

| Check | Result after 0.90 |
|-------|-------------------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1101 passed; none failed or ignored (24 new Unicode tests) |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 18 pass / 0 known_gap; compat 6 pass |
| `pyrs --version` | `PyRs 0.90.0` |

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

## 0.109.0: build caching

A one-line program took 2.61 s to build, 2.39 s of it `cc -O2 -c runtime.c` --
92% of the floor, paid again on every invocation. Two layers close it: the
runtime objects are compiled once and reused (84 ms for a new program), and
whole programs are keyed on their inputs so an unchanged `pyrs run` skips
analysis, codegen and linking entirely (11 ms).

The cache is global rather than per-project, because the runtime objects
depend only on the compiler and the embedded sources: every project wants the
same ones, and an explicit `pyrs run -i prog.py` with no project still hits.

A stale entry is a wrong answer that looks like a right one, so keys cover
everything that can change the output bytes and entries are checksum-verified
before reuse rather than trusted. Two defects found while testing would each
have made the cache quietly useless: the runtime key is computed over
*preprocessed* C, whose line markers embed the per-run temporary path, so it
never repeated until preprocessing moved to `-P`; and computing a key ran
`cc --version` and `cc -dumpmachine` on every invocation, including hits,
until the toolchain identity was recorded under a stamp of the compiler
binary.

## 0.108.0: `str()` and `repr()` of containers

`print([1, 2])` wrote `[1, 2]`, but `str([1, 2])`, `f"{xs}"` and `"%s" % xs`
were rejected -- so `print(f"result: {xs}")`, about the most ordinary line in
a Python program, could not be written, and neither could a function that
*returns* a rendered value.

The formatting logic already existed and was already right. It just could not
be reached from anything but `print`, because the print routines wrote straight
to `stdout`. They now write through an output sink, and `str()` captures what
`print` would have emitted, so the two agree by construction rather than by
two implementations kept in step. That property is what the tests check:
several print the value *and* `str()` of it in the same program.

The milestone contract is:

- `str(x)` / `repr(x)` for `list`, `tuple`, `dict`, `set`, nested arbitrarily,
  over every element type `print` already handled.
- f-strings, `%` and `str.format()` come with it -- all three already routed
  through the `str()` lowering.
- `repr()` and `ascii()` become builtins; they existed only as the f-string
  `!r` / `!a` conversions.
- A format spec on a container (`f"{xs:>10}"`) stays rejected, which is
  CPython's `TypeError` moved to compile time. `ascii()` of a container stays
  rejected too: unlike `repr` it must escape non-ASCII *inside* the elements,
  which the shared rendering does not do.

Inherited rather than introduced: sets iterate in insertion order here and in
hash order in CPython, so `str({3, 1, 2})` differs exactly as
`print({3, 1, 2})` already did.

## 0.107.0: tuple dict/set keys

Dict keys and set elements were `int` or `str` only, so the composite key that
a transition table, a sparse grid or a two-argument memo wants had to be
flattened into a string by hand. The restriction was found the direct way: a
state machine keyed by `(state, event)` was rejected outright.

Only *hashing* was missing. `slot_eq` already compared `TAG_TUPLE` slots
structurally through `pyrs_tuple_eq`, so equal tuples already compared equal --
there was just no way to reach the right bucket. `hash_key` now has a
`TAG_TUPLE` arm that folds the element hashes and recurses, so nesting and
unhashable elements both fall out of the existing dispatch. The GC needed
nothing: dict and set key slots are already traced, conservatively, which was
verified under collection pressure rather than assumed.

The milestone contract is:

- `int`, `str` and tuples of hashable things, nested arbitrarily, work as keys
  and set elements everywhere: literals, subscripts, `in`, `get`/`pop`/`del`,
  iteration, `dict()`, and both comprehension forms.
- `d[i, j]` means `d[(i, j)]`, trailing comma included -- the parse-level
  rejection carried since 0.87 is gone. A tuple subscript of a *list* is now
  the type error CPython also raises, rather than a parse error.
- `bool` stays rejected on purpose: CPython's `True == 1` would require a bool
  key to collide with an int one, which this subset does not model.

Three further key-type gates (`dict()` from pairs, set comprehensions, dict
comprehensions) each carried their own hardcoded `Int | Str` match; they now
share one predicate, which is why they moved together.

## 0.106.0: class-body constants

Any assignment in a class body was rejected, so a class could not carry a
constant: enum-like values, limits, `PI`. The recorded reason was that a class
attribute with a default would leave zeroed instance storage -- true of an
instance *field* default, and the reason to keep rejecting that, but a class
constant is not an instance field and does not need the layout at all.

The milestone contract is:

- `class C: LIMIT = 10`, read as `C.LIMIT` and `self.LIMIT`, inherited and
  overridable, for int / float / str / bool and negated numbers, annotated or
  not.
- An instance field of the same name shadows it, as in CPython.
- The value must be a literal; a computed one is rejected with the reason.
- Assigning to a constant is rejected -- unless an instance field of that name
  exists, which is the shadowing case and is a real assignment.

Constants are *substituted where they are read* rather than stored. That is
exact for something immutable, and it removes the whole problem the original
rejection was about: no storage, no initialisation ordering, nothing to zero.
It is also what makes assignment meaningless, which the diagnostic says.

Three diagnostics were wrong before and are worth noting, because each sent
the reader somewhere unhelpful: `C.N = 2` reported `name 'C' is not defined`,
`self.N = 2` reported that the object had no such attribute, and `C.M` for an
unknown `M` also reported `C` as undefined.

## 0.105.0: n-ary `zip` and `enumerate(start)`

`zip` accepted exactly two arguments and `enumerate` only a keyword `start=`,
so `zip(a, b, c)` and `enumerate(xs, 1)` were compile errors.

The milestone contract is:

- `zip` over one or more iterables, truncating to the shortest, producing a
  tuple of that arity.
- `enumerate` with `start` positional or keyword, rejecting both together.
- Both accept every iterable, as the other eager builtins have since 0.98.

Small, and the point is where it came from: a survey of eighteen builtin
argument forms against CPython found exactly these two, which is the useful
result of such a survey -- the other sixteen already matched.

## 0.104.0: `typing` imports and `Iterator[T]` annotations

Two gaps, the first blocking any file that had the second.

`from typing import ...` failed to *load* -- `No module named 'typing'` -- so
an ordinary typed Python file could not be compiled however simple its
contents. `typing` and `collections.abc` are now annotation-only modules: the
loader skips them, the import binds nothing, and no module-init call is
emitted for a module with no body.

A generator could be created, iterated and passed to a builtin, but not to a
user function. There was no spelling for a generator parameter, and inference
cannot supply one -- a `for` loop body says nothing about whether its subject
is a list, a str or a generator. `Iterator[T]` and `Generator[T, None, None]`
now annotate one, which is how Python annotates it, so the same source still
runs under CPython.

`Iterable[T]` and `Sequence[T]` are rejected rather than mapped: they cover a
list as well as a generator, and those are distinct types here, so there is no
single thing to resolve them to. The diagnostic names both alternatives.

A `-> Iterator[T]` return already *is* the generator type, so the four sites
that build a generator's signature -- which have to agree, or a call site sees
a different element type than the body produces -- now share one rule that
unwraps it instead of wrapping it again.

## 0.103.0: annotated attribute assignment

`self.x: T = value` was rejected -- the parser allowed an annotation only on a
bare name -- which made an attribute whose initial value has no inferable type
unwritable. `self.xs = []` reported `'C' object has no attribute 'xs'` and
`self.d = {}` could not infer a dict type, so an empty list or dict attribute
could not be created at all.

The milestone contract is:

- `self.x: T = value` in `__init__` declares the field's type: scalars,
  containers, nested containers and unions.
- An unannotated attribute still infers from its value.
- An annotation that disagrees with its value is a type error, as every other
  annotation here is.

Two places had to agree: the pass that collects a class's fields from
`__init__` now prefers the annotation over inferring from the right-hand side,
and the assignment lowering no longer rejects the annotation outright. The
value is coerced to the declared field type, so a conflicting second
annotation surfaces as a value mismatch rather than being ignored.

## 0.102.0: module-level containers are visible to functions

A module-level scalar could already be read from a function; a list, dict, set
or tuple could not, and reported `name 'X' is not defined`. A lookup table or
config dict at module scope is ordinary Python.

The milestone contract is:

- Global storage types are seeded from container literals, nested ones
  included, so a module-level table is readable from a function. Anything the
  seeder cannot type leaves that global unseeded -- the safe direction, since
  the name is then simply not in scope as before.
- An empty `[]` nested inside a container takes the surrounding element type
  rather than being rejected.
- Global containers remain shared state, not copies.

The second item is the interesting one. `[]` has no element type, so it is
typed provisionally as `list[Any]`; `xs: list[str] = []` and `f([])` already
worked because an annotation or a parameter supplied the type, but nested in a
container literal nothing did, and `[["a"], []]` was a type error. Both the
list-element and dict-value joins now accept a provisional empty list, and
`coerce` gives it the target list type -- the runtime value, a length-zero
list, is identical either way.

Found by running small realistic programs against CPython rather than probing
constructs; a graph-traversal script needed the first item, and the second
surfaced while fixing it.

## 0.101.0: tuple sort keys

`sorted(items, key=lambda p: (-p[1], p[0]))` is the way to sort by more than
one criterion, and it was rejected: a `key=` function had to return a bare
scalar.

The milestone contract is:

- `key=` may return a tuple or list of orderable values, compared
  lexicographically, for `sorted`, `list.sort`, `min` and `max` in both forms,
  with `reverse=`, from a lambda or a named function.
- A key type with no ordering is still rejected, and the message says what is
  accepted.

The restriction sat only on the key path: tuples were already orderable
everywhere else, so this is the key path adopting the compiler's existing
`is_orderable_ty` rule and the lexicographic lowering `(1, 2) < (1, 3)` uses.
A raw `Binary` node on a tuple is not something codegen handles, which is why
the three places that compare key values had to route through it.

How this was found is worth recording. Micro-construct probes had stopped
turning up anything of this size; running four small *realistic* programs
against CPython did — three matched and the fourth, a word-frequency script,
needed exactly this. An existing test asserted the old limitation (a `list`
key rejected) and now pins a genuinely unorderable one instead.

## 0.100.0: `str.format()` and `%` formatting

Neither existed -- `.format` was absent from the str method table and `%` was
rejected as an operator on str -- so a large amount of ordinary Python could
not be compiled. f-strings cover new code, but rewriting an existing codebase
by hand is not a workaround.

The milestone contract is:

- `.format()` in every addressing mode (auto, indexed, keyword, mixed) with
  the spec mini-language and `!r` / `!s` / `!a`.
- `%` with the common conversions, flags, width, precision and `%%`, for both
  the tuple and bare-value forms.
- Argument-count and field-name mistakes are compile errors, where CPython
  raises at run time.

Both desugar into the `JoinedStr` parts f-strings already produce, so the
mini-language is implemented once and nothing new reaches the runtime; `%d`
becomes `{:d}`, `%-5s` becomes `{:<5}`, and so on. That is also what requires
a *literal* format string, and the rejection now says so and points at
f-strings instead of reporting an unsupported method or operator.

A nested `{}` inside a format spec is rejected rather than reused from the
f-string splitter: it names an argument in `.format()` and an expression in an
f-string, and quietly picking one interpretation would be wrong.

## 0.99.0: bare `raise`

`except E: log(); raise` is how a program observes an error without
swallowing it, and it had no workaround here -- raising a new exception loses
the original type and message, which is the whole point.

The milestone contract is:

- A bare `raise` in an `except` handler re-raises what that handler caught,
  preserving type and message: builtin and user classes, out of functions and
  generators, through `finally`, and picking the innermost handler.
- With no active handler it is a compile error, where CPython raises
  `RuntimeError` at run time.

The mechanism is the interesting part. The handler prologue calls
`pyrs_exc_clear()` before running its body -- so that a nested `try` inside
the handler does not see a stale exception -- which means the pending
exception is already gone by the time the body runs, and `pyrs_reraise()`
would have found nothing. The exception object is captured immediately before
that clear, kept on a stack in the emitter (handlers nest), and re-raised
through the existing `pyrs_raise_exc`. Nothing new was needed in the runtime.

A bare `raise` also had to be taught to the return-path and may-raise
analyses, or a function ending in one is reported as falling off the end.

## 0.98.0: the eager builtins accept any iterable

`sorted`, `sum`, `max`, `min`, `set`, `list` and `str.join` took a list (and a
generator, since 0.95) and rejected everything else, so `sorted(some_set)`,
`sorted(some_dict)`, `sum(range(n))` and `list(range(n))` were compile errors
even though `for x in` accepts all of them. A matrix of seven iterable shapes
against nine operations had 30 rejections; it now has none except the ones
below.

The milestone contract is:

- Those builtins accept list, tuple, set, dict (its keys, as in CPython), str,
  range and generator, with `key=` and `reverse=` over all of them.
- `any` / `all` gain dict but not range, and keep short-circuiting.
- `tuple()` is unchanged: tuples are fixed-arity here.

The generator drain from 0.95 generalizes into one helper over prepared
comprehension parts, so the element type and iteration order are exactly what
a `for` loop would produce. `range` needs the argument-level entry point
rather than the value-level one, because it cannot be lowered as a value at
all; the lowering probes, falling back to the comprehension path when the
expression does not lower.

Two deliberate exclusions, both about not trading a rejection for a wrong or
ruinous answer. `any` / `all` do not materialize, or `all(range(10**9))` would
build a billion elements where CPython returns False on the first. And
`tuple()` must not be materialized: it needs the tuple itself, and a list is
precisely what it cannot accept -- an earlier cut of this change did
materialize there and broke `tuple(t)`, which the suite caught.

Materializing a range is a real memory cost `sum(range(n))` does not pay in
CPython. It is a cost, not a wrong answer, and lazy `range` / `enumerate` /
`zip` remains its own roadmap item.

## 0.97.0: lambda parameter inference

A lambda cannot carry annotations -- the first `:` starts the body -- so
requiring them made lambdas unusable, and `sorted(xs, key=lambda v: -v)`, the
idiom they exist for, was a compile error. Named functions already worked as
`key=`, so the entire gap was parameter typing.

The milestone contract is:

- A `key=` lambda takes its parameter type from the element type, covering the
  case body inference cannot reach: `lambda s: len(s)` says nothing about `s`.
  `sorted`, `list.sort`, `min`, `max`, with `reverse=` and any sortable return.
- Every other lambda gets the body-usage inference nested `def`s already had.
  `lower_lambda` was requiring an annotation up front and never reaching it.
- A lambda whose body constrains nothing and that has no consumer to ask is
  still rejected; a `def` can be annotated.

The hint travels through the same side channel generator expressions use, but
a lambda's parameters carry user-chosen names, so it is scoped: set, lower,
restore. Without that a `key=lambda s: ...` would leave `s` typed as `str` for
any later bare parameter named `s`, which a test pins.

One existing test asserted the old limitation -- that `f = lambda x: x + 1` is
rejected -- and now pins what genuinely cannot be inferred instead.

## 0.96.0: generator expressions

`(elem for target in iter if cond)` was a parse error, which made the four
most common consuming idioms unavailable at once: `sum(x for x in xs)`,
`any(...)`, `max(...)` and `",".join(...)`. It was the largest single cluster
in a survey of common constructs against CPython, and 0.95 was its
prerequisite — a generator object nothing could consume would not have helped.

The milestone contract is:

- Parenthesized, and bare when it is a call's sole argument, as in CPython.
  Multiple `for` clauses and `if` filters.
- Genuinely lazy: the element expression runs on demand, `any` / `all`
  short-circuit through it, and the outermost iterable is evaluated once at
  creation. The loop variable does not leak.
- Element types other than `int` are inferred.

Desugared at the *AST* level into a synthesized nested generator function,
which is what keeps it small: free-variable capture, generator detection and
the call path all already exist, and no new IR was needed.

Two things did not come for free. The element type has to be known before the
body is lowered, because the yield sites are checked against it, and it
depends on the loop targets — which exist only inside the body. It is computed
up front by binding the targets exactly as a list comprehension does, and
passed to the lowering through a side channel; that channel has to follow the
function to its mangled IR name, or the body is checked against the wrong
type. And the outermost iterable is passed as a real argument rather than
captured, which gives CPython's eager evaluation *and* sidesteps a module-level
problem: a global cannot be captured by a closure here, so `sum(x for x in xs)`
at top level would otherwise fail where the same line inside a function works.

An iterable that is not a first-class value (`range(...)` is only legal as a
`for` iterable) cannot be hoisted, so the lowering probes: if the iterable does
not lower as an expression, it stays in the body where the loop handles it
natively. Probing rather than enumerating keeps this correct as more iterables
become values.

Capturing a module-level variable is now a compile error rather than a runtime
NameError -- the closure's cell is never filled, because the assignment writes
a global. That was pre-existing and affected lambdas identically.

## 0.95.0: generators as arguments to the eager builtins

A generator function could only be consumed by a `for` loop or a
comprehension. Every eager builtin rejected one, so `list(g())` was a compile
error and the values could only be extracted by writing the loop by hand.

The milestone contract is:

- `list`, `set`, `sorted`, `sum`, `max`, `min` and `str.join` accept a
  generator, materializing it through the same comprehension machinery
  `[x for x in gen]` already used. Sound precisely because these drain their
  argument anyway: side effects, order and result are unchanged.
- `any` and `all` accept a generator and short-circuit. They must not
  materialize, or a side-effecting or infinite generator would diverge from
  CPython — the tests print from inside the generator to pin how far it ran.
- An unannotated generator infers its yield type from the first `yield`
  instead of assuming `int`.

That last item was a prerequisite, not a bonus: with the yield type hard-coded
to `int`, `def g(): yield "a"` failed at the yield, so `",".join(g())` was
unreachable no matter what `join` accepted. The fix has to be applied at four
sites — the lowering site and three signature-collection sites — because a
disagreement between them hands a call site a different element type than the
body produces.

`tuple(gen)` remains rejected: tuples are fixed-arity here, and the existing
diagnostic already says so.

## 0.94.0: user-defined exception classes

`class E(Exception)` had no spelling at all. The exception type in `raise` and
`except` was resolved by the *parser* against a hardcoded list of builtins, so
a custom exception could not be named — and unlike most gaps there was no
workaround, only falling back to a builtin type that loses the distinction the
program is drawing.

The milestone contract is:

- `class E(Exception): pass`, and chains of them. A subclass is caught by any
  ancestor and by `except Exception`; a base is *not* caught by its subclass;
  unrelated user exceptions do not catch each other. A tuple filter may mix
  user and builtin types.
- `raise E`, `raise E()` and `raise E("msg")`, for user classes and builtins
  alike. An uncaught exception with no message prints the type name alone, as
  CPython does.
- Builtin exceptions, including the `OSError` family, are unchanged.

Name resolution moved out of the parser, which cannot know what classes exist,
into the semantic phase. `ExcType` gained a `User(tag)` variant rather than a
parallel type, so the ~130 existing builtin uses were untouched; `tag()` is now
spelled out per variant so the compiler catches a new builtin that forgets one.
Exception classes are registered before regular class collection and never
receive a ClassId, layout or vtable — they are a tag, a name and a parent.
Codegen emits name and parent tables the same way it already emits class
names, and the runtime walks the parent chain in `pyrs_exc_matches`.

Registration is a single source-order pass, not a fixed point: an earlier draft
resolved `class B(A)` written above `class A(Exception)`, which made PyRs accept
a program CPython rejects with NameError.

Out of scope, each rejected with a diagnostic that names the actual problem:
methods or fields on an exception class, using one as a value, subclassing
`GeneratorExit`, `raise ... from ...`, and `.args` as a tuple.

## 0.93.0: conditional expressions

`a if c else b` was a parse error — not an exotic corner, but one of the most
common expressions in Python, with no spelling at all before this milestone.

The milestone contract is:

- A conditional expression is accepted everywhere an expression is, and only
  the selected branch is evaluated, so `1 // n if n else -1` and
  `xs[0] if xs else "empty"` do not trap. The condition runs exactly once.
- Precedence and associativity match Python: `or` and `not` bind tighter,
  chains are right-associative, and the condition is an `or_test`, so
  `1 if 2 if 3 else 4 else 5` is rejected as CPython rejects it.
- A bare conditional is excluded from comprehension iterables and filters,
  because the trailing `if` there belongs to the comprehension.
- Mixed numeric branches keep each branch's own type (`1 if c else 2.5` is
  `1`), extending the 0.89 rule; unrelated branch types become a union.

Lowered to a temp assigned in the two arms of an `If`, wrapped in the `Block`
node comprehensions already use to put statements inside an expression, so
laziness comes for free: the branch not taken is never emitted into the same
basic block. No new IR.

Found while testing: four recursive AST walkers — free-variable capture,
`yield` detection, lambda collection and the call graph — silently skipped the
new node through their catch-all arms, so a name used only inside a
conditional looked undefined in a closure. That class of bug is invisible to
the type checker and is why the suite covers closures, generators and
comprehensions rather than just values.

Arithmetic and comparison on a mixed-numeric union remain unsupported, as they
already were for list elements since 0.89. The diagnostic now explains the
cause and suggests two remedies that were verified to work.

## 0.92.0: string literal escapes and PEP 701 f-strings

Two gaps found while testing the Unicode milestones, both silent wrong
answers in the supported surface rather than missing features.

The lexer recognised only `\n \t \r \0 \\ \' \"` and kept everything
else verbatim, so `"\x00"` was the four characters `\`, `x`, `0`, `0` and
`len` reported `4`. Adds `\xNN`, `\uXXXX`, `\UXXXXXXXX`, one-to-three-digit
octal and the missing control escapes `\a \b \f \v`. Unknown escapes still
survive verbatim, as in CPython. Malformed ones are rejected with a specific
message (`truncated \xXX escape`, `illegal Unicode character U+11FFFF`)
rather than mangled.

Two forms CPython accepts are rejected deliberately, both with a diagnostic:
a lone surrogate, which has no UTF-8 form and so cannot be represented in a
UTF-8 string (`chr(0xD800)` still produces the bytes at run time, an
inconsistency recorded above), and `\N{NAME}`, which needs the Unicode name
database the compiler does not carry — keeping it verbatim would be wrong
rather than merely incomplete, and CPython rejects a bare `\N` too.

The f-string token was a regex that stopped at the first unescaped quote, so
a replacement field could not hold a string literal: `f"{d["k"]}"` lexed as
`f"{` followed by loose tokens. It is now a scanner tracking brace depth and
nested literals, and the parser's brace scan and its `!`/`:` split skip
nested literals too, so `f"{'}'}"` and `f"{d[':']}"` are correct.

## 0.91.0: Unicode case and character classes

0.90 made every string *offset* a code point but deliberately left character
*properties* ASCII-only, so `"ß".upper()` was still `"ß"`, `"naïve".upper()`
was `"NAïVE"`, and `"é".isalpha()` was `False`.

The milestone contract is:

- `upper`, `lower`, `title`, `capitalize`, `swapcase` and `casefold` follow
  Unicode 16.0.0, including the mappings that change length —
  `"ß".upper()` is `"SS"`, `"ﬁ".upper()` is `"FI"` — and titlecase
  characters, so `"ǅungla".title()` matches CPython.
- `isalpha`, `isdigit`, `isdecimal`, `isnumeric`, `isalnum`, `isspace`,
  `isupper`, `islower`, `istitle`, `isprintable` and `isidentifier` use
  Unicode categories and derived properties. `isascii` is `cplen == len`.
- Whitespace-driven `strip`/`split`/`rsplit` use the Unicode whitespace set
  and never split a character. `splitlines` already used the full Unicode
  boundary set. `repr` escapes by Unicode printability rather than byte
  range, so `repr("café")` is `"'café'"` and a zero-width space is escaped.

The tables are generated by `scripts/gen_unicode_tables.py`, which asks the
installed CPython about every code point in `range(0x110000)` rather than
re-parsing the UCD. The roadmap already names that interpreter as the
specification for differential tests, so this gives agreement by
construction, needs no network in CI, and covers SpecialCasing — which the
`unicodedata` module does not expose but `str.upper()` does. The output is
committed; `make hygiene` fails if the stamped Unicode version stops
matching the running interpreter.

Two-stage lookup with 128-entry deduplicated blocks and case mappings stored
as deltas: 303 distinct records for all 1.1M code points, ~155 KiB of C that
compiles in 40 ms against `runtime.c`'s 2.2 s, so per-compile cost is not a
consideration.

## 0.90.0: Unicode code point offsets

`PyrsStr` was a UTF-8 byte buffer, and every operation except `ord`, `chr`
and `ascii` treated it as a plain byte array. Any non-ASCII text therefore
disagreed with CPython silently: `len("héllo")` was `6`, `"héllo"[1]` was
half a character, `"héllo".find("l")` reported a byte offset, and `for c in
s` yielded byte fragments.

The milestone contract is:

- `len`, indexing, slicing (including a step), iteration, `list(str)` and
  `set(str)` count and select whole code points, for 1-, 2-, 3- and 4-byte
  characters, combining sequences and an embedded NUL.
- `find`/`rfind`/`index`/`rindex`/`count` return code point offsets and
  accept code point `start`/`end` bounds, as do `startswith`/`endswith`.
  `split`/`rsplit`/`partition`/`splitlines`/`strip` never split a character,
  and `strip(chars)` compares whole code points.
- `center`/`ljust`/`rjust`/`zfill`/`expandtabs` and f-string format widths
  and precision measure characters. `translate`/`maketrans` key on code
  point ordinals. `ord` is O(1).
- ASCII strings keep the existing byte paths and behavior exactly.

The representation stays UTF-8; the header gains a cached code point count
as its *first* word, so `len(s)` is O(1) and codegen's `emit_len` — which
loads the first `i64` of every sized object — is unchanged, as are `print`,
file I/O, hashing, comparison and the CPython bridge. `str_alloc` leaves
that count at `-1` and every producer must finish with `str_done_ascii`,
`str_done_cplen` or `str_done_scan`, so a missed site reports a negative
length loudly instead of miscounting silently.

Case transforms and the `is*` predicates are deliberately **not** in scope
and keep their documented ASCII-only behavior; that is a property boundary,
not an offset one, so nothing mixes byte and character offsets in between.

Measured on this host: a string-saturated ASCII workload costs 5.3% (141.0
ms → 148.5 ms, best of 7), from the extra header word and the ASCII branch
in indexing; `fib(30)`, which touches no strings, is unchanged (15.7 ms →
15.4 ms). Indexing a non-ASCII string is O(n), against CPython's O(1); a
one-entry sequential-access memo, invalidated on every collection, keeps a
forward walk amortised O(1) so `for c in s` and index loops stay linear.

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
| `len("héllo")` | `5` | `5` | **closed in 0.90** |
| `"héllo"[1]` | `é` | `é` | **closed in 0.90** |
| `"héllo".find("l")` | `2` | `2` | **closed in 0.90** |
| `len("🐍")` | `1` | `1` | **closed in 0.90** |
| `"naïve café".upper()` | `NAÏVE CAFÉ` | `NAÏVE CAFÉ` | **closed in 0.91** |
| `"ß".upper()` | `SS` | `SS` | **closed in 0.91** |
| `"é".isalpha()` | `True` | `True` | **closed in 0.91** |

Every row measured on 2026-09-05 is now closed. The table is kept as the
record of what release gate 2 has cleared, not as an outstanding list; new
probes append to it.
| `2 ** 53 + 1 == 9007199254740992.0` | `False` | `False` | **closed in 0.86** |
| `[1, 2.5, 1]` | `[1, 2.5, 1]` | `[1, 2.5, 1]` | **closed in 0.89** |
| `a != b`, `a: Base` holding a `Child` defining `__ne__` | `Child.__ne__` runs | `Child.__ne__` runs | **closed in 0.89** |
| `def f(x: "Base")` | accepted | accepted | **closed in 0.87** |
| `str(KeyError("k"))` | `'k'` | `'k'` | **closed in review** |
| `e.args[0]` after `d["z"]` | `z` | `z` | **closed in review** |
| `ascii(["é"])` | `['\xe9']` | `['\xe9']` | **closed in review** |
| `f"{xs:}"` | `[1, 2]` | `[1, 2]` | **closed in review** |
| `e.args` display | `('z',)` | `['z']` | open — see below |
| `str((1, 2))`, `f"{[1, 2]}"` | `(1, 2)`, `[1, 2]` | `(1, 2)`, `[1, 2]` | **closed in 0.108** |
| `list(zip(infinite(), [1]))` | `[(0, 1)]` | does not terminate | open — see below |
| `(x for x in range(bound()))` | `bound()` at creation | `bound()` at first iteration | open — see below |
| `"ΟΣ".lower()` | `ος` | `ος` | **closed in review** |
| `"{0} {0}".format(side())` | one call | one call | **closed in review** |
| `print([e])` for a caught `e` | `[ValueError('x')]` | `[ValueError('x')]` | **closed in review** |
| `repr(RuntimeError(""))` | `RuntimeError('')` | `RuntimeError()` | open — see below |

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

What remains is the shape of `args`, not its contents: PyRs models it as a
`list`, so it displays as `['z']` where CPython shows the tuple `('z',)`.
Tuples here are fixed-arity, and `args` is 0-or-1 elements decided at runtime,
so there is no tuple type to give it without variable-length tuples.

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

- [ ] **Lazy iteration protocol.** Advance iterables in lockstep instead of
      materializing them: `zip` must stop at the shortest input rather than
      draining each one first (today `zip(infinite(), [1])` hangs), and a
      generator expression must evaluate its outermost iterable at creation
      as CPython does. One protocol closes both.
- [ ] Runtime operations for `Any` and mixed containers; call-site inference
      for unannotated functions and lambdas.
- [ ] Dynamic-length heterogeneous tuples; general hash/equality protocol;
      `float`/`bool`/`frozenset` keys; dict views.
- [x] Tuple keys (0.107): dict keys and set elements may be tuples of
      hashable things, nested arbitrarily, which also unblocked the `a[i, j]`
      subscript rejected with a specific diagnostic since 0.87. `bool` stays
      out because `True == 1` would demand collision with an int key.
- [ ] Callable metadata, signature binding, positional-only and keyword-only
      rules, `*args`/`**kwargs`, decorator factories, stacked decorators.
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
      numeric list *literals* already keep exact per-element types (0.89);
      this item is about values that already have a narrower list type.
- [ ] Complete iterator protocol: lazy `range`/`enumerate`/`zip`/`reversed`/
      `map`/`filter`, `iter`/`next` defaults, `StopIteration.value`,
      `yield from` send/throw, generator cleanup.
- [ ] Exception args as tuples, user exception classes, hierarchy, chaining,
      re-raise, traceback locations, multiple context managers.
- [ ] A native array type, which `@` needs; rejected with a specific
      diagnostic since 0.87.

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
- [x] Measured compile time and runtime; safely cache runtime objects keyed
      on runtime content, compiler, target and options (0.109). Whole
      programs are cached on the same terms, so an unchanged `pyrs run`
      compiles nothing: 2.6 s to 11 ms, and 84 ms for a new program against a
      warm runtime cache.
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
