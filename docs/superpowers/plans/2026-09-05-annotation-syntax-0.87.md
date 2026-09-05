# Annotation and subscript syntax compatibility — 0.87.0

## Review and milestone choice

While probing 0.85 against CPython 3.14, five of the intended test programs
failed to compile for a reason unrelated to what they were testing:

```
error[parse]: expected a type (...) in parameter annotation,
              found string literal
 --> p4_ne_virtual.py:4:29
  |
4 |     def __eq__(self, other: "Base") -> bool:
```

PyRs rejected source that CPython accepts. That caps how much real Python
can even be *attempted*, independently of how correct the compiler is on
the code it does accept — so it gates measurement itself. `"Base"` is the
PEP 484 forward-reference form, pervasive in typed Python and unavoidable
before CPython 3.14's PEP 649 whenever an annotation names the class being
defined.

Two further syntaxes appeared in the recorded scientific probes, where the
manifest captured misleading diagnostics: `a[i, j]` reported
`expected ']' to close the subscript, found ','` and `A @ B` reported
`expected ')' after method arguments, found '@'`. Neither says what is
actually unsupported.

## Milestone contract

- Annotations accept string literals in every position that takes a type:
  parameters, returns, and variable annotations. Contents are lexed and
  parsed as a type, so generics (`"list[int]"`), unions (`"int | None"`),
  `"Optional[int]"` and class names all work, including nesting.
  Diagnostics are re-anchored to the literal, because offsets inside the
  string do not correspond to positions in the file.
- `from __future__ import ...` is accepted and does nothing. Every feature
  CPython lists is either mandatory in Python 3 or, for `annotations`,
  already how PyRs behaves. Unknown features are rejected with CPython's
  wording (`future feature X is not defined`), because silently ignoring
  one would compile a program under semantics it did not ask for.
  `from __future__ import *` is rejected. Plain `import __future__` stays
  an ordinary module import and still reports a missing module.
- Tuple subscripts and `@` are rejected with an explicit reason naming the
  missing capability, rather than a parse error about a delimiter.

## Why the last item is a diagnostic rather than support

Both syntaxes are unreachable in today's type system, and it is better to
say so than to imply progress. `a[i, j]` means `a[(i, j)]`, but dict keys
are restricted to int and str (`dict keys/elements of type tuple[int, int]
are not supported yet`), so no type can accept a tuple subscript. `@`
needs an array type that does not exist. Parsing them into IR that every
backend path must then reject would add plumbing for no capability gain.
Real support belongs to the milestones that add hashable tuple keys and a
native array type.

## Implementation plan

- [x] Accept `Token::Strlit` in `parse_type_atom`; lex and parse the
      contents with a sub-parser, requiring full consumption and
      re-anchoring inner diagnostics to the literal's span.
- [x] Reject empty and malformed string annotations with a clear message.
- [x] Treat `from __future__ import ...` as a validated no-op in semantic
      import collection, statement lowering, and the class-import pass;
      keep it out of the module init graph.
- [x] Skip `__future__` only for the `from` form in the module loader's
      dependency collection.
- [x] Specific diagnostics for `@` in operator position (at `parse_term`,
      Python's precedence for it) and for a comma in subscript position.
- [x] 15 differential tests at O0/O2/O3 comparing stdout *and* exit status,
      plus rejection tests asserting nothing executed first.
- [x] Bump the 7 crates, lockfile, README and SPECIFICATIONS to 0.87.0.

## Boundaries

No Unicode, numeric-fidelity or protocol-dispatch work. No new type
support: tuple dict keys, arrays and `@` semantics are out of scope, as is
`typing` module surface (`TypeAlias`, `Annotated`, generics over user
classes). Runtime evaluation of annotation *objects*
(`__annotations__`, `typing.get_type_hints`) remains unsupported; PyRs
consumes annotations at compile time only.

## Acceptance

Programs using string annotations and `__future__` imports compile and
match CPython at O0/O2/O3 in both stdout and exit status. Unknown future
features are rejected with CPython's wording. Tuple subscripts and `@`
name their own limitation. Decorator `@` keeps working.

## Validation (2026-09-05)

Toolchain: Rust 1.96.1, LLVM 22.1.8, CPython 3.14.7, GCC 16.2.1.

| Check | Result |
|-------|--------|
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test -p pyrs --test annotation_syntax` | 15 passed |
| `cargo test --workspace` | 1066 passed, 0 failed, 0 ignored |
| `make examples` | All 13 example entry points matched CPython |
| `make compatibility` | native 12 pass / 6 known_gap; compat 6 pass |
| Release `pyrs --version` | `PyRs 0.87.0` |
