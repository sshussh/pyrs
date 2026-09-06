# 0.108.0 — rendering containers as text

## The gap

`print([1, 2])` wrote `[1, 2]`, but `str([1, 2])`, `f"{xs}"` and `"%s" % xs`
were all rejected:

```
str() cannot convert list[int] yet
```

So the most ordinary line in a Python program — `print(f"result: {xs}")` —
could not be written, and neither could any function that *returns* a rendered
value. This was a rejection, not a wrong answer, and the formatting logic
already existed and was already correct; it simply could not be reached from
anything but `print`. Found while testing 0.107 and recorded then as a gap.

## Why the print routines could not be reused

`pyrs_print_list`, `pyrs_print_dict`, `print_slot` and the rest wrote straight
to `stdout` with `fputs` / `fputc` / `fwrite` / `printf`. There was no way to
ask them for a string, and reimplementing them for `str()` would have created
two renderers to keep in step — the failure mode being a `print` and an
`f"{...}"` of the same value disagreeing.

## The sink

Every print routine now writes through `out_puts` / `out_putc` / `out_write` /
`out_printf`, which append to a thread-local capture buffer when one is
installed and go to `stdout` otherwise. `pyrs_repr_list` / `_tuple` / `_dict` /
`_set` install a buffer, call the matching `pyrs_print_*`, and copy the result
out as a `PyrsStr`.

So `str(xs)` is *defined* as the text `print(xs)` writes. The two cannot
diverge, elements, quoting and nesting included, and CPython agrees: `str` and
`repr` of a container are the same string, and both use `repr` for elements.

The buffer saves and restores the enclosing one, so nesting is harmless; it is
plain `malloc` rather than GC memory, since it holds no object references.
`pyrs_print_sep` / `pyrs_print_end` and the `fflush` calls stay on `stdout` —
they are never captured.

## Surface

- `str(x)` and `repr(x)` for `list`, `tuple`, `dict`, `set`, nested
  arbitrarily, and every element type `print` already handled.
- f-strings (`f"{xs}"`, `f"{xs!r}"`), `%` formatting and `str.format()` — all
  three already routed through the `str()` lowering, so they came with it.
- `repr()` and `ascii()` are now builtins. They existed only as the f-string
  `!r` / `!a` conversions; the lowering is shared, so this is a name binding.
- A format *spec* on a container (`f"{xs:>10}"`) stays rejected. CPython
  raises `TypeError: unsupported format string passed to list.__format__`, so
  this is the same rejection moved to compile time.
- `ascii()` of a container stays rejected. Unlike `repr`, it would have to
  escape non-ASCII *inside* the elements, and the shared rendering does not do
  that. Rejecting is honest; rendering non-ASCII unescaped would not be.

One divergence is inherited rather than introduced: sets iterate in insertion
order here and in hash order in CPython, so `str({3, 1, 2})` differs exactly as
`print({3, 1, 2})` already did. That is a pre-existing documented property of
the set type, and the tests avoid depending on it.

## Implementation

| Site | Change |
|---|---|
| `codegen/runtime/runtime.c` | `OutBuf` + `out_*` helpers; the 13 print routines rewritten to use them; `pyrs_repr_list` / `_tuple` / `_dict` / `_set` capture. |
| `ir/src/lib.rs` | new `ExprKind::ContainerRepr(Box<Expr>)` — one node, since codegen can dispatch on the operand's own type. |
| `codegen/src/emit.rs` | `ContainerRepr` emits the matching `pyrs_repr_*` call, reusing `elem_tag` exactly as `emit_print_value` does. |
| `semantic/src/lib.rs` | container arms in `lower_cast(Str, …)` and in `lower_repr_like` (non-`ascii` only); `repr` / `ascii` added to `BUILTINS` with a dispatch arm. |

## Validation

| Check | Result |
|-------|--------|
| `cargo test --workspace` | 1348 passed; none failed or ignored (21 new) |
| `make ci` | Passed (fmt, clippy, test, hygiene, examples, compatibility) |
| `make compatibility` | native 66 pass / 0 known_gap; compat 22 pass |
| `pyrs --version` | `PyRs 0.108.0` |

A print-saturated workload (200k `print` calls at `-O2`) was measured against
a 0.107 build: 53.8 ms vs 54.6 ms best-of-5, with the 0.107 build itself
varying 50.5-54.1 ms across repeats. The sink's per-write branch is below the
noise even on the workload built to expose it.

`cli/tests/container_str.rs` covers each container, empty ones, string quoting
and escaping, nesting, non-ASCII elements, `None` and floats, all three
interpolation forms, `!r`, using the result as an ordinary string, the
`repr`/`ascii` builtins on scalars, a report-building program, a
collection-pressure run, and the three rejections — each against CPython at
`-O0/-O2/-O3`. Several cases print the value *and* `str()` of it in the same
program, which is the property the sink is there to guarantee.

## Out of scope

`ascii()` of containers; format specs on containers; `str()` of a generator or
`Any`; CPython's `<module.Class object at 0x…>` instance repr, which stays
`<Class object>` in both `print` and `str`, as before.
