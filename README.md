# PyRs

[![CI](https://github.com/sshussh/pyrs/actions/workflows/ci.yml/badge.svg)](https://github.com/sshussh/pyrs/actions/workflows/ci.yml)

A Python compiler built in Rust, targeting native code through LLVM.

PyRs compiles a statically-typed subset of Python straight to machine code —
no interpreter, no VM. Compute-bound code runs 45–60× faster than CPython
(see [Benchmarks](#benchmarks)).

**New here? The [PyRs Guide](docs/GUIDE.md) covers everything**: the CLI,
the Makefile, the full language reference, every difference from CPython,
runtime errors, and performance notes.

```console
$ cat examples/fib.py
def fib(n: int) -> int:
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)

print(fib(30))

$ pyrs compile -i examples/fib.py -o fib
$ ./fib
832040
```

## Usage

```console
pyrs compile -i prog.py -o prog     # build a native executable
pyrs run     -i prog.py             # compile and run immediately
pyrs lex     -i prog.py             # dump tokens
pyrs parse   -i prog.py             # dump the AST
```

`compile` options: `-O 0..3` (optimization level, default 2) and
`--emit-llvm` (also write the generated LLVM IR to `<output>.ll`).

## The language (v0.17)

Versioning is **MAJOR.MINOR.PATCH**. PyRs stays on **0.y.z** (next
milestone after this one is **0.18.0**, not 1.0) until it is ready for
**real-world use**; only then **1.0.0**. Crate versions and
`pyrs --version` match this label. Core-language growth comes first;
**classes** and **GC / heap freeing** are planned as the **last two**
core features (never-free is interim; GC is still required for 1.0).
No new stdlib until the language can host pure-PyRs libraries.

A statically-typed Python subset:

- **Types:** `int` (arbitrary precision; tagged small / heap limbs), `float` (f64), `bool`, `str`, `None`,
  unions (`int | None`, `str | int | None`), `Optional[T]`, `list[T]`,
  `tuple[T1, T2, …]`, `dict[K, V]`, `set[T]` — including nested lists
  (`list[list[float]]` matrices). Dict/set keys are `int` or `str` only;
  list elements and dict values may be Optional/unions; homogeneous
  closures (same params/ret and capture env shape, with or without
  captures) may be list/tuple elements
- **Functions:** `def` with optional parameter/return annotations
  (defaults infer param types; return type inferred from `return` when
  omitted), defaults and keyword args, recursion, forward references;
  pass/return tuples and other containers; nested `def` / `lambda` as
  first-class closures (including in containers and sibling/forward
  nested calls); late free-var binding (`def f(): return n` then
  `n = 5`); `nonlocal`; generator functions with `yield` / `yield from`
  (may escape, capture free vars, use `try`/`except`/`finally` including
  yield in finally; `send` / `throw` / `close`)
- **Statements:** `if`/`elif`/`else`, `while` / `for` (including
  `else` on loops — runs only if no `break`),
  `for x in range(...)` / lists / strings / files / tuples / dict keys /
  sets / generators (including unpack targets `for a, b in xs` and
  `for a, *rest in xs`), `break`/`continue`, assignments (plain, annotated,
  multi-target, unpacking `a, b = t`, augmented — including
  `xs[i] += v`), `del d[k]`, `return`, `pass`, `raise ExcType("msg")`,
  `try`/`except`/`else`/`finally` (including inside generators),
  `match`/`case` (literal, wildcard, capture, or-patterns, guards,
  sequence with optional `*rest`, mapping with optional `**rest`,
  `as` patterns)
- **Expressions:** full arithmetic including `**`, comparisons with
  chaining (`0 < x < 10`), `in`/`not in` (substring and membership),
  `is`/`is not` (None checks plus pointer/slot identity for same-type
  heap objects and scalars), bitwise `& | ^ ~ << >>`
  (and augassign) on int/bool, `and`/`or`/`not`
  (short-circuit; `and`/`or` return an operand, not always `bool`, and
  may yield a union when operands differ, e.g. `0 or "x"`), casts
  `int()`/`float()`/`bool()`/`str()`, `len()`, `abs()`, `min()`/`max()`
  (two args or one `list[int|float|bool]`), `sum()` on
  `list[int]`/`list[float]`, indexing with
  negative indices, full slicing `s[a:b:c]` including `[::-1]` reversal,
  `print(...)` with any mix of values (including tuples/dicts/sets)
- **f-strings:** `f"x={x}, next={x + 1}"` and multi-line `f"""…"""` /
  `f'''…'''` with `{{`/`}}` escapes, nesting, conversions `!s`/`!r`/`!a`,
  and free-form format specs (fill/align/sign/`#`/`0`/width/precision/
  types `dboxXfeEgGs%`, nested `{x:{w}.{p}f}`); no `{x=}`, grouping
  `,`/`_`, or types `n`/`c` yet; multi-line *expressions* inside `{...}`
  need parentheses; same-delimiter triples inside `{...}` unsupported —
  use the other quote style
- **Strings:** immutable; single/double and triple-quoted literals
  (`"""…"""` / `'''…'''`, multi-line; escapes as for single-line);
  module/function first-statement string docstrings are accepted as
  no-op expression statements (no `__doc__` attribute yet); `+` concat,
  `*` repeat, lexicographic comparisons, indexing, slicing, `in`,
  iteration, `len()`, `str(x)` conversions, and methods: `upper` `lower`
  `strip` `lstrip` `rstrip` `startswith` `endswith` `find` `rfind`
  `rindex` `count` `replace` `split` `join` `isdigit` `isalpha`
  `isspace` `isupper` `islower`
- **Lists:** homogeneous, growable; literals, comprehensions
  (`[x * x for x in xs if x > 0]`, multi-`for` / multi-`if`, unpack
  targets `[a+b for a, b in pairs]`; simple names use Python 3 scoping
  and do not leak — and faster than the equivalent loop when length is
  knowable: results are pre-sized and appends inlined),
  indexing (read/write), slicing (copies, like Python),
  `append`/`pop`/`insert`/`remove`/`index`/`clear`/`sort`, `sorted()`,
  `+`/`*` (concat / repeat), `==`/`!=`, `in`, `len`, iteration;
  assignment aliases like Python
- **Tuples:** fixed-arity, heterogeneous; literals `(a, b)`, `(a,)`,
  `()`; index (incl. negative); `len`; unpacking; print like CPython
- **Dicts:** `dict[K, V]` with `K` in `{int, str}`; literal `{k: v}`,
  `{}` (needs annotation); get/set, `del d[k]`, `in` on keys, `len`,
  insertion-order key iteration; methods `get` (with default, or bare
  `get(k)` → `Optional[V]` / `None` on miss),
  `pop`, `keys`/`values`/`items` (return lists), `clear`
- **Sets:** `set[T]` with `T` in `{int, str}`; nonempty `{a, b}`, empty
  `s: set[int] = set()`; `add`/`remove`/`discard`/`clear`, `in`, `len`,
  iteration
- **Exceptions:** `raise ValueError("msg")` (and KeyError, IndexError,
  ZeroDivisionError, TypeError, RuntimeError); `try`/`except`/`except
  Type as e`/`else`/`finally`; uncaught traps print CPython-like messages and
  exit 1; runtime traps are catchable inside `try`
- **Globals:** top-level variables are readable from any function;
  writing needs a `global x` declaration, exactly like Python
- **I/O:** `input([prompt])` from stdin; `import sys` + `sys.argv` for
  command-line arguments; files via `open(path, mode)` with
  `.read()`/`.readline()`/`.readlines()`/`.write()`/`.close()`,
  `with open(...) as f:` blocks, and CPython's exact error messages —
  compiled programs are real CLI tools
- **Modules & packages:** split a program across files and packages —
  `import utils`, `import a, b as c` (multi-name), `import pkg.mod` /
  `import pkg.mod as m`, `from pkg.mod import name`, `from pkg import mod`,
  package re-exports in `__init__.py` (`from .mod import f` / `from .
  import mod`; last top-level binding wins, with CPython fromlist hasattr
  short-circuit so assign/`def` then `from . import same_name` keeps the
  value and does not run the submodule), relative forms inside packages,
  and partial package init (child module top level may read simple parent
  assigns set before the child import; child function bodies may use
  deferred parent attrs/calls after full init); a directory with
  `__init__.py` is a regular package; a directory **without** `__init__.py`
  is a **namespace package** (PEP 420 subset: single origin, no multi-path
  split; prefer `__init__.py` > `name.py` > namespace dir); `from M import *`
  expands public names (or static `__all__` list/tuple of string lits) at
  module level only; module bodies run once at the import site
  (like Python). Import search order (stacked, first hit wins): (1) entry
  script directory, (2) `PYRS_STDLIB` if set, (3) workspace `stdlib/` when
  present (dev; not XOR with env), (4) **embedded** stdlib inside the
  `pyrs` binary (always; no companion directory needed). User code shadows
  stdlib; once a package is found under one origin, children stay there
  (no split packages). Cycles and missing modules/names are compile
  errors that point at the offending file
- **Stdlib (subset, frozen):** pure-PyRs `os.path` — `join(a, *parts)`
  (POSIX), `dirname`, `basename`; `os.getcwd()` (C runtime); `math` —
  constants `pi`/`e` and unary `sqrt`/`sin`/`cos`/`tan`/`log`/`log10`/
  `exp`/`floor`/`ceil`/`fabs` (intrinsics / libm); `json.dumps` for
  int/float/bool/str and homogeneous list/dict-of-str, plus typed
  `json.loads_*` helpers (no dynamic `json.loads`). `import sys` remains
  special-cased for `sys.argv`. **No new stdlib until the core language
  is far enough for pure-PyRs libraries** (see roadmap / `AGENTS.md`);
  interim modules may be rewritten pure later
- **Entry point:** top-level statements run like a script; if there are
  none, a zero-argument `main()` is called automatically

Python semantics are preserved where it counts:

- `7 / 2 == 3.5` — true division always yields float
- `-7 // 2 == -4`, `-7 % 3 == 2` — floored division and modulo
- `-2 ** 2 == -4`, `2 ** -1 == 0.5`, right-associative `2 ** 3 ** 2`
- `1 < middle() < 10` evaluates `middle()` exactly once and
  short-circuits, exactly like Python
- `ZeroDivisionError`, `IndexError`, `ValueError`, `KeyError`, … trap
  with exit 1 when uncaught (or transfer to an active `except`)
- floats print with shortest round-trip representation
  (`0.1 + 0.2` → `0.30000000000000004`, `1.0` → `1.0`); lists print as
  `[1, 2, 3]` / `['a', 'b']`; tuples/dicts/sets print like CPython
- iterating a list re-reads the live length, so appending inside the
  loop behaves like CPython
- variables use function-wide scoping; a variable's type is fixed by its
  first assignment

Known limits (v0.17): `int` is arbitrary precision (tagged small ±2⁶² /
heap limbs; limbs never freed, no interning/`is` identity for equal
values), `min`/`max`
two-arg form unifies to a common numeric type (`min(1, 1.5)` is `1.0`,
not the int `1`); iterable `min`/`max` is only for
`list[int|float|bool]` (empty list → ValueError like CPython),
control-flow narrowing covers `is None` / `is not None` (and `not`,
`and`/`or` body peels and **mid-expression** refine of
`x is not None and x > 0` / `x is None or x < 0`) on locals, cells, and
module Optionals (free reads, no `global` required) in `if`/`while` /
match guards (not full SAT / attribute narrowing); post-loop / post-if
rebinds clear stale peels; `is`/`is not` works with `None` and same-type
identity (heap pointers, scalar slots, float bitcast — not CPython int
interning); `x ** e` with a *dynamic* negative int exponent traps (a
constant like `2 ** -1` works and gives float), int↔float comparisons
convert the int to float (exactness loss past 2^53), list literals
coerce mixed numerics to one element type, `nan in [nan]` is False
(IEEE equality), str methods use ASCII case/whitespace rules, heap
memory is never freed, files support text modes "r"/"w"/"a" only, no
multi-path split namespace packages, no `from sys import *`, a package
importing itself by name, or treating modules as first-class values
beyond attribute/call chains; `os.path` is POSIX only; `*args` /
`**kwargs` on defs and `*`/`**` unpacking in calls are supported for
homogeneous list/dict types; starred assignment `a, *rest = xs` and
list displays `[*a, *b]` work for lists/tuples; `json` has no dynamic
`loads`; f-string `{x=}`, grouping (`,`/`_`), and types `n`/`c` are
unsupported; match/case is still a subset (**no class patterns**; or-patterns bind
only the matching alt; duplicate names/keys rejected); generators
support `yield` / `yield from` on list/tuple/str/generator (including
inside `finally`; subgen `return` feeds yield-from; close cascades to
yield-from subgens), `try`/`except`/`else`/`finally` (phase and try
exit kind preserved across yield resume), `close()` (GeneratorExit +
finally; ignore-GE → RuntimeError), `send(None|value)` (value must
match yield type; non-None before first yield → TypeError; yield
expression is `Optional[Y]`), and `throw(ExcType)` /
`throw(ExcType("msg"))` (inject at yield; uncaught propagates);
`for`/`send` treat exhaustion as Optional None rather than raising
StopIteration; after close/exhaust/uncaught throw, further send is
None and does not re-enter the body; `send`/`throw` are **not**
forwarded through `yield from` (the subgenerator is only advanced with
`None` — full PEP 380 send/throw delegation is unsupported);
`except GeneratorExit`
is supported; free captures use cells (late bind; load before assign
→ NameError); nested defaults freeze at def time (escaped free-var
defaults need literals); lambda params without defaults still need
annotations or defaults for inference; homogeneous closures in
containers need matching params/ret/capture-env shape; no classes /
GC yet.

Errors come with source snippets:

```
error[semantic]: type mismatch in argument 1 of 'f': expected int, found float
 --> bad.py:4:7
  |
4 | x = f(2.5)
  |       ^^^
```

## Architecture

Cargo workspace with a strict, unidirectional data flow
(see [SPECIFICATIONS.md](SPECIFICATIONS.md)):

```
source ─→ lexer ─→ parser ─→ semantic ─→ ir ─→ codegen ─→ executable
          logos    AST       typecheck   typed  LLVM IR    LLVM opt+emit,
          INDENT/  recursive + lower     tree   text       linked by cc
          DEDENT   descent
```

- **`common`** — spans and diagnostics shared by every phase
- **`lexer`** — `logos`-based scanner with an indent-stack state machine for
  Python's semantic whitespace and implicit line joining
- **`parser`** — hand-written recursive descent, precedence-layered
  expressions
- **`semantic`** — name resolution, type checking, implicit numeric
  promotion, return-path analysis; lowers AST to IR
- **`ir`** — fully typed tree; the contract handed to the backend
- **`codegen`** — emits LLVM IR text; a thin C++ shim (built via CMake)
  parses, verifies, optimizes and emits object code; a tiny C runtime
  provides Python-faithful printing and runtime traps
- **`cli`** — the driver

## Benchmarks

`benchmarks/run.sh` compiles each program with `pyrs -O2`, checks its output
is byte-identical to `python3`'s, then reports best-of-3 wall times:

| benchmark  | workload                                   | python3 | PyRs   | speedup |
|------------|--------------------------------------------|--------:|-------:|--------:|
| fib        | recursion, 12M calls (`fib(35)`)           |  1.163s | 0.025s |   45.8× |
| listcomp   | comprehensions, 3M-element map/filter      |  0.570s | 0.033s |   17.0× |
| mandelbrot | float math, 500×500 escape iterations      |  0.944s | 0.017s |   54.8× |
| matmul     | nested lists, 250×250 matrix multiply      |  0.783s | 0.018s |   44.7× |
| nbody      | float + list, 5-body gravity, 100k steps   |  1.352s | 0.008s |  172.7× |
| primes     | int loops, trial division to 300k          |  0.629s | 0.064s |    9.8× |
| sort       | list indexing, bubble sort of 5000         |  1.008s | 0.022s |   46.6× |
| strings    | per-char iteration, 2.6M comparisons       |  0.656s | 0.103s |    6.4× |
| **total**  |                                            |  6.535s | 0.257s |   25.4× |

(Linux, LLVM 22, CPython 3.14; run `./benchmarks/run.sh` to reproduce.)

v0.3 inlined list element access into the generated IR (bounds check +
direct load/store, so LLVM keeps hot values in registers) and interned
single-character strings (indexing/iterating a str allocates nothing) —
that took sort from 13× to 45×, nbody from 59× to 164×, and strings
from 3× to 6×.

## Building

Requires Rust (edition 2024), LLVM (`llvm-config` on PATH), CMake, and a C
compiler.

```console
cargo build --release
cargo test
```

### Continuous integration

GitHub Actions (see `.github/workflows/`):

| Workflow | When | What |
|----------|------|------|
| **CI** | push/PR to `main` | `fmt`, clippy, tests, example parity, opt-level smoke |
| **Benchmarks** | weekly / manual / bench-related pushes | `benchmarks/run.sh` (artifact log) |
| **Release** | tags `v*.*.*` | Linux `x86_64` tarball + checksum + GitHub Release |
| **Docs & hygiene** | docs/CI path changes | required files + workflow YAML shape |

Local gate (same spirit as CI): `make doctor && make ci`.
Release tags: `git tag v0.17.0 && git push origin v0.17.0`.