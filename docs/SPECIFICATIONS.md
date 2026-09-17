# Specification

What PyRs guarantees, what it supports today, and the rules that decide both.
This document is the precise statement; [GUIDE.md](GUIDE.md) is the readable
one and [ARCHITECTURE.md](ARCHITECTURE.md) describes the implementation.

## Versioning

Semantic versioning, MAJOR.MINOR.PATCH, with one number covering the language
surface, every crate and the CLI (`env!("CARGO_PKG_VERSION")`). While MAJOR is
0, MINOR advances for a milestone and PATCH for fixes. **1.0.0 is reached when
PyRs is ready for real world use**, not because the minor number has grown
large.

**v0.153** / `0.153.0`. Optional release tags: `vX.Y.Z`.

`make hygiene` enforces agreement across the seven crate manifests,
`Cargo.lock`, this document, the README and the compiled binary's `--version`.
A release whose artifacts disagree about what they are is not releasable.

## Goals

| Goal | Meaning |
|---|---|
| Drop-in replacement, long term | Grow the supported surface until PyRs can replace CPython for real workloads |
| Byte parity, hard rule today | For every supported construct, stdout, stderr and exit code match CPython on the same program, except where a deviation is documented |
| Native performance | Compute bound code compiles to machine code comparable to C for the same algorithm |
| Honest errors | An unsupported feature fails at compile time with a diagnostic naming it. Runtime traps reuse CPython's messages where parity is claimed |

Naming: **PyRs** in prose, `pyrs` for the binary and crate, `pyrs_*` for C
runtime symbols and mangled user functions.

## The supported subset

**Today (v0.153.0 subset):**

### Types and storage

* A variable's storage type is the join of every right hand side, plus any
  annotation. A bare multi-assignment can produce a union (`x = 1; x = "a"`
  gives `int | str`), and numeric multi-assignment promotes (`int` then `float`
  gives `float`). An annotation fixes storage and is never silently widened.
* `int` is arbitrary precision, `float` is IEEE 754 double, `str` is Unicode
  16.0.0 by code point, `bool`, `None`, fixed arity heterogeneous `tuple`,
  homogeneous `list[T]`, `dict[K, V]` and `set[T]` with `K` and `T` in
  `{int, str}`, file handles, closures, generators and class instances.
* An unannotated empty `[]` defaults to `list[Any]`, and a later `append` or
  `insert` in the same body still fixes a more specific element type.
* Implicit promotion is `bool` to `int` to `float` in arithmetic, arguments and
  returns, plus subclass to base for parameters, returns, list appends and
  unions containing the base.
* `Any` and `object` mean the same thing: a tagged value that dispatches at run
  time. See [DYNAMISM.md](DYNAMISM.md).

### Inference

Annotations are optional. Inference runs as rounds of the real lowering: every
call, to a function, a method including virtually dispatched ones, a
constructor, a dunder behind an operator or a `key=` callable, records the type
it passes to each unannotated parameter, and every unannotated function records
what it returns and yields. The next round seeds from that.

Rounds settle optimistically, ignoring a dynamic argument while it may be a
parameter not yet typed, and then one strict round widens any seed a dynamic
argument contradicts, so a seed is never a guess the program can falsify.

Two exact types decide `Any`, and `int` with `float` does not merge because the
conversion would change output. A common base class is kept. `None` makes an
optional. An override and the method it overrides pool their evidence. A
function whose value escapes keeps body inference. A dynamic argument from a
function unreachable from the program's roots is ignored. A parameter nothing
types is dynamic. A fully annotated program runs one round.

With `--infer ml` or `--infer dl` a learned predictor types a parameter that has
no call site evidence. A prediction that breaks its function's lowering, or
whose function escapes, is withdrawn. Full detail in
[INFERENCE.md](INFERENCE.md).

### Scoping, control flow and classes

* Function wide local scoping with `global` and `nonlocal`. Nested `def` and
  `lambda` become closures, with free variables in cells boxed at the outer
  bind and defaults frozen at definition.
* Generators support `yield`, `yield from` over a list, tuple, string or
  generator, `try` with `finally` across a suspension, and `close`, `send` and
  `throw`.
* Flow narrowing on `is None`, `is not None` and `isinstance`, including
  through unions, optionals and base to subclass, composing left to right
  through `and`, refining mid expression and in match guards.
* `match` and `case` for a subset of patterns, including `as`, `*rest`,
  `**rest`, alternatives and guards.
* Exceptions are `raise` plus `try`, `except`, `else` and `finally`, built on
  setjmp frames, single threaded and process global. Runtime traps are
  catchable. `return`, `break` and `continue` pop the frame and run `finally`.
* Classes are closed world: an instance is `{ i64 type_id, fields... }`,
  methods are IR functions, single inheritance with virtual dispatch, field
  layout specialized per class. Properties, static and class methods, context
  managers, iterator and comparison protocols, arithmetic and bitwise operator
  overloading including reflected and in place forms, and user defined
  exceptions are supported. Multiple inheritance and an open `__dict__` are
  not, although a program that uses `getattr` or `setattr` gives the classes it
  reflects on an overflow dictionary.

### Module resolution

Absolute imports search four roots, first hit wins: the entry script's
directory, `PYRS_STDLIB` when set, the workspace `stdlib/` during development,
and the standard library embedded in the binary. Once a top level package is
found under one origin, its submodules resolve only under that origin.

Supported forms are `import M`, `import M as A`, `import pkg.mod`,
`import pkg.mod as m`, `from M import x, y as z`, parenthesized import lists,
`from pkg.mod import x`, `from pkg import mod`, relative imports inside
packages, and module level `from m import *` expanding public names or a static
`__all__`. Namespace packages follow a PEP 420 subset. Function local imports
work, except star imports. Cycles and missing names are compile errors with
spans pointing at the importing file.

The root module is `__main__`. Module bodies run at their import site and
parent package initializers run before their children.

## Symbol and layout ABI

| Kind | Convention |
|---|---|
| User function | `@pyrs_<name>` |
| Module global | `@g.<name>` |
| Compiler temporary | local beginning with `.` |
| Runtime API | `pyrs_*` C functions |
| String | `{ i64 len, bytes... }`, length prefixed with a trailing NUL |
| List | `{ i64 len, i64 cap, i64* data }`, 8 byte slots |
| Print and contains tags | scalars 0 to 3, nested list element tag `4 + 8 * inner` |
| Dynamic value | `{ i32 tag, i64 payload }` in two registers |

Changing any of these requires a coordinated edit of `codegen/src/emit.rs`,
`codegen/runtime/runtime.c` and the tracing logic in `codegen/runtime/gc.c`.

## Diagnostics

Every user facing failure from the lexer, parser or semantic phase is a
`common::Diagnostic { phase, message, span, file }`, rendered with
`file:line:column` and a caret underline beneath the narrowest useful span.

If a bad program can reach codegen, semantic analysis is missing a check. Users
should not routinely see `error[codegen]`.

Runtime errors print to standard error and exit 1, with CPython's text wherever
the runtime claims parity.

## Parity and testing bar

1. **Differential testing.** For each supported feature, the same program under
   `python3` and under `pyrs` must produce the same stdout, the same stderr and
   the same exit code. Expected output is captured from CPython, never written
   by hand.
2. **Unit tests** live in each crate; end to end tests compile real binaries,
   including multi-file projects.
3. **`make ci`** runs format, clippy with warnings as errors, the workspace
   tests, hygiene, example parity and the compatibility probes.
4. **GitHub Actions** mirrors that gate on Ubuntu with LLVM 18 and CPython
   3.14, plus separate benchmark, documentation and release workflows.
5. **Benchmarks** verify byte identical output before timing.

Parity is a hard rule for supported features until a deviation is deliberately
documented.

## Documented deviations

Each is recorded with its reason in
[the language reference](GUIDE.md#differences-from-cpython). The current set
includes set iteration order, `e.args` as a list, no `is` interning for equal
integers, amortized rather than exact O(1) indexing of non-ASCII strings,
mutable default arguments at module level, and an uncaught exception printing
its type and message without a traceback block.

## Out of scope today

Not supported, and rejected at compile time by name: `eval` and `exec`,
monkey patching, metaclasses, `__slots__`, `__new__`, class decorators,
multiple inheritance, generics, `bytes`, `bytearray` and `memoryview`, codecs
other than UTF-8, `async` and `await`, `raise X from Y`, `f"{x=}"`, two
argument `super()`, stacked decorators, and the majority of the standard
library.

The direction is to move toward fuller optional typing and more of CPython's
dynamic semantics over time, without giving up parity on the supported core.
[ROADMAP.md](ROADMAP.md) tracks what 1.0 requires.
