# Command line reference

Every command, every option, its accepted values and its default. For projects
and the manifest see [TOOLING.md](TOOLING.md); for the language itself see
[GUIDE.md](GUIDE.md).

```
pyrs <command> [options]
pyrs [options] <script.py> [arguments...]
```

| Global option | Meaning |
|---|---|
| `-h`, `--help` | Print help. `--help` on a command prints the long form with possible values and defaults |
| `-V`, `--version` | Print the compiler version |

## Commands

| Command | Purpose |
|---|---|
| [`compile`](#pyrs-compile) (alias `build`) | Compile to a native executable |
| [`run`](#pyrs-run) | Compile to a temporary location and execute |
| [`check`](#pyrs-check) | Analyze without generating or running a program |
| [`test`](#pyrs-test) | Compile and run the project's tests natively |
| [`profile`](#pyrs-profile) | Run an instrumented build that records type tags |
| [`build-extension`](#pyrs-build-extension) | Build an experimental CPython extension |
| [`init`](#pyrs-init) | Add a `[tool.pyrs]` table to `pyproject.toml` |
| [`cache`](#pyrs-cache) | Inspect, clean and prune the build cache |
| [`clean`](#pyrs-clean) | Remove the project's build output directory |
| [`tree`](#pyrs-tree) | Show the import graph the compiler resolved |
| [`doctor`](#pyrs-doctor) | Report the toolchain found, and whether it is usable |
| [`completions`](#pyrs-completions) | Print a shell completion script |
| [`lex`, `parse`](#pyrs-lex-pyrs-parse) | Dump tokens or the syntax tree, for debugging the compiler |

## The short form

`pyrs` with a path instead of a command is `pyrs run`:

```console
$ pyrs prog.py arg1 arg2      # same as: pyrs run -i prog.py -- arg1 arg2
$ pyrs -c 'print(42)'         # compile and run inline source
$ pyrs -                      # read source from standard input
```

Options must precede the script. Everything after it is passed to the program,
not to the compiler, so `pyrs prog.py --flag` gives `--flag` to the program.
With `-i` the two are not separated by position, so a program argument that
looks like an option needs `--`:

```console
$ pyrs run -i prog.py -- --flag x     # --flag reaches the program
$ pyrs run -i prog.py --flag x        # error: --flag is read as a pyrs option
```

On Unix, `sys.argv[0]` is the script path given to PyRs; a separately compiled
executable uses its own invocation name. Both `-c` and `-` resolve imports from
the current working directory.

## Options shared by several commands

These mean the same thing wherever they appear.

| Option | Accepted values | Default | Meaning |
|---|---|---|---|
| `-i`, `--input FILE` | a path | the project's `[tool.pyrs] entry` | Source file |
| `-O`, `--opt-level N` | `0`, `1`, `2`, `3` | the manifest, then `2` | LLVM optimization level |
| `--target-cpu CPU` | `generic`, `native`, or a model name such as `x86-64-v3` | see below | What the emitted code may assume. `native` is not portable to another machine |
| `--no-cache` | flag | off | Recompile from scratch, reusing and publishing nothing |
| `--infer ENGINE` | `static`, `ml`, `dl` | `static` | What types a parameter no call site reaches. `static` is the compiler's own rules, `ml` a linear model, `dl` a transformer. See [INFERENCE.md](INFERENCE.md) |
| `--message-format FORMAT` | `human`, `json` | `human` | `human` prints annotated source snippets, `json` one object per line |

`--infer ml` and `--infer dl` bypass the build cache, because the same source
compiles to a different program under them.

**The default target CPU depends on what the command produces.** A command that
writes a durable artifact defaults to `generic`, so the result runs on another
machine. A command that builds something to run here and now defaults to
`native`, so it may use this host's instruction set extensions. The manifest's
`target-cpu`, and then the flag, override both.

| Command | Default |
|---|---|
| `compile`, `build`, `build-extension` | `generic` |
| `run`, `test`, `profile` | `native` |

## pyrs compile

Compile to a native executable. `pyrs build` is the same command.

```console
$ pyrs compile -i prog.py -o prog
$ pyrs compile -i prog.py -O 3 --emit-llvm
$ pyrs build                                  # in a project: writes target/<name>
```

| Option | Accepted values | Default | Meaning |
|---|---|---|---|
| `-i`, `--input FILE` | a path | the manifest's entry | Source file |
| `-o`, `--output PATH` | a path | `target/NAME` in a project, else `a.out` | Output executable |
| `-O`, `--opt-level N` | `0` to `3` | manifest, then `2` | Optimization level |
| `--target-cpu CPU` | `generic`, `native`, model name | `generic` | Target CPU |
| `--emit-llvm` | flag | off | Also write the generated LLVM IR to `<output>.ll` |
| `--no-cache` | flag | off | Recompile from scratch |
| `-q`, `--quiet` | flag | off | Do not print the build summary |
| `--profile FILE` | a path | none | Consume a type profile from `pyrs profile` |
| `--infer ENGINE` | `static`, `ml`, `dl` | `static` | Learned inference engine |
| `--message-format FORMAT` | `human`, `json` | `human` | Diagnostic format |

`--emit-llvm` writes exactly what PyRs hands to LLVM, which is readable and
diffable. A missing, unreadable, stale or version mismatched `--profile` file
is ignored with a warning and the build degrades to the unprofiled emit, never
to a wrong one.

### Examples

```console
$ pyrs compile -i prog.py -o prog            # -i, -o
  Finished prog in 72ms
$ pyrs compile -i prog.py -O 0 -q            # -O, -q: no summary printed
$ pyrs compile -i prog.py -o prog --emit-llvm
$ head -1 prog.ll
; generated by pyrs
$ pyrs compile -i prog.py --target-cpu native -o prog   # this host only
$ pyrs compile -i prog.py --target-cpu x86-64-v3 -o prog
$ pyrs compile -i prog.py --no-cache -o prog            # 3.0s, not 8ms
$ pyrs compile -i prog.py --profile prog.prof -o prog
$ pyrs compile -i lib.py --infer ml -o lib
$ pyrs compile -i bad.py --message-format json
{"level":"error","phase":"semantic","message":"type mismatch in assignment ...
```

A stale profile is a warning, not a failure:

```console
$ pyrs compile -i prog.py --profile old.prof -o prog
warning: type profile old.prof is stale; compiling without it
  Finished prog in 75ms
```

## pyrs run

Compile to a temporary directory, execute, clean up, and exit with the
program's exit code. When the program imports other modules or packages, the
whole import graph is compiled and linked into one executable.

```console
$ pyrs run -i prog.py
$ pyrs run -i prog.py -- --flag-for-the-program
$ pyrs run --compat -i prog.py
$ pyrs run --compat -m http.server
```

| Option | Accepted values | Default | Meaning |
|---|---|---|---|
| `[ARGS]...` | anything | none | Script and arguments, or just arguments with `-i`, `-c` or `-m`. `-` reads standard input |
| `-i`, `--input FILE` | a path | the manifest's entry | Source file |
| `-c CODE` | Python source | none | Execute a string as Python source |
| `-m MODULE` | a module name | none | Run an installed module. Requires `--compat` |
| `--compat` | flag | the manifest's `execution` | Execute the whole program with CPython, including installed packages |
| `--no-compat` | flag | off | Compile and run natively even when the manifest asks for compat |
| `--python PATH` | a path | see [interpreter resolution](TOOLING.md#interpreter-resolution) | CPython executable for `--compat` |
| `-O`, `--opt-level N` | `0` to `3` | manifest, then `2` | Optimization level |
| `--target-cpu CPU` | `generic`, `native`, model name | `native` | Target CPU |
| `--no-cache` | flag | off | Recompile from scratch |
| `--infer ENGINE` | `static`, `ml`, `dl` | `static` | Learned inference engine |
| `--message-format FORMAT` | `human`, `json` | `human` | Diagnostic format |

`pyrs run` prints no build summary at all, because it stands in for `python3`
and is compared against it byte for byte.

### Compatibility mode

`--compat` runs the program under CPython instead of compiling it. It is always
explicit, declared in the manifest or passed on the command line, and is never
an automatic fallback: a native binary can never silently become an interpreted
one, and the default mode always reports its compilation errors rather than
retrying elsewhere.

On Unix, compatibility mode replaces the PyRs process, preserving streams,
environment, exit status and signals. Native `run` uses a child process and
maps termination by signal to the shell convention of `128 + signal`.

PyRs does not install packages. `-m` currently requires `--compat`.

### Examples

```console
$ pyrs run -i prog.py                        # -i
[3, 6]
$ pyrs run -i prog.py -- --flag x            # [ARGS]: after --, they are the program's
$ pyrs run -c 'print(6 * 7)'                 # -c
42
$ echo 'print("from stdin")' | pyrs run -    # - reads standard input
from stdin
$ pyrs run -i prog.py -O 3 --target-cpu native
$ pyrs run -i prog.py --no-cache             # ignore the cache both ways
$ pyrs run -i lib.py --infer dl              # transformer engine
$ pyrs run -i prog.py --message-format json  # machine readable diagnostics
$ pyrs run --compat -i prog.py               # whole program under CPython
$ pyrs run --compat --python .venv/bin/python -i prog.py
$ echo '{"a":1}' | pyrs run --compat --python python3 -m json.tool
{
    "a": 1
}
$ pyrs run --no-compat -i prog.py            # override execution = "compat"
```

Options for `pyrs` must precede a positional script or `-m` module, because
everything after one belongs to the program. `pyrs run --compat -m json.tool
--python python3` gives `--python` to `json.tool`, which rejects it.

## pyrs check

Run the lexer, parser, module loader and semantic analysis, and report
diagnostics. The linker is not invoked and no user code is executed. A
successful check means the front end accepts the source, not that runtime
parity has been proved.

```console
$ pyrs check -i prog.py
$ pyrs check --show-types -i prog.py
$ pyrs check --message-format json -i prog.py
```

| Option | Accepted values | Default | Meaning |
|---|---|---|---|
| `-i`, `--input FILE` | a path | the manifest's entry | Source file |
| `--show-types` | flag | off | Print the signature every function was compiled with, inferred types included |
| `--infer ENGINE` | `static`, `ml`, `dl` | `static` | Learned inference engine |
| `--message-format FORMAT` | `human`, `json` | `human` | Diagnostic format |

`check` also accepts `--show-features`, which is hidden from `--help`. It
prints the evidence the learned predictors read for each parameter, one JSON
object per line, without analyzing the program. It exists for
`scripts/infer_eval/parity.py`, which checks the compiler's feature extractor
against the one the models were trained on, and its output format is not
stable.

### Examples

```console
$ pyrs check -i prog.py                      # silent, exit 0, when it is accepted
$ pyrs check --show-types -i prog.py
scale(values: list[int], factor: int) -> list[int]
$ pyrs check --show-types --infer ml -i lib.py
describe(label: str, verbose: bool) -> str
$ pyrs check --show-types --infer static -i lib.py
describe(label: Any, verbose: Any) -> str
$ pyrs check --message-format json -i bad.py
{"level":"error","phase":"semantic","message":"type mismatch in assignment to 'x' ...
$ pyrs check --show-features -i lib.py       # hidden, for the parity script
{"func": "describe", "param": "label", "feats": [...], "events": ["returned"], ...}
```

## pyrs test

Discover, compile and run the project's tests natively. See
[TOOLING.md](TOOLING.md#pyrs-test) for discovery rules and the report format.

```console
$ pyrs test
$ pyrs test add            # only tests whose name contains "add"
$ pyrs test --list
$ pyrs test -i tests/
```

| Option | Accepted values | Default | Meaning |
|---|---|---|---|
| `[FILTER]` | text | none | Only run tests whose name contains this |
| `-i`, `--input PATH` | file or directory | the project's roots and `tests/` | Where to search |
| `-O`, `--opt-level N` | `0` to `3` | manifest, then `2` | Optimization level |
| `--target-cpu CPU` | `generic`, `native`, model name | `native` | Target CPU |
| `--no-cache` | flag | off | Recompile from scratch |
| `--list` | flag | off | List the tests that would run, without running them |

### Examples

```console
$ pyrs test                                  # everything discovered
running 2 tests
test test_demo::test_add ... ok
test test_demo::test_fails ... FAILED
$ pyrs test add                              # [FILTER]: names containing "add"
$ pyrs test --list                           # names only, nothing runs
test_demo::test_add
test_demo::test_fails
$ pyrs test -i tests/                        # search one directory
$ pyrs test -O 0 --no-cache                  # fastest compile, nothing reused
$ pyrs test --target-cpu generic             # portable rather than this host
```

The exit status is 0 when every test passes and 1 when any fails.

## pyrs profile

Run an instrumented build that records, for every polymorphic site, up to four
concrete type tags and a hit count. See [DYNAMISM.md](DYNAMISM.md) for what the
compiler does with the result.

```console
$ pyrs profile -i prog.py -o prog.prof
$ pyrs profile prog.py arg1 arg2
$ pyrs compile --profile prog.prof -i prog.py -o prog
```

| Option | Accepted values | Default | Meaning |
|---|---|---|---|
| `[ARGS]...` | anything | none | Script and arguments when not passed with `-i` |
| `-i`, `--input FILE` | a path | none | Source file |
| `-o`, `--output FILE` | a path | `<script>.prof` | Where to write the profile |
| `-O`, `--opt-level N` | `0` to `3` | `2` | Optimization level |
| `--target-cpu CPU` | `generic`, `native`, model name | `native` | Target CPU |
| `--message-format FORMAT` | `human`, `json` | `human` | Diagnostic format |

The artifact is text. Its header pins the compiler version and a digest of the
source, so a later build cannot silently trust the wrong types.

### Examples

```console
$ pyrs profile -i prog.py                    # writes prog.prof
  Wrote prog.prof
$ pyrs profile -i prog.py -o custom.prof     # -o
$ pyrs profile -o custom.prof prog.py a b    # options first, then script and its args
$ pyrs profile -i prog.py -O 3 --target-cpu native
$ pyrs profile -i prog.py --message-format json
$ head -5 prog.prof
pyrs-profile 1
compiler 0.153.0
source 3639b61a27fa7588328bfec03ca4e17de1f06bd63e50b425e534a1165abfbbb0
site 0
  0 5
```

## pyrs build-extension

Build numerical functions into a CPython extension module that ordinary Python
can import. The target interpreter must be GIL enabled, version 3.12 or later,
on Linux, with development headers installed.

```console
$ pyrs build-extension -i kernels.py --module kernels_native --python python3 -O 2
$ pyrs build-extension                      # from [tool.pyrs.extension]
```

| Option | Accepted values | Default | Meaning |
|---|---|---|---|
| `-i`, `--input FILE` | a path | `[tool.pyrs.extension] source` | Source containing the function definitions |
| `--module NAME` | an ASCII identifier | `[tool.pyrs.extension] module` | Import name of the resulting extension |
| `--python PATH` | a path | see [interpreter resolution](TOOLING.md#interpreter-resolution) | The CPython whose headers and ABI to target |
| `-o`, `--output PATH` | a path | `MODULE` plus Python's extension suffix | Output extension path |
| `-O`, `--opt-level N` | `0` to `3` | `2` | Optimization level |
| `--target-cpu CPU` | `generic`, `native`, model name | `generic` | Target CPU |
| `--infer ENGINE` | `static`, `ml`, `dl` | `static` | Learned inference engine |

Every kernel parameter needs an annotation. Nothing inside an extension calls
its kernels, so a bare parameter has no evidence of its type, and its signature
is a contract with every CPython caller. The build names the parameter rather
than guessing, unless `--infer ml` or `--infer dl` asks for a learned
prediction. See [INTEROPERABILITY.md](INTEROPERABILITY.md) for the supported
argument and result types.

### Examples

```console
$ pyrs build-extension -i kern.py --module kern_native --python python3 -O 2
kern_native.cpython-314-x86_64-linux-gnu.so
$ python3 -c 'import kern_native; print(kern_native.dot([1.0, 2.0], [3.0, 4.0]))'
11.0
$ pyrs build-extension -i kern.py --module k -o build/k.so --python python3
$ pyrs build-extension -i kern.py --module k --target-cpu native --python python3
$ pyrs build-extension                       # from [tool.pyrs.extension]
```

An unannotated parameter is named rather than guessed at, and `--infer` is the
documented alternative:

```console
$ pyrs build-extension -i kern2.py --module k2 --python python3
extension function 'bmi': parameter 'weight_kg' needs an annotation; nothing in
an extension calls its kernels, so there is no evidence of its type (annotate
it, or pass --infer ml or --infer dl to use a learned prediction)
$ pyrs build-extension -i kern2.py --module k2 --python python3 --infer ml
```

## pyrs init

Add a `[tool.pyrs]` table to a project, scaffolding one when needed.

```console
$ pyrs init myapp
$ pyrs init --script            # a single main.py instead of src/
$ pyrs init --vcs none
```

| Option | Accepted values | Default | Meaning |
|---|---|---|---|
| `[PATH]` | a directory | the current directory | Project directory |
| `--name NAME` | text | the directory's name | Project name |
| `--entry ENTRY` | a path relative to the project | the scaffolded entry | Entry module to record |
| `--script` | flag | off | Scaffold a single `main.py` instead of a `src/` package layout |
| `--vcs VCS` | `git`, `none` | `git` when available | Version control to initialize |

When a `pyproject.toml` already exists, only the table is added.

### Examples

```console
$ pyrs init myapp                            # [PATH]: src/ layout, git initialized
$ pyrs init --script --vcs none --name demoapp .
  ./README.md
  ./.gitignore
  ./main.py
$ cat pyproject.toml
[tool.pyrs]
entry = "main.py"
opt-level = 2
execution = "native"
$ pyrs init --entry src/app/cli.py .         # record a different entry
```

A `src/` project records `entry = "src/srcapp/main.py"` and `root = "src"`, and
builds to `target/srcapp`. A `--script` project records `entry = "main.py"` and
builds to `target/main`.

## pyrs cache

Inspect and bound the build cache. In a project the cache lives under
`target/`; outside one it uses `$XDG_CACHE_HOME/pyrs`. `PYRS_CACHE_DIR`
overrides either.

```console
$ pyrs cache dir
$ pyrs cache info
$ pyrs cache clean --programs
$ pyrs cache prune --older-than 7d --max-size 2GiB --dry-run
```

| Subcommand | Purpose |
|---|---|
| `dir` | Print the cache directory |
| `info` | Report entry counts and sizes for each cache layer |
| `clean` | Remove cached entries |
| `prune` | Remove entries that are stale or over a size budget |

`dir` and `info` take no options. `clean` and `prune` take:

| Option | Accepted values | Default | Meaning |
|---|---|---|---|
| `--programs` | flag | off | Only compiled programs |
| `--runtime` | flag | off | Only compiled runtime objects |
| `--dry-run` | flag | off | Report what would be removed without removing it |

```console
$ pyrs clean --dry-run
would remove /home/you/project/target
$ pyrs clean
```

`prune` adds:

| Option | Accepted values | Default | Meaning |
|---|---|---|---|
| `--older-than DURATION` | `30m`, `24h`, `7d` | none | Drop entries not reused within this long |
| `--max-size SIZE` | `500MB`, `2GiB` | none | Drop least recently used entries until the cache fits |

Both may be given at once, and then both apply. Eviction is least recently
used, and the `toolchain` layer is never pruned.

### Examples

```console
$ pyrs cache dir
/home/you/project/target
$ pyrs cache info
cache: /home/you/project/target
  programs        1 entries   317.8 KiB
  runtime         1 entries   382.5 KiB
  toolchain       1 entries        64 B
  total                        700.4 KiB
$ pyrs cache clean --programs --dry-run
would have removed 1 entry (317.8 KiB)
$ pyrs cache clean --runtime                 # only the runtime objects
$ pyrs cache clean                           # every layer
$ pyrs cache prune --older-than 7d --max-size 2GiB --dry-run
would have pruned 0 entries (0 B)
$ pyrs cache prune --max-size 500MB --programs
```

## pyrs clean

Remove the project's build output directory, including its cache, the way
`cargo clean` does.

| Option | Accepted values | Default | Meaning |
|---|---|---|---|
| `--dry-run` | flag | off | Report what would be removed without removing it |

## pyrs tree

Print the import graph the compiler resolved. PyRs is closed world, so this is
the exact set of modules that will be compiled into the program rather than an
estimate.

| Option | Accepted values | Default | Meaning |
|---|---|---|---|
| `-i`, `--input FILE` | a path | the manifest's entry | Source file |
| `--paths` | flag | off | Show the file each module was resolved from |
| `--depth N` | an integer | unlimited | Limit the depth shown, without changing what is counted |

```console
$ pyrs tree
__main__
├── app
└── app.util

3 modules
$ pyrs tree --paths
__main__ (/home/you/project/src/app/main.py)
...
$ pyrs tree --depth 1                        # expand one level only
$ pyrs tree -i other.py                      # a file outside the project
```

## pyrs doctor

Report the toolchain PyRs found and whether it is usable: the interpreter, the
C compiler, LLVM, and whether the CPython version matches the one PyRs was
built and tested against. Takes no options.

```console
$ pyrs doctor
pyrs 0.153.0
  target       x86_64-linux
  C compiler   cc (cc (GCC) 16.2.1 20260810)
  interpreter  python3 (python3 on PATH, Python 3.14)
  cache        /home/you/project/target (700.4 KiB)
  project      /home/you/project/pyproject.toml
               entry src/app/main.py, root /home/you/project/src

no problems found
```

## pyrs completions

Print a shell completion script to standard output.

| Argument | Accepted values |
|---|---|
| `<SHELL>` | `bash`, `elvish`, `fish`, `powershell`, `zsh` |

```console
$ pyrs completions bash > /etc/bash_completion.d/pyrs
$ pyrs completions zsh  > "${fpath[1]}/_pyrs"
$ pyrs completions fish > ~/.config/fish/completions/pyrs.fish
```

## pyrs lex, pyrs parse

Dump the token stream or the abstract syntax tree. These are compiler debugging
tools, and their output format is not stable.

| Option | Accepted values | Default | Meaning |
|---|---|---|---|
| `-i`, `--input FILE` | a path | required | Source file |
| `-o`, `--output FILE` | a path | standard output | Where to write the dump |

```console
$ pyrs lex -i prog.py                        # tokens to standard output
$ pyrs lex -i prog.py -o tokens.txt
$ pyrs parse -i prog.py -o ast.txt
```

## Environment variables

Read by the compiler:

| Variable | Effect |
|---|---|
| `PYRS_PYTHON` | CPython executable for `--compat` and extensions, after `--python` and the manifest |
| `PYRS_STDLIB` | Directory searched for standard library modules before the embedded copy. For development |
| `PYRS_CACHE_DIR` | Cache location, overriding both the project and the global default |
| `PYRS_CACHE_LIMIT` | Ceiling for opportunistic pruning. Default 2 GiB; `0` disables it |
| `CC` | C compiler used for the final link. Part of the cache key |
| `PYRS_CFLAGS`, `PYRS_LDFLAGS` | Extra flags for the final link. Part of the cache key. Deliberately not spelled `CFLAGS` |
| `PYRS_EXTENSION_CFLAGS` | Extra flags when building a CPython extension, used by the sanitizer targets |
| `PYRS_QUIET` | Suppress the interpreter version mismatch warning |

Read by compiled programs at run time:

| Variable | Effect |
|---|---|
| `PYRS_GC` | `marksweep` (the default) or `none` to disable collection while still tracking allocations |
| `PYRS_GC_THRESHOLD` | Bytes allocated before a collection is requested. Default 1048576, values below 1024 are clamped |
| `PYRS_GC_STRESS` | `1` collects before every managed allocation. For validation, not benchmarks |
| `PYRS_GC_STATS` | `1` writes one diagnostics line to standard error at exit |
| `PYRS_PROFILE_OUT` | Where an instrumented build writes its profile |

[GC.md](GC.md) documents the collector variables in full.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | A compile error, or an uncaught exception in the program |
| the program's own | `pyrs run` propagates whatever the program returned |
| 128 + signal | Native `run` when the program was terminated by a signal |

Compatibility mode on Unix replaces the PyRs process, so the exit status is
CPython's own.
