# Dynamic values

What happens when the compiler does not know a type, how a value carries its
type at run time, and the three mechanisms that make dynamic code fast instead
of merely correct.

Read [INFERENCE.md](INFERENCE.md) first for how a value comes to be dynamic in
the first place.

## The rule

**No program is refused for being too dynamic.** A value whose type the
compiler cannot determine still compiles, still runs, and behaves the way
CPython behaves, including raising CPython's own errors:

```python
def show(x):
    return x + x

print(show(2), show("ab"), show(1.5))    # 4 abab 3.0
```

An annotation makes code faster. It does not make code possible.

Every operation that a static rule cannot accept dispatches at run time
instead: arithmetic and comparison operators, `str()` and `repr()`, method
calls, indexing and slicing, `len`, `in`, iteration, printing, the `%` string
formatting operator, and the conversions. When the value turns out not to
support the operation, the error is CPython's, with CPython's message:
`None + 1` raises a `TypeError` reading `unsupported operand type(s) for +:
'NoneType' and 'int'`.

## Representation: a pair of registers, not an allocation

A dynamic value is `{ i32, i64 }`: a tag and a payload, living in two
registers.

The tag says what the payload means. The payload is the value itself for
anything that fits in 64 bits (a tagged small integer, a float bit cast, a
boolean) and a pointer for anything that does not (a string, a list, a class
instance).

The important property is what is absent. There is no heap box. Passing a
dynamic value to a function passes two registers; returning one returns two
registers; storing one in a local costs two stack slots. An earlier design
allocated a `PyrsUnionBox` with the same two fields, and replacing the
allocation with the pair is the single largest reason dynamic code is
competitive at all.

The same tag space serves unions, so `int | str` and a fully dynamic value are
the same shape at run time and the same code path handles both. A container
whose element type is statically known carries no per element tag, because the
tag is supplied once per operation from the static element type. Only a
`list[Any]` pays 16 bytes per element.

For the garbage collector, a pair in registers is an ordinary conservative
root, and the payload word is candidate checked like any other erased slot.
See [GC.md](GC.md).

## Kernels

Each operation has one runtime function that takes tagged values and does what
CPython does: `pyrs_dyn_add`, `pyrs_dyn_lt`, `pyrs_dyn_getitem`,
`pyrs_dyn_method`, `pyrs_dyn_len`, and so on. They take pairs directly rather
than boxes, so calling one costs a call and no allocation.

Three of them are worth singling out because the naive implementation is a
correctness trap rather than just a slow path.

**`sorted` and `list.sort`.** A comparison sort over dynamic values must do
more than produce sorted output. CPython's `TypeError` for an unorderable pair
names the two types in the order its own algorithm happens to meet them, so
reproducing the message requires reproducing the algorithm's structure. The
kernel follows CPython's timsort rather than merely a stable merge sort. A
whole program that builds and sorts a million dynamic values runs in 0.20s
against CPython's 0.14s. The implementation this replaced was quadratic, and
was 23 times slower than CPython at 16,000 elements.

**`key=` sorting.** A `key=` callable is applied once per element, then the
keys are sorted. Recomputing the key inside the comparator turns an O(n log n)
sort into O(n log n) key evaluations, which for an expensive key is the whole
cost of the operation.

**`%` formatting on a dynamic string.** A statically typed `str` needs a
literal format string, because its conversions are resolved at compile time. A
dynamic one is formatted at run time instead, with mapping keys, `*` width and
precision, `%c` and `%a` all implemented, and the error for a malformed format
is CPython's:

```python
def pick(n):
    return "%(k)s" if n > 0 else 5

print(pick(1) % pick(-1))    # TypeError: format requires a mapping
```

Reporting that as an unsupported feature would be a lie about what the compiler
can do: CPython rejects it too, with exactly this message.

## Making it fast

Correctness gets you a kernel call per operation. Three mechanisms remove most
of those calls.

### 1. Profiles

An ahead of time compiler has no feedback channel: it cannot observe what
actually ran. `pyrs profile` supplies one.

```console
$ pyrs profile -i prog.py -o prog.prof
$ pyrs compile --profile prog.prof -i prog.py -o prog
```

The instrumented binary records, for every polymorphic site, up to four
concrete type tags and a hit count. The artifact is text, and its header pins
the compiler version and a digest of the source:

```text
pyrs-profile 1
compiler 0.153.0
source da5580b8e8452c0cfa268751aae4165299d26a9c2cad0e3ca43065536c88403f
site 0
  0 5
```

A missing, unreadable, stale or mismatched profile is ignored with a warning
and the build degrades to the unprofiled emit. It never degrades to a wrong
one. `--profile` changes only the code emitted, never observable behaviour.

Four tags rather than one because four types cover 96.1% of executions across
the measured workloads, where a single type covers 79.5%.

### 2. Guarded specialization

A site the profile saw with exactly one type compiles to a guard around the
typed operation:

```text
if (tag == expected) { the typed operation, inlined }
else                 { the pyrs_dyn_* kernel }
```

There is no deoptimization and no JIT. The slow path was compiled ahead of
time, beside the fast one. A site the profile does not describe calls the
kernel directly, so code size grows only where a profile justifies it.

The dangerous case for any such scheme is a profile that is wrong for the run.
That case was tested directly: a function profiled on integer arguments,
compiled with that profile, then executed with string arguments. The guard
misses, the kernel takes over, and the output matches CPython, including under
`PYRS_GC_STRESS=1`.

At 50 million iterations of `total = total + step` with both operands dynamic,
best of three:

| Path | Time |
|---|---:|
| `total: int`, statically typed | 0.035s |
| dynamic, no profile | 0.235s |
| dynamic, profiled | 0.042s |

The profiled dynamic loop comes within 20% of the statically typed one, and is
5.6 times faster than the unprofiled kernel.

### 3. Polymorphic caches

A site with more than one type is not monomorphic, and a single guard would
miss most of the time. Such a site caches up to four typed operand pairs, which
is what closes the gap between the 79.5% of executions one type covers and the
96.1% four types cover.

A cache trained on numbers and then run on strings produces CPython's output,
which is the property that makes the mechanism a performance feature rather
than a correctness risk.

## Objects that grow

Class instances are closed world by default: the field layout is fixed at
compile time and attribute access is an offset. A program that uses `getattr`
or `setattr` on an instance causes that class to grow an overflow dictionary,
and only that class, and only in the program that needs it. Programs that do
not use dynamic attributes pay nothing for the ones that do.

`type(x)` produces a first class type value that supports exact identity
checks, so `type(x) is C` works.

## What this costs, and when to annotate

The honest summary is a hierarchy:

1. A statically typed operation is one or two machine instructions.
2. A profiled monomorphic dynamic operation is the same, behind one tag
   compare.
3. A polymorphic site with four or fewer types is a short chain of compares.
4. An unprofiled dynamic operation is a call into a kernel, roughly 7 times
   slower than the first case on integer arithmetic, and the call also blocks
   loop invariant hoisting for the code around it. The multiple is larger for
   operations whose kernel does more, such as a method lookup on a string.

Annotate the hot path. Leave the rest alone. `pyrs check --show-types` shows
which functions are dynamic today, and `pyrs profile` turns the worst of them
into case 2 without any source change at all.
