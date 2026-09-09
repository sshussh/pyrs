# Numeric types, and why the array type is a library

**Status: implemented in 0.135.** The first milestone of the
library-enablement plan, and the one that decides the shape of the rest.

## Why this first

The 0.134 audit measured the language surface and found syntax was not the
gap. The follow-up question — *can we start building libraries?* — was answered
by writing the core of each one and running it. Every candidate was blocked,
each on a different feature, but they were not equally urgent:

| Probe | Blocker |
|---|---|
| `Series.__add__` | **no arithmetic dunders** |
| `a @ b` | `@` rejected in the parser |
| `class Stack(Generic[T])` | no generics |
| `@dataclass` | no class decorators |
| `def add(self, *, help="")` | no keyword-only parameters |

Arithmetic dunders come first because nothing numeric can be written *at all*
without them. There was no `__add__` string anywhere in the compiler — this was
unimplemented, not half-wired.

## Extending the comparison path, not adding a second one

The rich-comparison dunders already dispatched correctly through five hops:
`lower_binary` → a class guard → `lower_class_compare` with its op→dunder table
and CPython-ordered resolution → `lower_class_cmp_call`, which spills both
operands to temps before dispatching → `lower_instance_method_call`, which
decides virtual dispatch (a closed-world `switch` on the object header's
`type_id`, not a vtable).

Two things in it were comparison-shaped and had to change:

**The result type.** `lower_class_cmp_call` hard-coded `ir::Ty::Bool` and
pushed any other return through `to_bool`. That is right for a comparison and
wrong for arithmetic: `V.__add__ -> V` has to yield a `V`. The operand spilling
was factored into `lower_class_binop_call`, returning the call with its own
type; the comparison wrapper adds the `to_bool` and the `__ne__`-from-`__eq__`
inversion on top.

**How the families reflect.** A comparison swaps the *operator* — `a < b`
becomes `b.__gt__(a)`. Arithmetic swaps the *name* — `a + b` becomes
`b.__radd__(a)`. That is a sibling table, not a reuse, and conflating them
would have produced silently wrong slot choices.

What *is* shared is the part that matters. Both operands are evaluated into
temps **before** dispatch, so source order holds even when a reflected call
makes the right operand the receiver. `cli/tests/protocol_order.rs` pins that
for comparisons; `arithmetic_protocols.rs` now pins it here.

### Slot resolution

CPython's order, and the reason each step exists:

1. **A proper subclass on the right, with the reflected slot.** Lets a subclass
   override its base's arithmetic from either side.
2. **The left operand's slot.**
3. **The right operand's reflected slot.** This is what makes `2.0 * vec` work:
   `float` has no `__mul__` for a `Vec`, so there is nothing to find at step 2.

Each step is gated on the slot's parameter type actually accepting the other
operand, so a slot that cannot handle its argument declines rather than
failing inside the call. There is no `NotImplemented` value here, so when every
step declines the result is a compile-time error naming both types.

### In-place

`+=` prefers `__iadd__` and falls back to `__add__`, as CPython does, and the
difference is observable: `__iadd__` mutates and returns self, so an alias sees
the change and `is` stays true; the fallback builds a new object and the alias
does not move. Both halves are tested. The aug-assign path already routed
through `lower_binary` after 0.134 made that arm reuse the standard load and
store, so this was one wrapper rather than four.

## `@` is an operator, and that reframes the roadmap

`@` was rejected in the parser: *"PyRs has no array type for it to operate
on."* That reason stopped being true the moment `__matmul__` existed. `a @ b`
is a method call like any other operator.

`docs/ROADMAP.md` carried "a native array type, which `@` needs" as an open
item. **That was the wrong framing**, and the correction is the most consequential
line in this milestone: `@` needs `__matmul__`, and an array type is a
*library*. Shapes, strides, zero-copy views and NumPy buffer interop are still
real work — they are interop gate 5 — but they are a performance and interop
concern once the library exists, not a prerequisite for writing it.

That follows from the parity decision. CPython has no array type, so the only
way `Array[T]` can be byte-parity tested is if the same source file runs under
both engines — which means it must be written in PyRs.

## Unary `+` was silently discarded

The parser dropped it — `Token::Plus => { self.advance(); self.parse_unary() }`
— making `+x` a literal no-op. That was already wrong for `+"a"`, which CPython
rejects with `bad operand type for unary +: 'str'`, and it would have made
`__pos__` a silent no-op the moment unary dunders landed. `UnaryOp::Pos` is now
recorded and semantic decides.

This is the same shape as the three defects 0.134 fixed: a construct that
worked in one spelling and silently did nothing in another.

## What operator overloading costs

The open question this milestone existed to answer. A 120×120 matmul, the same
algorithm written twice:

```
class-based (a @ b)          6.5 ms   vs CPython 90.2 ms    13.9x
free function matmul(a, b)   6.1 ms   vs CPython 84.2 ms    13.9x
```

**Identical speedup.** Dispatch is resolved statically — the guard checks the
operand's static type and emits a direct or switch call, and the loop body is
the same instructions either way. The 6% absolute gap is the wrapper object's
allocation and field loads, not the operator.

This is the evidence that the rest of the plan is sound. If a class-based
matmul had cost 3×, an array type written in PyRs would have been a bad idea
and the native-primitive framing would have been right after all.

## Two defects found on the way

**`f.write()` returned the wrong number.** The UTF-8 byte count where CPython
returns the character count, so `f.write("héllo")` gave 6 instead of 5. A
silent parity break in already-supported surface, fixed to `cplen`.

**A container of instances ignores `__repr__`.** `print(obj)` dispatches
correctly; `print([obj])` renders `<Name object>` per element. The runtime
formats container elements from a numeric type tag with no hook back into user
code. Pre-existing — confirmed by running the case against 0.134 rather than
assumed — and now recorded in the README divergence list, GUIDE §9b and
`scripts/coverage_probe.py`, where a fix will fail the run as a stale record.
It matters for the data path: printing a frame of rows hits it every time.

## How this is checked

`examples/vectors.py` is the acceptance test — `Vec3`, a dense `Matrix` and an
`Accumulator` in ordinary Python, byte-identical under both engines through
`make examples`. It exists five milestones before the plan's own proof point so
that a wrong dispatch or a catastrophic slowdown would have shown here.

`cli/tests/arithmetic_protocols.rs` — 17 differential tests at -O0/-O2/-O3 and
under `PYRS_GC_STRESS=1`, since every one of these operators allocates its
result and a mis-rooted temporary is a use-after-free rather than a wrong
number. The full binary and bitwise families; unary dispatch; `@` beside a
decorator, so the two uses of the token stay distinguishable; reflected
dispatch from a scalar; a subclass winning the first attempt; virtual dispatch
through a base-typed binding; the result keeping the dunder's return type;
left-to-right evaluation under reflection; an operand that raises before any
dispatch happens; in-place versus fallback identity; and the comparison path
proving it did not move.

Coverage went 141 to 145 of 204 probes.

## Next

M2 is `Any` ergonomics — the expected-type hint reaching non-`Name` assignment
targets, which is the actual dataframe blocker, and `isinstance` narrowing on
`Any`. Then the callable surface, then generics, where `Array[T]` lives.
