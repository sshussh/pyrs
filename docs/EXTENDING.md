# Extending PyRs

A practical guide for anyone who wants to add a language feature, builtin,
method, or runtime behavior to **PyRs** — a compiler that turns a subset of
Python into native executables via LLVM.

PyRs aims to be a long-term **drop-in replacement for CPython** for the
surface it supports. Supported constructs must match CPython
**byte-for-byte** (stdout, stderr, exit codes) unless a deviation is
documented in the README and GUIDE.

---

## Contents

1. [Who this is for](#1-who-this-is-for)
2. [Setup](#2-setup)
3. [Pipeline](#3-the-pipeline-in-one-picture)
4. [Where changes live](#4-where-each-kind-of-change-lives)
5. [Primitives vs stdlib](#5-primitives-vs-stdlib)
6. [Worked example: str method](#6-worked-example-str-method-sisdigit)
7. [Worked example: builtin](#7-worked-example-builtin-absx)
8. [Worked example: statement form](#8-worked-example-statement-form)
9. [Testing discipline](#9-the-testing-discipline)
10. [Gotchas](#10-gotchas-that-have-bitten-before)
11. [Onboarding walkthrough](#11-onboarding-walkthrough)
12. [Crate-by-crate tour](#12-crate-by-crate-tour)
13. [IR inventory](#13-ir-inventory)
14. [Runtime ABI](#14-the-runtime-abi)
15. [Testing cookbook](#15-testing-cookbook)
16. [Debugging](#16-debugging-techniques)
17. [Diagnostics style](#17-diagnostics-style-guide)
18. [Performance](#18-performance-notes)
19. [Ship checklist](#19-the-ship-checklist)

**Companion docs:**

| Document | Role |
|----------|------|
| [GUIDE.md](GUIDE.md) | Language reference (what exists today) |
| [SPECIFICATIONS.md](SPECIFICATIONS.md) | Architecture, crates, build/link |
| [PRIMITIVES.md](PRIMITIVES.md) | Builtin/method/stdlib policy; IR vs C |
| [AGENTS.md](../AGENTS.md) | Short conventions for automated agents |
| [README.md](../README.md) | Overview, feature list, benchmarks |

---

## 1. Who this is for

This guide assumes you can:

- Build a Rust workspace project
- Read recursive-descent parser / typed IR patterns
- Run shell commands and `diff` outputs

You do **not** need to be a project maintainer. Follow the pipeline order,
prefer existing patterns, and never invent expected test output from
memory — always capture it from `python3`.

---

## 2. Setup

```console
$ make doctor          # rustc, cargo, cmake, cc, llvm-config, python3
$ make release         # target/release/pyrs
$ make test            # unit + e2e
$ make ci              # fmt-check + clippy -D warnings + tests + examples
```

Requirements: Rust (edition **2024**), LLVM (`llvm-config` on `PATH`),
CMake, a C/C++ compiler, and `python3` for parity checks.

Useful day-to-day targets:

```console
$ make run FILE=examples/fib.py
$ make emit-llvm FILE=prog.py     # writes prog.ll
$ make examples                   # parity for examples/*.py
$ make bench                      # benchmarks vs CPython
```

---

## 3. The pipeline in one picture

```
source text
   │  lexer/src/lib.rs          logos tokens + INDENT/DEDENT synthesis
   ▼
Vec<(Token, Span)>
   │  parser/src/lib.rs         recursive descent  →  parser/src/ast.rs
   ▼
ast::Module                     (syntax only, no types)
   │  semantic/src/lib.rs       names, types, coercions, desugaring
   ▼
ir::Module                      (ir/src/lib.rs — fully typed, no sugar)
   │  codegen/src/emit.rs       LLVM IR *text* emission
   ▼
LLVM IR string
   │  codegen/shim/src/lib.cc   parse → verify → optimize → object file
   ▼                            (you almost never touch this)
.o file  + codegen/runtime/runtime.c  ──cc -lm──►  native executable
```

**Ground rules:**

1. **Errors are `Diagnostic`s with spans.** Never panic on user input.
2. **IR is the contract.** Semantic resolves *everything* (types, casts,
   desugaring). Codegen does zero inference. If codegen must “figure
   something out,” fix semantic instead.
3. **Python semantics win.** Differential test vs `python3` is the law for
   supported constructs.
4. **New language features almost never need C++ shim changes.** The shim
   compiles whatever LLVM text it is given. Runtime behavior goes in
   `runtime.c` (or pure LLVM intrinsics from emit).
5. **Naming:** user functions → `pyrs_<name>`; runtime → `pyrs_*`; LLVM
   globals → `@g.<name>`; compiler temps → locals starting with `.`.

Multi-file programs: `cli/src/modules.rs` loads the import graph;
`semantic::analyze_program` lowers modules in topological order into one
`ir::Module`. Root module name is `__main__`.

---

## 4. Where each kind of change lives

| You want to add | Touch these, in order |
|-----------------|------------------------|
| Operator / token | lexer → parser (precedence) → semantic → codegen |
| `str` method | `ir::StrFn` → semantic table → codegen declare/call → `runtime.c` |
| `list` method | semantic (+ `ir` if new node) → codegen → often `runtime.c` |
| Builtin function | semantic `lower_call` (+ reserved `BUILTINS` if needed) → `ir` if needed → codegen (IR intrinsic and/or C) |
| Statement form | parser AST → semantic `lower_stmt` → `ir` only if needed → `emit_stmt` |
| New type | `ir::Ty` → parser `TypeName` → semantic coercions → codegen slots/print → runtime |
| Runtime-only fix | `runtime.c` + differential test |
| Multi-file resolution | `cli/src/modules.rs` + `semantic` import binding |

Prefer **desugaring into existing IR** over new nodes when possible
(study `lower_for_range`, `lower_list_comp`, `lower_with`).

---

## 5. Primitives vs stdlib

Before adding something large, decide *which layer* it belongs to
([PRIMITIVES.md](PRIMITIVES.md)):

| Layer | Examples | Home |
|-------|----------|------|
| Builtin / type method | `len`, `abs`, `str.find`, `list.append` | Compiler + IR and/or C kit |
| Hot path | list index, `len`, `abs` | Prefer **LLVM IR / intrinsics** |
| Cold / OS / layouts | `str.split`, `open`, slot mutators | **`runtime.c`** |
| High-level library | future `math`, `os.path`, `json` | **PyRs modules later** — not unbounded C |

Do **not** implement a high-level stdlib feature only in `runtime.c` if it
could be composed from primitives once the language is ready.

---

## 6. Worked example: str method (`s.isdigit`)

`isdigit` is **already in the tree**. This section shows the vertical
slice for a typical `str` method so you can mirror the pattern on a new
one.

Five vertical slices:

### 1. `ir/src/lib.rs` — `StrFn` variant

```rust
/// `s.isdigit()` → bool
IsDigit,
```

### 2. `semantic/src/lib.rs` — method table in `lower_str_method`

```rust
"isdigit" => (IsDigit, ir::Ty::Bool, 0),
```

Update the “supported: …” error string for unknown methods so users see
the new name.

### 3. `codegen/src/emit.rs` — `StrCall` mapping + declare

```rust
StrFn::IsDigit => ("pyrs_str_isdigit", true, false),
// in finish():
out.push_str("declare i32 @pyrs_str_isdigit(ptr)\n");
```

The `true` flag means “C returns `i32`, convert to `i1`.”

### 4. `codegen/runtime/runtime.c` — implementation

Match CPython edge cases first (`"".isdigit()` is `False`):

```c
int pyrs_str_isdigit(const PyrsStr *s) {
    check_ref(s);
    if (s->len == 0) {
        return 0;
    }
    for (long long i = 0; i < s->len; i++) {
        if (s->data[i] < '0' || s->data[i] > '9') {
            return 0;
        }
    }
    return 1;
}
```

### 5. Tests + docs

- Semantic unit test: result type is `bool`, kind is `StrCall { IsDigit, … }`.
- E2e: stdout captured from `python3`, not invented.
- README method list + GUIDE strings section.

---

## 7. Worked example: builtin (`abs(x)`)

`abs` is **already implemented**. This section shows how a reserved
builtin with an LLVM intrinsic body is wired end-to-end.

### Facts about the current design

- Reserved in `BUILTINS`: cannot `def abs(...): ...`.
- Lowered in `lower_call` after user/module function lookup fails.
- `bool` → `BoolToInt` then `Abs`; `int` / `float` keep their type.
- Wrong types: compile error `bad operand type for abs(): '…'` (caret on
  the argument).
- IR: `ExprKind::Abs(Box<Expr>)`.
- Codegen: `llvm.abs.i64(..., i1 false)` (wrap at `i64::MIN`) and
  `llvm.fabs.f64` (so `-0.0` → `0.0`).

### Pattern for a new builtin

1. If it must not be redefined, add its name to `BUILTINS`.
2. Add a `lower_call` arm: arity, lower args, type-check / promote.
3. Prefer existing IR nodes; add `ExprKind` only when needed.
4. Emit IR (inline / intrinsic) **or** call `pyrs_*` in C.
5. Differential tests for values **and** error paths.

---

## 8. Worked example: statement form

For something like `while ... else` / `for ... else`:

1. **Parser:** after the loop block, accept optional `else:` suite
   (same pattern as `parse_if`). Extend the AST node with `orelse`.
2. **Semantic:** desugar into existing IR — typically a hidden
   “did we break?” bool temp (`ctx.fresh_temp`), set on `Break`, tested
   after the loop. Study `lower_for_range` and `lower_for`.
3. **IR/codegen:** only if desugar is impossible. New `Stmt` variants
   need a careful `emit_stmt` arm (`fresh_block`, `start_block`,
   `terminated`).
4. Inspect output: `pyrs compile --emit-llvm -i t.py -o t` and read
   `t.ll`. Shim verifier errors mean invalid IR, not “Python wrong.”

---

## 9. The testing discipline

The project’s core claim is **byte-parity with CPython** for supported
features.

1. **Never invent expected output.** Capture with
   `python3 -c '...'` or by running the same program under CPython.
2. **Differential-test every feature:**
   ```console
   $ python3 prog.py > a; target/release/pyrs run -i prog.py > b; diff a b
   ```
   Cover empties, negatives, `-0.0`, bounds, and mixed types where legal.
3. **Test traps:** wrong types → *compile-time* diagnostics; runtime
   errors → CPython message + exit code 1
   (`run_program_expect_fail` in `cli/tests/e2e.rs`).
4. **Unit tests** per crate; **e2e** for real binaries; multi-file via
   `run_project` / `compile_project_expect_fail`.
5. **Gate:** `make ci` (fmt-check, clippy `-D warnings`, tests, example
   parity). Note: `make examples` currently globs `examples/*.py` only;
   multi-file demos under `examples/modules/` are covered by e2e.

---

## 10. Gotchas that have bitten before

- **LLVM text requires a NUL-terminated buffer** — handled in the shim via
  `getMemBufferCopy`; do not “optimize” that away.
- **`pyrs_die` takes a C string**, but PyRs string constants are
  `{ i64 len, bytes… }`. Codegen skips +8 bytes past the header. Always
  use `emit_die`.
- **List slots are 8 bytes:** float bitcast; pointers `ptrtoint` /
  `inttoptr`; bools `zext`/`trunc`. Use `slot_from_value` /
  `value_from_slot` only.
- **Print / contains tags are recursive:** scalars 0–3; list element tag
  is `4 + 8 × inner`. Rust `elem_tag` and C `pyrs_print_list` must agree.
- **`Ty` is `Copy` via interned lists:** build with `ir::list_of(elem)`;
  matches give `&Ty` (often dereference).
- **Blocks / terminators:** after `ret` / `br` / `unreachable`, rely on
  `self.terminated`. Capture `self.cur_block` *after* emitting a
  sub-expression when building phis (guards may open new blocks).
- **Entry / multi-module:** top-level of the root defines globals;
  dependency modules are namespaced (`module.name` in IR).
- **rustfmt + clippy in CI** — `make fmt` before committing.
- **LLVM version drift:** the C++ shim has small compatibility shims for
  triple APIs across LLVM releases; avoid unrelated C++ churn.

---

## 11. Onboarding walkthrough

This section is **not a backlog**. It is a short path through the tree
using two features that are **already shipped** — so a new contributor can
see how a real change is laid out before writing their own.

Worked write-ups of the same features: [§6](#6-worked-example-str-method-sisdigit)
(`isdigit`) and [§7](#7-worked-example-builtin-absx) (`abs`).

### Step 1 — Build and run something known

```console
$ make doctor
$ make release
$ make run FILE=examples/fib.py
$ python3 examples/fib.py    # same output
```

### Step 2 — Trace a str method end-to-end (`isdigit`)

Open these sites in order (or `rg IsDigit` / `rg isdigit`):

| Layer | Where to look |
|-------|----------------|
| IR | `ir/src/lib.rs` — `StrFn::IsDigit` |
| Semantic | `semantic/src/lib.rs` — `"isdigit" =>` in `lower_str_method` |
| Codegen | `codegen/src/emit.rs` — `StrFn::IsDigit` → `pyrs_str_isdigit` + `declare` |
| Runtime | `codegen/runtime/runtime.c` — `pyrs_str_isdigit` |
| Tests | `semantic` unit test `str_isdigit_*`; e2e `str_isdigit_matches_python` |

Optional: write a two-line program, compile with `--emit-llvm`, and find
the `call … @pyrs_str_isdigit` in the `.ll` file.

### Step 3 — Trace a builtin end-to-end (`abs`)

| Layer | Where to look |
|-------|----------------|
| Reserved name | `BUILTINS` in `semantic/src/lib.rs` (includes `"abs"`) |
| Lowering | `"abs" =>` arm in `lower_call` (bool → int, then `Abs`) |
| IR | `ExprKind::Abs` in `ir/src/lib.rs` |
| Codegen | `ExprKind::Abs` → `llvm.abs.i64` / `llvm.fabs.f64` |
| Tests | `abs_int_float_and_bool`, `abs_matches_python`, wrong-type compile error |

Notice: **no `runtime.c` change** — some primitives are pure LLVM.

### Step 4 — Feel the parity loop

```console
$ python3 -c 'print(abs(-5), abs(-0.0), abs(True))'
$ target/release/pyrs run -i /tmp/t.py   # after writing the same prints
```

Any future feature you add should survive this loop (plus edge cases and
error paths). See [§9](#9-the-testing-discipline).

### Step 5 — When you open a real PR

1. Prefer cloning an existing pattern (another `StrFn`, another `lower_call`
   arm, another desugar in `lower_for_*`) rather than inventing structure.
2. Differential test first; then implementation; then docs if the user-
   visible surface moved.
3. Ship with [§19](#19-the-ship-checklist).

Language gaps and product direction live in the README / GUIDE / roadmap
notes — not in this guide as a task list.

---

## 12. Crate-by-crate tour

Function names below are real — grep for them.

### `common` — spans and diagnostics

`Span { start, end }` is a byte range. `Diagnostic { phase, message, span,
file }` renders a caret snippet via `render`. `file` is the multi-module
file id (0 = first loaded module index from the driver). You construct
diagnostics often and almost never change this crate’s design.

### `lexer` — tokens and indentation

`Token` is a `logos` enum. The `Lexer` wraps logos with an indent stack
(visual width), pending `Indent`/`Dedent`, parenthesis depth for line
joining, and blank/comment suppression. `Token::describe()` feeds errors.

Adding a token: variant + `describe` / `token_text` + tests. Longest match
wins (`**=` before `**`).

### `parser` — recursive descent

Helpers: `peek`, `peek2`, `advance`, `eat`, `expect`, `error`.

Statements: `parse_module` → `parse_stmt` → simple/compound forms.
Blocks: `parse_block` (colon + NEWLINE + INDENT…DEDENT or one-line suite).

Expression ladder (lowest precedence first):

```
parse_expr → parse_or → parse_and → parse_not → parse_comparison
           → parse_arith → parse_term → parse_unary → parse_power
           → parse_postfix → parse_primary
```

Unsupported Python should say `"… is not supported yet"` naming the
feature when recognized.

### `semantic` — the brain

**Single file:** `analyze` → wraps `analyze_program` for one module.  
**Multi file:** `analyze_program(&[ModuleInput])` — signatures, import
bindings, lower in dependency order, merge into one `ir::Module`.

Per-module context (`ModuleCtx`): import bindings (`sys`, module alias,
`from` symbols), dependency surfaces, name prefix for non-root modules.

`FnCtx` threads: signatures, globals, `global` declarations, locals,
loop depth, temps (`fresh_temp`).

Functions you extend most:

| Function | Owns |
|----------|------|
| `lower_stmt` / `lower_block` | statement dispatch |
| `lower_expr` | expression dispatch |
| `lower_call` / `lower_call_with_sig` / `lower_module_call` | calls + builtins |
| `lower_binary` / `lower_str_binary` / `lower_contains` | operators |
| `lower_str_method` / `lower_list_pop` / `lower_method_stmt` | methods |
| `lower_file_method` / `lower_with` | files |
| `lower_list_lit` / `lower_list_comp` | lists + comprehensions |
| `lower_for` / `lower_for_range` | loop desugaring |
| `lower_compare_chain` | chained comparisons |
| `lower_cast` | `int`/`float`/`bool`/`str` |
| `bind_name` / `lower_assign` / `lower_aug_assign` | assignment |
| `coerce` / `coerce_assign` / `promote_numeric` / `unify_numeric` | promotions |
| `block_returns` / `stmt_returns` | return-path analysis |

**Invariant:** everything leaving this crate is fully typed and explicit.

Reserved builtins (cannot redefine): `print`, `len`, `range`, `input`,
`open`, `abs`.

### `ir` — the contract

Pure data: `Ty`, `Module`, `Function`, `Stmt`, `Expr`, `ExprKind`,
`BinOp`, `UnOp`, `StrFn`, `FileFn`. See [§13](#13-ir-inventory).

### `codegen` — LLVM IR text + shim + runtime

- **`emit.rs`:** `Emitter` builds textual IR.
- **`runtime/runtime.c`:** C kit linked into *user* programs.
- **`shim/`:** C++ LLVM driver linked into the *compiler* binary only
  (`codegen/build.rs` + CMake).

Reuse helpers: `line`, `tmp`, `fresh_block` / `start_block`,
`intern_string`, `emit_die`, `guard_zero`, `emit_ref_check`, `emit_len`,
`emit_list_elem_addr`, `slot_from_value` / `value_from_slot`,
`emit_short_circuit`.

New runtime symbols need a `declare` line in `finish()`.

### `cli` — driver

`compile` / `run` / `lex` / `parse`. Module load lives in `modules.rs`.
Final user link is roughly:

```text
cc program.o runtime.c -O2 -lm -o <output>
```

Touch CLI for new flags or load rules — not for ordinary language ops.

---

## 13. IR inventory

### Statements (`ir::Stmt`)

| Node | Meaning | Typical emission |
|------|---------|------------------|
| `Assign` | store local | `store` to alloca |
| `GlobalAssign` | store module global | `store` to `@g.*` |
| `IndexAssign` | `xs[i] = v` | bounds check + store |
| `ListAppend` | `xs.append(v)` | `pyrs_list_push` |
| `ListAppendUnchecked` | append with known capacity | inline store + len bump |
| `If` | if/elif/else | branch tree |
| `While { cond, body, step }` | all loops; `continue` → `step` | multi-block loop |
| `Return` | return | `ret` |
| `ExprStmt` | evaluate, discard | — |
| `Print` | `print(...)` | typed print helpers + sep/end |
| `Die(msg)` | runtime trap | `pyrs_die` + `unreachable` |
| `Break` / `Continue` | loop control | `br` to loop targets |

### Expressions (`ir::ExprKind`) — selected

| Node | Meaning |
|------|---------|
| Constants / `Local` / `GlobalLoad` / `Call` / `Binary` / `Unary` | core |
| `Let { name, value, body }` | temp binding (e.g. comparison chains) |
| `Index` / `Slice` | subscripts; missing slice bounds = `i64::MIN` sentinel |
| `StrCall { func: StrFn, args }` | str methods → `pyrs_str_*` |
| `FileCall { func: FileFn, args }` | file methods → `pyrs_file_*` |
| `Open` / `Input` / `Argv` | I/O and CLI |
| `Contains` | `in` / `not in` |
| `ListPop` / `ListLit` / `ListNew` / `Len` | list construction and length |
| `Block { stmts, result }` | statements inside expressions (comprehensions) |
| `Abs` | `abs(x)` via LLVM abs/fabs |
| `Min` / `Max` | 2-arg `min`/`max` (numeric; select on compare) |
| `Sum` | `sum(list[int\|float])` open-coded loop |
| Numeric / string casts / `ToBool` | `int`/`float`/`bool`/`str` and truthiness |

### `StrFn` (current)

`Upper`, `Lower`, `Strip`, `Lstrip`, `Rstrip`, `StartsWith`, `EndsWith`,
`Find`, `RFind`, `RIndex`, `Count`, `Replace`, `SplitWs`, `Split`, `Join`,
`IsDigit`, `IsAlpha`, `IsSpace`, `IsUpper`, `IsLower`.

### `FileFn` (current)

`Read`, `ReadLine`, `ReadLines`, `Write`, `Close`.

### `BinOp` notes

- `Div` is always float (true division).
- `FloorDiv` / `Mod` follow Python floored semantics.
- `Pow`: int → `pyrs_ipow` (dynamic negative exponent traps); float →
  guarded `llvm.pow`.
- On `str`: `Add` = concat, `Mul` = repeat, comparisons are lexicographic.

---

## 14. The runtime ABI

### List slots (8 bytes)

| PyRs type | Slot encoding | LLVM |
|-----------|---------------|------|
| int | value | — |
| float | IEEE bits | `bitcast` |
| bool | 0/1 | `zext` / `trunc` |
| str, list | pointer | `ptrtoint` / `inttoptr` |
| file | not stored in lists today | — |

### Layouts (shared with codegen)

```c
PyrsStr  { long long len; char data[]; }   // payload at +8, NUL-terminated
PyrsList { long long len; long long cap; long long *data; }
```

### Tags

Print/membership: `int=0`, `float=1`, `bool=2`, `str=3`; list-of-X =
`4 + 8·tag(X)` (recursive).

### Conventions

- Authoritative declare list: `Emitter::finish()` in `emit.rs`.
- Pointers as `ptr`; bools across the C boundary often as `i32` 0/1.
- Failures call `pyrs_die` (no error-code returns for most APIs).
- `check_ref` on pointer parameters → UnboundLocalError-style traps.
- Heap objects are **not freed** today (GC required before 1.0).

---

## 15. Testing cookbook

| Layer | Pattern |
|-------|---------|
| Lexer | `kinds("source")` → exact token sequence |
| Parser | `parse_ok` / `parse_err`; assert AST shape and precedence |
| Semantic | `analyze_ok` / `analyze_err` / `find_func(&m, ENTRY_NAME)`; top-level → `GlobalAssign` on entry |
| Codegen unit | lower to IR string; optionally compile object through shim |
| E2e | `run_program(tag, src)` → stdout; `run_program_expect_fail` → code + stderr |
| Multi-file e2e | `run_project` / `compile_project_expect_fail` |

Stdin/argv tests spawn the process with piped stdio (see existing e2e
cases). Tags keep temp directories unique under parallel tests.

---

## 16. Debugging techniques

1. **Read the IR:** `pyrs compile --emit-llvm -i t.py -o t` → `t.ll`.
2. **Trust the verifier:** `internal error: generated invalid IR: …`
   usually means wrong block edges, phis, or `lty` mismatches.
3. **Earlier phases:** `pyrs lex`, `pyrs parse`.
4. **Bisect divergences:** shrink the program; re-`diff` vs CPython.
5. **Runtime:** link with `-g` / ASan against `runtime.c`; printf is fine.
6. **Types:** temporary `eprintln!("{:?}", expr.ty)` — IR is `Debug`.

---

## 17. Diagnostics style guide

- Lowercase, no trailing period; name the thing:
  `name 'foo' is not defined`.
- Suggest a fix when one is obvious:
  `annotate the variable, e.g. 'xs: list[int] = []'`.
- Unsupported Python: `"… is not supported yet"` with the feature name.
- Narrowest useful span (the bad argument, not the whole statement).
- Runtime traps: **byte-identical to CPython** on your `python3` (wording
  can change across versions — re-check).
- Honest phases: if users see `error[codegen]` for bad source, semantic
  is missing a check.

---

## 18. Performance notes

- **Inline hot paths in IR** (list index, `len`, simple `abs`). LLVM can
  optimize IR it sees.
- **Call `runtime.c` for complex/cold work** (string algorithms, I/O).
  LLVM does **not** reliably optimize across that boundary.
- Measure: `./benchmarks/run.sh sort strings`, `make time FILE=…`.
- Trust O2 for scalar cleanup; emit clear IR rather than hand-SSA.
- **Never sacrifice parity for speed** without an explicit, documented
  deviation.

See also [PRIMITIVES.md](PRIMITIVES.md) § performance policy.

---

## 19. The ship checklist

1. `make ci` green (fmt, clippy, tests, examples).
2. Differential tests for the feature, including error paths.
3. If performance-relevant: `make bench` and update README numbers if they
   change meaningfully.
4. Update docs that would otherwise lie:
   - README feature list / known limits
   - GUIDE reference
   - this file (if pipeline patterns changed)
   - SPECIFICATIONS / PRIMITIVES (if architecture or kit policy moved)
5. Prefer small commits in dependency order when asked to split:
   `ir` → parser → semantic → codegen/runtime → tests → docs.
