# PyRs

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

## The language (v0.6)

A statically-typed Python subset:

- **Types:** `int` (i64), `float` (f64), `bool`, `str`, `list[T]` —
  including nested lists (`list[list[float]]` matrices)
- **Functions:** `def` with mandatory parameter annotations
  (`def f(x: int) -> int:`), recursion, forward references
- **Statements:** `if`/`elif`/`else`, `while`,
  `for x in range(...)` / lists / strings, `break`/`continue`,
  assignments (plain, annotated, augmented — including `xs[i] += v`),
  `return`, `pass`
- **Expressions:** full arithmetic including `**`, comparisons with
  chaining (`0 < x < 10`), `in`/`not in` (substring and membership),
  `and`/`or`/`not` (short-circuit), casts
  `int()`/`float()`/`bool()`/`str()`, `len()`, indexing with negative
  indices, full slicing `s[a:b:c]` including `[::-1]` reversal,
  `print(...)` with any mix of values
- **f-strings:** `f"x={x}, next={x + 1}"` with `{{`/`}}` escapes and
  nesting (no format specs yet — write `{str(x)}` style conversions)
- **Strings:** immutable; `+` concat, `*` repeat, lexicographic
  comparisons, indexing, slicing, `in`, iteration, `len()`, `str(x)`
  conversions, and methods: `upper` `lower` `strip` `lstrip` `rstrip`
  `startswith` `endswith` `find` `count` `replace` `split` `join`
- **Lists:** homogeneous, growable; literals, indexing (read/write),
  slicing (copies, like Python), `append`/`pop`, `in`, `len`, iteration;
  assignment aliases like Python
- **Globals:** top-level variables are readable from any function;
  writing needs a `global x` declaration, exactly like Python
- **I/O:** `input([prompt])` from stdin; `import sys` + `sys.argv` for
  command-line arguments; files via `open(path, mode)` with
  `.read()`/`.readline()`/`.readlines()`/`.write()`/`.close()`,
  `with open(...) as f:` blocks, and CPython's exact error messages —
  compiled programs are real CLI tools
- **Entry point:** top-level statements run like a script; if there are
  none, a zero-argument `main()` is called automatically

Python semantics are preserved where it counts:

- `7 / 2 == 3.5` — true division always yields float
- `-7 // 2 == -4`, `-7 % 3 == 2` — floored division and modulo
- `-2 ** 2 == -4`, `2 ** -1 == 0.5`, right-associative `2 ** 3 ** 2`
- `1 < middle() < 10` evaluates `middle()` exactly once and
  short-circuits, exactly like Python
- `ZeroDivisionError`, `IndexError`, `ValueError` trap with exit 1
  instead of being undefined behavior
- floats print with shortest round-trip representation
  (`0.1 + 0.2` → `0.30000000000000004`, `1.0` → `1.0`); lists print as
  `[1, 2, 3]` / `['a', 'b']`
- iterating a list re-reads the live length, so appending inside the
  loop behaves like CPython
- variables use function-wide scoping; a variable's type is fixed by its
  first assignment

Known limits (v0.6): no bigints (int is 64-bit and wraps), `and`/`or`
return `bool` rather than the operand, `x ** e` with a *dynamic*
negative int exponent traps (a constant like `2 ** -1` works and gives
float), int↔float comparisons convert the int to float (exactness loss
past 2^53), list literals coerce mixed numerics to one element type,
`nan in [nan]` is False (no identity semantics), str methods use ASCII
case/whitespace rules, heap memory is never freed, files support text
modes "r"/"w"/"a" only (no `for line in f`, no file-typed
parameters yet; `with` works for files only), and f-string format
specs / dicts / tuples / classes / exceptions are not in yet — the
parser reports "not supported yet" for each.

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

| benchmark  | workload                                   | python3 | pyrs   | speedup |
|------------|--------------------------------------------|--------:|-------:|--------:|
| fib        | recursion, 12M calls (`fib(35)`)           |  1.163s | 0.025s |   45.8× |
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
