# PyRs Compiler Specification & Architecture

Public architecture document for PyRs: goals, technology stack, pipeline,
crate boundaries, IR contract, runtime ABI, and build/link strategy.

**Companion docs:**

| Document                       | Role                                           |
| ------------------------------ | ---------------------------------------------- |
| [`README.md`](../README.md)    | Product overview, language surface, benchmarks |
| [`GUIDE.md`](GUIDE.md)         | Full language reference and toolchain usage    |
| [`PRIMITIVES.md`](PRIMITIVES.md) | Builtin/method/stdlib split; primitives kit  |
| [`EXTENDING.md`](EXTENDING.md) | Contributor guide: how to add features         |
| [`AGENTS.md`](../AGENTS.md)    | Conventions for automated agents               |

**Versioning:** SemVer **MAJOR.MINOR.PATCH**, one number for language
surface, crates, and CLI (`env!("CARGO_PKG_VERSION")`). While **MAJOR is
0**, increase **MINOR** for milestones (`0.10.0` → `0.11.0` → …) and
**PATCH** for fixes. **`1.0.0` only when PyRs is ready for real-world
use** (not merely because the minor is large). Current milestone:
**v0.22** / `0.22.0`. Optional release tags: `vX.Y.Z`.

---

## 1. Goals

**PyRs** is an ahead-of-time compiler for Python. It turns source into a
standalone native executable through LLVM. There is no interpreter or VM
in the compiled program.

| Goal                                | Meaning                                                                                                                                                        |
| ----------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Drop-in replacement (long-term)** | Grow the supported surface until PyRs can replace CPython for real workloads.                                                                                  |
| **Byte-parity (hard rule today)**   | For every construct that _is_ supported, stdout, stderr, and exit codes must match CPython on the same program (modulo documented deviations in README/GUIDE). |
| **Native performance**              | Compute-bound code should compile to machine code comparable to C for the same algorithm.                                                                      |
| **Honest errors**                   | Unsupported features fail at compile time with a named `"… is not supported yet"` diagnostic; runtime traps reuse CPython's messages where applicable.         |

**Naming:**

| Form         | Use                                                     |
| ------------ | ------------------------------------------------------- |
| **PyRs**     | Project name in prose and docs                          |
| **`pyrs`**   | Binary, Cargo package, CLI                              |
| **`pyrs_*`** | C runtime symbols and mangled user functions in LLVM IR |

---

## 2. Technology stack

| Layer                       | Choice                                                                         |
| --------------------------- | ------------------------------------------------------------------------------ |
| Frontend / driver           | Rust (edition **2024**)                                                        |
| LLVM bridge (shim)          | C++ (CMake target standard **C++26**)                                          |
| Object emission / opt       | LLVM via `llvm-config` (parse → verify → optimize → object)                    |
| Final link of user programs | System `cc` + small C runtime                                                  |
| Build                       | Cargo workspace + CMake (shim only)                                            |
| Lexer                       | [`logos`](https://crates.io/crates/logos) + custom INDENT/DEDENT state machine |
| Parser                      | Hand-written recursive descent (not a parser combinator library)               |
| CLI                         | [`clap`](https://crates.io/crates/clap) derive API                             |
| FFI build glue              | [`cmake`](https://crates.io/crates/cmake) crate from `codegen/build.rs`        |

There is **no** MessagePack (or other binary) serialization of the
compiler IR across the Rust/C++ boundary. The hand-off is **LLVM IR text**.
There is no planned dependency on `chumsky` in the current design.

**Host requirements to build the compiler:** Rust toolchain, `llvm-config`
on `PATH`, CMake, a C/C++ compiler, and `python3` for parity tests and
benchmarks. `make doctor` checks these.

---

## 3. Pipeline and data flow

Strict **unidirectional** flow. Each stage consumes only the previous
stage's output.

```
source text
   │  lexer          logos tokens + INDENT/DEDENT / line joining
   ▼
Vec<(Token, Span)>
   │  parser         recursive descent → syntax-only AST
   ▼
ast::Module
   │  semantic       names, types, coercions, desugaring
   ▼
ir::Module           fully typed tree (the contract)
   │  codegen/emit   LLVM IR as text
   ▼
LLVM IR string
   │  C++ shim       parseIR → verify → opt (O0–O3) → native .o
   ▼
object file  +  runtime.c  ──cc -lm──►  native executable
```

**Driver (`cli`):** for `compile` / `run`, resolve the import graph
(`cli/src/modules.rs`), parse every module, run `semantic::analyze_program`,
emit LLVM IR, optionally write `<output>.ll`, invoke the shim to produce
an object file, then compile/link `runtime.c` with `cc`.

**Invariant:** if codegen must “figure out” a type or desugar a construct,
the design is wrong — that work belongs in **semantic**.

---

## 4. Workspace crates

```
pyrs/                 Cargo workspace (resolver = "3")
├── common/           spans, diagnostics
├── lexer/
├── parser/           AST + recursive descent
├── semantic/         typecheck + lower to IR
├── ir/               pure data structures (no analysis)
├── codegen/          emit.rs + runtime.c + CMake shim
└── cli/              binary `pyrs`
```

| Crate          | Role                                                                    | Input                               | Output                  | Depends on               |
| -------------- | ----------------------------------------------------------------------- | ----------------------------------- | ----------------------- | ------------------------ |
| **`common`**   | Shared `Span`, `Phase`, `Diagnostic` (with file index for multi-module) | —                                   | types                   | —                        |
| **`lexer`**    | Tokenize; synthesize `INDENT`/`DEDENT`; handle implicit line joining    | source `&str`                       | `Vec<(Token, Span)>`    | `common`                 |
| **`parser`**   | Build untyped AST                                                       | source (lexes internally) or tokens | `ast::Module`           | `lexer`, `common`        |
| **`semantic`** | Resolve names/imports, type-check, lower                                | one or more ASTs                    | `ir::Module`            | `parser`, `ir`, `common` |
| **`ir`**       | Typed IR contract                                                       | —                                   | data types only         | `common`                 |
| **`codegen`**  | Emit LLVM IR text; FFI to shim; embed `RUNTIME_C`                       | `ir::Module`                        | IR string / object file | `ir`, CMake, LLVM        |
| **`cli`**      | Orchestrate pipeline; module load; link user programs                   | `.py` paths                         | executable / dumps      | all crates               |

Dependencies form a DAG. **`ir` does not depend on parser or semantic.**
The C++ shim is built as a static archive and linked into the **`pyrs`**
binary; it is not linked into user programs.

---

## 5. Phase responsibilities

### 5.1 Lexer

- Wraps **logos** with a visual-width indent stack (`calc_indent`,
  `indent_stack`), pending `Indent`/`Dedent` queue, and parenthesis depth
  for implicit line joining.
- Blank/comment-only lines do not affect indentation.
- Open blocks are closed with `Dedent` tokens at EOF.
- Errors are `Diagnostic`s with phase `Lex`, never panics on bad input.

### 5.2 Parser

- Hand-written recursive descent; expression ladder by precedence
  (`or` → `and` → `not` → comparison → … → primary).
- AST is **syntax only** — no types, no name resolution.
- Unsupported Python constructs should produce clear
  `"… is not supported yet"` errors (not a vague syntax error) when
  recognized.

### 5.3 Semantic analysis and lowering

- Single-file entry: `analyze`; multi-file: `analyze_program` (driver always
  goes through the multi-file path after module load).
- Resolves names (locals, `global`, module globals, imports).
- Type-checks with a fixed type after first assignment; parameter
  annotations are required only when there is no default to infer from;
  return types may be inferred from `return` statements.
- Applies implicit numeric promotion (`bool → int → float`) and inserts
  **explicit** IR casts.
- Desugars sugar (e.g. `for` → `while` + step, list comprehensions,
  `with` for files, comparison chaining with temps).
- Checks return paths on functions declared to return a value.
- Emits a single flat `ir::Module` (all functions and globals, entry name
  `__main__`).
- **Reserved builtins** (cannot be redefined by `def`): `print`, `len`,
  `range`, `input`, `open`, `abs`. Casts `int`/`float`/`bool`/`str` are
  separate syntax/`Cast` paths, not this reserved list.
- Builtin *calls* are lowered in `lower_call` (and method tables for
  `str` / `list` / `file`); see [PRIMITIVES.md](PRIMITIVES.md).

### 5.4 IR

Pure data. Every expression carries `ty`. Codegen matches on IR only.

Notable shapes:

- **Types:** `int` (i64), `float` (f64), `bool`, `str`, `list[T]`
  (interned via `list_of` so `Ty` stays `Copy`), `file`, `None`,
  unions/Optional, `tuple`, `dict`, `set`, `Closure`, `Cell`,
  `Generator`. `file` is a runtime handle used for `open` / methods /
  `with` and may appear in signatures.
- **Statements:** assign / global assign / index assign, list append
  (checked and unchecked), `if`, `while` (+ step for desugared `for`),
  return, print, die, break/continue, expression statements, try/raise,
  match (desugared), yield-related control.
- **Expressions:** constants, locals/globals, calls, binary/unary
  (incl. bitwise), index and slice, `str`/`file` method calls, list/dict/set
  ops, `MakeClosure` / `CallClosure`, `CellLoad`/`CellStore`,
  `MakeGenerator` / resume, `Let` temps, `Block`, casts, `len`, `abs`,
  `input`, `argv`, `open`, etc.

Compiler temps use names starting with `.` (illegal as Python identifiers).

### 5.5 Codegen (Rust)

- `emit_llvm_ir` lowers IR to **textual** LLVM IR.
- User functions are mangled as `pyrs_<name>` (module symbols may be
  namespaced in the IR as `module.name` before mangling).
- Module globals are LLVM globals `@g.<name>`.
- **Two native styles** (see PRIMITIVES.md performance policy):
  - **Inline IR / LLVM intrinsics** for hot or simple ops (list index +
    bounds checks, `len` field load, `abs` → `llvm.abs.i64` /
    `llvm.fabs.f64`, scalar arithmetic).
  - **Calls into `runtime.c`** for bulk string/list work, I/O, print,
    and complex helpers (`pyrs_str_*`, `pyrs_list_*`, `pyrs_open`, …).
- List values live in **8-byte slots** (int/bool as integers; float
  bitcast; pointers as `ptrtoint`/`inttoptr`). Helpers
  `slot_from_value` / `value_from_slot` own the encoding.
- Runtime traps go through helpers that pass C strings to `pyrs_die`
  (PyRs string constants are length-prefixed; the die path skips the
  8-byte header).

### 5.6 C++ shim

- File: `codegen/shim/src/lib.cc` (+ small headers for LLVM version
  differences, e.g. target-triple APIs across LLVM 18 vs 21+).
- Export: `pyrs_compile_ir(ir_bytes, len, out_path, opt_level, err_buf, …)`.
- Parses textual IR with LLVM’s IRReader (**NUL-terminated** buffer via
  `getMemBufferCopy` — required by the LL lexer).
- Verifies, runs the standard new pass manager pipeline at O0–O3, emits a
  PIC object for the host triple.
- **Language-agnostic:** new PyRs features do not require C++ changes
  (except when adapting to new LLVM C++ API breaks).
- Internal C++ symbols use hidden visibility
  (`CXX_VISIBILITY_PRESET hidden`) to avoid clashing with Rust’s link.

### 5.7 C runtime

- Source: `codegen/runtime/runtime.c`, embedded as `codegen::RUNTIME_C`
  and written to a temp file at link time by the driver.
- Provides Python-faithful printing (float shortest round-trip repr,
  `True`/`False`, list repr), string and list heap objects, file I/O,
  `input` / `sys.argv` wiring, arithmetic helpers (e.g. floored float
  ops, int pow), and trap messages that match CPython where required.
- Not every language op hits C: some are pure LLVM (e.g. `abs` on
  int/float). The runtime is the home for layout- and OS-facing work.
- **Memory:** strings and lists are allocated and **not freed** today
  (documented limitation). Freeing / GC is required before a 1.0 release.

### 5.8 Final link of user programs

The driver runs approximately:

```text
cc program.o runtime.c -O2 -lm -o <output>
```

The **compiler** (`pyrs` binary) links the C++ shim and LLVM; **user
programs** link only the object file from the shim plus `runtime.c`.

---

## 6. Multi-module compilation

**Resolution model (v0.11):**

- Absolute imports search multiple roots (first hit wins; no full CPython
  `sys.path`):
  1. **Entry script directory** (user code; shadows everything else)
  2. **`PYRS_STDLIB`** if set and is a directory (dev/test shadowing)
  3. Workspace **`stdlib/`** next to the `cli` crate when present (dev only;
     stacked after env, not XOR — both may appear)
  4. **Embedded** stdlib sources compiled into the `pyrs` binary
     (`cli/build.rs` embeds `stdlib/**/*.py`; synthetic display paths like
     `<stdlib>/os/path.py`)
- **No split packages:** once a top-level package is found under a given
  origin (filesystem root or embed), submodules resolve only under that
  same origin (CPython package `__path__` spirit). An incomplete user
  `os/` does not pick `os.path` from stdlib/embed.
- A directory with `__init__.py` is a **package**. `import pkg.mod` loads
  the package init then the submodule (or a nested package’s `__init__.py`).
  Intermediate path components must be packages. Embedded packages use the
  same layout (`os/__init__.py`, `os/path.py`).
- Supported forms: `import M` / `import M as A`, `import pkg.mod` /
  `import pkg.mod as m`, `from M import x, y as z`, `from pkg.mod import
  x`, `from pkg import mod` (submodule), and relative imports inside
  packages (`from . import x`, `from .mod import y`, `from .. import z`).
- Relative imports are rewritten to absolute names at load time using the
  importer’s `__package__`. They are illegal in non-packages / top-level
  scripts (`attempted relative import with no known parent package`).
- The root module’s synthetic name is **`__main__`** (`ENTRY_NAME` /
  `ROOT_NAME`); other modules use fully-qualified dotted names
  (`utils`, `pkg.mod`).
- `import sys` is special-cased (exposes `sys.argv`); it is not loaded
  as a file. The first real stdlib package is pure-PyRs **`os.path`**
  (`join` two-arg POSIX, `dirname`, `basename`); see
  [PRIMITIVES.md](PRIMITIVES.md).
- Cycles and missing modules/names are compile errors with spans pointing
  at the importing file.
- Package `__init__.py` may import its own submodules (partial package
  init; not treated as a cycle). Re-exports are visible as package
  attributes and via `from pkg import name`. **Last top-level binding**
  decides Module vs value for a name; a `from . import name` does **not**
  load or bind a submodule when `name` is already a value/function on the
  package (CPython fromlist `hasattr` short-circuit). Partial init: child
  **module top level** sees only simple parent assigns/`def`s before the
  child-loading import; child **function bodies** may use deferred parent
  attributes/calls after full parent init.
- Multi-name `import a, b as c` is supported.
- Namespace packages (PEP 420 subset): directory without `__init__.py` is
  a package (`ModuleLoc::Namespace`); empty synthetic body; nested dirs;
  prefer `__init__.py` > `.py` > namespace dir; no multi-path split.
- `from m import *`: module-level only; expands public names or static
  `__all__` (list/tuple of string lits); dynamic `__all__` rejected;
  `from sys import *` unsupported.
- Still unsupported: multi-path namespace packages, dynamic import.
  Function-local `import` / `from … import` are supported (local scope;
  not star). Load-time diagnostics use phase tag `load`.

**Pipeline:**

1. `cli::modules::load_program` parses the root and dependencies (including
   parent packages), rewrites relative imports, detects cycles, returns
   modules in **topological order** (dependencies first, root last). The
   vector index is the diagnostic **file id**.
2. `semantic::analyze_program` collects signatures and global surfaces,
   validates import bindings (including package submodules), lowers each
   module with a per-module namespace (root keeps bare IR names; others
   use dotted `module.` prefixes, e.g. `pkg.mod.fn`), and merges into one
   `ir::Module`. Nested attribute chains (`pkg.mod.x`) resolve at compile
   time.
3. Module bodies run at the import site (like Python); parent package
   inits run before children; one linked executable contains the whole
   program.

---

## 7. Type system (current vs direction)

**Today (v0.22.0 subset):**

- Storage type is the join of all RHS types (and annotation); bare
  multi-assign may produce a union (`x = 1; x = "a"` → `int | str`);
  numeric multi-assign promotes (int then float → float). Annotations
  fix storage (not silently widened).
- Limited **`Any`**: annotation + concrete↔Any coerce via heap print-tag
  box (`i64`); runtime TypeError on bad FromAny (bool→int / int→float
  promotions allowed; Any→union matches member tags). Not full gradual
  typing (no open setattr / bare-Any methods).
- Parameter annotations optional when a default is present; bare params
  may be monomorphically inferred from body usage (arithmetic,
  comparisons, methods, indexing, single-type `isinstance` — multi-type
  or container `isinstance` and conflicting uses require annotation).
  Return annotation optional (inferred from returns when feasible, else
  “returns nothing”).
- Homogeneous lists; unannotated empty `[]` defaults to `list[Any]`;
  later `append`/`insert` in the same function/module body still fixes
  a more specific element type (`xs = []; xs.append(1)` → `list[int]`;
  mixed appends join). Module-level empty lists are pre-seeded so nested
  free reads resolve. Heterogeneous fixed-arity tuples;
  `dict[K,V]` / `set[T]` with `K`/`T` in `{int, str}`.
- Implicit promotions: `bool → int → float` in arithmetic, args, returns;
  subclass → base for class params/returns/list append/unions containing
  the base.
- Function-wide local scoping with `global` / `nonlocal`; nested
  `def`/`lambda` as closures (free vars via cells boxed at outer bind;
  late free cells unbound until assign; nested assign needs `nonlocal`;
  defaults freeze at def; homogeneous closures with matching capture env
  may live in containers); generators (`yield` / `yield from`
  list/tuple/str/gen including in finally; `return` stops after SE;
  `try`/`except`/`else`/`finally` with yield, phase and exit restored on
  resume; `close()` / `send` / `throw`); control-flow narrowing on
  `is None` / `is not None` and `isinstance` (unions, Optional, class
  base → subclass for field access; `and` peels compose left-to-right so
  more-specific isinstance is kept across further peels; mid-expr refine,
  match-guard refine; post-loop/if rebind clears stale peels; free module
  Optionals peel; reassignment re-refines subclass RHS into class/union
  storage); `match`/`case` subset (`as`, `*rest`, `**rest`, or, guards).
- Minimal exceptions: `raise` + `try`/`except`/`else`/`finally` via setjmp
  frames (process-global, single-threaded); runtime traps (`pyrs_die`) are
  catchable. `return`/`break`/`continue` pop the frame and run `finally`.
  Named traps only: other prefixes match bare `except:` only.
- **User classes (v0.21):** closed-world `Ty::Class(ClassId)`; instance
  header `{ i64 type_id, fields… }`; methods as IR functions
  `Class.method`; single inheritance + virtual dispatch; field layout
  specialized; `isinstance` with parent walk and flow peels to subclass;
  no multi-base, bound-method values, open `__dict__`, or GC free.

**Direction:**

- Move toward **full optional typing** and more of CPython’s dynamic
  semantics over time, without abandoning parity for the supported core.
- Document every intentional deviation in README/GUIDE until removed.

Documented deviations (non-exhaustive; see GUIDE § Differences): 64-bit
wrapping ints, `and`/`or` may form unions when operands differ,
`dict.get` bare form returns `Optional[V]`, ASCII-only string
case/whitespace rules, no GC, dynamic negative int `**` traps, etc.

---

## 8. Diagnostics

- Every user-facing failure from lex/parse/semantic is a
  `common::Diagnostic { phase, message, span, file }`.
- Rendering produces a labeled error with file:line:col and a caret
  underline under the span (narrowest useful span preferred).
- Synthesized or locationless messages may use a default span and print
  without a snippet.
- **Phase honesty:** if a bad program can reach codegen, semantic is
  missing a check. Users should not routinely see `error[codegen]`.
- Runtime errors print to stderr and `exit(1)` with CPython-compatible
  text where the runtime claims parity.

---

## 9. Build and linking strategy (compiler binary)

LLVM is linked into the **`codegen`** crate (and thus the `pyrs` binary)
from **`codegen/build.rs`**, not a separate `cli/build.rs`.

Order of operations:

1. **CMake** builds `codegen/shim` → static archive `libcodegen_shim.a`
   (C++ objects only; **does not** link LLVM into the archive).
2. Cargo is told `links = "codegen_shim"` so native link flags propagate.
3. **`llvm-config --libdir`** and **`llvm-config --libs`** for the
   components the shim needs (`core`, `support`, `native`, `analysis`,
   `irreader`, `passes`, `target`, `mc`, `bitreader`, `bitwriter`, …).
   Linking is **dynamic by default** on many distros (`--link-static` is
   not forced); `build.rs` emits whatever `-l` / `-L` flags
   `llvm-config` returns.
4. **`llvm-config --system-libs`** for LLVM’s system dependencies.
5. Link **`libstdc++`** for the C++ runtime used by the shim.

Symbol visibility for the shim archive is **hidden** so internal C++
symbols do not collide with the Rust toolchain.

Rebuild triggers: changes under `codegen/shim/` (listed in
`cargo:rerun-if-changed`).

---

## 10. Symbol and ABI conventions

| Kind                     | Convention                                                                     |
| ------------------------ | ------------------------------------------------------------------------------ |
| User function in LLVM    | `@pyrs_<name>` (after IR naming / module prefix)                               |
| Module global            | `@g.<name>`                                                                    |
| Compiler temporary local | name starts with `.`                                                           |
| Runtime API              | C functions `pyrs_*` declared in emitted IR and defined in `runtime.c`         |
| LLVM intrinsics          | e.g. `llvm.abs.i64`, `llvm.fabs.f64`, `llvm.pow.f64` — no C body               |
| String layout            | `{ i64 len, bytes… }` length-prefixed (+ trailing NUL for C interop)           |
| List layout              | `{ i64 len, i64 cap, i64* data }` with 8-byte value slots                      |
| Print / contains tags    | Scalars 0–3; nested list element tag = `4 + 8 × inner` (Rust and C must agree) |
| CLI argv                 | `@pyrs_set_args` from generated `main`; `@pyrs_argv` returns `list[str]`       |
| Program entry in IR      | synthetic function / module name `__main__` for the root script                  |

Changing a layout or tag encoding requires a coordinated edit of
**emit.rs** and **runtime.c**.

---

## 11. Compatibility and testing bar

1. **Differential testing:** for each supported feature, the same program
   under `python3` and `pyrs` must match (stdout and, for traps, message +
   exit code). Capture expected output from CPython; do not invent it.
2. **Unit tests** live in each crate; **e2e tests** in `cli/tests/e2e.rs`
   compile real binaries (including multi-file module projects).
3. **Local CI gate:** `make ci` → rustfmt check, clippy (`-D warnings`),
   full workspace tests, example parity vs `python3`.
4. **`make examples`:** globs `examples/*.py`, `examples/modules/*.py`,
   and `examples/packages/main.py` (package tree entry point).
5. **GitHub Actions:** `.github/workflows/ci.yml` mirrors `make ci`;
   separate workflows exist for benches, docs, and releases.
6. **Benchmarks:** `benchmarks/run.sh` verifies byte-identical output
   before timing; results may be summarized in the README.

Parity is a **hard rule** for supported features until the project owner
documents a deliberate exception.

---

## 12. Architectural limits and roadmap anchors

These are product constraints that affect design choices:

| Area             | Current                                              | Direction                                                                 |
| ---------------- | ---------------------------------------------------- | ------------------------------------------------------------------------- |
| Modules          | Packages, relative imports, namespace pkgs, `import *` | richer package semantics if needed                                      |
| Memory           | Never free heap strings/lists                        | GC / freeing **before 1.0**                                               |
| Typing           | Multi-assign join + bare-param body infer + isinstance peels + subclass coercion + limited `Any` | Fuller optional typing + more dynamism                                 |
| Builtins / kit   | `isinstance` (incl. on `Any`), `any`/`all`, `enumerate`/`zip`/`reversed`, set/dict kit | Finite native kit first — [PRIMITIVES.md](PRIMITIVES.md)                  |
| stdlib           | Multi-root + embed; pure-PyRs `os.path` subset; `sys` special-case | Grow pure-PyRs modules on the kit; C only for new primitive families      |
| Language surface | Subset (see README v0.20.1); stay on `0.y` until ready | **1.0** = real-world ready; then grow toward CPython drop-in              |
| Product version  | `0.22.0` (and later `0.23.0`, …)                      | Do not ship **1.0.0** until memory + readiness bar are met                |

Features explicitly **out of IR/runtime today** (non-exhaustive): full
CPython class dynamism (v0.20 has closed-world classes + isinstance peels),
advanced match patterns, full `yield from` send/throw forwarding,
f-string `{x=}` / grouping / `n`/`c`, GC. Prefer compile-time rejection with a
clear message over silent wrong behavior. `*args`/`**kwargs` on defs and
call-site unpacking, `from m import *`, and namespace packages are supported.

**Strategy:** finish optimized **primitives** (IR + C) for current and new
core types; grow pure-PyRs **stdlib** modules under repo `stdlib/` (embedded
into `pyrs`). Do not grow `runtime.c` with high-level libraries.

---

## 13. CLI surface (driver contract)

| Command                                               | Behavior                                               |
| ----------------------------------------------------- | ------------------------------------------------------ |
| `pyrs compile -i FILE -o OUT [-O 0..3] [--emit-llvm]` | Full pipeline → native executable; optional `.ll` dump |
| `pyrs run -i FILE [-O N] [-- ARGS…]`                  | Compile to a temp dir, execute, propagate exit code    |
| `pyrs lex -i FILE`                                    | Token dump (compiler debugging)                        |
| `pyrs parse -i FILE`                                  | AST dump (compiler debugging)                          |

Default optimization level is **2**. Entry semantics: top-level statements
run as a script; if there are none, a zero-argument `main` is called when
present.

---

## 14. Design principles (summary)

1. **Unidirectional crates** — no reverse dependencies; IR is the
   semantic↔codegen contract.
2. **Python semantics win** on every supported construct.
3. **Textual LLVM IR** is the only Rust↔C++ payload for codegen.
4. **Thin shim** — all language knowledge stays in Rust + `runtime.c`
   (plus LLVM intrinsics emitted from Rust).
5. **Diagnostics over panics** for user programs.
6. **Measure parity first**, performance second; never trade silent
   wrongness for speed. Prefer IR for hot primitives; C for complex/OS.
7. **Primitives before stdlib** — finite native kit, then PyRs modules
   ([PRIMITIVES.md](PRIMITIVES.md)).
8. **Document deviations** until they are removed on the path to drop-in
   CPython replacement.
