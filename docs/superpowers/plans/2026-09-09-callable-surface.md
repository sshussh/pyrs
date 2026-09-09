# Signatures a library can publish

**Status: implemented in 0.138.** M3 of the library-enablement plan.

## The plan had two items; the one that mattered was a third

The plan listed keyword-only/positional-only parameters and module-level
functions as values. Both were real. But the first probe of a realistic API
found something neither covered:

```python
class Frame:
    def sorted_rows(self, *, ascending: bool = True) -> list[int]: ...
f.sorted_rows(ascending=False)
# error[semantic]: keyword arguments are not supported for this method call
```

**Keyword arguments were refused for *any* instance method** — not just for a
keyword-only parameter — while a free function accepted them. Library APIs are
overwhelmingly methods, so the parameter markers would have been unusable
where they are most wanted. The binding logic already lived in
`lower_call_with_sig`; the method path simply never handed it the keywords and
rejected them instead.

That is the third time this pattern has appeared in this plan: 0.134's
augmented assignment, 0.136's expected-type hint, and now this. In each case
one spelling of a construct worked and another silently did not, because the
second had its own partial implementation of something the first already did.

## The markers

`ast::FuncDef` carries `posonly_end` and `kwonly_start`, and `FuncSig` carries
them to the call site. Two rules follow, both CPython's: a keyword may not name
a positional-only parameter — it lands in `**kwargs` instead when there is one,
which is the entire point of `/` — and a positional argument may not fill a
keyword-only slot.

`method_user_sig` drops `self` for call matching, so both indices shift by one
there. That is the kind of detail a test has to pin rather than a reader
notice, and `markers_survive_methods_and_nesting` does.

**`kwonly_start` is an `Option<usize>`, and that is load-bearing.** The first
version used a plain index, and the natural default for a compiler-synthesized
signature — a lambda, the synthesized `__ne__`, module init — is `0`, which
with a plain index silently means *every* parameter is keyword-only. The
`Option` makes the wrong default unrepresentable. Four of the five synthesized
signatures take `None`; the fifth, `__ne__`, inherits `__eq__`'s shape because
it is generated from it.

The "no non-default after default" rule now applies only within the positional
run. `def f(*, a=1, b)` is legal Python: once a parameter is supplied by name,
ordering carries no information.

## Functions as values

The arm was never written. A module-level function in value position now
lowers to the same `MakeClosure` with an empty environment that the
free-function decorator desugar already built — about fifteen lines, reusing
`ir::closure_of_full`.

Combined with 0.137's union inference this makes a **dispatch table** work,
which was one of the original audit's eight blocked library cores:

```python
TABLE = {"build": cmd_build, "test": cmd_test}
```

That works because `join_elem_types` erases func identity when parameters,
return and capture shape all match, so two functions of the same signature
join to one closure type. Two functions with *different* signatures are still
refused and named — that needs a union of closure types and runtime dispatch
through it.

Two shapes cannot be a closure value at all, and are rejected rather than
losing arguments: a function taking `*args`/`**kwargs`, and one carrying
defaults. `Ty::Closure` has a fixed parameter list and no defaults.

## Two defects found while testing

**A closure held by a comprehension target could not be called.**
`[g(1) for g in fs]` reported `function 'g' is not defined` while the identical
plain `for` loop worked. A comprehension target is stored under a renamed
local, and the call path never consulted `comp_renames` the way the expression
path's `Name` arm does.

**`ClassName.static(1, b=2)` silently dropped its keyword** and used the
default, returning 11 where CPython returns 12. Not a rejection — a wrong
answer with no diagnostic, from a path that ignored keywords rather than
refusing them. It is pinned separately from the instance path for that reason.

Also corrected: the arity message counted keyword-only parameters as
positional, so `f(1, 2)` against `def f(a, *, b)` said "takes 2 argument(s)".

## Where this leaves the library probes

Two of the eight blockers the original audit found are closed. Both CLI cores
now match CPython byte for byte — the argparse-shaped API through method
keywords and `*`, and the subcommand table through functions as values.

Remaining from that list: `@dataclass` (M4), generics (M5), and the recursive
value model a dynamic `json.loads` needs.

## How this is checked

`cli/tests/callable_surface.rs` — 11 differential tests at -O0/-O2/-O3 and
under `PYRS_GC_STRESS=1`.

Both markers alone and together; the boundary surviving a method, an inherited
override and a nested `def`; six misuse diagnostics including the two the
parser must catch (a bare `*` with nothing after it, `/` before anything);
keywords through instance, static, class, statement-position and virtual
paths; builtin methods still refusing them, since their method table has no
keyword surface; functions as values in a dispatch table, as a `key=` and
through `map`; and the three shapes that cannot be a closure value.

Coverage went 151 to 156 of 210 probes.

## Next

M4, class decorators and `@dataclass`. The class-body allow-list has to widen
first — it rejects an annotated attribute without an `__init__` assignment —
and the milestone has to decide whether class decorators are a general
mechanism or a closed set of compiler-recognized names. Given the closed-world
model the closed set is the honest answer, and it is what makes `@dataclass`
expressible at all.
