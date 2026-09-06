# Project tooling: caching, `[tool.pyrs]`, and uv as the CPython backend

Three sequenced milestones — **0.109.0** build caching, **0.110.0** the
manifest and `pyrs init`, **0.111.0** uv-backed interpreter resolution. The
first depends on neither of the others and carries the only measured cost, so
it goes first.

**Status: implemented.** 0.109 shipped as planned. 0.110 and 0.111 landed
together, because interpreter resolution turned out to be a few functions
that the manifest and `build-extension` both needed rather than a milestone
of its own. What the implementation changed about this plan is recorded at
the end.

## The posture

PyRs source is **valid Python**, not a dialect. That is what lets the existing
Python ecosystem work on it unmodified, and it is a constraint to protect
rather than a coincidence: nothing PyRs adds should ever make source
non-Python. No pragmas, no magic comments, no custom syntax. Configuration
lives outside the source entirely.

Follow that through and most of a "tooling story" turns out to be someone
else's job already:

| Concern | Owner | Why not PyRs |
|---|---|---|
| Formatting, linting | ruff / black / `uv format` | Operates on valid Python; already solved |
| Interpreter, venv, packages | uv | Solved, fast, and PyRs already shells out to an interpreter |
| Project creation | `uv init` | Produces a valid project already; PyRs adds one table |
| Modules and packages | **already built** | `cli/src/modules.rs`: `__init__.py` packages, PEP 420 namespaces, relative imports, cycles, topological order |
| Compilation, runtime, caching | **PyRs** | Nobody else compiles Python to native code |

The cargo analogy holds with one correction. Cargo owns the project and rustc
owns the compilation; uv owns the **CPython environment** and PyRs owns the
**build**. uv has no notion of native compilation, so the build brain is
PyRs's regardless of how much of the environment uv manages.

Consequences, decided:

- **There is no `pyrs new`.** Project creation is `uv init`. PyRs contributes
  `pyrs init`, which adds `[tool.pyrs]` to an existing `pyproject.toml`.
- **uv is preferred, never required.** `pyrs compile -i prog.py` keeps working
  with no uv, no venv and no manifest. A native compiler that needs a package
  manager to emit a binary has given up the property that makes it worth
  having.
- **PyRs invokes uv's CLI and parses nothing of its internals.** `uv python
  find` and `uv run` are a narrow, stable surface. `uv.lock` and `uv init`'s
  template shape are not.
- **No `pyrs add` / `pyrs sync` wrappers.** They would inherit uv's semantics
  and error messages while PyRs owned the support burden, for no gain.

## Rejected: inferring compatibility mode from `dependencies`

An empty `dependencies` list looks like it should mean "nothing external is
imported, so compile natively", with a non-empty list selecting `--compat`.
It is recorded here as rejected because the reasoning is not obvious.

It contradicts the product contract in [ROADMAP.md](../../ROADMAP.md):
compatibility mode is "an explicit opt-in", its Python dependency "must be
visible", and it is "never an automatic retry". Selecting a whole-program
execution mode from a metadata table is exactly the invisible switch that
rules out.

The inference also fails in both directions. A project may declare `requests`
for a script while the entry point PyRs compiles imports nothing external —
forcing `--compat` there runs *the whole program* under CPython and abandons
native execution over an unrelated line. Conversely `dependencies = []` says
nothing about whether the program stays inside PyRs's subset; multiple
inheritance and `map`/`filter` are unsupported today with no dependency in
sight. And the switch would fire on unrelated actions: `uv add rich` for a
progress bar would silently turn a native binary into an interpreted program.

The exact signal already exists. `cli/src/modules.rs` resolves the full
transitive import graph and fails with a precise diagnostic — `No module named
'pandas'` is what the compatibility suite asserts today. That is derived from
the code, per entry point, rather than from a hand-maintained list describing
what pip should install.

**`dependencies` improves the diagnostic instead of the mode.** When native
resolution fails on `import pandas` and pandas is a declared dependency, say
so and name the fix. Same information, still explicit, and it cannot silently
change what the program is.

## 0.109.0 — build caching

### The measurement

On this machine, a **one-line** program takes **2.61 s** to build, of which
**2.39 s** is `cc -O2 -c runtime.c`. The C runtime is 92% of the floor and is
paid on every invocation, including every `pyrs run` of an unchanged program.

Two layers, and the milestone needs both:

1. **Runtime objects.** Compile `runtime.c` and `gc.c` once and reuse them.
   Takes any build from 2.61 s to roughly 0.2 s.
2. **User code.** Fingerprint the program and skip codegen entirely when
   nothing changed. Takes an unchanged `pyrs run` to roughly zero, which is
   the cargo-like behavior this is for.

A previous attempt built only layer 1 and explicitly deferred layer 2 ("user
code and the full import graph are recompiled on every build"). Layer 2 is
where the `pyrs run` experience actually comes from.

### The cache key, and why it must be complete

A stale-cache bug produces a **silently wrong binary**, which is a worse
failure than being slow. So the key covers everything that can change the
output bytes, and artifacts are verified before reuse rather than trusted.

Runtime objects are keyed on: format version, the `pyrs` executable's own
bytes, the embedded runtime/collector/header bytes, the resolved C compiler
path and its bytes, its `--version` and `-dumpmachine`, the fixed C arguments,
and the **preprocessed** source. Preprocessing is what makes a changed
`stdint.h`, include path or predefined macro visible; a key over the embedded
`runtime.c` bytes alone would happily reuse an object built against a
different system header.

User code is keyed on: the same format version and `pyrs` bytes, the opt
level, the target, and the content of **every module in the resolved import
graph** — which the module resolver already computes exactly, so the input set
has a precise definition rather than a guess.

Accepted and documented limits: `CC` is honored but `CFLAGS` is not, and a
compiler wrapper that changes behavior without changing its identity or
preprocessed output will not invalidate the key. Those cases need
`--no-cache` or a deleted `target/`.

Objects and executables stage privately and publish by rename, so an abrupt
kill can leave a staging directory but cannot publish a partial entry.

### The performance trap to avoid

Caching forces separate `runtime.o` and `gc.o` — those are the objects being
fingerprinted and reused. Applying that shape *everywhere*, including explicit
`-i` sources that have no cache, replaces one `cc program.o runtime.c gc.c`
with two `-c` compiles plus a link and measurably loses time (previously
measured at +54% on this machine). Explicit sources build into a temporary
directory and throw the objects away, so the split buys them nothing.

Three cases, not a boolean: **cached** (project build, reuse verified
objects), **separate** (project `--no-cache`, same shape, publish nothing) and
**inline** (explicit source, compile as part of the link). `--no-cache` keeps
the separate-object shape deliberately — it is a cache bypass, not a different
build strategy, and collapsing it would make the bypass untestable as one.

## 0.110.0 — `[tool.pyrs]` and `pyrs init`

Configuration goes in **`pyproject.toml`** under `[tool.pyrs]`, the mechanism
ruff, pytest and mypy already use. A PyRs project will have a
`pyproject.toml` regardless — for ruff, and for the venv `--compat` and
`build-extension` point at — and a second config file would make PyRs a
foreign object in a Python repo.

Verified: `uv add` preserves a hand-added `[tool.pyrs]` table verbatim,
position included. Co-habitation works in practice, not just in principle.

This means taking a real TOML dependency. The workspace has none today and a
previous attempt hand-wrote a subset to avoid one; that is the wrong trade
here, because the file is shared with tools that parse TOML fully and
"almost TOML" would be user-hostile in a way a private format would not.

### Schema

Only what is **not derivable from the filesystem** and **not expressible in
Python source**:

```toml
[tool.pyrs]
entry = "src/demo/__init__.py"   # which module is __main__
root = "src"                      # import root; defaults to the manifest's directory
opt-level = 2                     # default -O
target-dir = "target"             # artifacts, cargo-style
execution = "native"              # or "compat" — see below

[tool.pyrs.extension]             # what build-extension retypes on every call
module = "kernels_native"
source = "src/demo/kernels.py"
```

`src/` layout is the convention because it is what `uv init` scaffolds; the
resolver currently roots at the entry script's directory, and a declared
`root` is the one module-system change a project concept requires.

### Compatibility mode is declared, never inferred

```toml
execution = "compat"
```

Spelled as a mode rather than `compat = true`: it is a whole-program choice,
and a boolean leaves `compat = false` ambiguous between "native" and "unset".
Project-wide to start, since `--compat` today runs the entire program under
CPython and per-entry-point mode would be a larger change than it appears.

`--compat` on the command line still wins over the manifest, and `--no-compat`
must exist so a project can test whether its program has become natively
compilable without editing the file.

## 0.111.0 — uv as the CPython backend

`--compat` and `build-extension` both need a CPython. Today they default to
`python3` on `PATH`, which makes the extension path depend on the system
interpreter shipping development headers — a separate `python3-dev` package on
most distributions, and a confusing failure when it is missing.

A uv-provisioned interpreter always ships them. Verified: `uv python find`
returns the project interpreter, and its install contains `Python.h`.

Resolution order, most specific first: `--python`, then `[tool.pyrs]`, then
`uv python find` when uv is present and the project has a venv, then
`PYRS_PYTHON`, then `python3` on `PATH`. Every step is reported by
`pyrs check`, so which interpreter is in use is never a guess.

### The version trap

`uv init` selected **Python 3.12** in testing. PyRs's oracle, generated
Unicode tables and compatibility suite are pinned to **3.14.7**, and
`make hygiene` now enforces the minor version. A uv-scaffolded project would
therefore default to an interpreter that does not match the tables, silently,
until something Unicode- or compat-shaped disagrees.

So `pyrs init` writes an explicit `requires-python`, and interpreter
resolution validates the discovered interpreter against what PyRs was built
against, warning on a minor-version mismatch rather than proceeding quietly.

## Validation

Caching is the milestone where "it worked" is easy to assume and hard to
prove, so the tests count **compiler invocations** and distinguish runtime
compilation from preprocessing and linking — that is what makes "the cache was
used" an assertion rather than a hope. Cases: cold build, warm reuse, source
edit, opt-level change, compiler change, header change, corrupted entry,
concurrent builds, `--no-cache` bypass, and a failed build that must not
execute a stale artifact.

Manifest tests cover discovery from a subdirectory, explicit `-i` bypassing
discovery entirely, precedence of flags over the manifest, `--no-compat`, a
malformed manifest, unknown keys, and `pyrs init` refusing to clobber an
existing `[tool.pyrs]`.

uv tests must run **without uv installed** as well as with it, since optional
means the unavailable path is a supported configuration rather than an
untested one.

Cold and warm build timings are recorded as measurements, not CI assertions —
they are machine-sensitive and do not belong in a gate.

## Out of scope, with reasons

**A `[dependencies]` table and any package registry.** PyRs cannot compile
arbitrary PyPI code, so a dependency table would be a promise the compiler
cannot keep. CPython-side packages are uv's job, and `--compat` already
delegates to a real interpreter with a real environment — the dependency story
exists and is already correct.

**A formatter, linter or test runner.** ruff, black and `uv format` operate on
valid Python and already do this. A PyRs formatter would also silently delete
comments today, since the AST does not preserve source trivia.

**Per-module incremental codegen.** The user-code fingerprint is
whole-program: any changed module rebuilds everything. Splitting that needs
stable per-module boundaries in the IR, which do not exist yet, and
whole-program invalidation already collapses the common `pyrs run` case to
near zero.

**Workspaces, path and git dependencies, binary distribution, a native
library ABI.** Each needs its own design, and library layout and import-name
ownership have to be settled before pinning revisions means anything.


## What implementation changed

Recorded because the plan was wrong in ways worth keeping.

**0.111 was not a separate milestone.** Interpreter resolution is one small
module that `--compat`, `build-extension` and `pyrs check` all needed as soon
as the manifest existed. Splitting it out would have meant shipping a
manifest whose `python` key did nothing.

**uv had to be restricted to project environments.** The plan said "`uv
python find` when uv is present and the project has a venv"; the first
implementation dropped the second clause. Outside a project `uv python find`
still answers, with uv's own default — 3.12 here, against the system 3.14
PyRs is built for — so every compat run on the machine was silently
retargeted. Caught by running the command outside a project rather than by
reasoning about it.

**Two cache defects that testing found and design did not.** The runtime key
is computed over preprocessed C, whose line markers embed the per-run
temporary path, so the key never repeated and the runtime cache never hit —
visible only as six cache entries where there should have been one. And
computing any key ran `cc --version` and `cc -dumpmachine`, including on
cache hits, until the toolchain identity was recorded under a stamp of the
compiler binary. The second was caught by a test that counts compiler
invocations, which is exactly why that test counts rather than times.

**`--python` stopped requiring `--compat`.** Once `[tool.pyrs]` could select
compatibility mode, requiring the flag made the manifest's `python` key
unusable. It now warns when it cannot take effect rather than refusing, and
the invocation test that encoded the old constraint was repointed.

**A malformed `pyproject.toml` is reported, not skipped.** Discovery
originally treated an unparseable manifest as "no project here", which turns
a typo into a confusing absence of configuration.

Measured after the work, on the same machine as the 2.61 s baseline:

| | Before | After |
|---|---|---|
| Unchanged `pyrs run` | 2610 ms | **11 ms** |
| New program, warm runtime cache | 2610 ms | **84 ms** |
| `--no-cache` | 2610 ms | 2586 ms |
