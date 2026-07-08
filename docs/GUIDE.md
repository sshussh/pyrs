# The PyRs Guide

Everything pyrs offers and how to use it: the toolchain, the CLI, the
complete language reference, how it differs from CPython, and how to get
the most speed out of it.

pyrs compiles a statically-typed subset of Python straight to native
machine code through LLVM. There is no interpreter and no VM at runtime —
`pyrs compile` hands you a standalone executable.

---

## Table of contents

1. [Installation](#1-installation)
2. [Quick start](#2-quick-start)
3. [The command line](#3-the-command-line)
4. [The Makefile](#4-the-makefile)
5. [Language reference](#5-language-reference)
   - [Program structure](#program-structure)
   - [Types](#types)
   - [Variables and assignment](#variables-and-assignment)
   - [Operators](#operators)
   - [Strings](#strings)
   - [f-strings](#f-strings)
   - [Lists](#lists)
   - [Control flow](#control-flow)
   - [Functions](#functions)
   - [Built-in functions](#built-in-functions)
   - [Comments and line continuation](#comments-and-line-continuation)
6. [Runtime errors](#6-runtime-errors)
7. [Compiler diagnostics](#7-compiler-diagnostics)
8. [Differences from CPython](#8-differences-from-cpython)
9. [Performance](#9-performance)
10. [Under the hood](#10-under-the-hood)

---

## 1. Installation

Requirements: Rust (edition 2024), LLVM with `llvm-config` on PATH, CMake,
a C compiler, and `python3` if you want to run the parity checks and
benchmarks.

```console
$ make doctor          # verify the toolchain
  ok      rustc        rustc 1.92.0
  ok      cargo        cargo 1.92.0
  ok      cmake        cmake version 4.3.4
  ok      cc           cc (GCC) 16.1.1
  ok      llvm-config  22.1.6
  ok      python3      Python 3.14.6

$ make release         # build the compiler
$ make install         # optional: install `pyrs` into ~/.cargo/bin
```

## 2. Quick start

```python
# hello.py
def fib(n: int) -> int:
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)

print(f"fib(30) = {fib(30)}")
```

```console
$ pyrs compile -i hello.py -o hello    # native executable
$ ./hello
fib(30) = 832040

$ pyrs run -i hello.py                 # or compile-and-run in one step
fib(30) = 832040
```

The executable is standalone — it does not need pyrs or Python to run.

## 3. The command line

```
pyrs <command> [options]
```

| command   | what it does |
|-----------|--------------|
| `compile` | compile a source file to a native executable |
| `run`     | compile to a temporary location and execute immediately |
| `lex`     | dump the token stream (debugging the compiler) |
| `parse`   | dump the abstract syntax tree (debugging the compiler) |

### `pyrs compile`

```console
$ pyrs compile -i prog.py -o prog [-O 2] [--emit-llvm]
```

| option | meaning | default |
|--------|---------|---------|
| `-i, --input`  | source file | required |
| `-o, --output` | output executable path | `a.out` |
| `-O, --opt-level` | LLVM optimization level, 0–3 | `2` |
| `--emit-llvm`  | also write the generated LLVM IR to `<output>.ll` | off |

`--emit-llvm` is the window into the compiler: the `.ll` file is exactly
what pyrs hands to LLVM, readable and diffable.

### `pyrs run`

```console
$ pyrs run -i prog.py [-O 2]
```

Compiles to a temporary directory, executes, cleans up, and exits with
the program's exit code (0 on success, 1 if the program traps with a
runtime error).

### `pyrs lex` / `pyrs parse`

```console
$ pyrs lex -i prog.py              # tokens to stdout
$ pyrs parse -i prog.py -o ast.txt # AST to a file
```

Both print to stdout unless `-o` is given.

## 4. The Makefile

Day-to-day work is wrapped in `make` targets; `make help` lists them all.
The interesting variables are `FILE` (source file, default `main.py`),
`O` (opt level), and `RUNS` (benchmark repetitions).

```console
$ make run FILE=examples/fib.py     # compile and run any file
$ make time FILE=main.py O=3        # race the binary against python3
$ make emit-llvm FILE=prog.py       # write prog and prog.ll
$ make examples                     # byte-diff every example vs python3
$ make bench RUNS=5                 # the benchmark suite
$ make ci                           # format check + clippy + tests + parity
```

## 5. Language reference

pyrs compiles a statically-typed subset of Python. Valid pyrs programs
are valid Python programs with the same output — the reverse is not true,
since pyrs requires type annotations on functions and rejects dynamic
features (each with a "not supported yet" error naming the feature).

### Program structure

Top-level statements are the program, executed in order like a script:

```python
x = 6
print(x * 7)      # a complete program
```

Function definitions can appear anywhere at the top level and can call
each other regardless of definition order (forward references work). If a
file contains *only* function definitions and one of them is a
zero-parameter `main`, it is called automatically:

```python
def main():
    print("entry point")
# prints "entry point" — no explicit call needed
```

**Scoping follows Python.** A function reads its own locals first, then
the module globals (top-level variables). Assigning inside a function
creates a *local* that shadows any global of the same name — to write a
global, declare it first:

```python
counter = 0

def bump():
    global counter      # without this, counter = ... would be a new local
    counter += 1

bump()
print(counter)          # 1
```

`nonlocal` is not supported (there are no nested functions yet).

### Types

| type | representation | notes |
|------|----------------|-------|
| `int`   | 64-bit signed integer | wraps on overflow (no bigints) |
| `float` | IEEE-754 double | |
| `bool`  | `True` / `False` | assignable where int/float is expected |
| `str`   | immutable string | heap-allocated, length-prefixed |
| `list[T]` | growable homogeneous list | `T` is any type incl. another list (`list[list[float]]`) |

Implicit promotions (mypy-flavored): `bool → int → float`. They apply in
arithmetic, assignments, arguments, and returns:

```python
def halve(x: float) -> float:
    return x / 2

print(halve(7))        # int argument promotes: 3.5
n: int = True          # bool is assignable to int: n == 1
```

### Variables and assignment

A variable's type is fixed by its first assignment and cannot change:

```python
x = 1          # x: int, inferred
x = 2.5        # error: variable 'x' already has type int

y: float = 1   # annotated; the int promotes to 1.0
```

All assignment forms:

```python
name = expr                # plain
name: type = expr          # annotated (required for empty lists)
name += expr               # augmented: += -= *= /= //= %= **=
xs[i] = expr               # list element
xs[i] += expr              # augmented element (base and index evaluate once)
```

Not supported: multiple targets (`a = b = 1`), tuple unpacking
(`a, b = 1, 2`), and annotations on anything but a plain name.

### Operators

From lowest to highest precedence:

| level | operators |
|-------|-----------|
| 1 | `or` |
| 2 | `and` |
| 3 | `not x` |
| 4 | `==  !=  <  <=  >  >=  in  not in` (chainable) |
| 5 | `+  -` |
| 6 | `*  /  //  %` |
| 7 | `-x  +x` (unary) |
| 8 | `**` (right-associative) |
| 9 | calls, `x[i]`, `x[a:b]`, `x.method()` |

Arithmetic follows Python's semantics exactly:

```python
print(7 / 2)       # 3.5   — true division always yields float
print(-7 // 2)     # -4    — floor division rounds toward -inf
print(-7 % 3)      # 2     — remainder takes the divisor's sign
print(-2 ** 2)     # -4    — ** binds tighter than unary minus
print(2 ** -1)     # 0.5   — negative constant exponent gives float
print(2 ** 3 ** 2) # 512   — right-associative
```

Comparisons chain like Python, evaluating each operand once and
short-circuiting:

```python
if 0 <= x < len(xs):    # exactly one evaluation of each operand
    ...
```

`and` / `or` / `not` accept any value via truthiness (nonzero numbers,
non-empty strings and lists are truthy) and short-circuit. They return
`bool` — see [Differences from CPython](#8-differences-from-cpython).

`in` / `not in` test substrings and list membership:

```python
"ell" in "hello"        # True
3 in [1, 2, 3]          # True
"c" not in ["a", "b"]   # True
```

### Strings

Immutable, with literals in single or double quotes. Escapes: `\n` `\t`
`\r` `\0` `\\` `\'` `\"` (unknown escapes are kept verbatim).

```python
s = "hello" + " " + "world"    # concatenation
line = "-" * 20                # repetition (either operand order)
print("apple" < "banana")      # lexicographic comparison (all six ops)
print(s[0], s[-1])             # indexing, negative from the end
print(s[6:], s[:5], s[::-1])   # slicing with steps (see below)
print(len(s))                  # length
for c in s:                    # iterate characters (each is a 1-char str)
    print(c)
label = str(42)                # str() converts int/float/bool
```

Slicing takes `[lo:hi:step]` with any part omitted, exactly like Python:
negative bounds count from the end, out-of-range bounds clamp, an empty
range gives `""`, and negative steps walk backwards — `s[::-1]` reverses,
`s[8:2:-2]` == `"rwo"` for `"hello world"`. A zero step raises
`ValueError`.

String methods (ASCII case/whitespace rules):

```python
"  hi  ".strip()          # also lstrip / rstrip
"abc".upper()             # "ABC"; .lower() too
"hello".startswith("he")  # True; .endswith too
"banana".find("an")       # 1, or -1 when absent
"banana".count("an")      # 2 (non-overlapping)
"banana".replace("an", "-")   # "b--a"
"a,b,,c".split(",")       # ['a', 'b', '', 'c'] — keeps empty parts
"a b  c".split()          # ['a', 'b', 'c']    — whitespace runs
"-".join(["a", "b"])      # "a-b"
```

### f-strings

```python
name = "world"
n = 42
print(f"hello {name}, n={n}, next={n + 1}")
print(f"{{literal braces}} and {f'nested {n}'}")
```

Any expression can appear inside `{...}`, including slices, calls, and
nested f-strings. Interpolated values are converted with the `str()`
rules (int/float/bool/str). `{{` and `}}` produce literal braces.

Not supported yet: format specifiers (`{x:.2f}`) and conversions
(`{x!r}`) — both produce a targeted compile error suggesting `str(x)`.

### Lists

Homogeneous and growable. The element type comes from the literal, or
from an annotation when the literal is empty:

```python
xs = [1, 2, 3]              # list[int], inferred
ys = [1, 2.5]               # list[float] — numeric join promotes the 1
zs: list[str] = []          # empty literals need an annotation
```

(The annotation's element type also propagates into function arguments
and returns: `f([])` works when the parameter is `list[T]`.)

Operations:

```python
xs[0] = 10                  # element write
print(xs[-1])               # negative indexing
xs.append(4)                # grow
last = xs.pop()             # remove & return last
first = xs.pop(0)           # or by index (negative OK)
print(xs[1:3], xs[:2])      # slicing copies, like Python
print(len(xs), 2 in xs)     # length, membership
for x in xs:                # iteration
    print(x)
print(xs)                   # Python repr: [1, 2, 3] / ['a', 'b']
```

Two behaviors carried over from Python that surprise people:

```python
ys = xs          # ALIASES — both names refer to the same list
ys[0] = 99       # xs[0] is now 99 too
ys = xs[:]       # a slice makes an independent copy

for x in xs:     # iteration re-reads the live length,
    xs.append(x) # so appending inside the loop extends it (careful!)
```

List comprehensions work with an optional filter and follow Python 3
scoping (the variable shadows inside and does not leak):

```python
squares = [x * x for x in range(10)]
big = [w.upper() for w in words if len(w) > 3]
matrix = [[v * 2 for v in row] for row in grid]
```

They are also the fastest way to build a list: the compiler pre-sizes
the result when the length is knowable and inlines the appends, making
a comprehension ~2.4x faster than the equivalent append loop. Multiple
`for`/`if` clauses in one comprehension are not supported yet — nest
comprehensions instead.

Lists slice with steps too: `xs[::-1]` reverses, `xs[::2]` takes every
other element. Lists nest — `grid[i][j]`, `grid[i][j] = v`, and printing
all work:

```python
grid = [[1, 2], [3, 4]]
grid[0][1] = 20
print(grid)            # [[1, 20], [3, 4]]
m: list[list[str]] = []
m.append(["a", "b"])
```

Not supported yet: list `+`/`*`/`==`, `insert`/`remove`/`index`/`sort`,
`in` on lists of lists, and slice assignment.

### Control flow

```python
if x < 0:
    print("negative")
elif x == 0:
    print("zero")
else:
    print("positive")

while x > 0:                 # any truthy condition
    x -= 1
    if x == 3: break         # single-line suites work
    if x % 2: continue

for i in range(10): ...      # 0..9
for i in range(2, 10): ...   # 2..9
for i in range(10, 0, -3):   # 10, 7, 4, 1
    print(i)
for x in [1, 2, 3]: ...      # over a list
for c in "hello": ...        # over a string's characters
```

`range()` is lazy (no list is materialized) and accepts any int
expressions; a step of zero raises `ValueError` at runtime (or at compile
time when it's a constant). The loop variable keeps its final value after
the loop, like Python. `break`/`continue` work in both loop kinds;
`continue` in a `for` still advances the iteration.

Not supported yet: `for`/`while` `else` clauses.

### Functions

```python
def clamp(x: float, lo: float, hi: float) -> float:
    if x < lo:
        return lo
    if x > hi:
        return hi
    return x
```

- Parameter annotations are **required**; the return annotation defaults
  to "returns nothing" when omitted.
- A function declared `-> T` must return on every path — the compiler
  checks this (an infinite `while True:` without `break` counts as
  not falling through).
- Recursion and mutual recursion work; functions may be called before
  their definition in the file.
- Function names are internally prefixed, so naming a function `printf`
  or `malloc` cannot collide with the C library.

Not supported yet: default values, keyword arguments, `*args`, nested
functions, closures, `lambda`, and redefining a function.

### Built-in functions

| builtin | accepts | returns |
|---------|---------|---------|
| `print(a, b, ...)` | any values, any count | space-separated, newline at end |
| `len(x)` | str, list | int |
| `range(...)` | 1–3 ints | only as a `for` iterable |
| `global x` | (statement) | write access to a module global |
| `input([prompt])` | optional str prompt | line from stdin (no newline); `EOFError` at EOF |
| `open(path[, mode])` | str path, mode "r"/"w"/"a" | file value with read/readline/readlines/write/close |
| `sys.argv` | needs `import sys` | list[str]; `[0]` is the binary path |
| `int(x)` | int, float (truncates toward zero), bool | int |
| `float(x)` | int, float, bool | float |
| `bool(x)` | int, float, bool, str, list (truthiness) | bool |
| `str(x)` | int, float, bool, str | str |

`print` formatting matches CPython: floats use the shortest
representation that round-trips (`0.1 + 0.2` → `0.30000000000000004`,
`10.0` → `10.0`, `1e16` → `1e+16`), bools print `True`/`False`, lists
print `[1, 2, 3]` / `['a', 'b']`. The builtins cannot be redefined.

### Comments and line continuation

```python
total = 0  # comments run to end of line

value = (1 +          # newlines inside (), [], {} are joined implicitly
         2 + 3)

value = 1 + \
        2             # explicit backslash continuation also works
```

Indentation defines blocks; spaces and tabs both work (a tab counts as 8
columns) but inconsistent dedents are an error. Number literals accept
underscores (`1_000_000`), floats accept `1.5`, `.5`, `2.`, `1e3`,
`2.5e-2`.

### Files

`open(path)` (read), `open(path, "w")` (write/truncate), and
`open(path, "a")` (append) return a file value:

```python
out = open("report.txt", "w")
out.write("hello\n")           # returns the character count
out.close()                    # idempotent, like Python

f = open("report.txt")
text = f.read()                # everything remaining
f.close()

for line in open("report.txt").readlines():   # lines keep their '\n'
    print(line.strip())
```

`with` works for files and guarantees the close on every exit path,
including early `return`/`break` (the return value is evaluated before
the close, so `return f.read()` behaves exactly like Python):

```python
with open("report.txt") as f:
    text = f.read()

def first_line(p: str) -> str:
    with open(p) as fh:
        return fh.readline().strip()
```

`readline()` returns `""` at end of file. Errors match CPython exactly:
missing files raise `FileNotFoundError: [Errno 2] ...`, operations on a
closed file raise `ValueError: I/O operation on closed file.`, and
reading a write-mode file raises `io.UnsupportedOperation: not
readable`. Writes are flushed immediately, so data survives even if you
forget `close()`.

Not supported yet: binary modes, iterating the
file object directly (use `.readlines()`), printing file objects,
multiple context managers in one `with`, `with` on non-files, and
file-typed function parameters (there is no `file` annotation — handles
are inferred locally).

### Standard input and arguments

```python
import sys

name = input("who? ")          # prints the prompt, reads a line
print(f"hello {name}")
for arg in sys.argv[1:]:       # arguments after the program name
    print(arg)
```

`pyrs run` forwards trailing arguments: `pyrs run -i tool.py a b c`.
For a compiled binary they're just process arguments: `./tool a b c`.
`sys.argv[0]` is the binary path (Python shows the script path — the
only structural difference).

## 6. Runtime errors

There are no exceptions to catch (yet) — Python-style runtime errors
print the same message CPython would, then exit with code 1:

| error | raised by |
|-------|-----------|
| `ZeroDivisionError: division by zero` | `/`, `//`, `%` with a zero divisor; `0.0 ** negative` |
| `IndexError: string index out of range` | out-of-bounds `s[i]` |
| `IndexError: list index out of range` | out-of-bounds `xs[i]` read |
| `IndexError: list assignment index out of range` | out-of-bounds `xs[i] = v` |
| `IndexError: pop from empty list` / `pop index out of range` | `xs.pop(...)` |
| `ValueError: range() arg 3 must not be zero` | zero range step at runtime |
| `ValueError: slice step cannot be zero` | zero slice step at runtime |
| `ValueError: empty separator` | `s.split("")` |
| `EOFError: EOF when reading a line` | `input()` at end of stdin |
| `FileNotFoundError: [Errno 2] ...` / `PermissionError` / `IsADirectoryError` | `open()` failures |
| `ValueError: I/O operation on closed file.` | using a closed file |
| `io.UnsupportedOperation: not readable` / `not writable` | wrong-mode file operations |
| `ValueError: cannot convert float NaN to integer` | `int(nan)` |
| `OverflowError: cannot convert float infinity to integer` | `int(inf)` |
| `ValueError: integer to a negative power...` | `x ** e` with a dynamic negative int `e` |
| `UnboundLocalError: value used before assignment` | reading a str/list variable assigned on no path |
| `MemoryError: out of memory` | allocation failure |

```console
$ pyrs run -i crash.py
ZeroDivisionError: division by zero
$ echo $?
1
```

## 7. Compiler diagnostics

Every phase reports errors against your source with a caret:

```
error[semantic]: type mismatch in argument 1 of 'f': expected int, found float
 --> prog.py:4:7
  |
4 | x = f(2.5)
  |       ^^^
```

The phase tag tells you what stage rejected the program: `lex` (bad
character, bad indentation), `parse` (syntax), `semantic` (names, types,
return paths), `codegen` (internal — you should never see one; it means a
compiler bug). Unsupported Python features produce parse/semantic errors
that name the feature: `classes are not supported yet`,
`slice steps are not supported yet`, and so on. Compilation stops at the
first error.

## 8. Differences from CPython

Everything pyrs *does* support behaves like Python — these are the known,
deliberate exceptions:

1. **`int` is 64-bit** and wraps on overflow; there are no big integers.
   Integer literals beyond ±2⁶³ don't lex.
2. **`and`/`or` return `bool`**, not the winning operand
   (`0 or "x"` is `True`, not `"x"`).
3. **Static types.** Variables can't change type; lists are homogeneous
   (mixed numeric literals coerce to the joined type — `[1, 2.5]` becomes
   `[1.0, 2.5]`); annotations on parameters are mandatory. Promotions
   convert the value itself, so `n: int = True` makes `n` print as `1`
   where Python keeps the bool and prints `True` (they still compare
   equal).
4. **`x ** e` with a dynamic negative int exponent traps** at runtime
   (constant exponents like `2 ** -1` correctly give a float).
5. **int↔float comparisons convert the int to float**, losing exactness
   above 2⁵³ (Python compares exactly).
6. **No identity semantics**: `nan in [nan]` is `False` (Python's
   membership checks identity first).
7. **Possibly-unbound variables** read as `0`/`0.0`/`False` for scalars;
   str/list reads trap with `UnboundLocalError`. (Straight-line
   use-before-assignment is caught at compile time.)
8. **str methods use ASCII rules** for case (`upper`/`lower`) and
   whitespace (`strip`/`split`) — Python is Unicode-aware.
9. **Memory is never freed** (no GC yet); fine for short-lived programs,
   a known limitation for long-running ones.
10. **`float ** float` with a negative base and fractional exponent**
    gives `nan` (Python returns a complex number).

Not implemented yet (clear compile errors): classes, dicts, sets, tuples,
imports, exceptions, `with`, `match`, generators/`yield`, `lambda`,
nested functions, closures, `nonlocal`, f-string format specs,
keyword/default arguments, multiple assignment, and unpacking.

## 9. Performance

Measured by `make bench` (best of 3, output byte-verified against
python3 before timing — Linux, LLVM 22, CPython 3.14):

| benchmark  | workload                        | python3 | pyrs   | speedup |
|------------|---------------------------------|--------:|-------:|--------:|
| fib        | recursion, 12M calls            |  1.158s | 0.027s |   42.6× |
| mandelbrot | float math, 500×500             |  0.953s | 0.018s |   53.9× |
| nbody      | float + list, 100k steps        |  1.379s | 0.008s |  164.4× |
| primes     | int loops, trial division       |  0.631s | 0.063s |   10.0× |
| sort       | list indexing, bubble sort      |  1.011s | 0.022s |   45.2× |
| strings    | per-char iteration              |  0.675s | 0.112s |    6.0× |
| **total**  |                                 |  5.807s | 0.251s |   23.2× |

What makes code fast in pyrs:

- **Numeric loops and recursion** compile to the same machine code C
  would get — expect 40–160×.
- **List indexing is inlined** (bounds check + direct load/store), so
  array algorithms run near-native.
- **String building with `+` in a loop is O(n²)** (no CPython in-place
  append trick) — and per-character work costs a runtime call. Strings
  are the slowest corner today.
- `-O2` is the default; `-O3` occasionally helps hot float code; `-O0`
  compiles fastest for debugging.

## 10. Under the hood

```
source ─→ lexer ─→ parser ─→ semantic ─→ ir ─→ codegen ─→ cc link ─→ binary
          logos    AST       typecheck   typed  LLVM IR    + C runtime
          INDENT/  recursive + lower     tree   text  ↓
          DEDENT   descent                      C++ shim: parse, verify,
                                                optimize (O0–O3), emit .o
```

Each stage is its own crate (`common`, `lexer`, `parser`, `semantic`,
`ir`, `codegen`, `cli`). The Rust side emits LLVM IR as *text* — run
`pyrs compile --emit-llvm` to read it — and a thin C++ shim drives LLVM's
parser, verifier, optimizer, and object emitter. A small C runtime
(`codegen/runtime/runtime.c`) supplies Python-faithful printing
(shortest round-trip floats), string/list storage, and the runtime error
traps; it is compiled and linked into every binary by `cc`.

Details and build strategy: [SPECIFICATIONS.md](../SPECIFICATIONS.md).
Worked examples live in [examples/](../examples), benchmarks in
[benchmarks/](../benchmarks).
