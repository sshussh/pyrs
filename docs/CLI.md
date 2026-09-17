# Command line reference

Every `pyrs` command, its options and what it is for. For projects, the
manifest and the build cache see [TOOLING.md](TOOLING.md).

```
pyrs <command> [options]
```

| Command | Purpose |
|---|---|
| `compile` (alias `build`) | Compile to a native executable |
| `run` | Compile to a temporary location and execute |
| `check` | Analyze without generating or running a program |
| `test` | Compile and run the project's tests natively |
| `profile` | Run an instrumented build that records type tags |
| `build-extension` | Build an experimental CPython extension |
| `init` | Add a `[tool.pyrs]` table to `pyproject.toml` |
| `cache` | Inspect, clean and prune the build cache |
| `clean` | Remove the project's build output directory |
| `tree` | Show the import graph the compiler resolved |
| `doctor` | Report the toolchain found, and whether it is usable |
| `completions` | Print a shell completion script |
| `lex`, `parse` | Dump tokens or the syntax tree, for debugging the compiler |

## Shared options

These appear on more than one command and mean the same thing everywhere.

| Option | Meaning | Default |
|---|---|---|
| `-i`, `--input FILE` | Source file. Optional inside a project, which supplies the entry | |
| `-O`, `--opt-level 0..3` | LLVM optimization level | 2, or the manifest |
| `--target-cpu CPU` | `generic` for a portable binary, `native` for this host, or a model name | generic |
| `--no-cache` | Recompile from scratch, reusing and publishing nothing | off |
| `--infer static\|ml\|dl` | What types a parameter no call site reaches. See [INFERENCE.md](INFERENCE.md) | static |
| `--message-format human\|json` | Annotated source snippets, or one JSON object per line | human |
| `-q`, `--quiet` | Suppress the build summary | off |

## pyrs compile

```console
$ pyrs compile -i prog.py -o prog
$ pyrs compile -i prog.py -O 3 --emit-llvm
$ pyrs build                                  # inside a project, writes target/<name>
```

Additional options:

| Option | Meaning |
|---|---|
| `-o`, `--output PATH` | Output executable. Defaults to `a.out`, or `target/<name>` in a project |
| `--emit-llvm` | Also write the generated LLVM IR to `<output>.ll` |
| `--profile FILE` | Consume a type profile from `pyrs profile` |

`--emit-llvm` is the window into the compiler. The `.ll` file is exactly what
PyRs hands to LLVM, and it is readable and diffable.

A missing, stale or mismatched `--profile` file is ignored with a warning. The
flag changes only the code emitted, never observable behaviour.

## pyrs run

```console
$ pyrs run -i prog.py
$ pyrs prog.py arg1 arg2           # the short form
$ pyrs -c 'print(42)'              # inline source
$ pyrs -                           # source from standard input
$ pyrs run --compat -i prog.py     # run the whole program under CPython
```

Compiles to a temporary directory, executes, cleans up, and exits with the
program's exit code. When the program imports other modules or packages, the
whole import graph is compiled and linked into one executable.

In the short form, options must precede the script and everything after it is
passed to the program. On Unix, `sys.argv[0]` is the script path given to PyRs.

Additional options:

| Option | Meaning |
|---|---|
| `-c CODE` | Compile and run inline source |
| `-m MODULE` | Run a module. Currently requires `--compat` |
| `--compat` / `--no-compat` | Select or refuse CPython execution for the whole program |
| `--python PATH` | The interpreter for compatibility mode |

### Compatibility mode

`--compat` runs the program under CPython instead of compiling it, with access
to that environment's installed packages such as NumPy and pandas. It is always
explicit, declared in the manifest or passed on the command line, and is never
an automatic fallback: a native binary can never silently become an interpreted
one, and the default mode always reports its compilation errors.

Interpreter selection is `--python`, then `PYRS_PYTHON`, then `python3` on
`PATH`. PyRs does not install packages.

On Unix, compatibility mode replaces the PyRs process, preserving streams,
environment, exit status and signals. Native `run` uses a child process and
maps termination by signal to the shell convention of `128 + signal`.

## pyrs check

```console
$ pyrs check -i prog.py
$ pyrs check --show-types -i prog.py
$ pyrs check --message-format json -i prog.py
```

Runs the lexer, parser, module loader and semantic analysis, and reports
diagnostics. It does not invoke the linker and does not execute user code. A
successful check means the front end accepts the source, not that runtime
parity has been proved.

`--show-types` prints the signature every function compiled with, which is the
direct way to see what type inference decided.

## pyrs test

```console
$ pyrs test
$ pyrs test --list
$ pyrs test -i tests/
```

Discovers test functions, compiles them natively and runs them. `--list` shows
what would run without running it. See [TOOLING.md](TOOLING.md) for discovery
rules and the report format.

## pyrs profile

```console
$ pyrs profile -i prog.py -o prog.prof
$ pyrs compile --profile prog.prof -i prog.py -o prog
```

Runs an instrumented build that records, for every polymorphic site, up to four
concrete type tags and a hit count. The artifact is text, hashed by source and
pinned to the compiler version, so a later build cannot silently trust the
wrong types. See [DYNAMISM.md](DYNAMISM.md).

## pyrs build-extension

```console
$ pyrs build-extension -i kernels.py --module kernels_native --python python3 -O 2
$ python3 -c 'import kernels_native; print(kernels_native.dot([1.0], [2.0]))'
```

Builds numerical functions into a CPython extension module that ordinary Python
can import. The target interpreter must be GIL enabled, version 3.12 or later,
on Linux, with development headers installed.

Every kernel parameter needs an annotation. Nothing inside an extension calls
its kernels, so a bare parameter has no evidence of its type, and its signature
is a contract with every CPython caller. The build names the parameter rather
than guessing, or takes a learned guess when asked with `--infer ml`.

See [INTEROPERABILITY.md](INTEROPERABILITY.md) for the supported argument and
result types and the boundary rules.

## pyrs init

```console
$ pyrs init myapp        # new project with a src/ layout
$ pyrs init              # add [tool.pyrs] to an existing pyproject.toml
```

## pyrs cache, pyrs clean

```console
$ pyrs cache             # what the project cache holds
$ pyrs cache --prune     # drop entries no longer reachable
$ pyrs clean             # remove target/ entirely, like cargo clean
```

## pyrs tree

```console
$ pyrs tree
```

Prints the import graph the compiler resolved, which is the fastest way to see
why a module resolved to the file it did.

## pyrs doctor

```console
$ pyrs doctor
```

Reports the interpreter, toolchain and libraries PyRs found and whether they
are sufficient, including the CPython version it will compare against and
whether that matches the one PyRs was built for.

## pyrs completions

```console
$ pyrs completions bash > /etc/bash_completion.d/pyrs
$ pyrs completions zsh  > "${fpath[1]}/_pyrs"
```

## pyrs lex, pyrs parse

```console
$ pyrs lex -i prog.py
$ pyrs parse -i prog.py -o ast.txt
```

Compiler debugging tools. Both print to standard output unless `-o` is given.
