# PyRs

[![CI](https://github.com/sshussh/pyrs/actions/workflows/ci.yml/badge.svg)](https://github.com/sshussh/pyrs/actions/workflows/ci.yml)

A Python compiler written in Rust, emitting native code through LLVM.

PyRs compiles a **statically typed subset of Python** straight to machine
code — no interpreter, no VM, no runtime dependency on CPython. Compute-bound
code runs **3–48× faster than CPython**, 9.5× across the benchmark corpus
([benchmarks](#benchmarks)).

```console
$ cat examples/fib.py
def fib(n: int) -> int:
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)

print(fib(30))

$ pyrs run -i examples/fib.py
832040
```

## What this is, and is not

**It is a compiler for a subset.** Programs that compile are ordinary Python
files — valid input to `python3`, ruff, black and mypy, with no pragmas,
magic comments or custom syntax. That is a deliberate constraint, not a
coincidence: it is what lets the existing Python ecosystem work on PyRs code
unmodified.

**It is not a CPython replacement.** The subset is statically typed and
closed-world. There is no `eval`, no monkey-patching, no metaclasses, and the
standard library is a small pure-PyRs core rather than a port. Code outside
the subset is rejected at compile time, before any of it runs, with a
diagnostic naming the feature — never silently mistranslated.

**It is pre-1.0 and under active development.** Version numbers advance by
milestone; reaching a particular minor version is not a readiness claim. No
stable release or tag exists yet. See the [roadmap](docs/ROADMAP.md) for what
1.0 requires.

Current milestone: **v0.128.0**.

Correctness is measured rather than asserted: language features are
differentially tested against CPython 3.14 at `-O0`, `-O2` and `-O3`, and the
[compatibility probes](compatibility/README.md) run whole programs under both
engines and compare bytes.

## Install

Requires Rust (edition 2024), LLVM (`llvm-config` on `PATH`), CMake and a C
compiler.

```console
git clone https://github.com/sshussh/pyrs.git
cd pyrs
make doctor
make install
```

The resulting `pyrs` is self-contained: the C runtime,
collector, Unicode tables and PyRs standard library are embedded in the
binary, so compiled executables need no PyRs installation at run time.

## Using it

```console
pyrs run -i prog.py              # compile and run
pyrs build -i prog.py -o prog    # build a native executable
pyrs check -i prog.py            # type-check without building
pyrs prog.py arg1 arg2           # python-style invocation
pyrs test                        # compile and run the project's tests
pyrs doctor                      # what PyRs found, and whether it is enough
```

Builds are cached, so an unchanged `pyrs run` recompiles nothing:

|                   |    cold |    cached |
| ----------------- | ------: | --------: |
| unchanged program | 2610 ms | **11 ms** |
| new program       | 2610 ms | **84 ms** |

For a project, configuration lives in `pyproject.toml` — the file the rest of
the Python toolchain already reads:

```toml
[tool.pyrs]
entry = "src/app/main.py"
root = "src"
opt-level = 2
```

`pyrs init myapp` scaffolds that, plus the `src/` layout, `.gitignore`,
README, pinned `.python-version` and repository `cargo new` and `uv init` both
produce — and adds only the table when a `pyproject.toml` already exists.
Inside a project, `pyrs build` compiles the entry through the declared import
root to `target/`, `pyrs clean` removes it, and `pyrs cache` inspects and
prunes the machine-wide build cache. See [TOOLING.md](docs/TOOLING.md) for the
manifest, project layout, build caching and interpreter resolution.

## The language

The [**PyRs Guide**](docs/GUIDE.md) is the reference: the full language, every
difference from CPython, runtime errors, diagnostics and performance notes.
In outline, the subset covers:

- **Types** — `int` (arbitrary precision), `float`, `bool`, `str` (Unicode
  16.0.0), `None`, unions and `Optional`, `list`, `tuple`, `dict`, `set`,
  files, closures, generators, class instances
- **Statements** — `if`/`while`/`for`/`match`, `try` with the CPython
  exception hierarchy, `with`, `del`, `assert`, `global` and `nonlocal`,
  unpacking and augmented assignment
- **Expressions** — arithmetic including `**`, chained comparisons, `in`,
  walrus, `is`, bitwise operators, conditional expressions, comprehensions
  (list/dict/set), generator expressions, lambdas, f-strings with the format
  mini-language, `.format()` and `%`
- **Functions** — annotations optional and inferred where unambiguous,
  defaults, keyword arguments, `*args`/`**kwargs`, closures, `nonlocal`,
  generators with `send`/`throw`/`close`
- **Classes** — closed-world with virtual methods, inheritance, properties,
  static and class methods, context managers, iterator and comparison
  protocols, user-defined exceptions
- **Modules** — `import` and `from` in their usual forms, regular and PEP 420
  namespace packages, relative imports, cycles reported at compile time
- **Standard library** — a deliberately small pure-PyRs core: `os.path`,
  `math`, a typed `json` subset, `sys.argv`/`sys.exit`, `__name__` and the
  `if __name__ == "__main__":` guard

## What it does not do

Rejected at compile time, with a diagnostic naming the feature and saying
what to do instead — always before any user code runs, never mistranslated:

- **Dynamism** — `eval`/`exec`, monkey-patching, metaclasses, `__slots__`,
  `__new__`, open `__dict__`, class decorators, first-class class values
- **Type system** — multiple inheritance, heterogeneous containers without a
  union annotation, `Any` method dispatch, generics
- **Library surface** — `bytes`/`bytearray`/`memoryview`, encodings other
  than UTF-8, most of the standard library, all of PyPI
- **Syntax corners** — `raise X from Y`, `f"{x=}"`, two-arg `super()`,
  stacked decorators, `async`/`await`

Known behavioural divergences, each documented with its reason in the
[guide](docs/GUIDE.md#9-differences-from-cpython):

- Sets iterate in insertion order; CPython's order is unspecified and varies
  between runs, so there is no single order to match — use `sorted()`
- `e.args` is a list, not a tuple, because tuples here are fixed-arity and
  `args` holds 0 or 1 elements decided at run time; its length and contents
  match CPython
- No `is` interning for equal integers, and `nan in [nan]` is `False`
- Indexing a non-ASCII string is amortised rather than exactly O(1)

## Running Python that PyRs cannot compile

Compatibility mode runs a whole program under CPython instead, including
installed packages. It is **explicit** — declared in the manifest or passed on
the command line — and never an automatic fallback, so a native binary can
never silently become an interpreted one:

```console
pyrs run --compat -i prog.py
```

Separately, the experimental [CPython bridge](docs/INTEROPERABILITY.md)
compiles numerical functions into importable native extensions, and can read
NumPy float64 buffers without copying — the first step toward mixed
Python/native execution.

## Architecture

A Cargo workspace with strictly unidirectional data flow (see
[SPECIFICATIONS.md](docs/SPECIFICATIONS.md)):

```
source  ->  lexer   ->  parser    ->  semantic ->  ir ->  codegen ->  executable
            logos       AST           typecheck    typed  LLVM IR     LLVM opt+emit,
            INDENT/     recursive     + lower      tree   text        linked by cc
            DEDENT      descent
```

| Crate      | Responsibility                                                    |
| ---------- | ----------------------------------------------------------------- |
| `common`   | Spans and diagnostics shared by every phase                       |
| `lexer`    | `logos` scanner with an indent stack for semantic whitespace      |
| `parser`   | Hand-written recursive descent, precedence-layered                |
| `semantic` | Name resolution, type checking, numeric promotion, lowering to IR |
| `ir`       | Fully typed tree — the contract handed to the backend             |
| `codegen`  | LLVM IR text; a C++ shim verifies, optimizes and emits objects    |
| `cli`      | Driver, module loading, build cache, project manifest             |

The C runtime provides Python-faithful operations, runtime traps and a
nonmoving mark–sweep [collector](docs/GC.md).

## Benchmarks

`benchmarks/run.sh` compiles each program with `pyrs -O2`, checks its output
is byte-identical to `python3`'s, then reports best-of-5 wall times:

| benchmark  | workload                                  | python3 |   PyRs | speedup |
| ---------- | ----------------------------------------- | ------: | -----: | ------: |
| nbody      | float + list, 5-body gravity, 100k steps  |  0.769s | 0.016s |   48.2× |
| mandelbrot | float math, 500×500 escape iterations     |  0.566s | 0.015s |   37.0× |
| pipeline   | lazy `map`/`filter` over 2M elements      |  0.271s | 0.018s |   15.0× |
| matmul     | nested lists, 250×250 matrix multiply     |  0.518s | 0.039s |   13.1× |
| fib        | recursion, 30M calls (`fib(35)`)          |  0.614s | 0.048s |   12.8× |
| primes     | int loops, trial division to 300k         |  0.382s | 0.031s |   12.4× |
| iteration  | `zip`/`enumerate`, 2M paired steps        |  0.288s | 0.029s |    9.9× |
| sort       | list indexing, bubble sort of 5000        |  0.588s | 0.071s |    8.3× |
| listcomp   | comprehensions, 3M-element map/filter     |  0.363s | 0.058s |    6.3× |
| strings    | per-char iteration, 2.6M comparisons      |  0.332s | 0.096s |    3.4× |
| exceptions | 400k calls, 171k raise/catch round trips  |  0.091s | 0.081s |    1.1× |
| **total**  |                                           |  4.781s | 0.503s |    9.5× |

**Integer arithmetic used to be the weak spot; 0.126 closed it.** Every `int`
operation was an out-of-line call into the runtime, because arbitrary precision
needs a tagged representation with an overflow check and the runtime is linked
as a separate object the optimizer cannot inline through. `primes` ran at 0.8×.
Each operation now has an inline fast path on the tagged words, with the
runtime call kept for the bignum edge — `primes` is 12.4×, and every benchmark
in the corpus is faster than CPython.

The gain is larger than removing the calls: an opaque call in a loop also
blocks loop-invariant hoisting and redundant-load elimination for everything
*around* it, so the float benchmarks — which never called into the runtime for
arithmetic, but did for their loop counters — roughly doubled as well.

`exceptions` at 1.1× is the remaining outlier. The loop around the raises is
much faster than CPython; each raise/catch costs about 525 ns against CPython's
228 ns, and a function containing a `try` currently forces every local to
memory.

(Linux, LLVM 22, CPython 3.14; run `./benchmarks/run.sh` to reproduce.)

## Development

```console
make doctor    # check the toolchain
make ci        # the full local gate
```

`make ci` runs format, clippy with `-D warnings`, the workspace tests,
`make hygiene`, byte-exact example parity and the compatibility probes.

| Target                     | What it checks                                                                                                                                                                                                                           |
| -------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `make examples`            | Example parity against `python3`, comparing **stdout bytes, stderr bytes and exit status**. Building and running are separate steps so toolchain warnings are never mistaken for program output. (`make examples-all-opts` for O0/O2/O3) |
| `make hygiene`             | Version agreement across the 7 crates, `Cargo.lock` and the docs; the Unicode tables' generating interpreter; every relative documentation link; and the gates' own failure paths                                                        |
| `make asan` / `make ubsan` | The extension boundary suite with the C adapter, runtime and collector instrumented                                                                                                                                                      |
| `make compatibility`       | Native and CPython probes at O0/O2/O3 under GC stress                                                                                                                                                                                    |

Failing integration tests retain their inputs under `target/tmp`, which is
what CI uploads, so a CI-only failure can be reproduced from the artifact.

CI runs the same gate on Ubuntu with LLVM 18 and CPython 3.14, plus weekly
benchmarks and a tagged release workflow.

Release tags: `git tag v0.128.0 && git push origin v0.128.0`.

## Documentation

| Document                                        | What it is for                                                      |
| ----------------------------------------------- | ------------------------------------------------------------------- |
| [GUIDE.md](docs/GUIDE.md)                       | The reference: language, CLI, diagnostics, differences from CPython |
| [TOOLING.md](docs/TOOLING.md)                   | Projects, the `[tool.pyrs]` manifest, build caching, uv             |
| [ROADMAP.md](docs/ROADMAP.md)                   | Open gaps, workstreams and the 1.0 release gates                    |
| [CHANGELOG.md](CHANGELOG.md)                    | What changed in every release                                       |
| [SPECIFICATIONS.md](docs/SPECIFICATIONS.md)     | Architecture and phase contracts                                    |
| [PRIMITIVES.md](docs/PRIMITIVES.md)             | The runtime primitive inventory                                     |
| [GC.md](docs/GC.md)                             | The collector's design and invariants                               |
| [EXTENDING.md](docs/EXTENDING.md)               | Adding language features to the compiler                            |
| [INTEROPERABILITY.md](docs/INTEROPERABILITY.md) | The experimental CPython bridge                                     |
