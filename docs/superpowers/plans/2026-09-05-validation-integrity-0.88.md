# Validation and documentation integrity — 0.88.0

## Review and milestone choice

The 0.86 and 0.87 milestones were verified with gates that could not
detect several classes of regression, and the roadmap contained evidence
claims that nothing could reproduce. A gate that cannot fail is not a
gate, so this milestone fixes the measuring instruments before more
correctness work depends on them.

Four concrete problems, all found by review rather than by the gates:

1. **Example parity was not byte-exact.** The recipe compared
   `got=$($(PYRS) run -i $$ex)` against `want=$($(PYTHON) $$ex)`. Command
   substitution strips trailing newlines, so a program emitting the wrong
   number of them passed. stderr was never compared at all.
2. **The roadmap reported 1028 tests for 0.85; the real count was 1034.**
   A document whose purpose is evidence had arithmetic nobody checked.
3. **The sanitizer claim was not reproducible.** The 1.0 plan stated the
   adapter and runtime "passed AddressSanitizer and UndefinedBehaviorSanitizer
   checks", but there was no CI job, no Makefile target, and
   `cli/src/extension.rs` hardcoded `-O2` with no flag hook, so no
   supported way existed to produce an instrumented build.
4. **Failing tests deleted their own evidence.** CI uploads `target/tmp`,
   but the integration harnesses built under the system temp directory and
   removed it on `Drop`, so the upload was always empty.

## Milestone contract

- Example parity compares stdout bytes, stderr bytes and exit status of the
  compiled program. Compiling and running are separate steps, so C toolchain
  output on `pyrs run`'s stderr is never compared against an interpreter's;
  build output surfaces only when the build fails. The logic lives in
  `scripts/check_examples.py`, runnable directly, with `--opt-levels`.
- `make hygiene` checks version agreement across the seven crate
  manifests, `Cargo.lock`, three README sites, two SPECIFICATIONS sites
  and the compiled binary's `--version`, and resolves every relative
  documentation link.
- Both gates have failure-path tests. Fourteen tests deliberately break
  each condition and assert the gate reports it, including the exact
  trailing-newline case the old recipe could not see.
- `make asan` and `make ubsan` build the C adapter, runtime and collector
  instrumented via a new `PYRS_EXTENSION_CFLAGS` hook and run the
  extension boundary suite.
- Failing integration tests retain their inputs under
  `CARGO_TARGET_TMPDIR` (`target/tmp`).
- One roadmap document, not two.

## The gate's own first failure

The first version compared the stderr of `pyrs run` directly against
CPython's, and it turned all 13 examples red on CI while passing locally.
The cause was not a parity defect: `pyrs run` compiles the C runtime as
part of the run, and CI's clang emitted a warning that GCC 16 locally did
not, so every example's "stderr" contained toolchain chatter.

That is a design error in the gate, not a flaky environment. Comparing a
compiler's stderr against an interpreter's conflates two unrelated
channels. The gate now runs `pyrs compile` and then executes the produced
binary, which isolates program output from toolchain output; build output
is reported only when the build fails. Program stderr is still compared
strictly, and two tests pin the distinction: one asserts build noise does
not fail a matching example, another asserts program stderr still does.

The warning itself was real and pre-existing:
`(e->msg != NULL && e->msg->data != NULL)` tests a flexible array member,
which can never be null, so clang reported a tautological comparison. The
dead half of the condition is removed.

## Notes on the implementation

The sanitizer runtime has to be preloaded. The instrumented code is a
shared library `dlopen`ed by an uninstrumented `python3`, so ASan is not
first in the initial library list and aborts; the targets resolve
`libasan.so`/`libubsan.so` through `cc -print-file-name` and set
`LD_PRELOAD`. Leak detection is off because the conservative collector
does not free at exit by design; buffer and reference lifetimes are
asserted directly by the extension suite instead. Only the C translation
unit is instrumented, not the LLVM-generated kernel object, so results
must be described as adapter/runtime coverage rather than full coverage.

The link checker strips code spans before matching. `fs[0](args)` and
`t[i](args)` are Python but match markdown link syntax exactly; running
the first version against the repository produced two false positives,
which is now a regression test.

Writing the checks as scripts rather than workflow steps means they run
locally in `make ci` as well as in CI, and their behaviour is testable.

## Roadmap reconciliation

`docs/ROADMAP.md` and `docs/ROADMAP-1.0.md` had diverged into
contradiction: one listed exact int/float comparison as an open gap
requiring follow-up while the other recorded it as fixed, and they
disagreed on whether to bump minor versions per milestone. They also
duplicated the gap inventory. The delivery plan's unique content (product
contract, workstreams A-F, release gates, semantic references) moved into
`docs/ROADMAP.md`, the milestone history and gap table stayed, and a
measured-defect table probed against CPython 3.14 replaced prose claims.
`docs/ROADMAP-1.0.md` is deleted and its three inbound links repointed.

## Implementation plan

- [x] `scripts/check_examples.py`: byte-exact stdout/stderr/exit-status
      parity, `--opt-levels`, `--only`, and a reported oracle failure
      rather than a skip.
- [x] `scripts/check_hygiene.py`: version agreement (20 sites) and
      relative-link resolution (53 links), skipping code spans.
- [x] `scripts/test_gates.py`: 14 failure-path tests, including a check
      that the real repository passes both gates.
- [x] `PYRS_EXTENSION_CFLAGS` hook; `make asan` / `make ubsan` /
      `make sanitizers` with `LD_PRELOAD` resolution and a clear error
      when the sanitizer runtime is absent.
- [x] Integration harnesses build under `CARGO_TARGET_TMPDIR` and retain
      inputs when `std::thread::panicking()`.
- [x] Merge the two roadmaps; delete `ROADMAP-1.0.md`; repoint links.
- [x] Wire `hygiene` into `make ci`; document the targets in the README.
- [x] Bump to 0.88.0 (verified by watching the new gate catch the drift).

## Boundaries

CI workflow YAML is unchanged; the gates are wired into `make ci` and can
be called from CI in a later change. The `scientific-compatibility` job
remains deliberately unwired, since making it a required gate would make
every run depend on installing NumPy/pandas from PyPI.

Not addressed: tag-to-version agreement at release time, clean-machine
archive verification, instrumenting generated LLVM code, fuzzing, and the
`if-no-files-found: ignore` upload behaviour that would still mask a
missing artifact.

## Acceptance

A program that differs from CPython only in trailing newlines, only in
stderr, or only in exit status fails `make examples`. A version bump that
misses a documentation site fails `make hygiene`. `make asan` and
`make ubsan` complete the boundary suite. A failing integration test
leaves its inputs in `target/tmp`. One roadmap document remains.

## Validation (2026-09-05)

Toolchain: Rust 1.96.1, LLVM 22.1.8, CPython 3.14.7, GCC 16.2.1.

| Check | Result |
|-------|--------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1066 passed, 0 failed, 0 ignored |
| `make hygiene` | 16 gate tests passed; versions agree at 0.88.0 across 20 sites; 52 links resolve |
| `make examples` | 13/13 byte-exact including program stderr and exit status |
| `make compatibility` | native 12 pass / 6 known_gap; compat 6 pass |
| `make asan` | 9 passed, 1 skipped; no findings |
| `make ubsan` | 9 passed, 1 skipped; no findings |
| Retention probe | A deliberately failing test left `prog.py` in `target/tmp` |
| Release `pyrs --version` | `PyRs 0.88.0` |
