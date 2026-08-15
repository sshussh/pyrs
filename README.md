# PyRs

[![CI](https://github.com/sshussh/pyrs/actions/workflows/ci.yml/badge.svg)](https://github.com/sshussh/pyrs/actions/workflows/ci.yml)

A Python compiler built in Rust, targeting native code through LLVM.

PyRs compiles a statically-typed subset of Python straight to machine code —
no interpreter, no VM. Compute-bound code runs 45–60× faster than CPython
(see [Benchmarks](#benchmarks)).

**New here? The [PyRs Guide](docs/GUIDE.md) covers everything**: the CLI,
the Makefile, the full language reference, every difference from CPython,
runtime errors, and performance notes.

```console
$ cat examples/fib.py
def fib(n: int) -> int:
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)

print(fib(30))

$ pyrs compile -i examples/fib.py -o fib
$ ./fib
832040
```

## Usage

```console
pyrs compile -i prog.py -o prog     # build a native executable
pyrs run     -i prog.py             # compile and run immediately
pyrs lex     -i prog.py             # dump tokens
pyrs parse   -i prog.py             # dump the AST
```

`compile` options: `-O 0..3` (optimization level, default 2) and
`--emit-llvm` (also write the generated LLVM IR to `<output>.ll`).

## The language (v0.59.0)

Versioning is **MAJOR.MINOR.PATCH**. PyRs stays on **0.y.z** (next
milestone after this one is **0.60.0**, not 1.0) until it is ready for
**real-world use**; only then **1.0.0**. Crate versions and
`pyrs --version` match this label. PyRs now ships its first default heap
collector: a **nonmoving mark–sweep** backend with conservative native-root
discovery. This replaces the old never-free runtime; it is the safe first
backend, not the eventual moving generational/Immix design. See
[Garbage collection](docs/GC.md). Classes remain a closed-world subset
(v0.21 adds `__str__`/`__repr__`, zero-arg `super()`, and more kit — see
below). No new stdlib until the language can host pure-PyRs libraries.

A statically-typed Python subset:

- **Types:** `int` (arbitrary precision; tagged small / heap limbs), `float` (f64), `bool`, `str`, `None`,
  unions (`int | None`, `str | int | None`), `Optional[T]`, limited
  **`Any`** (dynamic slot box; concrete↔Any coerces with a runtime
  TypeError check — not full CPython dynamism), `list[T]`,
  `tuple[T1, T2, …]`, `dict[K, V]`, `set[T]` — including nested lists
  (`list[list[float]]` matrices). Dict/set keys are `int` or `str` only;
  list elements and dict values may be Optional/unions/`Any`; homogeneous
  closures (same params/ret and capture env shape, with or without
  captures) and user class instances may be list/tuple elements.
  Multi-assign joins storage types
  (`x = 1; x = "a"` → `int | str`; numeric multi-assign promotes).
  Bare params may be inferred monomorphically from body usage
  (arithmetic, comparisons, methods, indexing, `isinstance` branches);
  empty `xs = []` followed by `xs.append(v)` / `insert` fixes `list[T]`;
  unannotated empty `[]` with no append/insert defaults to **`list[Any]`**;
  class names are valid type annotations (`def f(p: Point)`); annotations
  still **fix** storage when present (not silently widened)
- **Classes (v0.21):** `class C:` / `class D(C):` with instance methods
  (`def m(self, …)`), fields assigned in `__init__` only (no class-body
  attributes), construction `C(...)`, attribute load/store,
  `self.method()`, single inheritance with override + virtual dispatch
  when the static type is a base, **zero-arg `super().m(...)`** (static
  parent call; cooperative `super().__init__`), `isinstance(obj, C)` with
  inheritance, **`isinstance` flow narrowing to a more-specific subclass**
  (subclass fields/methods after peel; mid-expression `isinstance(x, B)
  and x.b`), subclass assignable where a base is expected (params,
  returns, list append of `list[Base]`, unions containing the base),
  cross-module `from m import C` / subclassing. **`__str__` / `__repr__`**
  (must return `str`): used by `print`/`str()` with virtual dispatch when
  present; default remains `<Name object>` (no address; runtime type_id).
  **Also (v0.23+):** `@staticmethod` / `@classmethod` / read-only `@property`, bound methods as values,
  `__iter__`/`__next__` for-loops, `__len__`/`__bool__`, class `with` context managers, single free-function
  decorators, match class patterns.
  **v0.25 protocol completion:** `__exit__` suppress (truthy return swallows the body exception);
  exception path passes `__exit__(None, exc, None)` (type and traceback remain `None` — no exception
  type objects / traceback objects yet); builtin `next(it)` / `next(it, default)` for user iterators
  and generators; user-class `__contains__` for `in` / `not in`.
  **v0.26:** `sorted(xs, key=f)`, `min(xs, key=f)`, `max(xs, key=f)` with a monomorphic
  `key=` callable (`T →` sortable `int|float|bool|str`); desugared in semantic (no C comparator).
  **v0.27:** `list.sort(key=f)` in-place with the same monomorphic `key=` surface (shared
  desugar with `sorted`; `reverse=` still residual).
  **v0.28:** `sorted(..., reverse=bool)` and `list.sort(reverse=bool)` (stable
  reverse-sort-reverse; works with `key=`; `reverse=` must be `bool`).
  **v0.29:** multi-arg `min(a, b, c, …)` / `max(…)` (numeric fold with
  `bool`→`int`→`float` unify) and multi-arg with monomorphic `key=`
  (`min(a, b[, c…], key=f)`); positionals must share one type when `key=` is used.
  **v0.30:** `min`/`max` iterable `default=` (`min(xs, default=d)` /
  `min(xs, key=f, default=d)`); empty → default (result type is
  `join(elem, default)`); multi-arg form rejects `default=` like CPython.
  **v0.31:** bare builtins as monomorphic `key=` — `len`, `abs`, and
  casts `int`/`float`/`bool`/`str` on `sorted` / `list.sort` / `min` / `max`
  (IR ops, not first-class values); other builtins still need a wrapper.
  **v0.32:** lexicographic `min`/`max` for homogeneous `str` (multi-arg and
  `list[str]` without `key=`); numeric multi-arg/list path unchanged.
  **v0.33:** `sorted` / `list.sort` `reverse=` uses CPython truthiness
  (`reverse=1` / runtime int, not only `bool`); const-folds 0/1/True/False.
  **v0.34:** lexicographic tuple ordering (`<`/`<=`/`>`/`>=`), multi-arg and
  list `min`/`max` over orderable tuples, and `sorted`/`list.sort` for
  `list[tuple[…]]` (elements: int|float|bool|str or nested orderable tuples).
  **v0.35:** lexicographic list ordering (`[1,2] < [1,3]`), multi-arg and
  list `min`/`max` over orderable lists, and `sorted`/`list.sort` for
  nested `list[list[…]]` of orderable elements.
  **v0.36:** bare `key=len` on class instances that define `__len__` → `int`
  (`sorted` / `list.sort` / `min` / `max`).
  **v0.37:** `in` / `not in` for nested lists (`[1, 2] in [[1, 2], [3]]`),
  using the same recursive equality as `==` / `list.index` / `list.remove`.
  **v0.38:** `list.count(x)` with the same recursive equality (including
  nested lists).
  **v0.39:** `list.reverse()` in-place (statement only; `reversed(xs)` /
  `xs[::-1]` still allocate a copy).
  **v0.40:** `sum(xs, start)` / `sum(xs, start=s)` — numeric start (default
  0 / 0.0); result type is `elem ⊔ start`.
  **v0.41:** `del xs[i]` for lists (negative indices; OOB → same
  `IndexError` as list assignment).
  **v0.42:** class `==` / `!=` — `__eq__` when defined (virtual), else
  pointer identity; ordering still residual.
  **v0.43:** list slice assignment `xs[lo:hi:step] = ys` and `del xs[lo:hi]`
  (same-elem list RHS; extended slices require matching length).
  **v0.44:** set `==` / `!=`, subset operators `<` / `<=` / `>` / `>=`, and
  `issubset` / `issuperset` / `isdisjoint`.
  **v0.45:** `round(x)` / `round(x, ndigits)` — ties to even; one-arg yields
  `int`; two-arg keeps `int` or `float`.
  **v0.46:** `ord(s)` / `chr(n)` — Unicode code point of a one-character
  string, and the inverse (`chr` accepts `0 ..= 0x10FFFF`; `bool` → `int`).
  String `len`/index stay byte-based.
  **v0.47:** integer literals `0x` / `0b` / `0o` (any case, PEP 515
  underscores) convert to the same `int` as decimal; invalid prefixes
  are compile errors.
  **v0.48:** `print(..., sep=..., end=...)` — `sep`/`end` are `str` or
  `None` (`None` restores the defaults `" "` / `"\\n"`).
  **v0.49:** `hex(n)` / `bin(n)` / `oct(n)` — lowercase `0x` / `0b` / `0o`
  strings (`hex(-10)` is `'-0xa'`); `bool` → `int` like `chr`.
  **v0.50:** `dict.setdefault(k[, default])` — insert on miss and return the
  stored value; bare form requires a value type that includes `None`.
  **v0.51:** `divmod(a, b)` — `(a // b, a % b)` for int/bool/float (operands
  evaluated once; mixed numeric promotes like `//`).
  **v0.52:** `print(..., flush=...)` — CPython truthiness (`True`/`1` flush
  stdout after writing; `False`/`0`/`None` no-op); `file=` still residual.
  **v0.53:** `dict.popitem()` — LIFO last-inserted `(k, v)` pair; empty dict
  raises `KeyError: 'popitem(): dictionary is empty'`.
  **v0.54:** `str.removeprefix` / `str.removesuffix` — drop an exact prefix or
  suffix when present (empty affix is a no-op).
  **v0.55:** `str.partition` / `str.rpartition` — first/last split into
  `(head, sep, tail)`; empty separator is `ValueError`.
  **v0.56:** `pow(base, exp)` is `**`; `pow(base, exp, mod)` is modular
  exponentiation (ints; negative exp is modular inverse).
  **v0.57:** `str.rsplit` and optional `maxsplit` on `split`/`rsplit`
  (`None` sep is whitespace; `maxsplit < 0` is unlimited).
  **v0.58:** `int(s[, base])` and `float(s)` parse strings (CPython rules;
  ASCII whitespace; `int` bases 0 and 2..=36; `float` accepts `inf`/`nan`).
  **v0.59:** `str.index` and optional `start`/`end` on `find`/`index`/`rfind`/
  `rindex` (CPython slice bounds; `None` allowed; miss is `-1` or ValueError).
  **Not yet:** multiple inheritance, metaclasses, `__new__`/`__slots__`, open
  `__dict__`, nested classes, class decorators, stacked free-function
  decorators, two-arg `super()`, class-body attrs, first-class class values;
  mixed non-numeric list **literals** need a union annotation (empty `[]` +
  mixed appends join; common fields on class unions are readable;
  exclusive subclass fields after multi-class `isinstance` use a
  **runtime type_id switch** — AttributeError when the live instance
  lacks the field; method calls on bare `Any` and open setattr remain
  unsupported)
- **Functions:** `def` with optional parameter/return annotations
  (defaults infer param types; bare params inferred from body when unique;
  return type inferred from `return` when omitted), defaults and keyword
  args, recursion, forward references;
  pass/return tuples and other containers; nested `def` / `lambda` as
  first-class closures (including in containers and sibling/forward
  nested calls); late free-var binding (`def f(): return n` then
  `n = 5`); `nonlocal`; generator functions with `yield` / `yield from`
  (may escape, capture free vars, use `try`/`except`/`finally` including
  yield in finally; `send` / `throw` / `close`)
- **Statements:** `if`/`elif`/`else`, `while` / `for` (including
  `else` on loops — runs only if no `break`),
  `for x in range(...)` / lists / strings / files / tuples / dict keys /
  sets / generators (including unpack targets `for a, b in xs` and
  `for a, *rest in xs`), `break`/`continue`, assignments (plain, annotated,
  multi-target, unpacking `a, b = t`, augmented — including
  `xs[i] += v` and `s |= t` for sets), `del xs[i]` / `del xs[i:j]` / `del d[k]`, `return`, `pass`,
  `raise ExcType("msg")`, `assert test` / `assert test, msg`
  (`AssertionError`), `try`/`except`/`except (A, B)`/`else`/`finally`
  (including inside generators),
  `match`/`case` (literal, wildcard, capture, or-patterns, guards,
  sequence with optional `*rest`, mapping with optional `**rest`,
  `as` patterns, class patterns with positional/keyword fields)
- **Expressions:** full arithmetic including `**`, comparisons with
  chaining (`0 < x < 10`), `in`/`not in` (substring, list/tuple/set
  membership including nested lists, dict keys),
  assignment expressions `name := value` (walrus),
  `is`/`is not` (None checks plus pointer/slot identity for same-type
  heap objects and scalars), bitwise `& | ^ ~ << >>`
  (and augassign) on int/bool; set `|` / `.union` / `|=`,
  `and`/`or`/`not`
  (short-circuit; `and`/`or` return an operand, not always `bool`, and
  may yield a union when operands differ, e.g. `0 or "x"`), casts
  `int()`/`float()`/`bool()`/`str()` (including `int(s[, base])` / `float(s)`
  from strings), `len()`, `abs()`, `round()`, `hex()`/`bin()`/`oct()`, `divmod()`, `pow()`, `min()`/`max()`
  (two-or-more numeric args, or one list: without `key=` only
  `list[int|float|bool]`; with monomorphic `key=` any `list[T]` or
  multi-arg homogeneous candidates),
  `sum()` on `list[int]`/`list[float]` (optional numeric `start=` /
  positional start), `isinstance(x, T)` /
  `isinstance(x, (T1,T2))` with flow-sensitive narrowing, `any`/`all`,
  `enumerate`/`zip`/`reversed` (materialize to lists when used as values),
  indexing with negative indices, full slicing `s[a:b:c]` including
  `[::-1]` reversal, `print(...)` with any mix of values (including
  tuples/dicts/sets) and `sep=` / `end=` (`str` or `None`) and `flush=`
  (truthy → `fflush(stdout)`)
- **f-strings:** `f"x={x}, next={x + 1}"` and multi-line `f"""…"""` /
  `f'''…'''` with `{{`/`}}` escapes, nesting, conversions `!s`/`!r`/`!a`,
  and free-form format specs (fill/align/sign/`#`/`0`/width/precision/
  types `dboxXfeEgGs%`, nested `{x:{w}.{p}f}`); no `{x=}`, grouping
  `,`/`_`, or types `n`/`c` yet; multi-line *expressions* inside `{...}`
  need parentheses; same-delimiter triples inside `{...}` unsupported —
  use the other quote style
- **Strings:** immutable; single/double and triple-quoted literals
  (`"""…"""` / `'''…'''`, multi-line; escapes as for single-line);
  module/function first-statement string docstrings are accepted as
  no-op expression statements (no `__doc__` attribute yet); `+` concat,
  `*` repeat, lexicographic comparisons, indexing, slicing, `in`,
  iteration, `len()`, `str(x)` conversions, and methods: `upper` `lower`
  `strip` `lstrip` `rstrip` `startswith` `endswith` `find` `index` `rfind`
  `rindex` `count` `replace` `split` `rsplit` `join` `isdigit` `isalpha`
  `isspace` `isupper` `islower` `removeprefix` `removesuffix`
  `partition` `rpartition`
- **Lists:** homogeneous, growable; literals, comprehensions
  (`[x * x for x in xs if x > 0]`, multi-`for` / multi-`if`, unpack
  targets `[a+b for a, b in pairs]`; simple names use Python 3 scoping
  and do not leak — and faster than the equivalent loop when length is
  knowable: results are pre-sized and appends inlined),
  indexing (read/write), `del xs[i]` / `del xs[i:j]`, slicing (copies, like Python),
  slice assignment `xs[i:j] = ys` / `xs[::2] = ys` (same-elem list),
  `append`/`pop`/`insert`/`remove`/`index`/`count`/`clear`/`reverse`/`sort`/`extend`/`copy` (homogeneous
  list arg), `list(iterable)`, `sorted(xs)` / `sorted(xs, key=f)` /
  `sorted(xs, reverse=…)` / `sorted(xs, key=f, reverse=…)`,
  `list.sort()` / `list.sort(key=f)` / `list.sort(reverse=…)` (and both kwargs),
  `+`/`*` (concat / repeat), `==`/`!=`, `in` / `not in` (including nested
  lists, same recursive equality as `==`), `len`, iteration;
  assignment aliases like Python
- **Tuples:** fixed-arity, heterogeneous; literals `(a, b)`, `(a,)`,
  `()`; index (incl. negative); `len`; unpacking; print like CPython
- **Dicts:** `dict[K, V]` with `K` in `{int, str}`; literal `{k: v}`,
  `{}` (needs annotation); get/set, `del d[k]`, `in` on keys, `len`,
  insertion-order key iteration; methods `get` (with default, or bare
  `get(k)` → `Optional[V]` / `None` on miss),
  `pop`, `setdefault` (insert on miss; bare form needs `None` in `V`),
  `popitem` (LIFO last-inserted pair),
  `keys`/`values`/`items` (return lists), `clear`, `update`, `copy`, `dict(pairs)`, dict comps
- **Sets:** `set[T]` with `T` in `{int, str}`; nonempty `{a, b}`, empty
  `s: set[int] = set()`; `add`/`remove`/`discard`/`clear`/`union`/`intersection`/`difference`/`symmetric_difference`/`update` /
  `issubset`/`issuperset`/`isdisjoint`,
  `|` / `&` / `-` / `^` / `|=`, `==` / `!=` / `<` / `<=` / `>` / `>=`, `set(iterable)`, set comps, `in`, `len`, iteration
- **Exceptions:** `raise ExcType("msg")` for ValueError, KeyError,
  IndexError, ZeroDivisionError, TypeError, RuntimeError, GeneratorExit,
  OverflowError, EOFError, FileNotFoundError, OSError, PermissionError,
  IsADirectoryError, NameError, UnboundLocalError, StopIteration,
  Exception, **AssertionError** (`assert test` / `assert test, msg`);
  `try`/`except`/`except Type as e` / `except (A, B)`/`else`/
  `finally`; CPython-like hierarchy match (`except OSError` catches
  `FileNotFoundError`/`PermissionError`/`IsADirectoryError`; `except
  Exception` catches normal exceptions but not `GeneratorExit`);
  `except … as e` binds a first-class **exception object** (`print(e)` /
  `str(e)` → message; truthy in conditions; `isinstance(e, OSError)` /
  multi-filter / unions with Exception); uncaught traps print
  CPython-like messages and exit 1. Exception objects may be list/tuple elements; `e.args` is `list[str]` (empty or one message);
  `repr(e)` / `!r` via ExcRepr. Residuals: full CPython `args` as tuple; dict/set of exceptions
- **Globals:** top-level variables are readable from any function;
  writing needs a `global x` declaration, exactly like Python
- **I/O:** `input([prompt])` from stdin; `import sys` + `sys.argv` for
  command-line arguments; files via `open(path, mode)` with
  `.read()`/`.readline()`/`.readlines()`/`.write()`/`.close()`,
  `with open(...) as f:` blocks, and CPython's exact error messages —
  compiled programs are real CLI tools
- **Modules & packages:** split a program across files and packages —
  `import utils`, `import a, b as c` (multi-name), `import pkg.mod` /
  `import pkg.mod as m`, `from pkg.mod import name`, `from pkg import mod`,
  package re-exports in `__init__.py` (`from .mod import f` / `from .
  import mod`; last top-level binding wins, with CPython fromlist hasattr
  short-circuit so assign/`def` then `from . import same_name` keeps the
  value and does not run the submodule), relative forms inside packages,
  and partial package init (child module top level may read simple parent
  assigns set before the child import; child function bodies may use
  deferred parent attrs/calls after full init); a directory with
  `__init__.py` is a regular package; a directory **without** `__init__.py`
  is a **namespace package** (PEP 420 subset: single origin, no multi-path
  split; prefer `__init__.py` > `name.py` > namespace dir); `from M import *`
  expands public names (or static `__all__` list/tuple of string lits) at
  module level only; module bodies run once at the import site
  (like Python). Import search order (stacked, first hit wins): (1) entry
  script directory, (2) `PYRS_STDLIB` if set, (3) workspace `stdlib/` when
  present (dev; not XOR with env), (4) **embedded** stdlib inside the
  `pyrs` binary (always; no companion directory needed). User code shadows
  stdlib; once a package is found under one origin, children stay there
  (no split packages). Cycles and missing modules/names are compile
  errors that point at the offending file
- **Stdlib (subset, frozen):** pure-PyRs `os.path` — `join(a, *parts)`
  (POSIX), `dirname`, `basename`; `os.getcwd()` (C runtime); `math` —
  constants `pi`/`e` and unary `sqrt`/`sin`/`cos`/`tan`/`log`/`log10`/
  `exp`/`floor`/`ceil`/`fabs` (intrinsics / libm); `json.dumps` for
  int/float/bool/str and homogeneous list/dict-of-str, plus typed
  `json.loads_*` helpers (no dynamic `json.loads`). `import sys` remains
  special-cased for `sys.argv`. **No new stdlib until the core language
  is far enough for pure-PyRs libraries** (see roadmap / `AGENTS.md`);
  interim modules may be rewritten pure later
- **Entry point:** top-level statements run like a script; if there are
  none, a zero-argument `main()` is called automatically

Python semantics are preserved where it counts:

- `7 / 2 == 3.5` — true division always yields float
- `-7 // 2 == -4`, `-7 % 3 == 2` — floored division and modulo
- `-2 ** 2 == -4`, `2 ** -1 == 0.5`, right-associative `2 ** 3 ** 2`
- `1 < middle() < 10` evaluates `middle()` exactly once and
  short-circuits, exactly like Python
- `ZeroDivisionError`, `IndexError`, `ValueError`, `KeyError`, … trap
  with exit 1 when uncaught (or transfer to an active `except`)
- floats print with shortest round-trip representation
  (`0.1 + 0.2` → `0.30000000000000004`, `1.0` → `1.0`); lists print as
  `[1, 2, 3]` / `['a', 'b']`; tuples/dicts/sets print like CPython
- iterating a list re-reads the live length, so appending inside the
  loop behaves like CPython
- variables use function-wide scoping; storage type is the join of all
  assignments (and annotation); bare multi-assign may produce a union

Known limits (v0.59.0): `int` is arbitrary precision (tagged small ±2⁶² /
GC-managed heap limbs; no interning/`is` identity for equal
values), `min`/`max`
multi-arg numeric form unifies to a common numeric type (`min(1, 1.5)` is
`1.0`, not the int `1`; pure `bool` args print as `0`/`1`); multi-arg and
iterable form also accept homogeneous `str`, orderable tuples, and orderable
lists (lexicographic); multi-arg with
monomorphic `key=` requires homogeneous positionals (mixed types → compile
error; use the iterable form or a common type); iterable `min`/`max` without
`key=` is for `list[int|float|bool|str|orderable tuple|orderable list]`
(empty without `default=` → ValueError like CPython); with monomorphic `key=`
any `list[T]` is fine; iterable `default=` joins with element type
(`default=None` → Optional); multi-arg rejects `default=`; bare `key=`
builtins supported are `len` (containers and classes with `__len__`), `abs`,
`int`/`float`/`bool`/`str` (other builtins still need a wrapper); `sorted(..., key=)`
and `list.sort(key=)` materialize a GC-managed auxiliary keys list of
length `n`; `sorted`/`list.sort` `reverse=` uses truthiness (bool/int/str/…);
`min`/`max` reject `reverse=` as unexpected (CPython has no such
kwarg),
control-flow narrowing covers `is None` / `is not None` (and `not`,
`and`/`or` body peels and **mid-expression** refine of
`x is not None and x > 0` / `x is None or x < 0`) **and** `isinstance`
peels (unions, Optional, and class base → subclass for field access)
on locals, cells, and module Optionals (free reads, no `global` required)
in `if`/`while` / match guards (not full SAT / open attribute narrowing);
post-loop / post-if rebinds clear stale peels; multi-member peels keep a
safe storage type for print/tags; bare-param inference is monomorphic only
(conflicting uses still need annotations; multi-type `isinstance(x, (int,
float))` and container `isinstance(x, list)` do not bare-infer); empty
lists without append/insert default to `list[Any]` (append/insert still
specialize); multi-class `isinstance` peels allow shared layout fields and
runtime exclusive-field access (AttributeError when missing); limited
`Any` only (no open setattr / bare-Any methods / full gradual typing);
`and`-chain peels compose left-to-right;
`is`/`is not` works
with `None` and same-type identity (heap pointers, scalar slots, float
bitcast — not CPython int interning); `x ** e` with a *dynamic* negative
int exponent traps (a constant like `2 ** -1` works and gives float),
int↔float comparisons convert the int to float (exactness loss past 2^53),
list literals coerce mixed numerics to one element type (mixed non-numeric
literal elements still error unless annotated as a union), `nan in [nan]`
is False (IEEE equality), str methods use ASCII case/whitespace rules,
`len`/index/slice on `str` are byte-based (`len("é")` is 2) while `ord`/`chr`
count Unicode characters, GC is
nonmoving mark–sweep with conservative native roots (so reclamation can be
delayed by pointer-like stack values), files support text modes "r"/"w"/"a"
only and still require `with` or explicit `close()` for deterministic resource
cleanup; no multi-path split namespace packages, no `from sys import *`, a package
importing itself by name, or treating modules as first-class values
beyond attribute/call chains; `os.path` is POSIX only; `*args` /
`**kwargs` on defs and `*`/`**` unpacking in calls are supported for
homogeneous list/dict types; starred assignment `a, *rest = xs` and
list displays `[*a, *b]` work for lists/tuples; `json` has no dynamic
`loads`; f-string `{x=}`, grouping (`,`/`_`), and types `n`/`c` are
unsupported; match/case includes class patterns (closed-world fields),
or-patterns bind only the matching alt; duplicate names/keys rejected;
generators
support `yield` / `yield from` on list/tuple/str/generator (including
inside `finally`; subgen `return` feeds yield-from; close cascades to
yield-from subgens), `try`/`except`/`else`/`finally` (phase and try
exit kind preserved across yield resume), `close()` (GeneratorExit +
finally; ignore-GE → RuntimeError), `send(None|value)` (value must
match yield type; non-None before first yield → TypeError; yield
expression is `Optional[Y]`), and `throw(ExcType)` /
`throw(ExcType("msg"))` (inject at yield; uncaught propagates);
`for`/`send` treat exhaustion as Optional None rather than raising
StopIteration; after close/exhaust/uncaught throw, further send is
None and does not re-enter the body; `send`/`throw` are **not**
forwarded through `yield from` (the subgenerator is only advanced with
`None` — full PEP 380 send/throw delegation is unsupported);
`except GeneratorExit`
is supported; free captures use cells (late bind; load before assign
→ NameError); nested defaults freeze at def time (escaped free-var
defaults need literals); lambda params without defaults still need
annotations or defaults for inference; homogeneous closures in
containers need matching params/ret/capture-env shape; classes are the
closed-world subset above (no multi-base / open attrs / full dynamism); GC
does not run user finalizers or implicitly close abandoned generators (see
[Garbage collection](docs/GC.md)).

Errors come with source snippets:

```
error[semantic]: type mismatch in argument 1 of 'f': expected int, found float
 --> bad.py:4:7
  |
4 | x = f(2.5)
  |       ^^^
```

## Architecture

Cargo workspace with a strict, unidirectional data flow
(see [SPECIFICATIONS.md](SPECIFICATIONS.md)):

```
source ─→ lexer ─→ parser ─→ semantic ─→ ir ─→ codegen ─→ executable
          logos    AST       typecheck   typed  LLVM IR    LLVM opt+emit,
          INDENT/  recursive + lower     tree   text       linked by cc
          DEDENT   descent
```

- **`common`** — spans and diagnostics shared by every phase
- **`lexer`** — `logos`-based scanner with an indent-stack state machine for
  Python's semantic whitespace and implicit line joining
- **`parser`** — hand-written recursive descent, precedence-layered
  expressions
- **`semantic`** — name resolution, type checking, implicit numeric
  promotion, return-path analysis; lowers AST to IR
- **`ir`** — fully typed tree; the contract handed to the backend
- **`codegen`** — emits LLVM IR text; a thin C++ shim (built via CMake)
  parses, verifies, optimizes and emits object code; a tiny C runtime
  provides Python-faithful operations, runtime traps, and nonmoving GC
- **`cli`** — the driver

## Benchmarks

`benchmarks/run.sh` compiles each program with `pyrs -O2`, checks its output
is byte-identical to `python3`'s, then reports best-of-3 wall times:

| benchmark  | workload                                   | python3 | PyRs   | speedup |
|------------|--------------------------------------------|--------:|-------:|--------:|
| fib        | recursion, 12M calls (`fib(35)`)           |  1.163s | 0.025s |   45.8× |
| listcomp   | comprehensions, 3M-element map/filter      |  0.570s | 0.033s |   17.0× |
| mandelbrot | float math, 500×500 escape iterations      |  0.944s | 0.017s |   54.8× |
| matmul     | nested lists, 250×250 matrix multiply      |  0.783s | 0.018s |   44.7× |
| nbody      | float + list, 5-body gravity, 100k steps   |  1.352s | 0.008s |  172.7× |
| primes     | int loops, trial division to 300k          |  0.629s | 0.064s |    9.8× |
| sort       | list indexing, bubble sort of 5000         |  1.008s | 0.022s |   46.6× |
| strings    | per-char iteration, 2.6M comparisons       |  0.656s | 0.103s |    6.4× |
| **total**  |                                            |  6.535s | 0.257s |   25.4× |

(Linux, LLVM 22, CPython 3.14; run `./benchmarks/run.sh` to reproduce.)

v0.3 inlined list element access into the generated IR (bounds check +
direct load/store, so LLVM keeps hot values in registers) and interned
single-character strings (indexing/iterating a str allocates nothing) —
that took sort from 13× to 45×, nbody from 59× to 164×, and strings
from 3× to 6×.

## Building

Requires Rust (edition 2024), LLVM (`llvm-config` on PATH), CMake, and a C
compiler.

```console
cargo build --release
cargo test
```

### Continuous integration

GitHub Actions (see `.github/workflows/`):

| Workflow | When | What |
|----------|------|------|
| **CI** | push/PR to `main` | `fmt`, clippy, tests, example parity, opt-level smoke |
| **Benchmarks** | weekly / manual / bench-related pushes | `benchmarks/run.sh` (artifact log) |
| **Release** | tags `v*.*.*` | Linux `x86_64` tarball + checksum + GitHub Release |
| **Docs & hygiene** | docs/CI path changes | required files + workflow YAML shape |

Local gate (same spirit as CI): `make doctor && make ci`.
Release tags: `git tag v0.59.0 && git push origin v0.59.0`.
