# Project tooling

PyRs source is **valid Python**, not a dialect. That is a constraint the
project protects rather than a coincidence, and it decides most of what PyRs
does *not* build: formatting, linting, packaging, environments and project
creation are all done well by tools that already operate on valid Python.

| Concern | Owner |
|---|---|
| Formatting, linting | `ruff`, `black`, `uv format`, `uv check` |
| Interpreter, virtual environment, packages | `uv` (or pip, or nothing) |
| Project creation | `uv init` |
| Modules and packages | Python's own model, implemented by PyRs |
| Compilation, runtime, build caching | PyRs |

The cargo analogy, with the correction that matters: cargo owns the project
and rustc owns the compilation, so uv owns the **CPython environment** and
PyRs owns the **build**. uv has no notion of native compilation.

## `[tool.pyrs]` in `pyproject.toml`

Configuration lives in the file the rest of the Python toolchain already
reads, under the `[tool.<name>]` table they already agree on. A PyRs project
will have a `pyproject.toml` regardless, and a second config file would make
PyRs a foreign object in a Python repo.

```toml
[tool.pyrs]
entry = "src/app/main.py"   # module run as __main__
root = "src"                # import root; defaults to the manifest's directory
target = "target"           # build output directory; what `pyrs clean` removes
target = "target"           # build output directory
opt-level = 2               # default -O
execution = "native"        # or "compat"
python = ".venv/bin/python" # interpreter for compat and extensions

[tool.pyrs.extension]       # what build-extension otherwise retypes each call
module = "kernels_native"
source = "src/app/kernels.py"
```

Every key is optional. **Unknown keys are rejected** rather than ignored: an
accepted-but-ignored key silently does nothing, and becomes a compatibility
obligation the moment someone writes it expecting an effect.

Discovery walks up from the working directory to the nearest `pyproject.toml`
that contains a `[tool.pyrs]` table. One without that table belongs to some
other Python project and does not make its directory a PyRs project. A
`pyproject.toml` that does not parse is reported, not skipped.

An explicit `-i`, `-c` or `-m` bypasses discovery entirely, and command-line
flags override the manifest.

### There is no dependency table, deliberately

PyRs cannot compile arbitrary PyPI code, so a `[dependencies]` table would be
a promise the compiler could not keep. Dependencies are uv's job, and
`--compat` already delegates to a real interpreter with a real environment —
the dependency story exists and is already correct.

For the same reason **execution mode is never inferred from dependencies**.
An empty dependency list looks like it should mean "compile natively", but
the implication fails in both directions: a project may declare `requests`
for a script whose entry point imports nothing external, and a project with
no dependencies at all can still use language features outside PyRs's subset.
It would also mean `uv add rich` silently turned a native binary into an
interpreted program. The product contract requires compatibility mode to be
explicit, so it is declared:

```toml
execution = "compat"
```

`--compat` still wins on the command line, and `--no-compat` overrides a
manifest that asks for it — so a project can test whether its program has
become natively compilable without editing the file.

## `pyrs init`

There is no `pyrs new`. The split is by **what is already there**, not by
which command was typed.

**A directory that already has a `pyproject.toml`** belongs to a project
someone else created — `uv init`, most likely. It gets exactly one table
added, its existing entry point adopted rather than a second one invented
beside it, and nothing else written:

```bash
uv init myapp && cd myapp
pyrs init
pyrs run
```

**A directory without one** gets the layout cargo and uv both scaffold,
because a user starting from nothing should not have to assemble it by hand
just because PyRs declined to own project creation:

```console
$ pyrs init myapp
initialized project `myapp` at myapp
  myapp/pyproject.toml
  myapp/.python-version
  myapp/README.md
  myapp/.gitignore
  myapp/src/myapp/__init__.py
  myapp/src/myapp/main.py
```

`root = "src"` is written alongside the layout: a `src/` layout is only
importable with a declared root, so scaffolding one without the other would
produce a project that does not resolve. `.gitignore` carries `/target` so
the first commit cannot contain build output; an existing one gets that line
appended and nothing else touched.

`.python-version` is pinned to the CPython PyRs was built against.
`requires-python` states the floor, but `.python-version` is what uv reads
when it provisions the environment — writing only the first left uv free to
pick its own default.

| Flag | Effect |
|---|---|
| `--name NAME` | Project name; defaults to the directory's. A package directory must be a Python identifier, so `my-app` produces `src/my_app/` |
| `--entry PATH` | Entry module to record. An entry under `src/` implies `root = "src"` |
| `--script` | A flat `main.py` instead of the `src/` layout, for a single-file program |
| `--vcs git\|none` | `git init` unless told not to; never nested inside an existing repository |

`init` refuses to overwrite an existing `[tool.pyrs]` table, and never
overwrites a file that is already there — every file it produces is a
starting point, and `init` on an existing project is a normal thing to do.

## Building a project

`pyrs build` (or `compile`, the same command) is project-aware in the same
way `run` is. With no `-i` it builds the manifest's entry through the
declared import root; with no `-o` it writes `target/NAME`, where `NAME` is
the entry's stem — or its package directory when the entry is `main.py`, so
`src/app/main.py` builds to `target/app` rather than every project building
to `target/main`.

```console
pyrs build          # -> target/app
pyrs clean          # removes target/
```

Outside a project the default is still `a.out`, and an explicit `-o` always
wins.

`pyrs clean` removes the project's target directory and nothing else. The
machine-wide build cache is `pyrs cache clean`: conflating them would mean
clearing one project's outputs slowed down every build on the system.

## `pyrs doctor`

`make doctor` checks that a machine can build the *compiler*. `pyrs doctor`
answers the different question a user has — can this binary compile my
program, and which interpreter will `--compat` use:

```console
$ pyrs doctor
pyrs 0.115.0
  target       x86_64-linux
  C compiler   cc (cc (GCC) 16.2.1)
  interpreter  python3 (python3 on PATH, Python 3.14)
  cache        /home/you/.cache/pyrs (1.3 GiB)
  project      /home/you/app/pyproject.toml
               entry src/app/main.py, root /home/you/app/src, target /home/you/app/target

no problems found
```

It reads the same resolution code the build runs, so it cannot describe a
different toolchain than the one used, and it exits non-zero when it finds a
problem.

## Shell completions

```console
pyrs completions zsh > ~/.zfunc/_pyrs
pyrs completions bash > /etc/bash_completion.d/pyrs
```

`bash`, `zsh`, `fish`, `elvish` and `powershell`.

## Building a project

`pyrs build` (an alias for `compile`) takes the entry, import root and
optimization level from the manifest, the same way `pyrs run` does:

```console
pyrs build            # -> target/NAME
pyrs build -O3
pyrs clean            # remove target/
```

`NAME` is the entry module's stem, or its package directory when that stem is
`main` — so `entry = "src/demo/main.py"` builds `target/demo` rather than
`target/main`. An explicit `-o` always wins, and outside a project the
default is still `a.out`.

`pyrs clean` removes the target directory and nothing else. The shared build
cache is `pyrs cache clean`; conflating the two would mean clearing one
project's outputs slowed down every build on the machine.

## Checking the toolchain

```console
pyrs doctor
```

Reports the C compiler, the resolved interpreter and whether its version
matches the one PyRs targets, the cache directory and its size, and the
project it would build — all resolved through the same code the build itself
runs, so the report cannot describe a different toolchain than the one used.
It exits non-zero when something is wrong, so a setup script can act on it.

`make doctor` is the different, contributor-facing question: can this machine
build the compiler.

## Shell completions

```console
pyrs completions zsh > ~/.zfunc/_pyrs
pyrs completions bash > /etc/bash_completion.d/pyrs
```

`bash`, `zsh`, `fish`, `elvish` and `powershell`.

## `pyrs test`

```console
$ pyrs test
running 3 tests
test test_util::test_add ... ok
test test_util::test_add_negative ... ok
test test_util::test_broken ... FAILED

failures:
    test_util::test_broken
        one plus one is not three

test result: FAILED. 2 passed; 1 failed
```

pytest under CPython already tests whether your logic is right. What it
cannot do is tell you whether the **compiled** program agrees with it, which
is exactly the failure mode of a compiler for a Python subset. The test files
stay ordinary Python, so both engines run them and the answers can be
compared rather than trusted.

Discovery follows pytest's conventions — `test_*.py` and `*_test.py`, under
the declared import root and under `tests/` when it exists, in sorted order.
A positional argument filters by test or module name, and `--list` shows what
would run without running it.

Tests taking parameters are **skipped, not rejected**: fixtures are why a
test takes arguments, PyRs cannot supply them, and failing the whole run over
a file pytest handles fine would make `pyrs test` unusable beside it. Any
exception fails its test, not just `AssertionError`, and one failure does not
stop the rest. A project with no tests exits 0.

The runner is a *generated program*, not a runtime feature: PyRs is
closed-world with no reflection, so the driver parses the modules it found,
emits a `__main__` calling each test inside a `try`, and compiles that like
any other program. Results go to a file rather than stdout, so a test's own
printing stays exactly what was written; a run that crashes mid-suite leaves
the results up to the crash on disk and is reported as `N not run`, never as
a pass.

## Diagnostics for tools

`check`, `build` and `run` take `--message-format`. The default is `human`;
`json` emits one object per line, following `cargo --message-format=json`:

```console
$ pyrs check -i prog.py --message-format json
{"level":"error","phase":"semantic","message":"type mismatch ...",
 "file":"prog.py","line":2,"column":10,"end_line":2,"end_column":13,
 "byte_start":20,"byte_end":23,"rendered":"error[semantic]: ..."}
```

`rendered` carries the annotated snippet the terminal would have shown, so a
tool need not reimplement the renderer. Lex, parse, import and semantic
failures all arrive with a real position; a failure that has none — an
unreadable file, a failed link — is still JSON, because a parser must not
break on exactly the errors it did not anticipate.

## `pyrs tree`

```console
$ pyrs tree
__main__
├── app
├── app.util
│   ├── app (*)
│   └── app.shared
└── app.shared (*)

4 modules
```

PyRs is closed-world, so this is the exact set of modules that will be
compiled into the program rather than an estimate. `(*)` marks a module
already shown, `--paths` shows where each was resolved from, and `--depth`
limits what is expanded without changing what is counted.

## Interpreter resolution

`--compat` and `build-extension` need a CPython, and `build-extension` needs
one with development headers — which a system interpreter often lacks without
a separate `python3-dev` style package. uv-provisioned interpreters always
have them, so uv is preferred when present.

Resolution order, most specific first:

1. `--python`
2. `[tool.pyrs] python`
3. the uv **project** environment (`uv python find`, only when it resolves
   inside a `.venv`)
4. `PYRS_PYTHON`
5. `python3` on `PATH`

uv is restricted to a project environment on purpose: outside a project
`uv python find` still answers, with whatever interpreter uv defaults to,
which is not the project's choice.

`pyrs check` reports the entry point, import root, execution mode and the
resolved interpreter with its source, so none of it is a guess.

### Keep the interpreter aligned with PyRs

The Unicode tables and the differential oracle are generated from a specific
CPython (see `PYRS_UNIDATA_CPYTHON` in the generated header). `uv init` picks
its own default — 3.12 at the time of writing, against PyRs's 3.14 — so
`pyrs init` writes an explicit `requires-python`, and a mismatched
interpreter produces a warning rather than a quiet divergence that only shows
up when something Unicode- or compatibility-shaped disagrees.

## Build caching

Compiled C runtime objects and whole programs are cached in
`$XDG_CACHE_HOME/pyrs` (override with `PYRS_CACHE_DIR`), so an unchanged
`pyrs run` compiles nothing. `--no-cache` on `run` and `compile` reuses and
publishes nothing.

The cache is global rather than per-project: the runtime objects depend only
on the compiler and PyRs's embedded sources, so every project on the machine
wants the same ones, and an explicit `pyrs run -i prog.py` with no project
still benefits.

Keys cover everything that can change the output bytes — the compiler's own
fingerprint, the C toolchain's identity, the optimization level, the target,
and the content of every module in the import graph — and entries are
checksum-verified before reuse. A stale entry would be a wrong answer that
looks like a right one, which is worse than a slow build.

`CC` is honored, and so are `PYRS_CFLAGS` and `PYRS_LDFLAGS` — both are
part of the key, so changing one invalidates rather than silently reuses.
They are deliberately *not* spelled `CFLAGS`: that is a make convention, is
routinely set machine-wide for unrelated builds, and `cc` does not read it on
its own, so adopting it would change PyRs's output because of a setting aimed
at something else.

One limit is accepted rather than papered over: a compiler wrapper that
changes behavior without changing its identity or its preprocessed output
will not invalidate the key. That case needs `--no-cache` or
`pyrs cache clean`.

### Managing it

A cache with no way to inspect or bound it is a directory that only grows.

```console
pyrs cache dir                     # where it lives
pyrs cache info                    # entries and bytes, per layer
pyrs cache clean [--programs]      # empty it
pyrs cache prune --max-size 2GiB   # or --older-than 7d
```

`--programs` and `--runtime` narrow any of them, and `--dry-run` reports what
would go without removing it. The distinction matters: the runtime objects
are a few hundred kilobytes shared by every build on the machine, while the
programs are a few hundred kilobytes *each*, so reclaiming space almost
always means `--programs`.

Eviction is least-recently-used. Entries carry a `used` stamp refreshed when
they are reused — at most hourly, so a warm cache pays no write per hit —
which is what makes the program you rebuild every day the last one dropped
rather than whichever the directory listing happened to yield first.

`prune` applies age and then size, so `--older-than 7d --max-size 500MB`
means both rather than whichever ran last. `toolchain` is never pruned: its
entries are 64 bytes each and losing one costs two subprocesses on the next
build.

PyRs also prunes opportunistically, at most once a day, keeping the cache
under `PYRS_CACHE_LIMIT` (default 2 GiB; `0` disables it). This is the one
place the tool deletes something it was not asked to, and it is deliberate: a
cache that reached 998 MB in a day of test runs during development is not one
a user can be expected to police by hand.
