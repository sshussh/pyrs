# Dynamism at native speed

**Status: plan, nothing implemented.** Research and a proposed milestone
order for closing the gap between PyRs's closed world and CPython's object
model *without* giving up the 6–41× that makes PyRs worth using.

Companion docs: [ROADMAP.md](../../ROADMAP.md) workstream C,
[SPECIFICATIONS.md](../../SPECIFICATIONS.md) (architecture),
[PRIMITIVES.md](../../PRIMITIVES.md) §9 (the stdlib freeze),
[GC.md](../../GC.md) (why representation and the collector are one problem).

---

## 1. The measurement that reframes the problem

The obvious framing is "PyRs is missing dynamic features; add them." That
framing is wrong, and one benchmark says so. The same loop, three times, at
-O2 (20M iterations, `total += i * 2`):

| Version | Time | Allocated | vs CPython |
|---|---:|---:|---:|
| `i: int` — fully typed | 0.025s | 0 B | **33.5× faster** |
| `v: int \| str = i` — two-member union | 0.024s | 0 B | **34.9× faster** |
| `v: object = i` — dynamic (`Any`) | 0.591s | **320 MB, 305 collections** | **1.4× faster** |
| CPython 3.14.7 | 0.838s | — | 1.0× |

Read the third row against the first: **using dynamism costs 23.6×**, and
lands almost exactly where CPython already is. A PyRs that is fully dynamic
*and* built on today's dynamic representation would be a Python
implementation with no performance argument — the one outcome that loses
outright.

Now read the second row. A two-member union is **free**. Zero allocations,
identical speed to the static version.

That is the whole plan in one comparison. `ir::Ty::Union` lowers to an LLVM
`{ i32, i64 }` — a tag and a payload, living in **two registers**.
`ir::Ty::Any` lowers to an `i64` pointing at a heap
`PyrsUnionBox { i32 print_tag; i64 payload }` — *the same two fields*, but
GC-allocated, one allocation per value.

**The fast dynamic representation already exists in this compiler, is
already correct, and `Any` simply does not use it.** Dynamism here is not
primarily a missing-features problem. It is a representation problem, and
the representation is already sitting in the tree.

### Why `Any` is boxed at all

Not an oversight. Two real constraints:

1. **Container slots are 8 bytes.** `PyrsList` is `{ len, cap, data }` with
   no per-element tag — the tag is supplied per operation from the static
   element type (`elem_tag`, `codegen/src/emit.rs:135`). A `list[Any]` has no
   static element type, so each element must carry its own tag, and 16 bytes
   do not fit in 8. Unions in containers are boxed for exactly this reason.
2. **The runtime ABI takes one word.** `pyrs_print_any`, `pyrs_any_list_get`
   and friends take a single `long long` slot.

Both constraints are about **storage and ABI**, not about locals. Unions
already thread the needle: unboxed in SSA, boxed at the container boundary.
`Any` should follow the rule that already exists.

---

## 2. What is actually missing

Eleven probes against the current compiler (0.141.1). These are the real
diagnostics, not a guess at them.

| Probe | Result today |
|---|---|
| `type(3)` | rejected: *classes are not first-class values here* |
| `getattr(c, "x")` | rejected: *instance fields are resolved statically* |
| `c.y = 5` (new attribute) | rejected: *'C' object has no attribute 'y'* |
| `k = C` (class as value) | rejected: *name 'C' is not defined* |
| `C.m = other` (monkey-patch) | rejected (via `self` inference failure) |
| `def f(x): return x.speak()` | rejected: *could not infer a unique type* |
| `x: object` then `x.upper()` | rejected: *'Any' has no method 'upper'* |
| `__getattr__` | rejected: *'C' object has no attribute 'zzz'* |
| `eval("1+1")` | rejected: *closed-world … no interpreter* |
| `[1, "a", 3.5]` heterogeneous literal | **works** (union element type) |
| `isinstance(x, int)` narrowing an `object` | **works** (0.136) |

They fall into four groups, and the groups have very different costs:

- **Types as values** — `type()`, `k = C`, classmethod-on-a-variable.
  Needs a runtime type object. Moderate.
- **Attributes named at run time** — `getattr`/`setattr`, new attributes,
  `__getattr__`. Needs a per-object shape or dict. This is the expensive one.
- **Operations on a value of unknown type** — method calls on `Any`,
  unannotated duck-typed parameters. Needs a generic dispatch kernel.
  Cheap to make *work*, expensive to make *fast*.
- **Code as data** — `eval`/`exec`. Needs an interpreter. Out of scope
  permanently, and the current diagnostic already says so correctly.

---

## 3. The thesis: pay-per-use dynamism

An AOT compiler cannot have unbounded dynamism for free. The design question
is not *whether* dynamism costs something — it is **who pays**.

CPython's answer: everyone pays, always. Every attribute is a dict lookup
whether or not anyone ever patches it.

PyRs's answer today: nobody may use it, so nobody pays.

The position worth taking is the third one:

> **The cost of a dynamic feature is paid by the code that uses it, and by
> nothing else in the program.**

Three consequences that should be treated as invariants, and every milestone
below checked against them:

1. **A program that does not use a feature must not slow down when the
   feature ships.** Adding shapes must not add a word to a class that never
   gains an attribute at run time.
2. **Specialization is an optimization, never a correctness requirement.**
   There must always be a generic path that is *correct*, so the compiler
   never has to reject a program for being too dynamic. Today rejection *is*
   the fallback, which is why the diagnostics above exist.
3. **A guard that never fires costs a predictable branch, not a call.**
   Well-predicted, ~1 cycle, and LLVM can hoist it out of loops.

---

## 4. Milestones

Ordered by *tax removed per unit of work*, not by how Pythonic the feature
sounds. The first two are worth more than the rest combined, because they
are what make everything after them affordable.

### D1 — `Any` gets the representation `Union` already has

**The single highest-value change in this document**, and by far the
smallest. `ir::Ty::Any` lowers to an SSA `{ i32, i64 }` — exactly what
`Ty::Union` lowers to — and boxes only where a union already boxes: a
container slot, or a runtime ABI that takes one word.

`Any` is morally the open union, and the tag spaces already agree
(`elem_tag` maps both `Union` and `Any` to 8; the `print_tag` values are the
same `TAG_*` constants). The boxing machinery at the boundary already exists
and is already exercised by unions in containers.

Expected: the 0.591s row collapses toward the 0.024s row for every `Any`
that does not escape into a container. That is the 23.6× tax, mostly gone,
before a single new feature ships.

Risks to settle in the design, not in review:
- **`lty(Ty::Any)` is `i64` in ~20k lines of lowering.** The change is
  mechanical but wide. Type it as a compile error by construction: change
  the LLVM type and fix what stops compiling.
- **GC.** A `{i32, i64}` in registers is a conservative root like any other
  live value; the payload word is already candidate-checked. Unions prove
  the collector handles this shape — but `PYRS_GC_STRESS=1` on every
  `Any`-heavy test is the gate, not an argument.
- **Float payloads.** A float in an `i64` payload is a bitcast today; that
  stays.

### D2 — A generic kernel, so nothing has to be rejected

`pyrs_dyn_add`, `pyrs_dyn_getattr`, `pyrs_dyn_call`, `pyrs_dyn_getitem`, …:
one runtime function per operation, taking tagged values, doing CPython's
dispatch, raising CPython's errors. Slow, always correct.

This is invariant 2 made real, and it is what converts every "not supported"
diagnostic in §2 into a *performance* question instead of a *possibility*
question. `x.upper()` on an `Any` stops being an error and becomes a call
the optimizer would like to remove.

It also does something subtler and more valuable: it lets every later
milestone be **an optimization with a reference implementation to diff
against**. Specialized paths can be differentially tested against the
generic path, in-process, which is a far tighter loop than diffing against
CPython.

### D3 — Shapes: attributes named at run time

Instances are `{ i64 type_id, field0, … }` with a static layout. To support
`getattr`, `setattr`, new attributes and `__getattr__`, the classic answer
is a hidden class / shape: the object header points at a shape describing
its fields, and adding a field transitions to a new shape.

Pay-per-use applies hardest here. Options, in preference order:

1. **Static layout stays; a lazily-allocated overflow dict is added only to
   classes the analysis cannot prove closed.** A class never touched by
   `setattr` keeps today's layout and today's speed, exactly.
2. Shape pointer on every object. Uniform, simpler, and it taxes every
   class in the program. Rejected under invariant 1 unless 1 proves
   unworkable.

The whole-program property is what makes option 1 sound: `analyze_program`
already lowers every module into one `ir::Module`, so "no `setattr` anywhere
targets this class" is a decidable question, not a guess.

### D4 — Types as values

A runtime type object per `ClassId`, which `type()` returns, `k = C` binds,
`isinstance` consults and `C(…)` calls. `ClassInfo` already holds the name,
parent, fields and method table — this is largely giving that struct a
runtime representation and a `Ty::Type(ClassId)` to name it.

Unlocks `type(x)`, classes in dicts and parameters, factory functions, and
`super()` on a class value.

### D5 — The inference engine (the actual key)

Everything above decides what dynamism *costs when used*. This decides **how
often it is used at all** — and it is the milestone that most directly
serves the goal, because a site the compiler can prove monomorphic never
touches D2's kernel.

Three concrete deficiencies in today's engine, all locatable:

1. **Inference is built to unify, not to enumerate.**
   `try_infer_param_from_body` (`semantic/src/lib.rs:4260`) returns
   `Option<Ty>` and fails on conflict. For a parameter that genuinely takes
   two types, conflict *is the answer* — the site wants "observed: {int,
   str}", which is a specialization plan, not an error.
2. **There is no program-level fixpoint.** `analyze_target`
   (`semantic/src/lib.rs:6259`) is explicit phase sequencing — pass 0 through
   pass 3, with a return-type patch-back loop that works only because methods
   are lowered before free functions. Call-site-driven inference needs a
   worklist over the call graph, iterated to a fixpoint with a named cap
   (the precedent is `resolve_params_with_body_infer`, which already caps at
   8 rounds).
3. **There is no feedback channel.** An AOT compiler has one advantage a JIT
   does not: it can be told what actually happens, offline.

That third point is the strategic answer to "dynamism without sacrificing
performance", and it deserves to be a named feature rather than a footnote:

> **`pyrs profile` — profile-guided type feedback.** A training run records
> observed type tags per polymorphic site into a profile file. The next
> compile specializes each site for its observed types, guards the
> specialization, and falls back to D2's kernel when the guard fails.

This is how a static compiler gets a JIT's information without a JIT. It
fits the existing product shape (an explicit, declared build input, like
`--compat`), it degrades safely (no profile → generic path → correct but
slow), and it is checkable: the profile records what was seen, and the guard
proves it at run time.

### D6 — Guarded specialization and the open world

With D5's information, each polymorphic site becomes:

```
if (tag == expected) { fast, inlined, monomorphic }
else                 { pyrs_dyn_* }
```

For an AOT compiler this is the *right* shape, and the reason is worth
stating: a JIT deoptimizes by discarding compiled code and recompiling. PyRs
cannot — there is no compiler at run time. But it does not need to, because
the generic path was compiled AOT alongside the fast one. **No JIT, no
recompilation, no code cache.** The cost is code size, bounded by only
emitting the pair where the analysis cannot close the world.

Closed-world dispatch stays the fast path where CHA proves the class set
closed — the existing `switch` on `type_id`
(`codegen/src/emit.rs:5520`) is a jump table LLVM optimizes well, and it is
usually *better* than an inline cache. Inline caches are for the sites CHA
cannot close.

### D7 — Monkey-patching and invalidation

`C.m = other` after the fact invalidates every devirtualized call to `C.m`.
Without a JIT the answer is not recompilation but a **version guard**: a
patchable class carries a version word, devirtualized sites that CHA could
not prove closed check it, and a patch bumps it. Classes the analysis proves
never patched carry nothing and check nothing.

This is last because it is the feature with the worst cost-to-use ratio, and
because everything before it must exist for it to be expressible.

---

## 5. What stays out, and why

| Item | Why |
|---|---|
| `eval` / `exec` of arbitrary strings | Needs an interpreter in the binary. The current diagnostic is right and should not soften. |
| `type("C", (), {})` — classes built at run time | Would require a full metaobject protocol and dict-based instances. Possible *later*, at CPython-like cost for those classes only — which is acceptable under pay-per-use, but it is a separate plan. |
| Metaclasses, `__slots__` tricks, descriptors beyond `@property` | No corpus evidence. Revisit only when the frozen workload corpus (workstream A) says otherwise. |
| Multiple inheritance / MRO | Independent of dynamism; already deferred on its own merits. |
| A tracing JIT | Changes the product from an AOT compiler to a runtime. D5+D6 are the AOT-shaped way to get most of the benefit. |

---

## 6. Verification

Per milestone, the convention in [EXTENDING.md](../../EXTENDING.md) §19,
plus two additions specific to this work:

1. **A dynamism tax benchmark, tracked in `README.md` like the others.** The
   three-row table in §1 becomes a permanent benchmark. The target for D1 is
   that the `object` row lands within 2× of the typed row; the target for
   the plan as a whole is that it stays there as features land. A milestone
   that closes a feature and widens that gap has failed, whatever else it
   did.
2. **Differential testing against D2's kernel.** Once the generic path
   exists, every specialized path can be checked against it in-process at
   `-O0/-O2/-O3` and under `PYRS_GC_STRESS=1`. That is a tighter loop than
   the CPython oracle and catches exactly the class of bug specialization
   introduces.

The CPython oracle remains the law for observable behaviour. `make
compatibility` and byte-exact `make examples` do not move.

---

## 7. The risk that could sink this

Not any single milestone — it is **D1's blast radius**. `Ty::Any` lowering to
`i64` is assumed across ~20k lines of lowering and every runtime function
that takes a slot. If that change cannot be made incrementally, the plan
stalls at its most valuable step.

The mitigation is available and should be decided before any code is
written: **unions already do this correctly**. The migration is not "invent
an unboxed representation" but "make `Any` take the path
`Ty::Union(&[…])` already takes", with the box appearing at exactly the
boundaries a union's box already appears at. If a prototype cannot make an
`Any` local behave like a union local in a week, that is the signal to stop
and re-plan rather than push through.

The second risk is quieter and worth naming: **shipping D2 without D1.** A
generic kernel over heap-boxed values would make every rejected program in
§2 compile, and every one of them would run at roughly CPython's speed. The
diagnostics would disappear and the performance argument would go with them.
D1 before D2 is not a preference; it is the order that keeps the product
honest.
