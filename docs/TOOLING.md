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

There is no `pyrs new`. Project creation is `uv init`; `pyrs init` adds the
one table PyRs needs to a `pyproject.toml` that already exists, and writes a
minimal one when there is not (so it works without uv installed). It refuses
to overwrite an existing `[tool.pyrs]` table, and leaves an existing entry
file alone.

```bash
uv init myapp && cd myapp
pyrs init --entry src/myapp/main.py .
pyrs run
```

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

Two limits are accepted rather than papered over: `CC` is honored but
`CFLAGS` is not, and a compiler wrapper that changes behavior without
changing its identity or preprocessed output will not invalidate the key.
Those cases need `--no-cache` or a deleted cache directory.
