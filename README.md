# PyRs

A Python compiler built in Rust, targeting native code through LLVM.

PyRs compiles a statically-typed subset of Python straight to machine code —
no interpreter, no VM. `fib(35)` runs ~40× faster than CPython.

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

## The language (v0.1)

A statically-typed Python subset:

- **Types:** `int` (i64), `float` (f64), `bool`; string literals in `print`
- **Functions:** `def` with mandatory parameter annotations
  (`def f(x: int) -> int:`), recursion, forward references
- **Statements:** `if`/`elif`/`else`, `while`, `break`/`continue`,
  assignments (plain, annotated, augmented), `return`, `pass`
- **Expressions:** full arithmetic with Python semantics, comparisons,
  `and`/`or`/`not` (short-circuit), casts `int()`/`float()`/`bool()`,
  `print(...)` with any mix of values
- **Entry point:** top-level statements run like a script; if there are
  none, a zero-argument `main()` is called automatically

Python semantics are preserved where it counts:

- `7 / 2 == 3.5` — true division always yields float
- `-7 // 2 == -4`, `-7 % 3 == 2` — floored division and modulo
- division by zero raises `ZeroDivisionError` (exit 1) instead of UB
- floats print with shortest round-trip representation
  (`0.1 + 0.2` → `0.30000000000000004`, `1.0` → `1.0`)
- variables use function-wide scoping; a variable's type is fixed by its
  first assignment

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

## Building

Requires Rust (edition 2024), LLVM (`llvm-config` on PATH), CMake, and a C
compiler.

```console
cargo build --release
cargo test
```
