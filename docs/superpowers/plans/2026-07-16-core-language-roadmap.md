# Core Language Roadmap (post-0.20.1, no GC) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the post-0.20.1 high-value core-language surface (class usability, container kit, class kit depth, protocols/syntax polish) so ordinary Python-shaped programs compile with CPython parity, without GC or new stdlib modules.

**Architecture:** Grow bottom-up through the existing pipeline (`lexer → parser → semantic → ir → codegen → runtime.c`). IR remains the contract: semantic resolves all dispatch, peels, and desugars; codegen only emits LLVM for typed IR. Prefer desugaring in semantic (e.g. `assert` → `if not …: raise`) over new IR nodes when possible. Ship as successive SemVer minors **0.21 → 0.24** (patch for fixes). **Never free heap memory; do not implement GC.**

**Tech Stack:** Rust 2024 workspace (`common`, `lexer`, `parser`, `semantic`, `ir`, `codegen`, `cli`), LLVM text IR + C++ shim, C runtime (`codegen/runtime/runtime.c`), differential e2e vs `python3` in `cli/tests/e2e.rs`, gate `make ci`.

**Out of scope (explicit):** GC / RC / free; new stdlib modules; multi-inheritance / metaclasses / open `__dict__`; full gradual Any method dispatch; f-string `{x=}` / grouping / `n`/`c` polish (optional later).

**How to execute:** Complete **one phase** end-to-end (feat commits → e2e → docs/version ship → `make ci`) before starting the next. Do not mix unrelated phases in one commit.

**Testing law (every task):**
1. Capture expected output with `python3 -c '…'` (never invent stdout).
2. Prefer e2e in `cli/tests/e2e.rs` using `run_program` / `run_program_expect_fail`.
3. Semantic-only errors: unit tests in `semantic/src/lib.rs` **or** e2e that checks compile stderr.
4. Run: `cargo test -p cli --test e2e <filter> -- --nocapture` then `make ci` before shipping a phase.

**Shared helpers (e2e pattern):**

```rust
// Already in cli/tests/e2e.rs — reuse, do not reimplement.
fn run_program(tag: &str, source: &str) -> String { /* pyrs run */ }
fn run_program_expect_fail(tag: &str, source: &str) -> (i32, String) { /* … */ }
```

**Key existing hooks (do not reinvent):**

| Concern | Location |
|---------|----------|
| Class layout / methods | `ir::ClassInfo`, `semantic` class collect ~L1500+, `lower_class_construct` ~L11634, `lower_instance_method_call` ~L11714 |
| Default class print/str | `ir::ExprKind::ObjectToStr`, `pyrs_print_class_instance`, codegen interns `"<Name object>"` |
| Virtual method call | `ir::ExprKind::CallMethod { virtual_dispatch, candidates, … }` |
| List methods | `lower_list_method_stmt` ~L9040; IR `ListAppend` / `ListInsert` / … |
| Set union | `ir::ExprKind::SetUnion`, `\|` / `.union` already work |
| List comprehensions | `lower_list_comp` ~L13400; AST `ListComp` |
| `with` (files only) | `lower_with` ~L8963; AST `StmtKind::With` |
| Exception objects | `Ty::Exception`, `PyrsExc { type_tag, msg }`, `pyrs_print_exc` / `pyrs_str_from_exc` |
| Container conversions blocked | `lower_cast` rejects `list(...)` / `tuple(...)` / `dict(...)` / `set(...)` ~L16926 |
| `super()` rejected | `semantic` ~L15090 `"super() is not supported yet"` |
| Lexer already has `assert` | `Token::Assert` — parser does not lower it yet |

---

## File map (all phases)

| Path | Role |
|------|------|
| `lexer/src/lib.rs` | New tokens only if missing (`:=` already may need `ColonEqual`; `@` for decorators if not a token) |
| `parser/src/ast.rs` | AST nodes: `Assert`, `NamedExpr`, decorators on `FuncDef`/`ClassDef`, `DictComp`/`SetComp`, method flags |
| `parser/src/lib.rs` | Parse new forms |
| `ir/src/lib.rs` | New `ExprKind`/`Stmt`/`Ty` only when desugar is insufficient; extend `ClassInfo` for method kinds / dunder slots |
| `semantic/src/lib.rs` | Primary work: lower, typecheck, desugar (file is ~20k lines — match local style) |
| `codegen/src/emit.rs` | Emit new IR; never re-infer types |
| `codegen/runtime/runtime.c` | List/set/dict/str helpers; exception attrs; class str with dunder hook if needed |
| `cli/tests/e2e.rs` | Differential + trap tests |
| `README.md`, `docs/GUIDE.md`, `docs/SPECIFICATIONS.md` | Ship notes per phase |
| All `*/Cargo.toml` + `Cargo.lock` | Version bump only in ship commits |

---

## Phase 0.21 — Class usability + cheap wins

**Ship as:** `0.21.0`  
**Theme:** Classes print usefully; inheritance uses `super()`; `assert` and `list.extend` land as small kit.

### Task 1: `__str__` / `__repr__` on user classes

**Files:**
- Modify: `semantic/src/lib.rs` (`lower_cast` Str path, print lowering, class method registration)
- Modify: `codegen/src/emit.rs` (`ObjectToStr` / print of `Ty::Class`)
- Modify: `codegen/runtime/runtime.c` only if print path must call through a function pointer table; prefer pure LLVM call to `pyrs_<Class>___str__` from codegen
- Test: `cli/tests/e2e.rs`

**Design:**
- If class (or parent) defines `__str__(self) -> str`, then `str(obj)` and default `print(obj)` call that method (virtual when static type is a base).
- Else keep current `ObjectToStr` → `"<Name object>"`.
- `__repr__`: used by containers later and `repr()` if/when added; for this task: if `__repr__` exists use it for `repr`-like needs; `str()` falls back to `__repr__` when `__str__` missing (CPython rule).
- Reject non-`str` return types at semantic time.

- [ ] **Step 1: Write failing e2e (parity)**

Append to `cli/tests/e2e.rs`:

```rust
#[test]
fn v021_str_dunder_print() {
    let src = "\
class P:
    def __init__(self, x: int):
        self.x = x
    def __str__(self) -> str:
        return f\"P({self.x})\"
print(P(3))
print(str(P(4)))
";
    let out = run_program("v021_str_dunder", src);
    let py = std::process::Command::new("python3")
        .arg("-c")
        .arg(src)
        .output()
        .expect("python3");
    assert!(py.status.success(), "{}", String::from_utf8_lossy(&py.stderr));
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}
```

- [ ] **Step 2: Run test — expect FAIL**

```bash
cargo test -p cli --test e2e v021_str_dunder_print -- --nocapture
```

Expected: mismatch (`<P object>` vs `P(3)`) or success of both but stdout differs.

- [ ] **Step 3: Semantic — resolve dunder for str/print**

In `lower_cast` for `TypeName::Str` + `Ty::Class(id)`:

```rust
// Pseudocode inside lower_cast / print path:
if let Some(func) = resolve_method(id, "__str__").or_else(|| resolve_method(id, "__repr__")) {
    // lower as CallMethod with self=value, ret must be Str
    return lower_instance_method_call(value, id, method_name, span, &[], ctx);
}
// else existing ObjectToStr
```

Also change `print` of class values to go through the same path (search print-arg lowering near where `ObjectToStr` / class print is chosen).

Register `__str__` / `__repr__` as normal methods in `ClassInfo.methods` (already collected if defined as `def`).

- [ ] **Step 4: Codegen**

If semantic emits `CallMethod` for str, codegen already handles it. If you keep a dedicated IR node `ObjectToStr`, update its emit to:

```text
// load type_id; switch to call most-specific __str__ among closed-world classes that define it;
// default branch: existing "<Name object>" via interned strings / pyrs_print_class_instance
```

Prefer semantic always emitting `CallMethod` when dunder exists so codegen stays dumb.

- [ ] **Step 5: Inheritance override**

Add second e2e:

```rust
#[test]
fn v021_str_dunder_virtual() {
    let src = "\
class A:
    def __str__(self) -> str:
        return \"A\"
class B(A):
    def __str__(self) -> str:
        return \"B\"
def show(x: A):
    print(x)
show(B())
";
    let out = run_program("v021_str_virt", src);
    let py = std::process::Command::new("python3").arg("-c").arg(src).output().unwrap();
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}
```

Ensure `CallMethod { virtual_dispatch: true, … }` when static type is base.

- [ ] **Step 6: Reject bad return type**

```rust
#[test]
fn v021_str_dunder_must_return_str() {
    let src = "\
class P:
    def __str__(self) -> int:
        return 1
print(P())
";
    // Prefer compile-time error from semantic
    let dir = TempDir::new("v021_str_bad");
    // use existing compile-fail helper if present; else run_program_expect_fail
}
```

Message: `"__str__ must return str"` (or existing type-mismatch on method ret).

- [ ] **Step 7: Commit**

```bash
git add semantic/src/lib.rs codegen/src/emit.rs codegen/runtime/runtime.c cli/tests/e2e.rs
git commit -m "$(cat <<'EOF'
feat: honor __str__/__repr__ for class print and str()

Call user dunders with virtual dispatch when present; keep
"<Name object>" as the closed-world default.
EOF
)"
```

---

### Task 2: `super()` (single inheritance)

**Files:**
- Modify: `semantic/src/lib.rs` (reject site ~super; class method lower; `lower_call`)
- Possibly: `ir/src/lib.rs` only if a `SuperCall` node is cleaner than desugaring to a static `Call` of parent method
- Test: `cli/tests/e2e.rs`

**Design (closed-world, single base):**
- Inside instance method of class `C` with parent `P`, `super().m(args)` → static call to `P`'s (or further ancestor's) implementation of `m`, with same `self`.
- `super().__init__(…)` must work for field init chains.
- Zero-arg `super()` only (no `super(C, obj)` two-arg form in this phase).
- Outside methods: error `"super() outside of a method is not supported"`.

- [ ] **Step 1: Failing e2e**

```rust
#[test]
fn v021_super_init_and_method() {
    let src = "\
class A:
    def __init__(self, x: int):
        self.x = x
    def tag(self) -> str:
        return \"A\"
class B(A):
    def __init__(self, x: int, y: int):
        super().__init__(x)
        self.y = y
    def tag(self) -> str:
        return super().tag() + \"B\"
b = B(1, 2)
print(b.x, b.y, b.tag())
";
    let out = run_program("v021_super", src);
    let py = std::process::Command::new("python3").arg("-c").arg(src).output().unwrap();
    assert!(py.status.success(), "{}", String::from_utf8_lossy(&py.stderr));
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}
```

- [ ] **Step 2: Run — expect FAIL** (`super() is not supported yet`)

```bash
cargo test -p cli --test e2e v021_super_init_and_method -- --nocapture
```

- [ ] **Step 3: Track current method class in `FnCtx`**

When lowering methods, set e.g. `ctx.current_class: Option<ClassId>` and `ctx.self_name`.

In `lower_call` when `func == "super"`:

```rust
// super() → temporary SuperProxy value OR immediate attribute lower
// Prefer: only support super().attr(...) as Attribute on a Super() call expr
```

Parser already parses `super` as a name/call. Handle:

```text
Call { func: Attribute { base: Call { func: "super", args: [] }, attr: "m" }, args }
```

Resolve parent of `ctx.current_class`; `resolve_method_starting_from(parent, "m")`; emit **non-virtual** `CallMethod` / `Call` with `self` as first arg.

- [ ] **Step 4: `super().__init__`**

Same path; ensure parent `__init__` IR name is used, not child's.

- [ ] **Step 5: Error cases**

```rust
#[test]
fn v021_super_outside_method() {
    let src = "print(super())\n";
    // compile or runtime — prefer semantic error
}
#[test]
fn v021_super_no_base() {
    let src = "\
class A:
    def m(self):
        super().m()
";
    // AttributeError or semantic: no base / no method
}
```

- [ ] **Step 6: Commit**

```bash
git add semantic/src/lib.rs ir/src/lib.rs cli/tests/e2e.rs
git commit -m "$(cat <<'EOF'
feat: support zero-arg super() for single inheritance

Desugar super().m(...) to a static parent method call with the
same self; enable cooperative __init__ chains.
EOF
)"
```

---

### Task 3: `assert` statement

**Files:**
- Modify: `parser/src/ast.rs` — add `StmtKind::Assert { test: Expr, msg: Option<Expr> }`
- Modify: `parser/src/lib.rs` — parse `assert` (token exists)
- Modify: `semantic/src/lib.rs` — desugar to `if not test: raise AssertionError(msg or "")`
- Note: add `AssertionError` to `ir::ExcType` + runtime if missing; else map to `AssertionError` message via `Exception`/`RuntimeError` only if docs say so — **prefer real AssertionError tag** for CPython parity

**Check first:**

```bash
rg -n "AssertionError" ir/src/lib.rs codegen/runtime/runtime.c
```

If absent, add `AssertionError` to `ExcType`, `pyrs_exc_name`, raise helpers, and `ExcType::all_names`.

- [ ] **Step 1: Parser unit test** (in `parser/src/lib.rs` tests)

```rust
#[test]
fn parse_assert() {
    let m = parse("assert 1\nassert x, \"bad\"\n").unwrap();
    assert!(matches!(m.body[0].kind, StmtKind::Assert { .. }));
}
```

- [ ] **Step 2: Failing e2e**

```rust
#[test]
fn v021_assert_pass_and_fail() {
    let src = "assert True\nassert 1 == 1\nprint(\"ok\")\n";
    let out = run_program("v021_assert_ok", src);
    assert_eq!(out, "ok\n");

    let (code, err) = run_program_expect_fail("v021_assert_fail", "assert False, \"nope\"\n");
    assert_ne!(code, 0);
    // Match installed python3 AssertionError wording
    let py = std::process::Command::new("python3")
        .arg("-c")
        .arg("assert False, \"nope\"")
        .output()
        .unwrap();
    let py_err = String::from_utf8_lossy(&py.stderr);
    // at least contain AssertionError and nope
    assert!(err.contains("AssertionError"), "stderr={err}");
    assert!(err.contains("nope"), "stderr={err}");
    let _ = py_err; // optionally assert_eq full trap format if identical
}
```

- [ ] **Step 3: Implement parse + lower**

Desugar in semantic:

```rust
// assert test, msg  →
// if not bool(test):
//     raise AssertionError(str(msg) or "")
```

Use existing `Raise` + `ToBool` / `not` paths.

- [ ] **Step 4: Commit**

```bash
git add parser/src/ast.rs parser/src/lib.rs ir/src/lib.rs semantic/src/lib.rs \
  codegen/runtime/runtime.c codegen/src/emit.rs cli/tests/e2e.rs
git commit -m "$(cat <<'EOF'
feat: parse and lower assert to AssertionError

Desugar assert into a boolean check and raise with optional message,
matching CPython trap text.
EOF
)"
```

---

### Task 4: `list.extend`

**Files:**
- Modify: `ir/src/lib.rs` — `Stmt::ListExtend { list, iterable }` **or** desugar to loop of append in semantic (prefer **IR + runtime** for speed on large lists)
- Modify: `semantic/src/lib.rs` — `lower_list_method_stmt` `"extend"`
- Modify: `codegen/src/emit.rs` + `runtime.c` — `pyrs_list_extend(PyrsList *dst, PyrsList *src)` for same-elem lists; for other iterables desugar to for-append

**Scope for 0.21:** `xs.extend(ys)` where both are `list[T]` same `T`. Reject heterogeneous / non-list with clear error; str extend can be follow-up (`list[str].extend` from str chars).

- [ ] **Step 1: Failing e2e**

```rust
#[test]
fn v021_list_extend() {
    let src = "\
xs = [1, 2]
xs.extend([3, 4])
print(xs)
xs.extend([])
print(len(xs))
";
    let out = run_program("v021_extend", src);
    let py = std::process::Command::new("python3").arg("-c").arg(src).output().unwrap();
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}
```

- [ ] **Step 2: Semantic**

```rust
"extend" => {
    // args len 1; lower arg; require list[T] same elem; Stmt::ListExtend
}
```

- [ ] **Step 3: Runtime**

```c
void pyrs_list_extend(PyrsList *dst, const PyrsList *src) {
    for (long long i = 0; i < src->len; i++)
        pyrs_list_push(dst, src->data[i]);
}
```

- [ ] **Step 4: Commit**

```bash
git add ir/src/lib.rs semantic/src/lib.rs codegen/src/emit.rs codegen/runtime/runtime.c cli/tests/e2e.rs
git commit -m "$(cat <<'EOF'
feat: implement list.extend for homogeneous lists

Lower extend to a runtime bulk push; keep element type checks in semantic.
EOF
)"
```

---

### Task 5: Ship 0.21.0

**Files:** all `*/Cargo.toml`, `Cargo.lock`, `README.md`, `docs/GUIDE.md`, `docs/SPECIFICATIONS.md`, `AGENTS.md` version notes

- [ ] **Step 1: Update language section** — document `__str__`/`__repr__`, `super()`, `assert`, `list.extend`; list residuals (no two-arg super, no `__format__`, …)

- [ ] **Step 2: Bump versions** `0.20.1` → `0.21.0` in every crate

- [ ] **Step 3: `make ci`**

```bash
make ci
```

Expected: all green.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
chore: ship language and crate version 0.21.0

Document class dunders, super(), assert, list.extend, and remaining limits.
EOF
)"
```

---

## Phase 0.22 — Container constructors & kit

**Ship as:** `0.22.0`  
**Theme:** Everyday data-plane Python without rewriting loops by hand.

### Task 6: `list(iterable)` conversion

**Files:**
- Modify: `semantic/src/lib.rs` — `lower_cast` / `lower_call` for builtin `list`
- Modify: `ir` + `codegen` + `runtime` as needed (`pyrs_list_from_*`)

**Supported iterables (minimum):**
- `list[T]` → shallow copy (new list, same slots)
- `str` → `list[str]` of 1-char strings
- `tuple[...]` homogeneous only first; hetero → error or `list[Any]` if policy allows — **prefer require homogeneous or annotated**
- `range` / generators / sets / dict keys: implement what for-loop already supports

- [ ] **Step 1: Capture CPython + e2e**

```rust
#[test]
fn v022_list_ctor() {
    let src = "\
print(list(\"ab\"))
print(list([1, 2]))
print(list({3, 1}))  # set iteration order: document if sorted or hash order — match python3 on this machine
";
    // Prefer cases with stable order: str and list first
    let src = "\
print(list(\"ab\"))
xs = list([1, 2])
xs.append(3)
print(xs)
";
    let out = run_program("v022_list_ctor", src);
    let py = std::process::Command::new("python3").arg("-c").arg(src).output().unwrap();
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}
```

- [ ] **Step 2: Implement**

Replace error at `lower_cast` `TypeName::List(_)`. For bare `list(x)` call path in `lower_call`, materialize by:
1. allocate empty list with inferred elem ty from iterable
2. for-each append (reuse for-loop lowering) **or** specialized runtime for str/list

- [ ] **Step 3: Commit** `feat: support list(iterable) conversions`

---

### Task 7: `tuple(iterable)`, `set(iterable)`, `dict` from pairs

**Files:** same stack as Task 6

- [ ] **Step 1: e2e**

```rust
#[test]
fn v022_tuple_set_dict_ctors() {
    let src = "\
print(tuple([1, 2]))
print(tuple(\"ab\"))
s = set([1, 2, 2])
print(1 in s, 3 in s)
d = dict([(\"a\", 1), (\"b\", 2)])
print(d[\"a\"], d[\"b\"])
";
    let out = run_program("v022_ctors", src);
    let py = std::process::Command::new("python3").arg("-c").arg(src).output().unwrap();
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}
```

- [ ] **Step 2: Type rules**
  - `tuple(list[T])` → use fixed-arity only when length known; **dynamic length homogeneous tuple is not in IR today** — if IR tuples are fixed-arity only, either:
    - **Option A (recommended for kit):** `tuple(xs)` for `list[T]` becomes error `"tuple(list) requires constant length"` OR implement as **list copy packaged as tuple only for small known lens**, OR
    - **Option B:** For `tuple(iterable)` of unknown length, **not supported** — only `tuple(existing_tuple)` identity and `tuple([a,b])` list **literal** path.
  - **Practical 0.22 decision:** Support `tuple(x)` when `x` is already a tuple (identity/copy) or a **list/tuple literal context** isn't available — check IR: if fixed-arity only, document `tuple(dynamic list)` as unsupported and support `set(list[T])`, `dict(list[tuple[K,V]])` fully.

**Locked decision for implementers:**
1. `set(list[T])` / `set(str)` → `set[T]` / `set[str]`  
2. `dict(list[tuple[K,V]])` with `K in {int,str}`  
3. `tuple(list[T])` → **if length unknown, reject** with `"tuple() from dynamic list is not supported yet"`; allow `tuple((a,b))` copy and maybe fixed unpack from list lit via existing paths.

- [ ] **Step 3: Commit** `feat: set/dict constructors and limited tuple()`

---

### Task 8: `list.copy`, `dict.copy`

- [ ] **Step 1: e2e**

```rust
#[test]
fn v022_list_dict_copy() {
    let src = "\
a = [1, 2]
b = a.copy()
b.append(3)
print(a, b)
d = {\"x\": 1}
e = d.copy()
e[\"y\"] = 2
print(d.get(\"y\"), e[\"y\"])
";
    let out = run_program("v022_copy", src);
    let py = std::process::Command::new("python3").arg("-c").arg(src).output().unwrap();
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}
```

- [ ] **Step 2: Implement** shallow copy via runtime `pyrs_list_copy` / `pyrs_dict_copy` (new list/dict, copy slots/entries)

- [ ] **Step 3: Commit** `feat: list.copy and dict.copy shallow copies`

---

### Task 9: Set algebra (`&`, `-`, `^`, methods)

**Files:**
- `ir`: `SetIntersect`, `SetDiff`, `SetSymDiff` (mirror `SetUnion`)
- `semantic`: binary ops on `Ty::Set`; methods `intersection`, `difference`, `symmetric_difference`, `issubset`/`issuperset` optional
- `runtime.c`: implement set ops on `PyrsSet`

- [ ] **Step 1: e2e**

```rust
#[test]
fn v022_set_algebra() {
    let src = "\
a = {1, 2, 3}
b = {2, 3, 4}
print(sorted(list(a | b)))
print(sorted(list(a & b)))
print(sorted(list(a - b)))
print(sorted(list(a ^ b)))
";
    // sorted(list(...)) for order stability
    let out = run_program("v022_set_ops", src);
    let py = std::process::Command::new("python3").arg("-c").arg(src).output().unwrap();
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}
```

- [ ] **Step 2: Implement ops** (reuse hash table of set; allocate new set)

- [ ] **Step 3: Commit** `feat: set intersection, difference, symmetric difference`

---

### Task 10: Dict & set comprehensions

**Files:**
- `parser/src/ast.rs`: `DictComp { key, value, generators }`, `SetComp { elem, generators }`
- `parser/src/lib.rs`: parse `{k: v for ...}` vs set/dict lit ambiguity (already must distinguish empty `{}` vs set)
- `semantic`: clone `lower_list_comp` → `lower_dict_comp` / `lower_set_comp` (push set add / dict assign instead of list append)

- [ ] **Step 1: Parser tests**

```rust
#[test]
fn parse_dict_set_comp() {
    let m = parse("xs = {x: x*x for x in [1,2]}\nys = {x for x in [1,1,2]}\n").unwrap();
    // match DictComp / SetComp
}
```

- [ ] **Step 2: e2e**

```rust
#[test]
fn v022_dict_set_comp() {
    let src = "\
d = {x: x * x for x in [1, 2, 3] if x > 1}
print(d[2], d[3])
s = {c for c in \"abca\"}
print(\"a\" in s, \"d\" in s, len(s))
";
    let out = run_program("v022_comps", src);
    let py = std::process::Command::new("python3").arg("-c").arg(src).output().unwrap();
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}
```

- [ ] **Step 3: Implement lower** by adapting `lower_list_comp` generators walk; result type `dict[K,V]` / `set[T]` inferred from key/elem expressions.

- [ ] **Step 4: Commit** `feat: dict and set comprehensions`

---

### Task 11: `sorted` / `min` / `max` with `key=` (optional hard stretch)

**Only if Phase 0.22 has budget after Tasks 6–10.** Requires calling a closure per element.

- Scope: `key` is a nested `def` / lambda `T -> comparable`; monomorphic.
- Implementation sketch: decorate sort comparator to compare `key(a)` vs `key(b)`.

If too hard, **skip** and document `"key= is not supported yet"` (already the message). Prefer shipping 0.22 without `key=` over a buggy comparator.

---

### Task 12: Ship 0.22.0

Same checklist as Task 5: docs + version `0.22.0` + `make ci` + chore commit.

---

## Phase 0.23 — Class kit depth

**Ship as:** `0.23.0`  
**Theme:** Methods people expect on classes without open dynamism.

### Task 13: `@staticmethod` and `@classmethod`

**Files:**
- `parser`: decorators on `FuncDef` — extend `FuncDef` with `decorators: Vec<Expr>` (names only: `staticmethod`, `classmethod`)
- `semantic`: method kind enum on class methods; static = no self; classmethod = first arg is class object

**Design limits:**
- Decorator must be exactly `@staticmethod` or `@classmethod` (name expr).
- No stacked custom decorators yet.
- `classmethod`: first param is **not** a full first-class class value API — implement as passing a **type token** (`ClassId` as i64) **or** only allow `cls(...)` construct and `cls` in annotations-equivalent uses. **Minimal useful surface:** `cls` can construct `cls(...)` if `cls` is the same class / subclass marker; attribute access on `cls` for class-level constants can wait.

**Recommended minimal classmethod:**

```python
class C:
    @classmethod
    def make(cls, x: int) -> "C":
        return cls(x)  # lower as construct of the runtime class of the receiver / static class
```

For closed-world, `@classmethod` called on class `C.make(1)` or instance `C(1).make(2)` resolves to `C`'s method with `cls` fixed to defining class (CPython uses runtime class of invoker — implement **runtime type_id → ClassId** for instance call, static class for `C.make`).

- [ ] **Step 1: e2e staticmethod**

```rust
#[test]
fn v023_staticmethod() {
    let src = "\
class M:
    @staticmethod
    def add(a: int, b: int) -> int:
        return a + b
print(M.add(2, 3))
m = M()
print(m.add(4, 5))
";
    let out = run_program("v023_static", src);
    let py = std::process::Command::new("python3").arg("-c").arg(src).output().unwrap();
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}
```

- [ ] **Step 2: e2e classmethod construct**

```rust
#[test]
fn v023_classmethod_make() {
    let src = "\
class P:
    def __init__(self, x: int):
        self.x = x
    @classmethod
    def one(cls) -> P:
        return cls(1)
print(P.one().x)
";
    // may need from __future__ annotations or bare P return without quotes if supported
}
```

- [ ] **Step 3: Parser** — `@` decorator lines before `def` inside class body

- [ ] **Step 4: Semantic** — store `MethodKind::{Instance, Static, Class}` in side table keyed by IR func name; adjust `lower_instance_method_call` / attribute call on class name

- [ ] **Step 5: Commit** `feat: staticmethod and classmethod decorators`

---

### Task 14: `@property` (read-only)

**Design:**
- `@property` on method `name(self) -> T` creates a **field-like attribute load** that calls the method.
- No setter/deleter in this task (`@x.setter` → `"not supported yet"`).
- Conflict: property name vs real field — reject field with same name.

- [ ] **Step 1: e2e**

```rust
#[test]
fn v023_property_read() {
    let src = "\
class C:
    def __init__(self, x: int):
        self._x = x
    @property
    def x(self) -> int:
        return self._x
c = C(7)
print(c.x)
";
    let out = run_program("v023_prop", src);
    let py = std::process::Command::new("python3").arg("-c").arg(src).output().unwrap();
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}
```

- [ ] **Step 2: Lower attribute load** — if attr is property, emit `CallMethod` zero-arg user args

- [ ] **Step 3: Commit** `feat: read-only @property attribute access`

---

### Task 15: Bound methods as values

**Design:**
- `m = obj.method` (no call) produces a bound-method value.
- New IR type: `Ty::BoundMethod { self_ty, func: String, … }` **or** reuse `Ty::Closure` with a single capture `self` and known func pointer.
- **Prefer closure encoding:** desugar to a nested function that closes over `self` and calls the method — may conflict with monomorphic closure-in-container rules; dedicated `BoundMethod` IR is clearer.

```text
ir::Ty::BoundMethod { class_id, method: String, virtual: bool }
ir::ExprKind::BindMethod { object, class_id, method, candidates }
ir::ExprKind::CallBoundMethod { bound, args }
```

- [ ] **Step 1: e2e**

```rust
#[test]
fn v023_bound_method_value() {
    let src = "\
class C:
    def __init__(self, x: int):
        self.x = x
    def add(self, y: int) -> int:
        return self.x + y
c = C(10)
f = c.add
print(f(3))
";
    let out = run_program("v023_bound", src);
    let py = std::process::Command::new("python3").arg("-c").arg(src).output().unwrap();
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}
```

- [ ] **Step 2: Attribute load without call** currently errors `"bound methods as values are not supported yet"` — replace with `BindMethod`

- [ ] **Step 3: Call of bound method** via `lower_call` when callee ty is `BoundMethod`

- [ ] **Step 4: Commit** `feat: bound method values and calls`

---

### Task 16: `__iter__` / `__next__` protocol (user iterators)

**Design:**
- `for x in obj` when `obj` is `Ty::Class(id)` and class defines `__iter__`:
  - call `__iter__` → iterator object (class instance)
  - loop: call `__next__` until `StopIteration`
- PyRs currently uses Optional exhaustion for generators — for user iterators, **prefer CPython `StopIteration`** via existing exception machinery so `next(it)` can work later.
- Minimum: `__iter__` returns `self` and `__next__` increments an index field.

- [ ] **Step 1: e2e**

```rust
#[test]
fn v023_user_iter() {
    let src = "\
class Counter:
    def __init__(self, n: int):
        self.n = n
        self.i = 0
    def __iter__(self) -> Counter:
        return self
    def __next__(self) -> int:
        if self.i >= self.n:
            raise StopIteration(\"\")
        v = self.i
        self.i = self.i + 1
        return v
print([x for x in Counter(3)])
";
    // list comp over user iter — or use for-loop print
    let src = "\
class Counter:
    def __init__(self, n: int):
        self.n = n
        self.i = 0
    def __iter__(self) -> Counter:
        return self
    def __next__(self) -> int:
        if self.i >= self.n:
            raise StopIteration(\"\")
        v = self.i
        self.i = self.i + 1
        return v
for x in Counter(3):
    print(x)
";
    let out = run_program("v023_iter", src);
    let py = std::process::Command::new("python3").arg("-c").arg(src).output().unwrap();
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}
```

- [ ] **Step 2: Extend for-loop lowering** in semantic (where list/str/range are special-cased) with class-iter branch using try/except StopIteration or runtime helper.

- [ ] **Step 3: `__len__` / `__bool__` (bundle small)**
  - `len(obj)` → `__len__` if present (ret int)
  - `bool(obj)` / truthiness → `__bool__` if present, else `__len__ != 0`, else default True for instances

- [ ] **Step 4: Commit** `feat: __iter__/__next__ for-loops and __len__/__bool__`

---

### Task 17: Ship 0.23.0

Docs + versions + `make ci` + chore commit.

---

## Phase 0.24 — Protocols, syntax, exception polish

**Ship as:** `0.24.0`  
**Theme:** `with` beyond files; function decorators; walrus; richer exceptions.

### Task 18: Context manager protocol on classes

**Design:**
- Generalize `lower_with`:
  1. Evaluate `item` → `mgr`
  2. `mgr.__enter__()` → bind optional target
  3. try body
  4. finally / except: `mgr.__exit__(exc_ty, exc, tb)` — **subset:** pass `None, None, None` on success; on exception pass simplified args (types only or None) if full traceback too hard
- **Minimum viable:** `__exit__(self, *args)` with three `Any|None` params **or** fixed `None` triple and ignore return (don't suppress exceptions unless `__exit__` returns True — implement suppress if ret is bool True)

Files already have special-case with; branch:

```rust
if item_ty == File { existing }
else if Class(id) && has __enter__ && __exit__ { protocol }
else { error }
```

- [ ] **Step 1: e2e**

```rust
#[test]
fn v024_with_protocol() {
    let src = "\
class CM:
    def __enter__(self) -> int:
        print(\"enter\")
        return 41
    def __exit__(self, a, b, c) -> None:
        print(\"exit\")
with CM() as x:
    print(x)
print(\"done\")
";
    // params a,b,c may need types: use Optional/Any annotations if required
}
```

Use annotations as required by current typing rules (`a: Any = None` etc.).

- [ ] **Step 2: Implement lower_with protocol path** with try/finally ensuring `__exit__` runs (mirror file close).

- [ ] **Step 3: Commit** `feat: with statement context manager protocol`

---

### Task 19: Function decorators (non-class)

**Design:**
- `@decorator` above `def f` desugars to:

```python
def f(...): ...
f = decorator(f)
```

- Decorator must be a known function `Callable[[F], F]`-ish: monomorphic function taking a closure/function and returning same shape.
- **Phase limit:** single decorator, name only (not `@dec(arg)`), functions only (class decorators later).

Parser: `FuncDef.decorators: Vec<String>` or `Vec<Expr>`.

- [ ] **Step 1: e2e**

```rust
#[test]
fn v024_function_decorator() {
    let src = "\
def deco(f):
    def g(x: int) -> int:
        return f(x) + 1
    return g
@deco
def h(x: int) -> int:
    return x * 2
print(h(3))
";
    let out = run_program("v024_deco", src);
    let py = std::process::Command::new("python3").arg("-c").arg(src).output().unwrap();
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}
```

- [ ] **Step 2: Lower** after nested def registration: assign `name = decorator(name)` with closure types matching.

- [ ] **Step 3: Commit** `feat: single function decorators`

---

### Task 20: Walrus operator `:=`

**Files:**
- `lexer`: ensure `:=` token (add `ColonEqual` if missing — check `rg "ColonEqual|:=\"" lexer`)
- `parser`: `ExprKind::NamedExpr { target: String, value: Box<Expr> }`
- `semantic`: evaluate value, assign to local, yield value as expression result

- [ ] **Step 1: e2e**

```rust
#[test]
fn v024_walrus() {
    let src = "\
if (n := 2 + 3) > 4:
    print(n)
xs = [1, 2, 3]
while (x := len(xs)) > 0:
    print(x)
    xs.pop()
";
    let out = run_program("v024_walrus", src);
    let py = std::process::Command::new("python3").arg("-c").arg(src).output().unwrap();
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}
```

- [ ] **Step 2: Implement** as `Block { stmts: [Assign], result: Local }`

- [ ] **Step 3: Commit** `feat: assignment expressions (walrus :=)`

---

### Task 21: Exception object polish

**Scope:**
1. `e.args` → tuple of message (CPython: `args` is tuple; often `("msg",)` or `()`)
2. `repr(e)` / f-string `!r` on exceptions
3. Allow `Ty::Exception` as list/tuple **elements** (slot tag already exists for print — extend list elem allowlist)
4. Optional: `raise e` re-raise bound exception object (not only `raise ExcType("msg")`)

- [ ] **Step 1: e2e**

```rust
#[test]
fn v024_exc_args_and_list() {
    let src = "\
xs = []
try:
    raise ValueError(\"boom\")
except ValueError as e:
    print(e)
    xs.append(e)
print(len(xs))
print(xs[0])
";
    let out = run_program("v024_exc", src);
    let py = std::process::Command::new("python3").arg("-c").arg(src).output().unwrap();
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}
```

- [ ] **Step 2: Runtime** — `PyrsExc` already has `msg`; expose `args` as 1-tuple via codegen attribute or `GetExcArgs` IR

- [ ] **Step 3: Commit** `feat: exception args, repr, and container elements`

---

### Task 22: Match class patterns (subset)

**Scope:** `case Point(x=…, y=…):` or positional `case Point(x, y):` for closed-world classes with known fields.

- Requires match pattern AST extension + isinstance + field binds.
- Do **after** properties so field vs property is defined.

- [ ] **Step 1: e2e**

```rust
#[test]
fn v024_match_class_pattern() {
    let src = "\
class P:
    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y
def f(v: P):
    match v:
        case P(x=0, y=y):
            print(\"x0\", y)
        case P(x=x, y=y):
            print(x, y)
f(P(0, 2))
f(P(3, 4))
";
    let out = run_program("v024_match_cls", src);
    let py = std::process::Command::new("python3").arg("-c").arg(src).output().unwrap();
    assert!(py.status.success());
    assert_eq!(out, String::from_utf8_lossy(&py.stdout));
}
```

- [ ] **Step 2: Implement** keyword patterns mapped to field loads after successful `ClassIsInstance`

- [ ] **Step 3: Commit** `feat: match class patterns for closed-world classes`

---

### Task 23: Ship 0.24.0

Docs + versions + `make ci` + chore commit. Update `AGENTS.md` current limits to reflect new surface; reaffirm **GC still deferred**.

---

## Cross-cutting rules for every task

1. **Pipeline order:** parser/AST → semantic → ir → codegen/runtime → tests → docs (only at phase ship for versioned docs).
2. **Diagnostics:** `"X is not supported yet"` for intentional residuals; never panic on user code.
3. **No codegen inference:** if emit needs a type, put it on the IR node in semantic.
4. **Differential tests only** for runtime behavior; capture with `python3`.
5. **Commits:** small feat commits per task; one test commit only if you split tests; chore ship per phase.
6. **Do not** start GC, multi-base classes, or new stdlib modules under this plan.
7. **After each phase:** `make ci` must pass before the next phase begins.

---

## Suggested commit cadence (summary)

| Phase | Feature commits | Ship |
|-------|-----------------|------|
| 0.21 | `__str__`/`__repr__`; `super`; `assert`; `list.extend` | `chore: 0.21.0` |
| 0.22 | `list()`; set/dict ctors; copy; set algebra; dict/set comps | `chore: 0.22.0` |
| 0.23 | static/class method; property; bound methods; iter/len/bool | `chore: 0.23.0` |
| 0.24 | with protocol; decorators; walrus; exc polish; match class | `chore: 0.24.0` |

---

## Self-review (coverage)

| Recommendation (no GC) | Task(s) |
|------------------------|---------|
| `__str__` / `__repr__` | Task 1 |
| `super()` | Task 2 |
| Container constructors | Tasks 6–7 |
| `list.extend`, copy, set algebra | Tasks 4, 8, 9 |
| Dict/set comprehensions | Task 10 |
| `assert` | Task 3 |
| Walrus | Task 20 |
| `@staticmethod` / `@classmethod` / `@property` | Tasks 13–14 |
| Bound methods | Task 15 |
| Exception object completeness | Task 21 |
| Context manager protocol | Task 18 |
| `__iter__` (+ `__len__`/`__bool__`) | Task 16 |
| Function decorators | Task 19 |
| Match class patterns (Tier C) | Task 22 |
| `sorted`/`min`/`max` key= | Task 11 optional |
| GC | **Excluded** |
| New stdlib / multi-inherit / full Any | **Excluded** |

**Placeholder scan:** none intentional; Task 11 is explicitly optional with a skip rule.  
**Type consistency:** dunders and protocols use existing `CallMethod` / class `type_id`; bound methods introduce `Ty::BoundMethod` once in Task 15 and reuse in later call sites.

---

## Risk notes

| Risk | Mitigation |
|------|------------|
| `semantic/src/lib.rs` size | Touch only local helpers; no drive-by reformat |
| Tuple dynamic length | Reject honestly (Task 7 decision) |
| classmethod first-class classes | Minimal construct-only `cls` |
| `__exit__` exception suppress | Implement bool return; test both paths |
| Closure + decorator typing | Keep monomorphic; e2e before generalizing |
| StopIteration vs Optional generators | User iterators use exceptions; generators stay Optional |

---

## Execution handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-16-core-language-roadmap.md`.

**Two execution options:**

1. **Subagent-Driven (recommended)** — fresh subagent per task, review between tasks, start at Task 1 (Phase 0.21).
2. **Inline Execution** — execute tasks in this session with checkpoints (best one phase at a time).

**Which approach?** (If you only want Phase 0.21 first, say so — recommended.)
