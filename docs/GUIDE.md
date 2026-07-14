# The PyRs Guide

Everything PyRs offers and how to use it: the toolchain, the CLI, the
complete language reference, how it differs from CPython, and how to get
the most speed out of it.

PyRs compiles a statically-typed subset of Python straight to native
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
   - [Files](#files)
   - [Standard input and arguments](#standard-input-and-arguments)
   - [Modules and packages](#modules-and-packages)
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

The executable is standalone — it does not need PyRs or Python to run.

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
what PyRs hands to LLVM, readable and diffable.

### `pyrs run`

```console
$ pyrs run -i prog.py [-O 2]
```

When the program imports other modules or packages, the whole import graph is
compiled and linked into the one executable. `run` compiles to a
temporary directory, executes, cleans up, and exits with the program's
exit code (0 on success, 1 if the program traps with a
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

PyRs compiles a statically-typed subset of Python. Valid PyRs programs
are valid Python programs with the same output — the reverse is not true,
since PyRs requires type annotations on functions and rejects dynamic
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

`int`, `float`, `bool`, `str`, `file`, `list[T]`, `tuple[T1, T2, …]`
(empty: `tuple[()]`), `dict[K, V]`, `set[T]`, and `None` (return only).
Dict keys and set elements are restricted to `int` and `str`.


| type | representation | notes |
|------|----------------|-------|
| `int`   | 64-bit signed integer | wraps on overflow (no bigints) |
| `float` | IEEE-754 double | |
| `bool`  | `True` / `False` | assignable where int/float is expected |
| `str`   | immutable string | heap-allocated, length-prefixed |
| `file`  | open file handle | from `open(...)`; usable in params/returns; not in lists |
| `list[T]` | growable homogeneous list | `T` is any type incl. another list (`list[list[float]]`); not `file` |
| `tuple[T1, …]` | fixed-arity heterogeneous tuple | empty `tuple[()]`; index / len / unpack / iterate (homogeneous) |
| `dict[K, V]` | hash map, insertion order | `K` is `int` or `str`; empty `{}` needs annotation |
| `set[T]` | hash set, insertion order | `T` is `int` or `str`; empty via `s: set[int] = set()` |

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
a = b = expr               # multi-target; RHS once, assign right-to-left
name: type = expr          # annotated (required for empty lists)
name += expr               # augmented: += -= *= /= //= %= **=
xs[i] = expr               # list element
xs[i] += expr              # augmented element (base and index evaluate once)
a = xs[i] = expr           # multi-target with index (shares the value)
```

Unpacking works: `a, b = 1, 2`, `a, b = t` (tuple or list RHS; length
mismatch traps like CPython). Annotations only on a plain single name
(no `a: int = b = 0`).

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
"banana".rfind("an")      # 3 (last occurrence); "".rfind is -1; empty needle -> len
"banana".rindex("an")     # like rfind, but ValueError if missing
"banana".count("an")      # 2 (non-overlapping)
"banana".replace("an", "-")   # "b--a"
"a,b,,c".split(",")       # ['a', 'b', '', 'c'] — keeps empty parts
"a b  c".split()          # ['a', 'b', 'c']    — whitespace runs
"-".join(["a", "b"])      # "a-b"
"123".isdigit()           # True; "" and non-digits are False
"abc".isalpha()           # True; ASCII letters only
" \\t".isspace()          # True; same whitespace set as strip/split
"ABC".isupper()           # True; "AbC" / digits-only are False
"a1".islower()            # True if all letters are lower and >=1 letter
```

### f-strings

```python
name = "world"
n = 42
pi = 3.14159
print(f"hello {name}, n={n}, next={n + 1}")
print(f"{{literal braces}} and {f'nested {n}'}")
print(f"pi={pi:.2f}")          # fixed-point: "pi=3.14"
print(f"{n:.2f}")              # int/bool promoted: "2.00"
```

Any expression can appear inside `{...}`, including slices, calls, and
nested f-strings. Interpolated values are converted with the `str()`
rules (int/float/bool/str). `{{` and `}}` produce literal braces.

**Format specs:** `{x:.Nf}` (fixed-point with `N` digits after the
decimal) is supported for `int`/`float`/`bool`. Other format codes
(`e`, `g`, width/alignment, …) and conversions (`{x!r}`, `{x!s}`) are
not supported yet and produce a targeted compile error.

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

List mutators: `append`, `pop([i])`, `insert(i, v)`, `remove(v)`,
`index(v)`, `clear()`, `sort()`. `insert` clamps the index like CPython;
`remove` / `index` trap with `ValueError` when the value is missing.
`sort()` is in-place (statement only); `sorted(xs)` returns a new sorted
copy. Supported element types: `int`, `float`, `bool`, `str` (no
`key=`/`reverse=` yet). Float NaN sorts last (stable total order).

List `+` concatenates (same element type); `*` repeats with an int count
(`n <= 0` yields `[]`). Both produce a new list (shallow copy of slots).
`==` / `!=` compare length and elements (same rules as `in`; nested lists
compare recursively).

Not supported yet: list ordering comparisons (`<` etc.), `in` on lists of
lists (membership still limited), and slice assignment.

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

# else runs only when the loop ends without break
for x in xs:
    if x < 0:
        break
else:
    print("all non-negative")

while n > 0:
    n -= 1
else:
    print("counted down")
```

`range()` is lazy (no list is materialized) and accepts any int
expressions; a step of zero raises `ValueError` at runtime (or at compile
time when it's a constant). The loop variable keeps its final value after
the loop, like Python. `break`/`continue` work in both loop kinds;
`continue` in a `for` still advances the iteration. A `for`/`while` `else`
clause runs when the loop finishes normally (including zero iterations)
and is skipped if the loop exits via `break`.

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
- Default values and keyword arguments work (`def f(a: int, b: int = 1)`
  and `f(1, b=2)`). Defaults are re-evaluated at each call that needs
  them (so `def f(xs: list[int] = [])` does not share one list across
  calls — a deliberate deviation from CPython). No `*args` / `**kwargs`.
- A function declared `-> T` must return on every path — the compiler
  checks this (an infinite `while True:` without `break` counts as
  not falling through).
- Recursion and mutual recursion work; functions may be called before
  their definition in the file.
- Function names are internally prefixed, so naming a function `printf`
  or `malloc` cannot collide with the C library.

Not supported yet: `*args` / `**kwargs`, nested functions, closures,
`lambda`, and redefining a function.

### Built-in functions

| builtin | accepts | returns |
|---------|---------|---------|
| `print(a, b, ...)` | any values, any count | space-separated, newline at end |
| `len(x)` | str, list, tuple, dict, set | int |
| `abs(x)` | int, float, bool | same numeric type (`bool` → `int`; `abs(True)` is `1`) |
| `min(a, b)` / `max(a, b)` | int, float, bool | common numeric type via `bool` → `int` → `float` (ties keep the first arg; 2-arg only) |
| `sum(xs)` | `list[int]` or `list[float]` | element type (`0` / `0.0` if empty; no `start=`) |
| `sorted(xs)` | `list[int\|float\|bool\|str]` | new sorted list (no `key=`/`reverse=`) |
| `range(...)` | 1–3 ints | only as a `for` iterable |
| `set()` | empty only; needs annotation | `s: set[int] = set()` |
| `global x` | (statement) | write access to a module global |
| `input([prompt])` | optional str prompt | line from stdin (no newline); `EOFError` at EOF |
| `open(path[, mode])` | str path, mode "r"/"w"/"a" | file value with read/readline/readlines/write/close |
| `sys.argv` | needs `import sys` | list[str]; `[0]` is the binary path |
| `int(x)` | int, float (truncates toward zero), bool | int |
| `float(x)` | int, float, bool | float |
| `bool(x)` | int, float, bool, str, list, tuple, dict, set | bool |
| `str(x)` | int, float, bool, str | str |

`print` formatting matches CPython: floats use the shortest
representation that round-trips (`0.1 + 0.2` → `0.30000000000000004`,
`10.0` → `10.0`, `1e16` → `1e+16`), bools print `True`/`False`, lists
print `[1, 2, 3]` / `['a', 'b']`, and tuples/dicts/sets print like
CPython. The builtins cannot be redefined.

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

# direct file iteration (readline until "")
f = open("report.txt")
for line in f:
    print(line.strip())
f.close()
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

`readline()` returns `""` at end of file. `for line in f` stops on that
empty string (lines keep their trailing `\n` when present). Errors match
CPython exactly: missing files raise `FileNotFoundError: [Errno 2] ...`,
operations on a closed file raise `ValueError: I/O operation on closed
file.`, and reading a write-mode file raises
`io.UnsupportedOperation: not readable`. Writes are flushed immediately,
so data survives even if you forget `close()`.

File parameters and returns use the `file` annotation:

```python
def first_line(f: file) -> str:
    return f.readline().strip()

def open_it(path: str) -> file:
    return open(path)
```

Not supported yet: binary modes, printing file objects, multiple context
managers in one `with`, `with` on non-files, and `list[file]`.

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

### Modules and packages

Split a program across files and packages. A directory with `__init__.py`
counts as a package.

**Import search order** (stacked; first hit wins — user code shadows the
stdlib):

1. Directory of the **entry script** (like `sys.path[0]` for `python main.py`)
2. **`PYRS_STDLIB`** if set and is a directory (dev/test override)
3. Workspace **`stdlib/`** when present next to the checkout (dev convenience;
   searched even when `PYRS_STDLIB` is set — not XOR)
4. **Embedded** stdlib baked into the `pyrs` binary (always available; a
   relocated compiler needs no companion `stdlib/` directory)

**No split packages:** once a top-level package is found under one origin
(entry / env / workspace / embed), its submodules resolve only there. An
incomplete user `os/` package does **not** pick `os.path` from the stdlib.

Compiled user programs remain standalone natives: stdlib sources are
compiled into the program at compile time once modules load.

```python
# geometry.py                          # utilpkg/__init__.py  (package)
PI = 3.14159                           # utilpkg/mathx.py
def circle_area(r: float) -> float:    #   def square(n: int) -> int: ...
    return PI * r * r

# main.py
import geometry
import geometry as geo
from geometry import PI
import utilpkg.mathx                   # loads utilpkg/__init__.py then mathx
import utilpkg.mathx as m              # alias binds the leaf module
from utilpkg.mathx import square
from utilpkg import mathx              # submodule as a local name

print(geometry.circle_area(2.0))
print(utilpkg.mathx.square(6), m.square(3))
```

### Standard library (subset)

Sources live under the repo `stdlib/` tree and are embedded into `pyrs` at
compiler build time. Prefer real package imports — only `sys` is special-cased.

| Module | Surface | Notes |
|--------|---------|-------|
| `sys` | `sys.argv` | Special-cased (not a `.py` file) |
| `os.path` | `join(a, b)`, `dirname(p)`, `basename(p)` | Pure PyRs; **POSIX** only; `join` is **two-argument** (no `*args`) |

```python
from os.path import join, dirname, basename
import os.path

print(join("a", "b"))           # a/b
print(join("a", "/b"))          # /b  (absolute second wins)
print(dirname("/a/b/c"))        # /a/b
print(basename("/a/b/c"))       # c
print(os.path.join("x", "y"))
```

`import os` works (`os/__init__.py` re-exports `path`). There is **no** full
`os` (no `getcwd`, environment, process APIs) and **no** `math` or other
batteries yet.

Relative imports are allowed **inside packages** (same rules as CPython
for the cases we claim):

```python
# utilpkg/a.py
from . import b            # sibling submodule
from .b import Z           # name from sibling
from ..other import x      # parent package (when nested)
```

- `import M` / `import M as A` — module functions and globals as
  `M.func(...)` / `M.value`.
- `import pkg.mod` — binds the top-level name `pkg`; access the leaf as
  `pkg.mod....`. Parent packages initialize first.
- `import pkg.mod as m` — binds `m` to the leaf module.
- `from M import a, b as c` — bring names into scope (functions, globals,
  or submodules of a package).
- **Package re-exports:** a package `__init__.py` may
  `from .mod import name` (or `from . import mod`). Those names are then
  available as `pkg.name` and `from pkg import name`.
- **Last top-level binding (package exports):** for `from pkg import name`
  / `pkg.name`, PyRs uses the last **top-level** binding of `name` in the
  package body, with CPython’s `hasattr` short-circuit on fromlist:
  - `name = …` / `def name` / `from other import name` bind a **value or
    function** export.
  - `from . import name` (or `from pkg import name` inside the package)
    binds a **submodule** only when `name` is **not** already bound as a
    value/function on the package. If it is already bound, the prior
    export is kept and the submodule file is **not** loaded or run.
  - Later assign/`def` after a submodule import overwrites the export
    with a value/function (last binding wins).
  - Tested orders: assign-then-`from . import`, `def`-then-`from . import`,
    value re-export-then-`from . import`, `from . import` then assign,
    and pure submodule `from . import mod`.
- **Partial package init:** while a package `__init__` imports a child:
  - At the child **module top level**, only **simple** parent assignments
    (literals / annotated assigns) and `def`s that appear **before** the
    child-loading import are visible — e.g. `VERSION = 1` then
    `from .mod import f` allows the child `from . import VERSION`. Names
    bound only after that import are a compile error at module top level
    (`cannot import name '…' from partially initialized package`).
  - Inside child **function bodies**, parent attributes and calls resolve
    with **deferred** lookup (CPython parity): names assigned or defined
    later in the parent body are OK when the function runs after the
    parent finishes initializing (`utilpkg.AFTER`, `utilpkg.g()`).
  - Only simple assignments are typed on the partial/deferred value
    surface today (e.g. `VERSION = make()` before the child import is not
    visible as a partial value).
- A module's top-level code runs **once**, at the point its first
  `import` is reached (depth-first, like Python).
- To mutate another module's global, call a function in that module
  that uses `global` (assigning `M.x = v` from outside is not
  supported); `from M import x` then reassigning `x` just makes a local,
  as in Python.

Errors are compile-time and point at the offending file: missing modules
(`No module named 'foo'`), intermediate non-packages
(`'foo' is not a package`), missing names
(`cannot import name 'bar' from 'foo'`), import cycles between unrelated
modules, relative imports outside a package, and relative imports that
go above the top-level package.

Still unsupported: `from M import *`, namespace packages (no
`__init__.py`), multi-name `import a, b` on one line, imports inside
functions or other blocks (only **module top-level** statements),
modules as first-class values beyond attribute/call chains,
re-assigning another module's attributes from outside, and a package
**importing itself** by name (`import utilpkg` inside
`utilpkg/__init__.py` is a compile error).

## 6. Runtime errors

Uncaught runtime errors print the same message CPython would, then exit
with code 1. Inside `try`/`except`, the same traps transfer to a matching
handler instead of exiting:

```python
try:
    raise ValueError("bad")
except ValueError as e:
    print(e)          # message body only (str)
finally:
    print("done")
```

Supported exception types for `raise` / typed `except`: `ValueError`,
`KeyError`, `IndexError`, `ZeroDivisionError`, `TypeError`, `RuntimeError`.
Bare `except:` catches all (including traps like `FileNotFoundError` /
`UnboundLocalError` that are not among the named types). The bound name
is the message string (not a full exception object). `try` has no `else`
clause. `return` / `break` / `continue` inside `try` run `finally` and
pop the catch frame before leaving (CPython-compatible).

| error | raised by |
|-------|-----------|
| `ZeroDivisionError: division by zero` | `/`, `//`, `%` with a zero divisor; `0.0 ** negative` |
| `IndexError: string index out of range` | out-of-bounds `s[i]` |
| `IndexError: list index out of range` | out-of-bounds `xs[i]` read |
| `IndexError: list assignment index out of range` | out-of-bounds `xs[i] = v` |
| `IndexError: tuple index out of range` | out-of-bounds `t[i]` |
| `IndexError: pop from empty list` / `pop index out of range` | `xs.pop(...)` |
| `KeyError: ...` | missing dict key / `set.remove` of absent element |
| `ValueError: not enough/too many values to unpack` | unpack length mismatch |
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

Everything PyRs *does* support behaves like Python — these are the known,
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

Container notes (v0.11):

- **tuple:** literals, index (const OOB is a compile error; dynamic OOB
  traps), `len`, unpack, `==`/`!=`, homogeneous `for`; membership `in`
  and methods are not supported yet.
- **dict:** keys are `int` or `str` only; `get` requires a default
  (no bare `None`); `keys`/`values`/`items` return lists (not views);
  insertion-order iteration over keys.
- **set:** elements are `int` or `str`; empty via `s: set[int] = set()`;
  `{}` is always an empty dict.

Exception notes: `except E as e` binds the message `str`, not an exception
object. Other traps (`EOFError`, `FileNotFoundError`, …) match bare
`except:` only, not `except RuntimeError`.

Not implemented yet (clear compile errors): classes, `from M import *`,
namespace packages, `match`, generators/`yield`, `lambda`, nested
functions, closures, `nonlocal`, full f-string format specs beyond
`{x:.Nf}`, `*args`/`**kwargs`, starred unpack, and most remaining
methods on tuple/dict/set.

## 9. Performance

Measured by `make bench` (best of 3, output byte-verified against
python3 before timing — Linux, LLVM 22, CPython 3.14):

| benchmark  | workload                        | python3 | PyRs   | speedup |
|------------|---------------------------------|--------:|-------:|--------:|
| fib        | recursion, 12M calls            |  1.158s | 0.027s |   42.6× |
| mandelbrot | float math, 500×500             |  0.953s | 0.018s |   53.9× |
| nbody      | float + list, 100k steps        |  1.379s | 0.008s |  164.4× |
| primes     | int loops, trial division       |  0.631s | 0.063s |   10.0× |
| sort       | list indexing, bubble sort      |  1.011s | 0.022s |   45.2× |
| strings    | per-char iteration              |  0.675s | 0.112s |    6.0× |
| **total**  |                                 |  5.807s | 0.251s |   23.2× |

What makes code fast in PyRs:

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
