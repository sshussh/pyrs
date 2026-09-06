# Changelog

## Unreleased

Review fixes on the 0.90–0.108 series.

- **A missing tuple key crashed instead of raising `KeyError`.** All four miss
  paths (`d[k]`, `del d[k]`, `d.pop(k)`, `set.remove(k)`) formatted any
  non-string key with an integer-only routine, so a tuple key was read as a
  tagged bigint and died with `MemoryError`. They now share one renderer built
  on the 0.108 output sink, which produces CPython's `repr(key)` for every key
  type: a str quoted, an int bare, a tuple parenthesised.
- **f-string replacement fields were unescaped twice.** The lexer decoded the
  whole payload, including field source that the parser then re-lexes, so
  `f"{'\\n'}"` collapsed to a newline instead of the two characters `\` and
  `n`. Escapes are now decoded in the literal chunks only, for both the
  single- and triple-quoted forms.
- **`zip()` with no arguments** returned an error; CPython gives an empty
  iterator, so `list(zip())` is now `[]`.
- **Two stale compatibility expectations.** `pandas-group-join` still named
  the tuple-subscript parser diagnostic that 0.107 retired. Sweeping every
  `unsupported` case against the current compiler found a second one:
  `numpy-linalg` expected a generic parse error where the dedicated
  matrix-multiply diagnostic is now reported. Both now match reality, and the
  sweep is clean.
- **The hygiene gate** claimed to pin the Unicode tables to the interpreter
  that generated them but compared only the UCD version, which two CPython
  releases can share. It now also compares the `PYRS_UNIDATA_CPYTHON` stamp,
  to the minor version — a patch bump does not change casing, and pinning it
  would fail the gate on any other 3.14.x.

## 0.108.0 — `str()` and `repr()` of containers

`print([1, 2])` wrote `[1, 2]`, but `str([1, 2])`, `f"{xs}"` and `"%s" % xs`
were rejected with `str() cannot convert list[int] yet` — so the most ordinary
line in a Python program, `print(f"result: {xs}")`, could not be written, and
neither could a function that *returns* a rendered value.

- `str(x)` and `repr(x)` render `list`, `tuple`, `dict` and `set`, nested
  arbitrarily, with every element type `print` already handled.
- f-strings, `%` formatting and `str.format()` all route through the same
  `str()` lowering, so `f"{xs}"`, `f"{xs!r}"`, `"%s" % xs` and
  `"{}".format(xs)` work.
- `repr()` and `ascii()` are builtins now. They existed only as the f-string
  `!r` / `!a` conversions; the lowering is shared, so this binds a name to it.
- A format *spec* on a container (`f"{xs:>10}"`) stays rejected — CPython
  raises `TypeError: unsupported format string passed to list.__format__`, so
  this is the same rejection at compile time. `ascii()` of a container also
  stays rejected: unlike `repr` it would have to escape non-ASCII *inside* the
  elements, which the shared rendering does not do.

The formatting logic already existed and was already right; it just could not
be reached from anything but `print`, because the print routines wrote straight
to `stdout`. They now write through an output sink, and `str()` captures what
`print` would have emitted — so the two agree by construction rather than by
two implementations kept in step.

Inherited, not introduced: sets iterate in insertion order here and in hash
order in CPython, so `str({3, 1, 2})` differs exactly as `print({3, 1, 2})`
already did.

## 0.107.0 — Tuple dict and set keys

Dict keys and set elements were restricted to `int` and `str`, so the composite
key a transition table, a sparse grid or a two-argument memo wants had to be
flattened into a string by hand.

- `int`, `str`, and tuples whose elements are themselves hashable — nested
  arbitrarily — now work as dict keys and set elements everywhere: literals,
  subscripts, `in`, `get` / `pop` / `setdefault` / `del`, iteration, `dict()`,
  and both dict and set comprehensions.
- `d[i, j]` is `d[(i, j)]`, trailing comma included (`d[3,]` is a 1-tuple).
  This parse-level rejection had been carried since 0.87 and named the key
  restriction as its reason. A tuple subscript of a *list* is now the type
  error CPython also raises, rather than a parse error.
- `bool` keys stay rejected on purpose: CPython's `True == 1` would require a
  bool key to collide with an int one, which this subset does not model.
  Unhashable types are still rejected, now naming what is allowed —
  `int, str, or a tuple of those`.

Only hashing was ever missing. `slot_eq` already compared tuple slots
structurally, so equal tuples already compared equal; there was no way to reach
the right bucket. `hash_key` gained a `TAG_TUPLE` arm that folds the element
hashes and recurses. The hash is internal and never observed, so it needs to
agree with that existing equality and nothing else.

Found while testing: `str()` and f-strings cannot stringify any container
(`str((1, 2))`, `f"{xs}"`) even though `print` formats them. Recorded as a gap
in the roadmap.

## 0.106.0 — Class-body constants

Any assignment in a class body was rejected, so a class could not carry a
constant at all: enum-like values, limits, `PI`. The stated reason was that a
class attribute with a default would leave zeroed instance storage — true of an
instance *field* default, but not of a class constant, which is not an
instance field.

- `class C: LIMIT = 10` declares a constant, read as `C.LIMIT` and `self.LIMIT`
  (and inherited by subclasses, which may override it). Int, float, str, bool
  and negated numbers, with or without an annotation.
- An instance field of the same name shadows the constant, as in CPython.
- The value must be a **literal**: constants are substituted where they are
  read rather than stored, which is exact for something immutable and needs no
  storage or initialisation ordering. A computed value is rejected with that
  reason and a pointer to `__init__`.
- Assigning to a constant is rejected for the same reason — there is nothing to
  assign to. Previously `C.N = 2` reported `name 'C' is not defined`, which
  sent the reader after a missing binding.
- An unknown attribute on a class name now names the class, instead of
  reporting the class itself as undefined.

## 0.105.0 — n-ary `zip` and `enumerate(start)`

`zip` accepted exactly two arguments and `enumerate` only a keyword `start=`,
so `zip(a, b, c)` and `enumerate(xs, 1)` — both ordinary Python — were compile
errors.

- `zip` takes any number of iterables (one or more) and truncates to the
  shortest, producing a tuple of that arity.
- `enumerate` takes `start` positionally as well as by keyword, and rejects
  being given both.
- Both materialize their arguments the way the other eager builtins have since
  0.98, so `zip(range(3), "ab")`, `zip(gen(), xs)` and
  `enumerate(range(3), 10)` work too — previously all three took only lists,
  strs and homogeneous tuples.

Found by running realistic programs against CPython.

## 0.104.0 — `typing` imports and `Iterator[T]` annotations

Two gaps, one blocking the other.

- `from typing import ...` and `import typing` (and `collections.abc`) failed
  to **load**: `No module named 'typing'`. An ordinary typed Python file could
  not be compiled at all, however simple its contents. These are now
  annotation-only imports — they bind nothing at run time and run no module
  body.
- A generator could be created, iterated and passed to a builtin, but not to a
  user function: there was no way to annotate a generator parameter, and a
  `for` loop body says nothing about whether its subject is a list, a str or a
  generator. `Iterator[T]` and `Generator[T, None, None]` now annotate one, in
  parameters and return types, so a generator pipeline (`squares(evens(nums))`)
  compiles — and the same source still runs under CPython.
- A `-> Iterator[T]` return annotation names the generator type directly and is
  no longer wrapped a second time; the older `-> T` spelling still names the
  yield type.
- `Iterable[T]` and `Sequence[T]` are rejected with the reason: they cover a
  list as well as a generator, which are distinct types here, so there is
  nothing to resolve them to. The message names `list[T]` and `Iterator[T]`.
  `Generator[...]` with non-None send or return types, and
  `from typing import *`, are rejected too.

Found by running realistic programs against CPython: a generator-pipeline
script was the only one of five that did not compile.

## 0.103.0 — Annotated attribute assignment

`self.x: T = value` was rejected: the parser allowed an annotation only on a
bare name. That made an attribute whose initial value has no inferable type
unwritable — `self.xs = []` reported `'C' object has no attribute 'xs'` and
`self.d = {}` could not infer a dict type, so an empty list or dict attribute
could not be created at all.

- `self.x: T = value` in `__init__` declares the field's type, for scalars,
  containers, nested containers and unions (`self.opt: int | None = None`).
- The annotation is the field's declared type; an unannotated attribute still
  infers from its value exactly as before.
- An annotation that disagrees with its value is a type error. CPython does
  not check annotations at run time, but a typed compiler does, consistently
  with every other annotation here.
- An annotation on a subscript (`xs[0]: int = 5`) stays rejected: it is legal
  Python but has no effect there.

Found by running small realistic programs against CPython; a state-machine
script kept a `self.log: list[str] = []`. With this, all five programs in that
batch compile and match.

## 0.102.0 — Module-level containers are visible to functions

A module-level scalar could already be read from a function; a list, dict, set
or tuple could not, and reported `name 'X' is not defined`. A lookup table or
config dict at module scope is ordinary Python.

- Global storage types are seeded from container literals as well as scalars,
  including nested ones, so `edges = {"a": ["b"], ...}` is readable from a
  function. An element the seeder cannot type leaves that global unseeded,
  which is the safe direction: the name is simply not in scope, exactly as
  before.
- An empty `[]` nested inside a container now takes the surrounding element
  type instead of being rejected. `[["a"], []]` and `{"a": ["b"], "d": []}`
  are ordinary Python; the empty literal has no element type of its own and is
  provisionally `list[Any]`, and the runtime value — a length-zero list — is
  the same either way. `xs: list[str] = []` and `f([])` already worked, so
  this closes the nested case.
- Global containers stay shared state: mutating one from a function is visible
  outside, as in CPython.

Found by running small realistic programs against CPython. A graph-traversal
script needed the first item, and the second surfaced while fixing it.

## 0.101.0 — Tuple sort keys

`sorted(items, key=lambda p: (-p[1], p[0]))` is *the* way to sort by more than
one criterion in Python, and it was rejected: a `key=` function had to return
a bare scalar.

- `key=` may return a tuple or a list of orderable values, compared
  lexicographically. Works for `sorted`, `list.sort`, `min` and `max`, in both
  their iterable and multi-argument forms, with `reverse=`, and for a named
  function as well as a lambda.
- Tuples were already orderable everywhere else — `(1, 2) < (1, 3)`,
  `sorted(list_of_tuples)`, `min`/`max` of tuples — so the restriction sat
  only on the key path; it now uses the same `is_orderable_ty` rule as the
  rest of the compiler, and the same lexicographic lowering.
- A key type with no ordering at all is still rejected, and the message names
  what is accepted.

Found by running small realistic programs against CPython rather than by
probing constructs: three of four matched, and the fourth was a word-frequency
script that needed exactly this.

## 0.100.0 — `str.format()` and `%` formatting

Neither existed: `.format` was not in the str method table and `%` was
rejected as an operator on str. A large amount of ordinary Python could not be
compiled at all — f-strings covered new code, but rewriting an existing
codebase by hand is not a workaround.

- `"{} {}".format(a, b)`, `"{0} {1}"`, `"{name}"` and mixed positional and
  keyword forms, with the full spec mini-language (`{:.2f}`, `{:>8}`,
  `{:05d}`, fill and alignment) and the `!r` / `!s` / `!a` conversions.
- `"%d-%s" % (a, b)` and the bare-value form `"%s" % x`, with `%d %i %s %r %a
  %f %e %g %x %o %b`, the `-`, `+`, `0` and space flags, width, precision and
  `%%`.
- Both desugar into the `JoinedStr` parts f-strings already produce, so the
  mini-language comes from the code that already implements it and nothing new
  reaches the runtime.
- Argument-count and field-name mistakes are compile errors, where CPython
  raises `IndexError` / `KeyError` at run time: too few arguments, an
  out-of-range index, an unknown keyword, and `%` with too few or too many.
- The format string must be a literal, which is what makes the compile-time
  desugaring possible. A runtime one is rejected with that reason and a
  pointer to f-strings, rather than the previous generic "method not
  supported" / "operator not supported".
- A nested `{}` inside a format spec is rejected in `.format()`: it names an
  argument there and an expression in an f-string, and quietly picking one
  would be wrong.

## 0.99.0 — Bare `raise` (re-raise)

`except E: log(); raise` is the standard way to observe an error without
swallowing it, and it had no workaround: raising a *new* exception loses the
original type and message, which is the entire point of the idiom.

- A bare `raise` inside an `except` handler re-raises what that handler
  caught, preserving type and message — for builtin and user exception
  classes, out of functions and generators, and through `finally`.
- It picks the innermost enclosing handler, and survives other work in the
  handler body, including a nested `try` that raises and handles its own
  exception.
- A bare `raise` with no active handler is a compile error rather than
  CPython's runtime `RuntimeError: No active exception to re-raise` — the
  compiler can see there is nothing to re-raise. This covers a bare `raise` in
  a `try` body or a `finally` as well.

The handler prologue calls `pyrs_exc_clear()` before running its body, so the
pending exception is gone by the time the body executes. The exception object
is now captured immediately before that clear, and a bare `raise` re-raises
it through the existing `pyrs_raise_exc`.

Found and left out of scope: `str(KeyError("k"))` is `k` here and `'k'` in
CPython, whose `KeyError.__str__` is the repr of its argument. That is
unrelated to re-raising and affects `raise KeyError` generally.

## 0.98.0 — The eager builtins accept any iterable

`sorted`, `sum`, `max`, `min`, `set`, `list` and `str.join` took a list (and,
since 0.95, a generator) and rejected everything else — so `sorted(some_set)`,
`sorted(some_dict)`, `sum(range(n))` and `list(range(n))` were all compile
errors, even though `for x in` accepts every one of those.

- Those builtins now accept list, tuple, set, dict (its keys, as in CPython),
  str, range and generator. `key=` and `reverse=` work over all of them.
- `range` is materialized through the same comprehension path
  `[x for x in range(n)]` already used, because it is not a first-class value
  here and so cannot be lowered and then converted. `list(range(n))` and
  `sum(range(n))` build the list, which CPython does not; that is a memory
  cost on a very large range, not a wrong answer.
- `any` and `all` gained dict, and deliberately did *not* gain range: they
  short-circuit, and materializing would answer `all(range(10**9))` by
  building a billion elements where CPython returns False on the first one.
- `tuple()` is unchanged and still needs a fixed-arity tuple — materializing
  would hand it a list, which is exactly what it cannot accept.
- The `range` diagnostic no longer says it only works in a `for` loop, which
  had become false; it now names the places that do accept it.

## 0.97.0 — Lambda parameter inference

A lambda cannot carry annotations — the first `:` starts the body — so
requiring them made lambdas unusable, and `sorted(xs, key=lambda v: -v)`, the
idiom they exist for, was a compile error. Named functions already worked as
`key=`, so the whole gap was parameter typing.

- A `key=` lambda takes its parameter type from the element type of the
  iterable being sorted or scanned. That covers the case body inference cannot
  reach: `lambda s: len(s)` says nothing about `s`, but the consumer knows.
  Works for `sorted`, `list.sort`, `min` and `max`, with `reverse=`, and for
  any sortable return type.
- Other lambdas now get the same body-usage inference nested `def`s already
  had, so `f = lambda a: a + 1` works. Previously `lower_lambda` required an
  annotation up front and never reached that inference.
- Defaults, captures, multiple parameters and returning a lambda from a
  function all work.
- Still rejected: a lambda whose body constrains nothing about its parameter
  and that has no consumer to ask (`f = lambda x: len(x)`). Use a `def`, which
  can be annotated.
- The type hint is scoped to the lambda being lowered. It is keyed by the
  parameter's own name, so without scoping a `key=lambda s: ...` would leave
  `s` typed for any later parameter that happened to share the name.

## 0.96.0 — Generator expressions

`(elem for target in iter if cond)` was a parse error, which made the four
most common consuming idioms unavailable at once: `sum(x for x in xs)`,
`any(... for ...)`, `max(... for ...)` and `",".join(str(x) for x in xs)`.

- Generator expressions work as a parenthesized value and, per CPython, may
  drop their own parentheses when they are a call's sole argument. Multiple
  `for` clauses and multiple `if` filters are supported.
- They are genuinely lazy. The element expression runs on demand, `any` / `all`
  short-circuit through them, and the outermost iterable is evaluated once when
  the generator is created — the tests print from inside the producing code, so
  a comprehension-shaped desugaring would show a different trace even where the
  final answer agreed.
- The loop variable does not leak into the enclosing scope, and enclosing
  locals, parameters and module-level functions are all visible inside.
- Element types other than `int` are inferred, so `list(str(x) for x in xs)`
  and `",".join(s + "!" for s in ss)` work.
- Rejected with a diagnostic rather than a wrong answer: a bare generator
  expression alongside other call arguments (CPython requires parentheses
  there), and capturing a *module-level* variable — the closure cell would
  never be filled because the assignment writes a global. That last case
  previously failed at run time with a NameError, and did so for lambdas too;
  both now fail at compile time with guidance to move the code into a function
  or pass the value in.

## 0.95.0 — Generators as arguments to the eager builtins

A generator function could only be consumed by a `for` loop or a
comprehension. Every eager builtin rejected one, so `list(g())` — probably the
most common thing anyone does with a generator — was a compile error, and the
only way to get the values out was to write the loop by hand.

- `list`, `set`, `sorted`, `sum`, `max`, `min` and `str.join` accept a
  generator. These drain their argument anyway, so the generator is
  materialized first and side effects, order and result are identical to
  consuming it lazily.
- `any` and `all` accept a generator and **short-circuit**, stopping as soon
  as the answer is known. They cannot materialize first: a side-effecting or
  infinite generator would behave differently from CPython. (Over a list they
  still walk the whole sequence, where it is unobservable.)
- `tuple(gen)` is still rejected, because tuples are fixed-arity here; the
  existing diagnostic says so.
- Fixed alongside: an unannotated generator hard-coded its yield type to
  `int`, so `def g(): yield "a"` failed with a type mismatch at the yield and
  a `str`, `float` or `bool` generator could not be written at all without a
  return annotation. The yield type is now taken from the first `yield` of a
  literal or an annotated parameter, searched through `if` / `for` / `while` /
  `try` / `with` bodies, and the three signature-collection sites agree with
  the lowering site — a mismatch there gave a call site a different element
  type than the body produced.

## 0.94.0 — User-defined exception classes

`class E(Exception)` had no spelling: the exception type in `raise` / `except`
was resolved by the *parser* against a hardcoded list of builtins. Unlike most
open gaps there was no workaround — only falling back to a builtin type, which
loses the distinction the program is making.

- `class E(Exception): pass` and chains (`class B(A)`) are supported. A
  subclass is caught by any ancestor and by `except Exception`; a base is not
  caught by its subclass; unrelated user exceptions do not catch each other.
- `raise E`, `raise E()` and `raise E("msg")` all work, for user classes and
  builtins alike. These first two forms were previously rejected outright.
- An uncaught exception with no message prints just the type name, as CPython
  does: `raise ValueError()` reports `ValueError`, not `ValueError: `. The
  bound `e` and its message are empty in that case rather than repeating the
  type name.
- Builtin exceptions are unaffected, including the `OSError` family, and a
  tuple filter may mix user and builtin types.
- Exception-name resolution moved from the parser to the semantic phase, which
  is the only place that knows which classes exist. The parser no longer
  validates exception names against a fixed list.
- Deliberately rejected, each with a diagnostic that says why rather than a
  generic one: methods or fields on an exception class (it carries a tag and a
  name, with no instance layout); using an exception class as a value
  (`x = E("m")`) — the name exists, the use does not; subclassing
  `GeneratorExit`, which is BaseException-only in CPython so `except Exception`
  would miss the subclass; and a base declared after its subclass, which
  CPython rejects with NameError.

## 0.93.0 — Conditional expressions

`a if c else b` was a parse error. Unlike the other open gaps this one is not
an exotic corner: it is one of the most common expressions in Python, and
`x = "big" if n > 3 else "small"` had no spelling at all.

- Conditional expressions are supported everywhere an expression is:
  assignments, returns, call arguments and defaults, subscripts and slices,
  container literals, f-strings, `while` and `assert` conditions, comprehension
  elements, generators and closures.
- Only the selected branch is evaluated, so the guard idioms work:
  `1 // n if n else -1` does not divide by zero, and `xs[0] if xs else "empty"`
  does not raise. The condition is evaluated exactly once.
- Precedence and associativity match Python: `or`/`not` bind tighter
  (`0 or 2 if False else 9` is `9`), chains are right-associative, and the
  condition itself is an `or_test`, so CPython's rejection of
  `1 if 2 if 3 else 4 else 5` is reproduced rather than silently nested.
- A bare conditional is excluded from comprehension iterables and filters, as
  in CPython: the trailing `if` in `[x for x in a if b]` belongs to the
  comprehension. `[x for x in range(3) if 1 if True else 0]` is rejected with a
  diagnostic pointing at the `else`.
- Mixed numeric branches keep each branch's own type, so `1 if c else 2.5` is
  `1`, not `1.0` — the same rule 0.89 established for `[1, 2.5]`. Unrelated
  branch types (`1 if c else "s"`) become a union.
- Arithmetic and comparison on a mixed-numeric union remain unsupported, as
  they already were for list elements. That diagnostic now explains why and
  suggests two fixes that were checked to work — give the parts one type, or
  narrow with `isinstance`. `float(x)` on the union and annotating the target
  do not work and are no longer implied.

## 0.92.0 — String literal escapes and PEP 701 f-strings

Two gaps found while testing the Unicode milestones. Both were silent wrong
answers in the supported surface rather than missing features.

- String literals decode `\xNN`, `\uXXXX`, `\UXXXXXXXX`, one-to-three-digit
  octal (`\101`) and the control escapes `\a \b \f \v`. The lexer previously
  recognised only `\n \t \r \0 \\ \' \"`, so `"\x00"` survived as the four
  characters `\`, `x`, `0`, `0` and `len` reported `4`. Unknown escapes still
  survive verbatim, as in CPython.
- Malformed escapes are rejected with a specific message —
  `truncated \xXX escape`, `truncated \uXXXX escape`,
  `illegal Unicode character U+11FFFF` — instead of being mangled.
- Two forms CPython accepts are rejected deliberately, with a diagnostic
  rather than a wrong answer: a lone surrogate (`"\ud800"`), which has no
  UTF-8 form and so cannot be represented in a PyRs string, and `\N{NAME}`,
  which needs the Unicode name database the compiler does not carry.
  `chr(0xD800)` still produces the bytes at run time.
- f-string replacement fields accept string literals in either quote,
  including the quote delimiting the f-string: `f"{d["k"]}"` (PEP 701). The
  f-string token was a regex that stopped at the first unescaped quote; it is
  now a scanner tracking brace depth and nested literals. The parser's brace
  scan and its `!` / `:` split skip nested literals too, so `f"{'}'}"` and
  `f"{d[':']}"` are correct.
- An unterminated single-quoted f-string now reports
  `unterminated f-string literal` rather than `unexpected character`.

## 0.91.0 — Unicode case transforms and character classes

0.90 made string offsets code points; this makes character *properties*
Unicode too, closing the last rows of the measured-defect table. The tables in
`codegen/runtime/unicode_data.c` are generated by
`scripts/gen_unicode_tables.py`, which asks the installed CPython about every
code point rather than re-parsing the UCD — the same interpreter the
differential tests compare against. `make hygiene` fails if the stamped
Unicode version stops matching the running interpreter.

- `upper`, `lower`, `title`, `capitalize`, `swapcase` and `casefold` follow
  Unicode 16.0.0, including mappings that change length: `"ß".upper()` is now
  `"SS"`, `"ﬁ".upper()` is `"FI"`, and `"naïve café".upper()` is
  `"NAÏVE CAFÉ"`. Titlecase characters are handled, so `"ǅungla".title()`
  matches CPython.
- `isalpha`, `isdigit`, `isdecimal`, `isnumeric`, `isalnum`, `isspace`,
  `isupper`, `islower`, `istitle`, `isprintable` and `isidentifier` use
  Unicode categories and derived properties. `"é".isalpha()` is now `True`,
  `"²".isdigit()` is `True` while `"²".isdecimal()` is `False`, and
  identifiers accept `café` and `π`. `isascii` is now a header comparison.
- Whitespace-driven `strip`, `split` and `rsplit` use the Unicode whitespace
  set and scan by character, so a multi-byte character can no longer be split
  by a byte that happens to match.
- `repr` escapes by Unicode printability rather than byte range, both for
  `!r` and for strings printed inside a container: `repr("café")` is
  `'café'`, and a zero-width space is escaped.
- Table cost: 303 distinct records cover all 1.1M code points; 155 KiB of C
  that compiles in 40 ms, against the C runtime's existing 2.2 s.
- Still open, and stated rather than implied: no normalization or
  grapheme-cluster segmentation (`len` counts code points, so `"e" + U+0301`
  is 2); no locale-sensitive casing beyond CPython's default; lone-surrogate
  and encoding-error behavior is unspecified; indexing a non-ASCII string is
  O(n), not CPython's O(1).
- One suite change: the `while_local_optional_reassign_none_terminates`
  timeout went from 5s to 30s. Its budget has to cover a full `pyrs compile`
  (~2.4 s serially, dominated by building the C runtime) under a saturated
  parallel test run; it exists to catch an infinite loop, and the file's other
  timeout tests already used 15s.

## 0.90.0 — Unicode code point offsets

String offsets are Unicode code points, matching CPython. `PyrsStr` stays a
UTF-8 buffer and gains a cached code point count as its first header word, so
`len(s)` is O(1), codegen's `emit_len` is unchanged, and `print`, file I/O,
hashing, comparison and the CPython bridge keep operating on bytes. ASCII
strings are recognised as `cplen == len` and keep the existing paths.

- `len`, indexing, slicing (including a step), iteration, `list(str)` and
  `set(str)` count and select whole characters across 1-, 2-, 3- and 4-byte
  code points, combining sequences and an embedded NUL. `len("héllo")` is now
  `5`, `"héllo"[1]` is `é`, and `len("🐍")` is `1`.
- `find`/`rfind`/`index`/`rindex`/`count` return character offsets and accept
  character `start`/`end` bounds, as do `startswith`/`endswith`.
  `"héllo".find("l")` is now `2`.
- `split`/`rsplit`/`partition`/`rpartition`/`splitlines` and the `strip`
  family never split a character; `strip(chars)` compares whole code points
  instead of bytes, so it can no longer leave an invalid sequence behind.
- `center`/`ljust`/`rjust`/`zfill`/`expandtabs` and f-string format widths and
  precision measure characters, and a multi-byte fill character is written
  whole. `translate`/`maketrans` key on code point ordinals. `ord` is O(1).
- Fixed alongside: four string allocation sites hand-rolled the old header
  layout (exception messages, object reprs, generator `throw`), one of which
  corrupted the heap once the header changed.
- Indexing a non-ASCII string is O(n), not CPython's O(1). A one-entry
  sequential-access memo, invalidated on every collection, keeps `for c in s`
  and index loops linear. On this host a string-saturated ASCII workload costs
  5.3%; code that touches no strings is unaffected.
- Case transforms (`upper`, `lower`, `title`, …) and the `is*` predicates keep
  their documented ASCII-only behavior. `"ß".upper()` is still `ß` and
  `"é".isalpha()` is still `False`; the generated Unicode 16.0.0 tables are the
  next milestone. Offsets and properties are now separate, so no operation
  counts bytes while another counts characters.
- Not addressed, and unrelated to offsets: the lexer accepts no `\xNN` or
  `\uXXXX` escapes, and an f-string replacement field cannot contain nested
  quotes.

## 0.89.0 — Value fidelity: numeric literals and default `!=`

- Mixed-numeric list and tuple literals keep each element's own type instead of
  promoting to one: `[1, 2.5, 1]` prints `[1, 2.5, 1]`, not `[1.0, 2.5, 1.0]`.
  `join_elem_types` builds a union for mixed `(int, float)`, `(int, bool)` and
  `(float, bool)` pairs; homogeneous literals are unaffected and keep
  single-type storage. The default-argument inference path was joining with the
  scalar-assignment rule and now agrees.
- A class defining `__eq__` with no `__ne__` anywhere in its ancestry gets one
  synthesized, calling `self.__eq__` negated through the normal vtable. `a != b`
  where `a` is `Base`-typed but holds a `Child` defining `__ne__` now reaches
  `Child.__ne__`, keeping both its result and its side effects. Synthesis walks
  classes parent-first so an ancestor's explicit `__ne__` is never shadowed.
- Converting an already-typed `list[int]` to `list[float]` or to a union by
  assignment remains unsupported; only a literal's own elements are joined.
  Recorded as its own roadmap item rather than implied closed.

## 0.88.0 — Trustworthy validation gates

- The example parity gate fails when either process fails, instead of comparing
  only captured output.
- Integration tests build under `CARGO_TARGET_TMPDIR` (`target/tmp`, the path CI
  uploads) and retain their inputs when the thread is panicking, so a failing
  test leaves usable artifacts.
- `make hygiene` checks version agreement across 20 sites and resolves every
  relative documentation link, and its own failure paths are tested.
- `make asan` / `make ubsan` build the adapter, runtime and collector
  instrumented and run the extension boundary suite. The LLVM-generated kernel
  object is not instrumented; this is adapter and runtime coverage.

## 0.87.0 — String annotations and `__future__` imports

- Annotations written as string literals are accepted and resolved, including
  nested generics, unions and `Optional`, forward references to the enclosing
  class, and local variable annotations.
- `from __future__ import annotations` is accepted as a no-op, as are the other
  mandatory-in-Python-3 future features.
- Multidimensional slice syntax (`a[i, j]`) and matrix multiplication (`@`) are
  rejected with specific diagnostics naming what is missing, rather than a
  generic parse error.

## 0.86.0 — CPython interoperability and stranded correctness fixes

Native compilation is the default; the target workload family is scientific/data
Python, including NumPy and pandas through optional CPython compatibility
execution. Reaching 0.86.0 does not establish 1.0 readiness; see the
[roadmap](docs/ROADMAP.md) for the remaining gates.

- Borrowed list headers from the CPython bridge carry a non-owned capacity
  marker, so every runtime growth site raises `BufferError` instead of calling
  the libc allocator on exporter- or `PyMem`-owned memory. Previously a resize
  would free a foreign pointer, corrupt the heap and invalidate a buffer lease
  that still had to be released. Read-only enforcement no longer rests solely on
  the extension frontend's allowlist.

- Python-style script, `-c` and stdin invocation, with script arguments and
  native `sys.argv[0]` preserved on Unix. Existing compiler subcommands remain.
- Explicit `--compat` executes a whole script, command or `-m` package using
  CPython. Select an environment with `--python` or `PYRS_PYTHON`. This mode uses
  installed Python packages and preserves their semantics; it does not compile
  them or accelerate them. It is never an automatic retry of native execution.
- `pyrs check -i script.py` checks the native frontend/import graph without
  linking or executing code. Optimization levels outside 0–3 are rejected.
- Private, atomically created temporary build directories with automatic cleanup
  on normal completion and ordinary compiler errors.
- Exact mixed integer/float comparisons, including bigints, fractional values,
  NaN and infinities, and numeric equality through boxed list values.
- Correct bigint-to-float ties-to-even rounding and overflow, and float-to-int
  conversion at the positive small-integer boundary (`2**62`).
- Binding checks for conditionally assigned locals, including zero/False/None
  values and generator suspension. Locals modified inside `try` survive exception
  handling at optimized compilation levels. Module globals, deletion and static
  use-before-assignment diagnostics remain separate work.
- Preserve side effects and unbound reads in None identity comparisons and
  `print(sep=.../end=...)` expressions with a None result.
- Versioned compatibility probes and reports that distinguish native gaps from
  compatibility passes and compare streams, exit status and generated files.
- Experimental `build-extension` target: compile numerical function modules into
  CPython extensions on Linux. Native library analysis/emission is separate from
  the Python adapter and does not require or automatically call `main()`.
- Extension functions can return strings: native UTF-8 is copied into a Python
  string while the result remains rooted. Invalid UTF-8 raises UnicodeDecodeError
  with input buffers released normally. String arguments remain unsupported.
- Exact scalar boundary guards, arbitrary-size integer conversion, keyword
  binding, native exception translation and read-only 1D float64 buffer borrowing.
  Python lists of floats use temporary copies; NumPy/pandas buffers can pass
  without copying. Buffer leases are released on ordinary success and failure.
- Native extension boundary tests at O0/O2/O3 under GC stress, including lifetime,
  thread/reentrancy, symbol isolation, and scientific package checks. The
  `examples/interop` demo verifies results and measures full call overhead.
- The bridge remains an explicit numerical API; automatic mixed execution,
  general objects/arrays and a stable standalone library ABI remain planned
  work. See the [interoperability contract](docs/INTEROPERABILITY.md) for
  current restrictions.
- Known gaps recorded rather than claimed fixed: string length/indexing still
  operate on UTF-8 bytes, and mixed numeric list literals still promote ints to
  floats. Both are reported as `known_gap` by the compatibility probes.
- CI wiring for the compatibility probes is deferred: the required
  `scientific-compatibility` job would have made every run depend on installing
  NumPy/pandas from PyPI. `make ci` runs the probes locally in the meantime.
