# Primitives, builtins, and the standard library

How PyRs should grow core operations, builtins/methods, and (later) a
standard library. **Strategy: primitives first** — a finite, optimized
native kit — then stdlib modules mostly in PyRs on top of that kit.

Companion docs: [SPECIFICATIONS.md](SPECIFICATIONS.md) (architecture),
[EXTENDING.md](EXTENDING.md) (full contributor guide for adding features),
[GUIDE.md](GUIDE.md) (language reference), [AGENTS.md](../AGENTS.md)
(agent conventions).

---

## 1. Three layers (do not conflate them)

| Layer | Examples | What it is | Default home |
|-------|----------|------------|--------------|
| **Builtin functions** | `len`, `print`, `min`, `open`, `abs` | Names the compiler treats as language/core API | **Intrinsic** in semantic → IR and/or `runtime.c` |
| **Methods on builtin types** | `str.find`, `list.append`, `file.read` | Operations on core representations | **Method table** in semantic → IR and/or `runtime.c` |
| **Standard library modules** | `math`, `os`, `json`, `pathlib` | Importable libraries (“batteries”) | **Mostly PyRs** (`.py`), calling the kit / thin C extensions |

**Explanation:** Builtins and type methods define the *language runtime*.
The stdlib is *product surface* that should not force every algorithm into
C. Special-casing `sys` was fine once; do not special-case every future
module the same way — use real package imports for new stdlib modules.

---

## 2. Where code should live

| Kind of work | Prefer | Avoid | Why |
|--------------|--------|-------|-----|
| Touches `PyrsStr` / list slots / file handles | C and/or IR | Pure PyRs reimplementation of layouts | Layouts live in the native runtime |
| OS / libc (stdio, FS, env, process) | C | Reimplementing syscalls in PyRs | Needs libc; trap messages centralized |
| Hot loop body (`len`, index, simple checks) | **LLVM IR** (inline) | Opaque C call in the inner loop | LLVM can optimize IR; not across `runtime.c` |
| Complex / cold algorithms on bytes | C (`pyrs_*`) | Huge match arms in emit | Correctness + maintainability |
| High-level glue once the language can express it | **PyRs stdlib** | Growing `runtime.c` without bound | Parity tests stay `diff` vs CPython |
| Needs dict / exceptions / classes you don’t have yet | Wait, or add a *type* primitive first | Faking `json` entirely in C | Wrong layer; blocks real drop-in path |

**Explanation:** “Primitives first” means **finish the native kit**, not
“everything forever is C.” High-level libraries belong in compiled PyRs
*after* the kit can support them.

---

## 3. Definition: what counts as a primitive

A symbol belongs in the **primitive kit** if **at least one** is true:

| Criterion | Meaning | Examples |
|-----------|---------|----------|
| **Representation** | Creates or mutates core layouts | `list_push`, str slice, future dict insert |
| **OS / I/O** | Talks to the environment | `open`, `input`, argv, later `getcwd` |
| **Hot / metal** | Must be cheap in tight loops | index, `len`, bounds check, simple `abs` |
| **Runtime contract** | Traps, print, alloc, type tags | `pyrs_die`, print tags, slot encoding |
| **Foundation for types** | Enables later language/stdlib | hash table ops, tuple pack, exception raise |

| Not a primitive | Why | Examples |
|-----------------|-----|----------|
| Pure composition of existing ops | Prefer PyRs stdlib | `os.path.join` (shipped v0.11), many `functools` helpers |
| Full library formats/protocols | Belongs in stdlib | `json.loads`, HTTP client |
| “Nice script helper” with no layout/OS need | Wait for stdlib or write in user code | ad-hoc pretty printers |

**Explanation:** If it doesn’t touch layout, OS, hot paths, or runtime
contract, keep it out of `runtime.c`. That keeps the kit finite and
optimized.

---

## 4. Decision table (use when adding a symbol)

Work top to bottom; stop at the first matching row.

| # | Question | If yes → | Notes |
|---|----------|----------|-------|
| 1 | Is it a core builtin or method on `int`/`float`/`bool`/`str`/`list`/`file` (or a new core type)? | **Compiler intrinsic / method table** + IR or C | Not a user-importable `.py` body (unless a later thin facade) |
| 2 | Does it need OS, libc, or raw object bytes? | **C primitive** | Exact CPython trap text where claimed |
| 3 | Is it on a measured hot path (benches / tight loops)? | Prefer **IR inline** (or promote C → IR later) | Correct C first, then specialize |
| 4 | Is it expressible with current PyRs + existing primitives? | Prefer **PyRs** (stdlib or user code) | Differential test vs `python3` |
| 5 | Does it need missing language features (dict, exc, classes, packages)? | **Add the missing primitive/type first**, or defer | Don’t fake a stdlib in C as a permanent answer |
| 6 | Still unclear? | Add to the **catalog as “proposed”**; implement only after family assignment | Avoid one-off symbols with no home |

---

## 5. Performance policy

Primitives-first **is** the right performance structure for an AOT
compiler — with the rule that **fastest ≠ always C**.

### 5.1 Speed ranking

| Rank | Mechanism | Optimizer sees body? | Use for |
|------|-----------|----------------------|---------|
| 1 (fastest) | IR fully inlined into caller | Yes | `len`, index, simple arithmetic helpers |
| 2 | Call to another function in the **same** LLVM module | Often yes (inliner) | Helpers emitted in IR; future whole-program stdlib |
| 3 | Call into `runtime.c` | **No** (opaque) | `find`, `split`, file I/O, large helpers |
| 4 (slowest anti-pattern) | Many tiny C calls per iteration | No | Per-byte `pyrs_*` in a hot loop |

**Explanation:** LLVM does not reliably optimize across the separately
compiled C runtime. Hot loops should not be a chain of opaque calls.

### 5.2 Implementation choice vs performance

| Choice | Performance impact | When it’s correct |
|--------|--------------------|-------------------|
| Primitive kit in IR + C | **Best foundation** | Core types and builtins |
| Hot primitive as IR | Often **2–10×+** vs C call in loops | Index, len, tight list updates (proven in benches) |
| Cold primitive as C | Negligible vs IR | Rare methods, I/O, complex string ops |
| Stdlib in **compiled** PyRs over fast primitives | Usually **same order** as C for the same algorithm | Glue, path logic, high-level modules |
| Stdlib / user logic as naive PyRs loops **instead of** kit ops | Can lose badly | e.g. reimplementing `find` in PyRs |
| Entire stdlib hand-written in C | Rarely worth it; opaque to LLVM unless fused | Only true systems code (regex engine, etc.) |

### 5.3 Rules of thumb

| Rule | Explanation |
|------|-------------|
| Mark catalog entries **hot: yes/no** | Hot → IR or few fat C calls; cold → C is fine |
| Correct first, optimize second | Differential parity before inlining |
| One C call per logical op | Prefer a single `find` over per-character runtime calls |
| Stdlib must not invent hot primitives silently | New hot path → kit review + benches |
| Compile user + stdlib together when possible | Helps LLVM inline thin PyRs wrappers |
| Design APIs for future GC/RC | Never-free is interim; alloc-heavy stdlib will suffer without a plan |

---

## 6. Primitive families (kit outline)

Organize work by **family**, not a flat list of C functions.

| Family | Responsibility | Examples (illustrative) | Body |
|--------|----------------|-------------------------|------|
| **Core / memory** | Alloc, ownership rules → GC later | `xmalloc`, future free/RC | C |
| **Numeric** | Scalar ops beyond pure LLVM | `abs`, `ipow`, floored float ops | IR and/or C |
| **str** | Immutable strings, ASCII policy today | `concat`, `find`, `split`, `isdigit` | C; index path may IR |
| **list** | Growable slots, mutators | `push`, `pop`, `insert`, `+`, `==` | C + IR for index/len |
| **file / io** | Text files, stdin, argv | `open`, `read`, `input`, `argv` | C |
| **print / traps** | CPython-like output and errors | `print_*`, `die` | C |
| **tuple** | Fixed sequences, unpack | pack/unpack, index, print | C + IR |
| **dict / set** | Hash tables, insertion order | get/set/del, iter, keys/values/items | C |
| **exceptions** | Raise/catch plumbing | `pyrs_raise`, try frames (`setjmp`), `die` → catch | C + IR landing |
| **OS extensions** (later) | Beyond files | env, cwd, stat | C (`_posix` / `_pyrs`) |

**Explanation:** Stdlib modules (`math`, `os.path`, `json`) are **not**
families in this table. They consume families. New C is justified when a
*family* grows (e.g. sockets), not when a high-level module is added.

---

## 7. Compiler surface vs runtime body

| Concern | Owner | Explanation |
|---------|-------|-------------|
| Name, arity, types, errors at compile time | **semantic** | Tables / match arms: Python name → kit entry |
| Fully typed ops, no sugar | **ir** | Enums like `StrFn`, expr/stmt nodes |
| Inline vs `call @pyrs_*` | **codegen / emit** | Performance policy lives here |
| Bytes, libc, trap strings | **runtime.c** (or later `.c` modules) | Single place for layout + messages |
| Import path, multi-file | **cli** modules loader (packages supported) | Stdlib discovery, not per-module special cases |

Every primitive should be a **vertical slice**:

```text
parity cases → runtime/IR → ir (if needed) → semantic → emit → tests → GUIDE
```

Unbound C with no compiler binding does not count as “done.”

---

## 8. Naming and ABI conventions

| Kind | Convention | Explanation |
|------|------------|-------------|
| C / LLVM runtime symbols | `pyrs_<area>_<op>` | e.g. `pyrs_str_find`, `pyrs_list_pop` |
| User functions in IR | `pyrs_<name>` (mangled) | Avoid libc collisions |
| Module globals | `@g.<name>` | |
| Compiler temps | names starting with `.` | Illegal as Python ids |
| List value slots | 8-byte encoding via helpers | Keep emit + runtime in lockstep |
| Print / contains tags | shared numeric scheme | Scalars 0–3; nested lists encoded |

**Future-facing:** each `pyrs_*` should be exposable later as something
like `_pyrs.str_find` for thin stdlib wrappers. Avoid magic that exists
only as a one-off emit path unless it is pure IR (`len`/index).

---

## 9. Stdlib approach (after the core language)

**Owner policy:** do **not** grow the stdlib until the **core language** is
far enough along that libraries can be written in **pure PyRs**. Load path
+ embed stay; new modules and large expansions wait. C is for **primitive
families** (libm, OS, regex, …), not for high-level library logic that the
language cannot yet express (e.g. dynamic `json.loads` without a value /
optional model).

| Phase | Stdlib stance | Explanation |
|-------|---------------|-------------|
| **Now (core-first)** | Freeze stdlib surface | No new modules; no expanding `math` / `json` / `os` via stubs |
| **Interim (shipped)** | Small `stdlib/` + some special cases | `os.path` pure; `math` / `json` / `getcwd` may use kit stubs — rewrite pure later |
| **Steady state** | Most modules in **PyRs** | Call builtins/methods + thin `_pyrs` / `_posix` for leftovers |
| **New C for stdlib?** | Only new **primitive families** | e.g. regex engine, not `pathlib` or JSON grammar in C |

### Example split (current + later)

| Module piece | Implementation | Explanation |
|--------------|----------------|-------------|
| `os.path.join` / `dirname` / `basename` | **Pure PyRs** in `stdlib/os/path.py` | Model for later libs; `join(a, *parts)` |
| `math.sin` / `sqrt` | LLVM / libm via kit (interim) | True primitive; thin pure wrappers later if desired |
| `json.dumps` / typed `loads_*` | C + stubs (interim) | Prefer pure PyRs once typing/dynamism allows |
| `json.loads` (dynamic) | **Later, pure PyRs** | Needs optional/union/`Any` or a value model — language first |
| `os.getcwd` | C primitive (interim) | OS family is a legitimate thin C edge |
| `sys.argv` | Kit special-case today | Migrate toward real module when ready |

**Embed rule:** edit `.py` files under workspace `stdlib/`; rebuild `pyrs`
to refresh the embedded copy. Search order keeps entry dir and
`PYRS_STDLIB` ahead of embed so tests/dev can shadow without patching the
binary.

---

## 10. Phased roadmap

| Phase | Focus | Exit criterion |
|-------|--------|----------------|
| **0. Catalog** | Families + hot/cold + parity notes | Written checklist (this doc + tracking list) |
| **1. Finish current types** | str/list/file/numeric builtins completeness | Scripts rarely need new C for text/list/file work |
| **2. Type primitives** | tuple → dict (→ set) | Data model enough for real programs (**done subset**) |
| **3. Control plane** | Exceptions; closures/*args growth (no GC yet) | Catchable traps; nested functions usable |
| **4. Core language** | Typing/dynamism, narrowing, kit, generators, … | Can write real libraries without compiler stubs |
| **5. Modules path** | Already: multi-root + embed | `import` loads stdlib `.py` |
| **6. Stdlib growth** | **Pure PyRs** modules on the kit | C only for primitive families — after phase 4 (may overlap late core) |
| **7. Final core** | **GC/RC/free** (classes shipped v0.19 subset) | GC remains required before 1.0; grow class dynamism carefully |
| **8. 1.0** | Real-world readiness | GC done + surface/stability bar |

**Explanation:** Do not grow a large stdlib (especially in C) before the
core language can host pure-PyRs implementations. Do not implement `json`
logic in C as a substitute for missing types/dynamism.

**Owner priority:** **classes** shipped (v0.19 closed-world subset).
Implement **garbage collection / heap freeing** only after other planned
core-language work the owner prioritizes — GC is the final major core
feature. Never-free is interim until phase 7; GC remains required for
**1.0**.

---

## 11. Catalog tracking template

Copy rows into a working checklist (issue tracker or a living section
below) as symbols are added.

| Symbol (Python) | Family | Hot? | Body (IR / C / both) | Compiler wired? | Parity tests? | Notes |
|-----------------|--------|------|----------------------|-----------------|---------------|-------|
| `len(x)` | core | yes | IR | yes | yes | |
| `s.isdigit()` | str | no | C | yes | yes | ASCII digits |
| `xs[i]` | list | yes | IR | yes | yes | bounds + slot |
| `open` | io | no | C | yes | yes | |
| `xs.insert` | list | med | C | … | … | example gap |
| `dict` get/set | dict | yes | C (+ IR later?) | … | … | planned family |
| `os.path.join` | *(stdlib)* | no | PyRs (`stdlib/os/path.py`) | yes (v0.11) | yes | 2-arg POSIX; not a kit entry |

---

## 12. Summary

| Topic | Decision |
|-------|----------|
| Overall strategy | **Primitives first**, then stdlib in PyRs |
| Builtins & type methods | Compiler + **IR/C kit**, not pure PyRs |
| Hot primitives | Prefer **IR**; C is default for complex/cold/OS |
| Stdlib | **Mostly PyRs** on top of the kit; thin C extensions only |
| Performance | Kit structure is right; **inline hot paths**; don’t reimplement kit ops in PyRs |
| `runtime.c` growth | **Finite families only**; no high-level libraries |
| 1.0 gate | **Real-world readiness** (incl. **memory management**); stay on `0.y` until then; never-free is interim |

When unsure, re-run the [decision table](#4-decision-table-use-when-adding-a-symbol) and assign a **family** before writing code.
