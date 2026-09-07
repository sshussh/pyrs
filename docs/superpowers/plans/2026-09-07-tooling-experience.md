# Tooling and experience: cache management, ergonomics, project scaffolding

Three sequenced milestones — **0.111.0** cache management, **0.112.0** CLI
ergonomics, **0.113.0** project scaffolding. Taken from what uv and Cargo do
that PyRs did not, filtered by what actually applies to a compiler whose
package management belongs to somebody else.

## What was measured first

Before planning, on the development machine at 0.110.0:

| Probe | Result |
|---|---|
| `du -sh ~/.cache/pyrs` | **998 MB**, 3736 program entries, no eviction logic |
| `pyrs comple -i x.py` | `failed to read comple` — a typo became a script path |
| `pyrs check --inpt x` | `error: unexpected argument found` — no name, no hint |
| `pyrs compile -O0` vs `-O2`, warm | 76 ms vs 77 ms |
| `pyrs compile -O0` vs `-O2`, cold | 2523 ms vs 2541 ms |
| Debug info in output binaries | none — no `-g`, so no gdb or perf symbols |
| `cli/tests/extension.rs` | 1 test, against 14 for the cache and 19 for projects |

The last two rows of the middle block decided something by themselves.

## What is deliberately not copied

**Cargo's dev/release profiles.** Their entire purpose is trading
optimization for compile speed, and that trade does not exist here: 76 ms
against 77 ms warm, 2523 against 2541 cold. The runtime-object cache already
flattened it in 0.109. A profile table would be ceremony around a 1 ms
difference, and every key in a manifest is a compatibility obligation.

**`pyrs add` and a dependency table.** Settled in 0.110 and still right:
PyRs cannot compile arbitrary PyPI code, so the table would be a promise the
compiler could not keep.

**`pyrs publish`.** PyRs emits executables. `uv build` and `uv publish` own
the wheel, and a native extension is a wheel concern.

**`pyrs self update`.** Blocked on prebuilt binaries, which is a CI-matrix
and static-LLVM-linking problem tracked separately as the host/target matrix.

---

## Milestone 0.111.0 — cache management

### The problem

The cache had no way to inspect it, no way to clean it, and no bound. The
documented remedy was deleting the directory, which also throws away the
runtime objects shared by every build on the machine in order to reclaim
space held by programs — the two layers have completely different economics:

| Layer | Entry size | Shared by | Rebuild cost |
|---|---|---|---|
| `runtime` | ~310 KB per entry, 3 entries total | every build on the machine | 2.4 s |
| `programs` | ~270 KB **each**, one per program × opt level | one program | ~80 ms |
| `toolchain` | 64 B | every build | two subprocesses |

So reclaiming space almost always means programs, and the command surface
has to let you say that.

### The commands

```
pyrs cache dir
pyrs cache info
pyrs cache clean [--programs|--runtime] [--dry-run]
pyrs cache prune [--older-than 7d] [--max-size 2GiB] [--programs|--runtime] [--dry-run]
```

`uv cache dir/clean/prune` is the shape. Neither `--programs` nor
`--runtime` means both, which is what someone typing the bare command wants.
`toolchain` is never pruned.

### Least-recently-used, which needs a clock the cache did not have

Entry mtime is *creation* time, so an LRU policy reading it would evict the
entry you hit most. Entries gain a `used` stamp, written on a verified hit
and rewritten at most hourly — so a warm cache pays no write per hit, while
the ordering stays fine enough for a policy measured in days.

`prune` applies age first and then the size ceiling to whatever survived, so
`--older-than 7d --max-size 500MB` means both rather than whichever ran last.

### The opportunistic ceiling

Default 2 GiB, checked at most once per day behind a `last-gc` stamp,
disabled with `PYRS_CACHE_LIMIT=0`, and run only after publishing a program —
the only moment the cache grows. The stamp is written *before* the walk, so a
prune that is killed does not make every later build retry it.

This is the one place the tool deletes something it was not asked to. It is
deliberate: 998 MB in a day is not a directory a user can police by hand, and
the cost of a wrong eviction is bounded at 80 ms.

### `PYRS_CFLAGS` / `PYRS_LDFLAGS`

0.110 documented "`CC` is honored but `CFLAGS` is not" as an accepted limit.
The honest fix is to honor them *and* key on them, which this does — under
PyRs-specific names. Not `CFLAGS`: that is a make convention, is routinely
set machine-wide for unrelated builds, and `cc` does not read it on its own,
so adopting it would change PyRs's output because of a setting aimed at
something else.

### The subcommand boundary

`pyrs script.py` works by inserting `run` before any first argument that is
not a subcommand — and that list was a hand-maintained copy in `cli.rs`. Two
consequences, one live and one waiting:

- `pyrs comple -i prog.py` reported `failed to read comple`: a message about
  a file the user never named.
- Adding `pyrs cache` would have made `pyrs cache info` mean "run the script
  named `cache`".

The list is now derived from the parser itself, and a first argument that is
not a subcommand, does not exist as a file, is not obviously a script path,
and is within two edits of a real name is refused with a suggestion. A file
that exists always wins over a spelling guess.

### Verification

Ten new tests in `cli/tests/build_cache.rs` (24 total) covering info, layer
selection, dry runs, LRU eviction with a real reuse ordering, age pruning,
refusal without a budget, invalid budgets, and `PYRS_CFLAGS` changing the
key. Seven unit tests in `cache.rs` for the duration and size parsers,
including the overflow case — a wrapped size would turn a huge request into a
tiny budget and delete nearly everything. Four in `cli/tests/invocation.rs`
pinning the shim to every subcommand clap knows about.

---

## Milestone 0.112.0 — CLI ergonomics

**`pyrs doctor`** answers the question a *user* has, where `make doctor`
answers a contributor's. It reads the same resolution code the build runs, so
it cannot describe a different toolchain than the one used, and exits
non-zero on a problem — a report that says "no problems" about a project that
cannot build would be worse than no report.

**`pyrs build`** is `compile`, and both become project-aware the way `run`
already was: no `-i` builds the manifest entry through the declared import
root, no `-o` writes `target/NAME` rather than `./a.out` in the current
directory. The directory is `[tool.pyrs] target`, defaulting to `target/`.

The naming needs care in one place. `pyrs clean` removes the *project's*
output directory; the machine-wide cache stays `pyrs cache clean`. Conflating
them would mean clearing one project's outputs slowed down every build on the
system — the confusion `cargo clean` avoids by owning only `target/`.

**`pyrs completions`** for five shells, and clap's `error-context` +
`suggestions` features, which also fix nested subcommands for free.

### Verification

18 tests in `cli/tests/tooling.rs`: default and explicit outputs, a
configurable target directory, the unchanged `a.out` default outside a
project, clean against dry-run and against the shared cache, doctor's exit
code on a broken manifest, completion output for every shell, and both flag
and nested-subcommand typos.

## Milestone 0.113.0 — project scaffolding

`pyrs init` wrote a flat `main.py` and one table. `cargo new` and `uv init`
both produce a `src/` layout, a `.gitignore`, a README, a pinned interpreter
and a repository.

**The split stays "no `pyrs new`", but by what is already there rather than
by which command was typed.** A directory with a `pyproject.toml` belongs to
a project someone else created and gets one table, its existing entry adopted
rather than a second one invented beside it, and nothing else. A directory
without one gets the full layout — a user starting from nothing should not
have to assemble it by hand just because PyRs declined to own project
creation.

Three details that are not cosmetic:

- `root = "src"` is written *with* the layout. A `src/` layout is only
  importable with a declared root, so scaffolding one without the other
  produces a project that does not resolve.
- `.python-version`, not just `requires-python`. The latter states a floor;
  the former is what uv reads when it provisions the environment, and PyRs's
  Unicode tables and differential oracle come from a specific CPython.
  Writing only `requires-python` left uv free to pick 3.12 against 3.14.
- `/target` in `.gitignore`, so the first commit cannot contain build output.

`--name my-app` produces `src/my_app/`: a package directory has to be a
Python identifier, the same mapping uv applies.

### Verification

`cli/tests/project.rs` grows from 19 to 25. Six existing tests changed
premise rather than breaking: they scaffolded a flat `main.py` and now pass
`--script`, and `a_declared_root_makes_a_src_layout_importable` inverted —
init now writes the root, so the test asserts that and then removes it to
show the root is what makes the layout work. The git tests need
`GIT_CEILING_DIRECTORIES`, because the test tree lives inside PyRs's own
repository and git's search walks upward.

## Milestone 0.114.0 — machine-readable diagnostics and the import graph

`--message-format=json` is the cheapest large-payoff item on the list, and
the one nothing else can substitute for: an editor cannot get a span out of
prose, so until it exists no editor integration is possible at all whatever
else the compiler gets right.

The work is not the JSON. It is that **diagnostics were losing their
structure on the way out**: `LoadError` was a rendered `String`, so every
lex, parse and import failure had already become prose before the driver saw
it, and only semantic errors still had a span. `LoadError` keeps the
diagnostic alongside the rendered text, and a `Failure` type carries both to
the print site, where the format decides.

Two details worth stating:

- A failure with **no** position — an unreadable file, a failed link — is
  still emitted as JSON. Falling back to prose there would mean a tool's
  parser breaks on exactly the errors it did not anticipate.
- The encoder escapes every control character, not just the familiar ones.
  Diagnostics quote user source, and a raw byte below 0x20 makes the whole
  line unparseable.

No dependency: one flat object shape does not earn `serde_json` in a
workspace of 37 crates.

`pyrs tree` rides along because it needs the same plumbing — the resolver
already computed the graph exactly, `Loaded` just never exposed it. For a
closed-world compiler the output is a fact rather than an estimate: it is the
set of modules that will be compiled in.

### Verification

Six unit tests in `diagnostics.rs` (location fields, single-line output,
hostile source text, the position-less case, the unchanged human format, the
phase surviving both) and twelve in `cli/tests/tooling.rs` (28 total),
including one that asserts all three failing phases reach JSON.

## Later, planned but not scheduled here

- Stable error codes and `pyrs explain`, especially valuable for a *subset*
  compiler where the common error is "this exists in Python but not here" and
  the useful answer is a paragraph.
- `--debug` (`-g`) so compiled programs can be profiled or debugged at all.
- `pyrs test`: compile `test_*.py` and run it natively. pytest under CPython
  tests your logic; only a native runner catches PyRs-vs-CPython divergence
  in your own code.
