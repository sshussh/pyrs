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

`pyrs doctor` (user-facing toolchain report), `pyrs clean` (project outputs),
`pyrs completions <shell>`, and clap's `suggestions`/`error-context` features
so an unknown flag names itself.

## Milestone 0.113.0 — project scaffolding

`pyrs init` currently writes a flat `main.py` and one table. Cargo and uv both
scaffold a `src/` layout, a README, a `.gitignore`, a pinned interpreter and a
git repository. Matching that is the milestone.

## Later, planned but not scheduled here

- `--message-format=json` for diagnostics, which is what makes an editor
  plugin possible at all.
- Stable error codes and `pyrs explain`, especially valuable for a *subset*
  compiler where the common error is "this exists in Python but not here" and
  the useful answer is a paragraph.
- `pyrs tree` over the import graph, which the module resolver already
  computes exactly.
- `--debug` (`-g`) so compiled programs can be profiled or debugged at all.
- `pyrs test`: compile `test_*.py` and run it natively. pytest under CPython
  tests your logic; only a native runner catches PyRs-vs-CPython divergence
  in your own code.
