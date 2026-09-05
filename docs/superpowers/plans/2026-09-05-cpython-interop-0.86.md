# CPython interoperability and stranded correctness fixes — 0.86.0

## Review and milestone choice

Measured against CPython 3.14 on `main` at `7e5d98c` (0.85.0), before any
change in this milestone:

| Probe | CPython 3.14 | PyRs 0.85.0 |
|-------|--------------|-------------|
| `2 ** 53 + 1 == 9007199254740992.0` | `False` | `True` |

That fix already existed, written and CI-validated, on an unmerged and
**unpushed** branch. `main` therefore shipped a known wrong answer whose
correction was sitting in a single local copy. Landing it is strictly
higher value than starting new work, so this milestone unstrands the whole
`feat/cpython-interop` line: Python-style invocation, explicit `--compat`
whole-program CPython execution, `pyrs check`, the experimental CPython
extension bridge, the compatibility probe harness, and the native
correctness fixes (exact mixed int/float comparison, conditional-local and
generator binding checks, preserved side effects in None-valued
comparisons and `print(sep=/end=)`).

One defect found in review had to be fixed before the bridge could land.

## The borrowed-buffer defect

`bridge_sequence` set `list.cap == list.len` on two headers whose storage
this runtime does not own: a `PyMem_Malloc` copy of a Python list, and a
borrowed `Py_buffer` export from NumPy/pandas. Any growth would then reach
`list_ensure_cap` / `pyrs_list_push` / `pyrs_list_insert`, which call the
**libc** allocator: `xmalloc` a new block, `memcpy`, then `free(l->data)`
on exporter- or `PyMem`-owned memory. The consequences were heap
corruption from the mismatched allocator, a stale `arg->owned` handed to
`PyMem_Free`, and a released buffer lease we were still obliged to return.

`docs/INTEROPERABILITY.md` promised buffers were read-only, but the only
thing enforcing it was an AST allowlist in `cli/src/extension.rs`. A
memory-safety invariant should not depend on a frontend analysis staying
complete.

Fix: borrowed headers carry `PYRS_LIST_BORROWED_CAP` (`-1`) and every
growth site calls `list_require_owned`, raising `BufferError`. The growth
conditions became `len >= cap` rather than `len == cap`, so a negative
marker enters the guarded branch instead of falling through into an
out-of-bounds store. Owned lists pay no extra cost: the check runs only
on the growth path, which already reallocates.

Direct element stores remain frontend-enforced and are documented as such.
Generated code indexes borrowed slots inline, so guarding stores would
either cost a branch on every list write or need a distinct immutable
buffer type. That is deliberately out of scope here.

## Implementation plan

- [x] Back up the unpushed branch (`git bundle`, verified complete history)
      before rewriting it.
- [x] Rebase `feat/cpython-interop` onto `main`; resolve the two README
      conflicts, keeping both the interop sections and the version heading.
- [x] Add `PYRS_LIST_BORROWED_CAP` + `list_require_owned`; guard
      `pyrs_list_push`, `pyrs_list_insert` and `list_ensure_cap`
      (`pyrs_list_extend` inherits the guard through push).
- [x] Mark both non-owned bridge paths (PyMem copy, borrowed export).
- [x] State in `docs/INTEROPERABILITY.md` exactly which layer enforces
      which half of the read-only rule, including what is *not* checked.
- [x] Bump the 7 crates, lockfile, README and SPECIFICATIONS to 0.86.0.
- [x] Verify: full local gate, extension boundary suite, and a direct
      C harness proving the guard traps.

## Boundaries

CI wiring is deferred to the validation milestone. The branch's `ci.yml`
change made `scientific-compatibility` a *required* gate that pip-installs
NumPy/pandas on every run, so a PyPI outage or a yanked pin would redden
`main` for reasons unrelated to the code. `make ci` still runs the
compatibility probes locally, so coverage is retained meanwhile.

No Unicode, numeric-fidelity, protocol-dispatch or syntax work. The bridge
stays experimental: no automatic mixed execution, no general object or
array semantics, no stable standalone library ABI, and runtime OOM still
calls `exit`, which would terminate a host interpreter.

## Acceptance

Exact mixed int/float comparison matches CPython. Growing a borrowed
buffer raises `BufferError` rather than corrupting the heap. The bridge's
existing boundary behaviour is unchanged. Documentation and
`pyrs --version` report 0.86.0 without implying 1.0 readiness.

## Validation (2026-09-05)

Toolchain: Rust 1.96.1, LLVM 22.1.8, CPython 3.14.7, GCC 16.2.1.

| Check | Result |
|-------|--------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace` | 1051 passed, 0 failed, 0 ignored |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 12 pass / 6 known_gap; compat 6 pass |
| `compatibility/test_extension.py` | 10 tests, 9 passed, 1 skipped (NumPy/pandas absent from CPython 3.14) |
| Borrowed-buffer guard, C harness | `BufferError: cannot resize a borrowed buffer`, exit 1 |
| Release `pyrs --version` | `PyRs 0.86.0` |

The 6 `known_gap` results are the recorded Unicode byte-length and mixed
numeric list defects, scheduled for the following milestones. They are
reported as gaps rather than passes.
