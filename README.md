# PyRs

[![CI](https://github.com/sshussh/pyrs/actions/workflows/ci.yml/badge.svg)](https://github.com/sshussh/pyrs/actions/workflows/ci.yml)

A Python compiler written in Rust, emitting native code through LLVM.

PyRs compiles a subset of Python straight to machine code. There is no
interpreter, no virtual machine, and no dependency on CPython in the program it
produces. Compute bound code runs 6 to 41 times faster than CPython.

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

## What it is

**A compiler for a subset of Python.** Programs that compile are ordinary
Python files: valid input to `python3`, ruff, black and mypy, with no pragmas,
magic comments or custom syntax. That constraint is deliberate. It is what lets
the existing Python ecosystem work on PyRs code unmodified.

**Typed, but not by hand.** Annotations are optional. The compiler recovers
types from the program's own call sites, and a value whose type it cannot
determine still runs, dispatching on the type it holds the way CPython does.
An annotation makes code faster, not possible.

**Not a CPython replacement.** The subset is closed world. There is no `eval`,
no monkey patching, no metaclasses, and the standard library is a small core
written in PyRs rather than a port. Code outside the subset is rejected at
compile time, before any of it runs, with a diagnostic naming the feature.
It is never silently mistranslated.

**Pre-1.0 and under active development.** Version numbers advance by milestone;
reaching a particular minor version is not a readiness claim.

Current milestone: **v0.153.0**.

## Install

Requires Rust (edition 2024), LLVM with `llvm-config` on `PATH`, CMake and a C
compiler.

```console
git clone https://github.com/sshussh/pyrs.git
cd pyrs
make doctor
make install
```

The resulting `pyrs` binary is self contained. The C runtime, collector,
Unicode tables and PyRs standard library are embedded in it, so compiled
executables need no PyRs installation at run time.

## Use

```console
pyrs run -i prog.py              # compile and run
pyrs build -i prog.py -o prog    # build a native executable
pyrs check -i prog.py            # analyze without building
pyrs prog.py arg1 arg2           # python-style invocation
pyrs test                        # compile and run the project's tests
pyrs doctor                      # what PyRs found, and whether it is enough
```

Builds are cached, so an unchanged `pyrs run` recompiles nothing: 11 ms against
a cold 2610 ms. Project configuration lives in `pyproject.toml`, the file the
rest of the Python toolchain already reads:

```toml
[tool.pyrs]
entry = "src/app/main.py"
root = "src"
opt-level = 2
```

## Correctness

Parity with CPython is measured, not asserted. Language features are
differentially tested against CPython 3.14 at `-O0`, `-O2` and `-O3`; whole
program probes run under both engines and compare bytes; and every benchmark
verifies its output before it is timed.

Where behaviour deliberately differs, the difference is documented with its
reason in the [language reference](docs/GUIDE.md#differences-from-cpython).

## Documentation

The [documentation index](docs/README.md) is the map. The starting points:

| Document | For |
|---|---|
| [Language reference](docs/GUIDE.md) | Writing PyRs programs |
| [Command line reference](docs/CLI.md) | Using the compiler |
| [Project tooling](docs/TOOLING.md) | Manifests, caching, releases |
| [Architecture](docs/ARCHITECTURE.md) | How the compiler is built |
| [Type inference](docs/INFERENCE.md) | How types are recovered without annotations |
| [Dynamic values](docs/DYNAMISM.md) | What happens when a type is unknown |
| [Performance](docs/PERFORMANCE.md) | Benchmarks and how they were earned |
| [Contributing](docs/EXTENDING.md) | Adding a feature to the compiler |

## Development

```console
make doctor    # check the toolchain
make ci        # format, clippy, tests, hygiene, example parity, compatibility
```

Releases are cut from CI rather than by hand: when CI passes on `main`, a
version with no tag yet gets one, and the matching `CHANGELOG.md` section
becomes the release notes. Pushing a tag by hand takes the same path,
`git tag v0.153.0 && git push origin v0.153.0`. Nothing is published from a
commit whose CI did not pass.

Linux x86_64 is the released build. macOS arm64 and Windows x86_64 are in the
matrix but unproven, and are non-blocking until they go green.

## License

See [LICENSE](LICENSE).
