# Project tooling

Projects, the manifest, testing, the build cache, interpreter resolution and
releases. For the commands themselves see [CLI.md](CLI.md).

A single file needs none of this. `pyrs run -i prog.py` works with no manifest,
no virtual environment and no uv. A project adds a place to record the entry
point, the import root and the build settings, so that `pyrs build` means one
unambiguous thing.

## What PyRs does not build

PyRs source is valid Python, not a dialect. That constraint decides most of
what the project deliberately does not build, because tools that already
operate on valid Python do it well.

| Concern | Owner |
|---|---|
| Formatting, linting | `ruff`, `black`, `uv format`, `uv check` |
| Interpreter, virtual environment, packages | `uv`, or pip, or nothing |
| Project creation | `uv init`, or `pyrs init` for the PyRs table |
| Modules and packages | Python's own model, implemented by PyRs |
| Compilation, runtime, build caching | PyRs |

The cargo analogy holds with one correction: cargo owns the project and rustc
owns the compilation, whereas here uv owns the CPython environment and PyRs
owns the build. uv has no notion of native compilation.

## The manifest

Configuration lives in `pyproject.toml`, under the `[tool.<name>]` table the
Python ecosystem already agrees on. A PyRs project will have a `pyproject.toml`
regardless, and a second configuration file would make PyRs a foreign object in
a Python repository.

```toml
[tool.pyrs]
entry = "src/app/main.py"   # the module run as __main__
root = "src"                # import root; defaults to the manifest's directory
target = "target"           # build output directory; what pyrs clean removes
opt-level = 2               # default -O
target-cpu = "generic"      # or "native", or a model name such as "x86-64-v3"
execution = "native"        # or "compat"
python = ".venv/bin/python" # interpreter for compat mode and extensions

[tool.pyrs.extension]        # what build-extension would otherwise retype
module = "kernels_native"
source = "src/app/kernels.py"
```

Every key is optional. **Unknown keys are rejected rather than ignored.** A key
that is accepted but does nothing is a silent failure, and becomes a
compatibility obligation the moment somebody writes it expecting an effect.

Discovery walks up from the working directory to the nearest `pyproject.toml`
containing a `[tool.pyrs]` table. A `pyproject.toml` without that table belongs
to some other Python project and does not make its directory a PyRs project.
One that does not parse is reported rather than skipped.

An explicit `-i`, `-c` or `-m` bypasses discovery entirely, and command line
flags override the manifest.

### Why there is no dependency table

PyRs cannot compile arbitrary PyPI code, so a `[dependencies]` table would be a
promise the compiler could not keep. Dependencies are uv's job, and `--compat`
delegates to a real interpreter with a real environment, so the dependency story
already exists and is already correct.

For the same reason, execution mode is never inferred from dependencies. An
empty dependency list looks like it should mean "compile natively", but the
implication fails in both directions: a project may declare `requests` for a
script whose entry point imports nothing external, and a project with no
dependencies at all can still use language features outside the subset. It would
also mean that `uv add rich` silently turned a native binary into an interpreted
program. Compatibility mode is therefore always declared:

```toml
execution = "compat"
```

`--compat` still wins on the command line, and `--no-compat` overrides a
manifest that asks for it, so a project can check whether its program has become
natively compilable without editing the file.

## pyrs init

```console
$ pyrs init myapp      # a new project
$ pyrs init            # add [tool.pyrs] to an existing pyproject.toml
```

Scaffolds what `cargo new` and `uv init` both produce: the `src/` layout, a
`.gitignore`, a README, a pinned `.python-version` and the repository. When a
`pyproject.toml` already exists it adds only the table.

## Building a project

`pyrs build` (or `compile`, the same command) and a project `pyrs run` build the
manifest's entry through the declared import root. They write `target/NAME`,
where `NAME` is the entry's stem, or its package directory when the entry is
`main.py`, so `src/app/main.py` builds to `target/app` rather than every project
building to `target/main`.

```console
$ pyrs build
  Finished target/app in 2.65s
$ pyrs build
  Finished target/app in 1ms (cached)
$ pyrs run             # runs target/app
$ pyrs clean           # removes target/
```

Outside a project the default output is `a.out`, and an explicit `-o` always
wins. The build summary goes to standard error so that standard output stays
clean, and `--quiet` suppresses it. `pyrs run` prints no summary at all: it
stands in for `python3` and is compared against it byte for byte.

## pyrs test

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

pytest under CPython already tests whether the logic is right. What it cannot
tell you is whether the compiled program agrees, which is exactly the failure
mode that matters for a compiler of a Python subset. The test files stay
ordinary Python, so both engines run them and the answers can be compared rather
than trusted.

Discovery follows pytest's conventions, `test_*.py` and `*_test.py`, under the
declared import root and under `tests/` when it exists, in sorted order. A
positional argument filters by test or module name, and `--list` shows what
would run without running it.

Tests that take parameters are skipped rather than rejected. Fixtures are why a
test takes arguments, PyRs cannot supply them, and failing an entire run over a
file that pytest handles fine would make `pyrs test` unusable beside it. Any
exception fails its test, not only `AssertionError`, and one failure does not
stop the rest. A project with no tests exits 0.

The runner is a generated program rather than a runtime feature. PyRs is closed
world with no reflection, so the driver parses the modules it found, emits a
`__main__` that calls each test inside a `try`, and compiles that like any other
program. Results are written to a file rather than standard output, so a test's
own printing stays exactly what was written, and a run that crashes mid suite
leaves the results up to the crash on disk and reports `N not run` rather than a
pass.

## Diagnostics for tools

`check`, `build` and `run` accept `--message-format`. The default is `human`.
`json` emits one object per line, following `cargo --message-format=json`:

```console
$ pyrs check -i prog.py --message-format json
{"level":"error","phase":"semantic","message":"type mismatch ...",
 "file":"prog.py","line":2,"column":10,"end_line":2,"end_column":13,
 "byte_start":20,"byte_end":23,"rendered":"error[semantic]: ..."}
```

`rendered` carries the annotated snippet the terminal would have shown, so a
tool does not need to reimplement the renderer. Lexer, parser, import and
semantic failures all arrive with a real position. A failure that has none, such
as an unreadable file or a failed link, is still JSON, because a parser must not
break on exactly the errors it did not anticipate.

## pyrs tree

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

PyRs is closed world, so this is the exact set of modules that will be compiled
into the program rather than an estimate. `(*)` marks a module already shown,
`--paths` shows where each was resolved from, and `--depth` limits what is
expanded without changing what is counted.

## Interpreter resolution

`--compat` and `build-extension` need a CPython, and `build-extension` needs one
with development headers, which a system interpreter often lacks without a
separate `python3-dev` style package. Interpreters provisioned by uv always have
them, so uv is preferred when it is present.

The order is:

1. `--python` on the command line
2. `python` in the manifest
3. the project's uv environment, when uv is installed
4. `PYRS_PYTHON`
5. `python3` on `PATH`

`pyrs check` and `pyrs doctor` report which interpreter was chosen and warn when
its version differs from the CPython that PyRs was built and tested against.
That warning matters because parity is verified against one version: a program
compared against a different interpreter can differ for reasons that are not
PyRs bugs.

## Build caching

A discovered project caches compiled C runtime objects and whole programs in its
`target/` directory, so an unchanged `pyrs run` compiles nothing.

| | Cold | Cached |
|---|---:|---:|
| Unchanged program | 2610 ms | 11 ms |
| New program | 2610 ms | 84 ms |

`--no-cache` reuses and publishes nothing. Invocations without a discovered
project use the global fallback at `$XDG_CACHE_HOME/pyrs`, and `PYRS_CACHE_DIR`
overrides either location.

The key covers everything that can change the output bytes: the compiler's own
fingerprint, the C toolchain's identity, the optimization level, the target, and
the content of every module in the import graph. Entries are checksum verified
before reuse. A stale entry would be a wrong answer that looks like a right one,
which is worse than a slow build.

`CC` is honored, and so are `PYRS_CFLAGS` and `PYRS_LDFLAGS`. Both are part of
the key, so changing one invalidates rather than silently reuses. They are
deliberately not spelled `CFLAGS`: that is a make convention, is routinely set
machine wide for unrelated builds, and `cc` does not read it on its own, so
adopting it would change PyRs output because of a setting aimed at something
else.

One limit is accepted rather than papered over. A compiler wrapper that changes
behaviour without changing its identity or its preprocessed output will not
invalidate the key. That case needs `--no-cache` or `pyrs cache clean`.

### Managing the cache

A cache with no way to inspect or bound it is a directory that only grows.

```console
pyrs cache dir                     # where it lives
pyrs cache info                    # entries and bytes, per layer
pyrs cache clean [--programs]      # empty it
pyrs cache prune --max-size 2GiB   # or --older-than 7d
```

`--programs` and `--runtime` narrow any of them, and `--dry-run` reports what
would go without removing it. Runtime objects are a few hundred kilobytes in
total, while programs are a few hundred kilobytes each, so reclaiming space
almost always means `--programs`.

Eviction is least recently used. Entries carry a `used` stamp refreshed when
they are reused, at most hourly, so a warm cache pays no write per hit. That is
what makes the program you rebuild every day the last one dropped, rather than
whichever the directory listing happened to yield first.

`prune` applies age and then size, so `--older-than 7d --max-size 500MB` means
both rather than whichever ran last. The `toolchain` layer is never pruned: its
entries are 64 bytes each, and losing one costs two subprocesses on the next
build.

PyRs also prunes opportunistically, keeping the cache under `PYRS_CACHE_LIMIT`
(default 2 GiB, `0` disables it). This is the one place the tool deletes
something it was not asked to, and it is deliberate: a cache that reached 998 MB
in a day of test runs is not one a user can be expected to police by hand.

The prune triggers on either a day having passed or an eighth of the limit
having been added since the last one. The growth trigger is not belt and
braces: with only the daily check, the development cache reached 3.6 GiB against
a 2 GiB ceiling in the 46 minutes after a check found it compliant. A time
interval keeps the walk rare; it does not bound a cache.

## Shell completions

```console
$ pyrs completions bash > /etc/bash_completion.d/pyrs
$ pyrs completions zsh  > "${fpath[1]}/_pyrs"
$ pyrs completions fish > ~/.config/fish/completions/pyrs.fish
```

## Releases

Releases are produced by `.github/workflows/release.yml` and never from a
working copy. When CI passes on `main`:

* the rolling `nightly` prerelease is rebuilt and its tag moved to that commit,
  so it always holds the last green tip;
* if the version in `cli/Cargo.toml` has no `v<version>` tag yet, that tag is
  created and a real release is cut for it, with the matching `CHANGELOG.md`
  section as its notes.

A milestone therefore releases itself once its version bump lands. Pushing a tag
by hand takes the same path and remains available as the escape hatch:
`git tag v0.153.0 && git push origin v0.153.0`. Nothing is ever published from a
commit whose CI did not pass.

### Platform status

Linux x86_64 is the released build, and the validated target for the
conservative garbage collector's root scanning. macOS arm64 and Windows x86_64
are in the release matrix but unproven, and are non-blocking until they go
green. Each needs its own toolchain validation before it can be treated as
release supported.
