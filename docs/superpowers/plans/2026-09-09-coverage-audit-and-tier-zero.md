# What the compiler actually supports, and the three places it lied

**Status: audit complete; Tier 0 implemented in 0.134.** The audit is the
first half of this document and the reason for the second.

## The audit

Performance work through 0.126–0.133 took the corpus from 3.2× to 7.1×. Before
adding features, the question was where the language surface actually stands
against CPython — measured, not read off the source.

280 probes, each a self-contained program, compiled with `pyrs run -i` and
diffed against CPython 3.14.7 on stdout, stderr and exit status. Probes whose
first failure came from an unrelated missing feature were re-run in isolation
before being counted, which mattered: nine first-pass failures were the probe's
fault, not the compiler's.

| Surface | PyRs | CPython |
|---|---:|---:|
| Keywords, full support | 30 | 35 |
| `list` / `dict` / `set` / `tuple` methods | 41 | 41 |
| `str` methods | 46 | 47 |
| Builtin functions | 41 | 69 |
| Exception types | 24 | 71 |
| Stdlib modules | 4 | 297 |

**Syntax is not the gap, and neither is the container library.** Every public
method of `list`, `dict`, `set` and `tuple` is implemented, and `str` is
missing only `encode`. Pattern matching is complete enough to surprise:
sequence patterns with a starred rest, class patterns with positional and
keyword fields, mapping patterns, or-patterns, as-patterns and guards all
compile and match.

What fails, fails on a **type rule or a missing literal form**. `async`/`await`
and PEP 695 `type` aliases are the only keywords rejected outright; `del`,
`with`, `raise`, `except`, `is` and `nonlocal` are partial. The literal gaps —
no `r'...'`, no `b'...'`, no adjacent string concatenation, no `...` — are
cheap to close and appear constantly. The class gaps are arithmetic dunders,
class decorators and `__hash__`/`__call__`.

The honest summary: a well-typed single-file program needing only `math`,
`json` and `os.path` will very likely compile and match CPython byte for byte.
Anything reaching for `re`, `bytes`, `dataclasses` or duck typing will not.

The full ranking, and the plan for closing it breadth-first, is a separate
document. This one covers what the audit found that was not a missing feature.

## Tier 0 — the three the audit found that were defects

Each had the same shape: **a construct that already worked in one spelling
silently failed, or silently lied, in another**, because the compiler had grown
a second implementation of something it already knew how to do. The fix in
every case was to delete the second one.

### `bool(x)` ignored `__bool__`

`if x:` and `not x` were correct — both route through `to_bool`, which consults
`__bool__`, then `__len__ != 0`, then falls back to true. The cast folded every
class instance to a literal `true` in `emit_truthiness`, with no diagnostic.

The cause is structural rather than an oversight: **`lower_cast` takes no
`FnCtx`.** It lowers representations, and a dunder call needs a context to
lower into. `str()` has a class arm only because `lower_class_to_str` happens
to need none; that trick does not generalize to a method call. So the `Bool`
arm had no way to dispatch even in principle, and grouped `Ty::Class(_)` with
the scalars.

`lower_cast_ctx` now intercepts the one conversion that needs a method call and
hands it to `to_bool`. That also restored `__len__` truthiness, which was wrong
for the same reason and had not been noticed.

### `nonlocal n; n += 1` reported `name 'n' is not defined`

`n = n + 1`, in the same position, compiled and ran. The canonical closure
counter did not work, and the message pointed at nothing real.

`x op= v` is `x = x op v`, but the augmented-assignment arm hand-rolled its own
lookup, load and store. Three defects fell out of that one duplication:

| Piece | What it did | What it missed |
|---|---|---|
| Lookup | probed `locals`, `binds_global`, `globals` | `cell_locals`, where a `nonlocal` name's type lives — `locals` holds only `.cell.<name>` |
| Load | built `Local` or `GlobalLoad` | `CellLoad` |
| Store | built `Assign` or `GlobalAssign` | `CellStore` |

It now performs the same load `lower_expr` performs and the same store
`bind_name` performs. Two more cases were fixed by that alone, both invisible
until the duplication went:

- **A narrowed local.** `x += 1` where `x` is `int | None` peeled to `int`
  reported `operator '+' is not supported for values of type None | int`,
  because the hand-rolled load used the storage type and never consulted
  `type_refinements`.
- **A comprehension rename**, which `lower_expr` resolves through
  `comp_renames` and the hand-rolled load did not.

This is the argument for reuse stated concretely: the duplicated read was wrong
in three independent ways, and each was invisible from the others.

### A conditional expression did not narrow its arms

```python
def f(x: int | None) -> int:
    return 0 if x is None else x
# error: cannot use None | int as int in return value;
#        use 'is None' check or provide a default with 'or'
```

The advice was to do what the author had already done — the worst kind of
diagnostic, because it reads as a compiler that disagrees with itself.

`lower_if_exp` lowered `test` for its value and discarded its narrowing
content, so both arms lowered under ambient refinements and joined back to the
union. The `and` / `or` arm of `lower_expr` already had exactly the
save / splice / restore this needed, in the same function, against the same
`FnCtx`. Ten lines.

Worth noting what this is *not*: narrowing in general expression position is
still limited, and `docs/GUIDE.md` §9.11 continues to bound it. This closed the
one position that was inconsistent with a statement spelling that worked.

## The fourth: mutable defaults, which the documentation got wrong

`docs/GUIDE.md` recorded non-shared mutable defaults as a deliberate deviation
from CPython. The compiler does not implement that deviation consistently.

`freeze_nested_defaults` evaluates each non-literal default **once at
definition time**, into a `.dflt.*` temp — which is what CPython does. Its
three callers are all lambda or nested-`def` paths. A module-level `def` is
lowered elsewhere and re-lowers `p.default` from the AST at every call site.

So this program disagrees with itself:

```
module-level: 1 1 1     # CPython: 1 2 3
nested:       1 2 3     # CPython: 1 2 3
```

**Not fixed here, and the reason is recorded rather than hidden.** A
module-level freeze needs the default stored as a module global evaluated in
`def` source order, not as a frame temp — the existing freeze is frame-scoped,
which is why `lower_closure_default` already rejects frozen temps for escaped
closures. The signature rewrite has to happen before any body is lowered, while
the store lands in module init, which is a later pass. That is a milestone, not
a fourth commit on this one.

What landed instead: the guide now says what actually happens and names the
scheduled fix, the README divergence list gained the entry it was missing, and
`compatibility/cases/mutable_defaults.py` pins both halves as a recorded
`mismatch`. The harness fails a `mismatch` case that starts passing as
`unexpected_pass`, so the fix cannot land without updating the record.

The README also gained the uncaught-exception entry it was missing. The type
and message match CPython exactly and the exit status is 1, but there is no
`Traceback (most recent call last):` block or frame list, so **stderr never
matches byte for byte on a crash**. Frame fidelity needs call-site line
tracking; recording the contract was the part that belonged here.

## How these are checked

`cli/tests/read_write_parity.rs` — 15 differential tests at -O0/-O2/-O3 and
under `PYRS_GC_STRESS=1`, since a cell is a heap allocation and a store through
the wrong slot is a use-after-free rather than a wrong number.

Every test pairs the previously-broken spelling with the one that already
worked, so a fix that regresses the working half fails here too:

- every augmented operator through a cell, and the longhand beside it
- `global` and module-level augmented assignment, and set in-place updates,
  which take the branches the fix replaced
- `bool()` against `if`/`not`, with `__bool__`, with `__len__`, with neither,
  and through virtual dispatch from a base-typed binding
- conditional-expression narrowing, including `and`-chain composition,
  `isinstance` peels, and that the refinement does not leak past the expression

**None of these four behaviours had a test anywhere in the repository**, which
is why three of them were wrong and the fourth was documented incorrectly. The
audit harness that found them belongs in `scripts/` so the numbers above stay
reproducible rather than a one-off measurement.
