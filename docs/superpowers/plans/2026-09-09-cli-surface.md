# What a command-line program needs

**Status: implemented in 0.141.** `examples/cli.py`, plus the language and OS
work below.

## Method

The same one as `json`: write the library, and let the compiler name the gaps.
An argument parser and a subcommand program were written first, in ordinary
Python, and then compiled. Everything here is a gap that work hit — nothing was
added because it seemed likely to be wanted.

That matters for what is *not* here. `argparse` itself is not shipped: adding a
module is stdlib **growth**, which `docs/PRIMITIVES.md` §9 still freezes. The
parser lives in `examples/`, where `make examples` holds it byte-identical to
CPython, and it exists to prove the language can host one.

## The gaps were not about arguments

Four blockers, and not one of them concerns parsing.

### A dispatch table at module scope was invisible inside functions

```python
COMMANDS = {"build": cmd_build, "test": cmd_test}

def main(argv: list[str]) -> int:
    handler = COMMANDS[argv[1]]      # name 'COMMANDS' is not defined
    return handler(argv[2:])
```

`seed_globals_from_script` runs in pass 0, before any expression has been
typed, so it works from `seed_ty_from_expr` — a syntactic guess at a global's
type. It handled literals and containers of literals. A `Name` was not handled
at all, so `{"build": cmd_build}` produced no type, the global went unseeded,
and the name did not exist inside any function body. At *module* scope it
worked, which is why the table looked fine until it was read from `main`.

The lowering was already there: a module-level function in value position
becomes `closure_of_full(params, ret, &[], ir_name)`. Only the seeder could not
reach it, because it is a free function with no access to the signature table.
It now consults a thread-local set per module — the same shape `seed_ty_from_expr`
already used for `lookup_class`, which is how `Point(1, 2)` seeds a class type.

Functions that cannot be closure values — `*args`, `**kwargs`, defaults,
generators — are left out of that map, so those globals stay unseeded exactly
as before. Skipping is the safe direction: the name is absent rather than
wrong.

### `Callable[[A, B], R]` did not parse

Which meant a handler could not be annotated anywhere it needed to live: a dict
value, a field, a parameter. It resolves to `closure_of(params, ret)` — a
closure with **no captures and no function name**.

Both halves of that matter. `CallClosure` already handles an empty `func` by
loading the code pointer from the object, so an indirect call works. And
captures are passed as *leading arguments* read out of the closure object by
static type, so a caller must know them — an annotation cannot. Empty captures
is therefore not a limitation to apologise for but the correct meaning: it
describes precisely the capture-free values, which is what a module function
and a non-capturing lambda are. `coerce`'s existing rule — closures with
matching params, return and captures retype, ignoring the name — already
accepted them.

`Callable[..., R]` is refused with its own message rather than a bracket error:
the parameter types are what a call site needs in order to pass them.

### `self.handler(x)` never looked for a field

Python has one attribute namespace, so a field holding a function is called the
way a method is. Here methods and fields are separate, and the method lookup
failed without consulting fields. It now tries a function-valued field before
reporting a missing method — and when the field exists but is not callable, the
message names it and its type instead of a bare "has no method 'x'", which
sends the reader looking for a missing `def`.

### `str()` of a dynamic value was refused

The reason a parser needs it: an option table holds strings, flags and `None`
together, so its values are `object`, and printing them is most of what the
program does.

`pyrs_print_any` already rendered a dynamic value with `str` semantics — bare
at the top level, quoted inside a container. `str()` is that under a capture,
which is how `repr` of a container already worked. So both are three lines of
runtime, and nested containers agree with CPython for free rather than by a
second implementation. `repr` differs only in quoting a top-level string.
`ascii()` is still refused: it needs the escape flag pushed through the box,
which the shared printer does not carry.

## The OS surface

### The standard streams are file objects

`sys.stdin` / `sys.stdout` / `sys.stderr` were rejected with "there is no file
object behind them". There is now — and it is the `File` type that already
existed, so `read`, `readline`, `readlines`, `write`, iteration and `with` all
apply without a parallel set of stream builtins. `flush()` is new and works on
any file.

They are singletons held in a rooted static, so `sys.stdout is sys.stdout` and
repeated access allocates nothing. Two things had to be handled because a
`FILE*` the process does not own is now reachable from PyRs:

- the collector's finalizer closes a `PyrsFile`; it now skips a standard one;
- `close()` on one raises rather than silently breaking every later print.

### `print(..., file=f)` reaches any file

Every print funnels through one writer, and the destination was a thread-local
bool. It is now a bool *or* a `FILE*`, set for the duration of one statement
and reset after — so a `str()` of a value inside the print still captures
rather than escaping to the file. The two standard streams stay a flag, since
they need no file object.

### Environment and paths

`os.environ` is a `dict[str, str]` snapshot; `getenv` is ordinary PyRs over it.
`os.path` gained `exists`, `isfile`, `isdir`, `splitext`, `isabs`, `normpath`,
`abspath` and `expanduser` — all pure PyRs over one primitive answering what is
at a path, plus `getcwd` and the environment, which `os.path` reaches through
its own lowered stubs because it cannot import `os`, which imports it.

They match `posixpath` on the parts that are easy to get wrong: a leading dot
is not an extension (`.bashrc` → `('.bashrc', '')`), `//` at the front is
preserved, and `..` above the root is dropped when rooted and kept when not.

## Two deliberate differences

**`sys.stdout.close()` raises.** CPython allows it. Here it would break every
later print with no way back, and a compiled program has no reason to want it.

**`os.environ` is a snapshot.** Assigning into it changes the dict and not the
process environment. There is no subprocess surface for the difference to
reach; when there is one, this is the decision to revisit.

## One mistake worth recording

A new test flaked roughly one run in three, and the first diagnosis — a race in
the shared build cache — was wrong. It reproduced only under parallel test
threads, which made a cache race look right, and `--no-cache` did not fix it,
which should have been the clue. The actual cause was in the new test: it
reused a `workspace(tag)` name an existing test already owned, so two tests
shared one directory and raced on `prog.py`.

The lesson is about the harness, not the flake: `workspace(tag)` keys on the
tag and the process id, so a duplicated tag silently aliases rather than
failing, and the symptom appears in whichever test loses. Worth remembering
before blaming infrastructure that a test file's tags are a namespace.

## How this is checked

- `examples/cli.py` — a parser with long and short flags, inline
  `--opt=value`, clustered short flags, values taken from the next token,
  `--`, positionals, required options and generated help, driven over written
  argument vectors so the output is deterministic; plus a subcommand program
  on a `Callable`-typed dispatch table. `make examples` holds it byte-identical
  to CPython at every optimization level.
- `cli/tests/cli_surface.rs` — 10 tests over the same surface, differential at
  -O0/-O2/-O3 and under `PYRS_GC_STRESS=1`.
- `cli/tests/module_and_streams.rs` gained the print-to-a-file case and lost
  the one asserting that it was refused.

## Noticed while writing it

- `input()` already works, and so do `str.partition`, `removeprefix`,
  `ljust`/`rjust`/`center`/`zfill`, `dict.setdefault`, `sorted(d.items())`,
  `enumerate`/`zip` and nested f-string widths (`f"{name:<{w}}"`). The
  parser needed no work in any of them.
- `os.get_terminal_size()` is still absent, so help text cannot wrap to the
  terminal. It needs `ioctl`, and nothing else here wanted it.
- `dict` keys may only be `int`, `str` or a tuple of those, which is why a
  parser keyed by option object is not expressible.
