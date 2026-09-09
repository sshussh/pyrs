#!/usr/bin/env python3
"""Measure the language surface PyRs supports, against a CPython oracle.

Each probe is a self-contained Python program exercising one construct. Both
engines run it; the classification is:

    ok        identical stdout, stderr and exit status
    reject    PyRs refused to compile it (the honest-error path)
    diverge   PyRs compiled and ran, but disagreed with CPython
    oracle    CPython itself rejected the probe (bad probe, or newer syntax)

A `reject` is not a failure of this script — most of them are the documented
subset boundary. A `diverge` is, unless it is one of the divergences the
README already records: it means something compiled and then lied, which the
project's byte-parity rule forbids. `--fail-on-diverge` fails on an
*unrecorded* one, and also on a recorded one that started passing, so the
list below cannot quietly go stale.

The point of keeping this in-tree is that "30 of 35 keywords" stays a
measurement rather than a claim. Run it after each milestone that widens the
surface.

    python3 scripts/coverage_probe.py --pyrs target/release/pyrs
    python3 scripts/coverage_probe.py --category keyword --verbose
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

PROBES: list[tuple[str, str, str]] = []

# Divergences documented in README.md ("Known behavioural divergences") and
# docs/GUIDE.md section 9. A probe named here is allowed to disagree with
# CPython; one that is *not* named fails the run, and one that is named but
# starts agreeing fails too, so the record cannot go stale.
KNOWN_DIVERGENCES: dict[str, str] = {
    # One cause: PyRs prints the exception line and exits 1, with no
    # `Traceback (most recent call last):` block or frame list.
    "uncaught traceback": "no traceback block on stderr",
    "ZeroDivisionError": "no traceback block on stderr",
    "IndexError": "no traceback block on stderr",
    "KeyError": "no traceback block on stderr",
    # Tuples here are fixed-arity; `args` holds 0 or 1 elements decided at
    # run time. Length and contents match CPython.
    "exception .args": "e.args is a list, not a tuple",
    # A nested def and a lambda freeze a non-literal default once, as CPython
    # does; a module-level def re-evaluates it. Also pinned as a `mismatch`
    # in compatibility/cases/mutable_defaults.py.
    "mutable default (module level)": "module-level defaults are not frozen",
}


def p(category: str, name: str, source: str) -> None:
    PROBES.append((category, name, source.strip() + "\n"))


# --------------------------------------------------------------- keywords
#
# Python has 35 reserved keywords and 4 soft keywords. Every one is exercised
# in a program that runs, so "supported" means it produced CPython's output,
# not merely that it parsed.

p("keyword", "False/None/True", "print(True, False, None)")
p("keyword", "and/or/not", "a = 1\nb = 0\nprint(a and b, a or b, not a)")
p("keyword", "as (import)", "import math as m\nprint(m.floor(2.5))")
p(
    "keyword",
    "as (except)",
    "try:\n    raise ValueError('x')\nexcept ValueError as e:\n    print(e)",
)
p(
    "keyword",
    "as (with)",
    "with open('/etc/hostname') as f:\n    print(len(f.read()) >= 0)",
)
p(
    "keyword",
    "assert",
    "try:\n    assert 1 == 2, 'nope'\nexcept AssertionError as e:\n    print('caught', e)",
)
p(
    "keyword",
    "async def",
    "import asyncio\nasync def f() -> int:\n    return 1\nprint(asyncio.run(f()))",
)
p(
    "keyword",
    "await",
    "import asyncio\nasync def g() -> int:\n    return 2\n"
    "async def f() -> int:\n    return await g()\nprint(asyncio.run(f()))",
)
p("keyword", "break", "for i in range(10):\n    if i == 3:\n        break\nprint(i)")
p(
    "keyword",
    "class",
    "class C:\n    def __init__(self) -> None:\n        self.x: int = 1\nprint(C().x)",
)
p(
    "keyword",
    "continue",
    "t = 0\nfor i in range(5):\n    if i % 2:\n        continue\n    t += i\nprint(t)",
)
p("keyword", "def", "def f(x: int) -> int:\n    return x + 1\nprint(f(1))")
p("keyword", "del (index)", "xs = [1, 2, 3]\ndel xs[1]\nprint(xs)")
p("keyword", "del (slice)", "xs = [1, 2, 3, 4]\ndel xs[1:3]\nprint(xs)")
p("keyword", "del (dict key)", "d = {'a': 1, 'b': 2}\ndel d['a']\nprint(d)")
p(
    "keyword",
    "del (name)",
    "def f() -> None:\n    x: int = 1\n    del x\nf()\nprint('ok')",
)
p(
    "keyword",
    "del (attr)",
    "class C:\n    def __init__(self) -> None:\n        self.x: int = 1\n"
    "c = C()\ndel c.x\nprint('ok')",
)
p("keyword", "elif/else", "x = 2\nif x == 1:\n    print('a')\nelif x == 2:\n    print('b')\nelse:\n    print('c')")
p("keyword", "except (bare)", "try:\n    raise ValueError('x')\nexcept:\n    print('any')")
p(
    "keyword",
    "except (tuple)",
    "try:\n    raise KeyError('k')\nexcept (ValueError, KeyError):\n    print('either')",
)
p(
    "keyword",
    "except*",
    "try:\n    raise ExceptionGroup('g', [ValueError('v')])\nexcept* ValueError:\n    print('group')",
)
p("keyword", "finally", "try:\n    print('body')\nfinally:\n    print('fin')")
p("keyword", "for/else", "for i in range(3):\n    pass\nelse:\n    print('else ran')")
p("keyword", "from import", "from math import sqrt\nprint(sqrt(9.0))")
p("keyword", "from import *", "from math import *\nprint(floor(2.5))")
p(
    "keyword",
    "global",
    "g: int = 0\ndef f() -> None:\n    global g\n    g = 5\nf()\nprint(g)",
)
p("keyword", "if/else (expr)", "print(1 if True else 2)")
p("keyword", "import", "import math\nprint(math.pi > 3)")
p("keyword", "in / not in", "print(1 in [1, 2], 3 not in [1, 2])")
p("keyword", "is / is not", "x = None\nprint(x is None, x is not None)")
p("keyword", "lambda", "f = lambda x: x * 2\nprint(f(3))")
p(
    "keyword",
    "match/case",
    "x = 2\nmatch x:\n    case 1:\n        print('one')\n    case 2:\n        print('two')\n"
    "    case _:\n        print('other')",
)
p(
    "keyword",
    "nonlocal",
    "def outer() -> int:\n    n: int = 0\n    def inner() -> None:\n        nonlocal n\n"
    "        n += 1\n    inner()\n    inner()\n    return n\nprint(outer())",
)
p("keyword", "pass", "def f() -> None:\n    pass\nf()\nprint('ok')")
p(
    "keyword",
    "raise",
    "try:\n    raise RuntimeError('boom')\nexcept RuntimeError as e:\n    print(e)",
)
p(
    "keyword",
    "raise from",
    "try:\n    try:\n        raise ValueError('a')\n    except ValueError as e:\n"
    "        raise RuntimeError('b') from e\nexcept RuntimeError as e:\n    print(e)",
)
p(
    "keyword",
    "raise (bare re-raise)",
    "try:\n    try:\n        raise ValueError('v')\n    except ValueError:\n        raise\n"
    "except ValueError as e:\n    print('re', e)",
)
p("keyword", "return", "def f() -> int:\n    return 42\nprint(f())")
p(
    "keyword",
    "try/except/else",
    "try:\n    x = 1\nexcept ValueError:\n    print('no')\nelse:\n    print('else')",
)
p("keyword", "while/else", "i = 0\nwhile i < 3:\n    i += 1\nelse:\n    print('while-else', i)")
p("keyword", "with (single)", "with open('/etc/hostname') as f:\n    print(len(f.readline()) >= 0)")
p(
    "keyword",
    "with (multiple items)",
    "with open('/etc/hostname') as a, open('/etc/hostname') as b:\n"
    "    print(a.readline() == b.readline())",
)
p("keyword", "yield", "def g():\n    yield 1\n    yield 2\nprint(list(g()))")
p(
    "keyword",
    "yield from",
    "def inner():\n    yield 1\ndef outer():\n    yield from inner()\n    yield 2\nprint(list(outer()))",
)
p("keyword", "type (soft kw, PEP 695)", "type Alias = int\nx: Alias = 3\nprint(x)")
p("keyword", "match as identifier", "match = 5\ncase = 6\nprint(match, case)")

# ------------------------------------------------------------- expressions

p("expression", "int literal forms", "print(0x1f, 0o17, 0b1011, 1_000_000)")
p("expression", "big int", "print(2 ** 200)")
p("expression", "float literal forms", "print(1.5, 1e3, .5, 1_000.5)")
p("expression", "float underscore after dot", "print(1.5_0)")
p("expression", "complex literal", "print((1 + 2j).real)")
p("expression", "adjacent string concat", "s = 'a' 'b'\nprint(s)")
p("expression", "raw string", "print(r'a\\nb')")
p("expression", "uppercase F-string", "x = 1\nprint(F'{x}')")
p("expression", "bytes literal", "print(b'abc')")
p("expression", "triple-quoted", 'print("""a\nb""")')
p("expression", "f-string", "x = 5\nprint(f'{x} {x * 2}')")
p("expression", "f-string format spec", "x = 3.14159\nprint(f'{x:.2f} {x:>10.1f}')")
p("expression", "f-string conversion", "s = 'a'\nprint(f'{s!r}')")
p("expression", "f-string nested field", "w = 8\nx = 3.5\nprint(f'{x:{w}.2f}|')")
p("expression", "f-string = debug", "x = 5\nprint(f'{x=}')")
p("expression", "arithmetic", "print(7 + 2, 7 - 2, 7 * 2, 7 / 2, 7 // 2, 7 % 2, 7 ** 2, -7 // 2, -7 % 2)")
p("expression", "bitwise", "print(6 & 3, 6 | 3, 6 ^ 3, ~6, 6 << 2, 6 >> 1)")
p(
    "expression",
    "matmul @",
    "class M:\n    def __matmul__(self, o: 'M') -> int:\n        return 1\nprint(M() @ M())",
)
p("expression", "comparison chain", "print(1 < 2 < 3, 1 < 3 < 2)")
p("expression", "walrus", "xs = [1, 2, 3]\nif (n := len(xs)) > 2:\n    print(n)")
p("expression", "conditional expr", "print('big' if 5 > 3 else 'small')")
p(
    "expression",
    "conditional expr narrows",
    "def f(x: int | None) -> int:\n    return 0 if x is None else x\nprint(f(None), f(7))",
)
p("expression", "list comp", "print([x * x for x in range(5) if x % 2 == 0])")
p("expression", "nested list comp", "print([[y for y in range(x)] for x in range(3)])")
p("expression", "double for comp", "print([(x, y) for x in range(2) for y in range(2)])")
p("expression", "set comp", "print(sorted({x % 3 for x in range(10)}))")
p("expression", "dict comp", "print({x: x * x for x in range(3)})")
p("expression", "generator expr", "print(sum(x for x in range(5)))")
p("expression", "container literals", "print([1], (1,), {1: 2}, {1, 2})")
p(
    "expression",
    "empty containers (annotated)",
    "xs: list[int] = []\nd: dict[str, int] = {}\ns: set[int] = set()\nprint(xs, d, s, ())",
)
p("expression", "star unpack in list", "a = [1, 2]\nprint([0, *a, 3])")
p(
    "expression",
    "star unpack in call",
    "def f(a: int, b: int) -> int:\n    return a + b\nargs = [1, 2]\nprint(f(*args))",
)
p("expression", "dict ** unpack literal", "d = {'a': 1}\nprint({**d, 'b': 2})")
p(
    "expression",
    "dict ** unpack call",
    "def f(a: int, b: int) -> int:\n    return a + b\nd = {'a': 1, 'b': 2}\nprint(f(**d))",
)
p("expression", "starred assignment", "a, *rest = [1, 2, 3]\nprint(a, rest)")
p("expression", "tuple swap", "a, b = 1, 2\na, b = b, a\nprint(a, b)")
p("expression", "nested unpack", "(a, (b, c)) = (1, (2, 3))\nprint(a, b, c)")
p("expression", "slice", "xs = [0, 1, 2, 3, 4]\nprint(xs[1:3], xs[::2], xs[::-1], xs[-2:])")
p("expression", "slice assignment", "xs = [0, 1, 2, 3]\nxs[1:3] = [9]\nprint(xs)")
p("expression", "str slice/index", "s = 'hello'\nprint(s[0], s[1:3], s[::-1])")
p("expression", "chained method call", "print('  a,b '.strip().split(','))")
p(
    "expression",
    "keyword args",
    "def f(a: int, b: int = 2) -> int:\n    return a * 10 + b\nprint(f(1, b=3))",
)
p(
    "expression",
    "keyword-only args",
    "def f(a: int, *, b: int) -> int:\n    return a + b\nprint(f(1, b=2))",
)
p(
    "expression",
    "positional-only args",
    "def f(a: int, /, b: int) -> int:\n    return a + b\nprint(f(1, 2))",
)
p("expression", "*args", "def f(*args: int) -> int:\n    return sum(args)\nprint(f(1, 2, 3))")
p(
    "expression",
    "**kwargs",
    "def f(**kw: int) -> int:\n    return sum(kw.values())\nprint(f(a=1, b=2))",
)
p("expression", "default arg", "def f(a: int = 5) -> int:\n    return a\nprint(f())")
p(
    "expression",
    "closure",
    "def make(n: int):\n    def inner(x: int) -> int:\n        return x + n\n    return inner\nprint(make(3)(4))",
)
p(
    "expression",
    "function as value",
    "def double(x: int) -> int:\n    return x * 2\n"
    "def apply(f, x: int) -> int:\n    return f(x)\nprint(apply(double, 1))",
)
p(
    "expression",
    "aug assign all ops",
    "x = 10\nx += 1\nx -= 2\nx *= 3\nx //= 2\nx %= 100\nx **= 2\nx &= 255\nx |= 1\nx ^= 2\n"
    "x <<= 1\nx >>= 1\nprint(x)",
)
p("expression", "multiple assignment", "a = b = c = 3\nprint(a, b, c)")
p("expression", "ellipsis", "def f() -> None:\n    ...\nf()\nprint('ok')")
p("expression", "match sequence pattern", "xs = [1, 2, 3]\nmatch xs:\n    case [a, *rest]:\n        print(a, rest)")
p(
    "expression",
    "match class pattern",
    "class P:\n    def __init__(self, x: int, y: int) -> None:\n        self.x: int = x\n"
    "        self.y: int = y\nmatch P(1, 2):\n    case P(x=1, y=b):\n        print('hit', b)",
)
p("expression", "match mapping pattern", "d: dict[str, int] = {'a': 1}\nmatch d:\n    case {'a': v}:\n        print(v)")
p("expression", "match guard", "x = 5\nmatch x:\n    case n if n > 3:\n        print('big', n)")

# ----------------------------------------------------------------- classes

p(
    "class",
    "single inheritance",
    "class A:\n    def f(self) -> int:\n        return 1\nclass B(A):\n    pass\nprint(B().f())",
)
p(
    "class",
    "virtual dispatch",
    "class A:\n    def f(self) -> int:\n        return 1\nclass B(A):\n    def f(self) -> int:\n        return 2\n"
    "def g(a: A) -> int:\n    return a.f()\nprint(g(A()), g(B()))",
)
p(
    "class",
    "super()",
    "class A:\n    def f(self) -> int:\n        return 1\n"
    "class B(A):\n    def f(self) -> int:\n        return super().f() + 1\nprint(B().f())",
)
p(
    "class",
    "multiple inheritance",
    "class A:\n    def f(self) -> int:\n        return 1\nclass B:\n    def g(self) -> int:\n        return 2\n"
    "class C(A, B):\n    pass\nprint(C().f(), C().g())",
)
p("class", "__str__", "class C:\n    def __str__(self) -> str:\n        return 'S'\nprint(str(C()))")
p("class", "__repr__", "class C:\n    def __repr__(self) -> str:\n        return 'R'\nprint(C())")
p(
    "class",
    "__eq__",
    "class C:\n    def __init__(self, v: int) -> None:\n        self.v: int = v\n"
    "    def __eq__(self, o: 'C') -> bool:\n        return self.v == o.v\nprint(C(1) == C(1), C(1) == C(2))",
)
p(
    "class",
    "__lt__ / sort",
    "class C:\n    def __init__(self, v: int) -> None:\n        self.v: int = v\n"
    "    def __lt__(self, o: 'C') -> bool:\n        return self.v < o.v\n"
    "xs = [C(2), C(1)]\nxs.sort()\nprint([c.v for c in xs])",
)
p(
    "class",
    "__len__/__getitem__",
    "class C:\n    def __len__(self) -> int:\n        return 3\n"
    "    def __getitem__(self, i: int) -> int:\n        return i * 2\nc = C()\nprint(len(c), c[2])",
)
p(
    "class",
    "__bool__",
    "class C:\n    def __bool__(self) -> bool:\n        return False\n"
    "c = C()\nprint(bool(c), not c)\nif c:\n    print('t')\nelse:\n    print('f')",
)
p(
    "class",
    "__contains__",
    "class C:\n    def __contains__(self, v: int) -> bool:\n        return v == 1\nprint(1 in C())",
)
p(
    "class",
    "__iter__/__next__",
    "class R:\n    def __init__(self) -> None:\n        self.i: int = 0\n"
    "    def __iter__(self) -> 'R':\n        return self\n    def __next__(self) -> int:\n"
    "        self.i += 1\n        if self.i > 3:\n            raise StopIteration\n"
    "        return self.i\nprint([x for x in R()])",
)
p(
    "class",
    "__enter__/__exit__",
    "from typing import Any\nclass Ctx:\n    def __enter__(self) -> int:\n        return 5\n"
    "    def __exit__(self, a: Any = None, b: Any = None, c: Any = None) -> None:\n"
    "        print('exit')\nwith Ctx() as v:\n    print(v)",
)
p(
    "class",
    "__add__ (arithmetic dunder)",
    "class V:\n    def __init__(self, x: int) -> None:\n        self.x: int = x\n"
    "    def __add__(self, o: 'V') -> 'V':\n        return V(self.x + o.x)\nprint((V(1) + V(2)).x)",
)
p(
    "class",
    "__call__",
    "class F:\n    def __call__(self, x: int) -> int:\n        return x + 1\nprint(F()(1))",
)
p(
    "class",
    "__hash__ (as dict key)",
    "class C:\n    def __init__(self) -> None:\n        self.x: int = 1\n"
    "    def __hash__(self) -> int:\n        return 1\nd = {C(): 1}\nprint(len(d))",
)
p("class", "@staticmethod", "class C:\n    @staticmethod\n    def f(x: int) -> int:\n        return x * 2\nprint(C.f(3))")
p("class", "@classmethod", "class C:\n    @classmethod\n    def f(cls) -> str:\n        return 'cm'\nprint(C.f())")
p(
    "class",
    "@property",
    "class C:\n    def __init__(self) -> None:\n        self._x: int = 4\n"
    "    @property\n    def x(self) -> int:\n        return self._x\nprint(C().x)",
)
p("class", "class attribute", "class C:\n    N: int = 7\nprint(C.N)")
p(
    "class",
    "@dataclass",
    "from dataclasses import dataclass\n@dataclass\nclass P:\n    x: int\n    y: int\nprint(P(1, 2))",
)
p(
    "class",
    "__slots__",
    "class C:\n    __slots__ = ('x',)\n    def __init__(self) -> None:\n        self.x: int = 1\nprint(C().x)",
)
p(
    "class",
    "function decorator",
    "def deco(f):\n    def wrap(x: int) -> int:\n        return f(x) + 1\n    return wrap\n"
    "@deco\ndef f(x: int) -> int:\n    return x\nprint(f(1))",
)
p(
    "class",
    "user exception hierarchy",
    "class Base(Exception):\n    pass\nclass Sub(Base):\n    pass\n"
    "try:\n    raise Sub('s')\nexcept Base as e:\n    print('caught', e)",
)
p("class", "isinstance", "class A:\n    pass\nprint(isinstance(A(), A), isinstance(1, int))")
p("class", "type(x)", "print(type(1).__name__)")

# ---------------------------------------------------------------- builtins

p("builtin", "len/abs/min/max/sum", "print(len([1, 2]), abs(-3), min(1, 2), max([1, 2]), sum([1, 2]))")
p("builtin", "sorted", "xs = [3, 1, 2]\nprint(sorted(xs), sorted(xs, reverse=True), sorted(['bb', 'a'], key=len))")
p("builtin", "enumerate/zip/reversed", "print(list(enumerate('ab')), list(zip([1, 2], 'ab')), list(reversed([1, 2])))")
p("builtin", "map/filter", "print(list(map(lambda x: x * 2, [1, 2])), list(filter(lambda x: x > 1, [1, 2])))")
p("builtin", "any/all", "print(any([False, True]), all([True, True]))")
p("builtin", "range 3-arg", "print(list(range(10, 0, -3)))")
p("builtin", "round/pow/divmod", "print(round(2.567, 2), pow(2, 10), divmod(7, 2))")
p("builtin", "casts", "print(int('42'), float('1.5'), str(3), bool(0))")
p("builtin", "int(x, base)", "print(int('ff', 16))")
p("builtin", "ord/chr/hex/oct/bin", "print(ord('a'), chr(98), hex(255), oct(8), bin(5))")
p("builtin", "repr", "print(repr('a\\n'), repr([1, 2]))")
p("builtin", "print sep/end/file", "import sys\nprint(1, 2, sep='-', end='!\\n', file=sys.stdout)")
p(
    "builtin",
    "open/read/write",
    "import os\np = '_pyrs_probe.txt'\nwith open(p, 'w') as f:\n    f.write('hi')\n"
    "with open(p) as f:\n    print(f.read())",
)
p("builtin", "file iteration", "with open('/etc/hostname') as f:\n    for line in f:\n        print(len(line) >= 0)\n        break")
p("builtin", "eval/exec", "print(eval('1+1'))")
p("builtin", "globals/locals", "x = 1\nprint('x' in globals())")
p("builtin", "id/hash", "print(hash('a') == hash('a'))")
p("builtin", "iter/next", "it = iter([1, 2])\nprint(next(it), next(it))")
p("builtin", "frozenset", "print(sorted(frozenset([1, 2, 2])))")
p("builtin", "bytes/bytearray", "b = bytes([65, 66])\nprint(b)")
p("builtin", "complex()", "print(complex(1, 2))")
p("builtin", "slice()", "print([1, 2, 3][slice(0, 2)])")
p("builtin", "format()", "print(format(3.14159, '.2f'))")
p("builtin", "callable", "print(callable(print))")
p("builtin", "getattr/setattr/hasattr", "class C:\n    def __init__(self) -> None:\n        self.x: int = 1\nprint(getattr(C(), 'x'))")

# -------------------------------------------------------------- containers

p(
    "container",
    "list methods",
    "xs = [3, 1]\nxs.append(2)\nxs.extend([4])\nxs.insert(0, 0)\nxs.remove(3)\nxs.sort()\nxs.reverse()\n"
    "print(xs, xs.index(1), xs.count(1), xs.pop())",
)
p("container", "list copy/clear", "xs = [1, 2]\ny = xs.copy()\nxs.clear()\nprint(xs, y)")
p(
    "container",
    "dict methods",
    "d = {'a': 1}\nd.update({'b': 2})\n"
    "print(d.get('c', 0), sorted(d.keys()), sorted(d.values()), sorted(d.items()), d.pop('a'), len(d))",
)
p("container", "dict setdefault", "d = {'a': 1}\nd.setdefault('b', 2)\nprint(sorted(d.items()))")
p("container", "dict | merge", "print({'a': 1} | {'b': 2})")
p("container", "set ops", "a = {1, 2}\nb = {2, 3}\nprint(sorted(a | b), sorted(a & b), sorted(a - b), sorted(a ^ b))")
p("container", "set methods", "s = {1}\ns.add(2)\ns.discard(1)\nprint(sorted(s), s.issubset({2, 3}))")
p("container", "tuple index/count", "t = (1, 2, 3)\nprint(t[0], len(t), t.index(2), t.count(1))")
p("container", "tuple concat/repeat", "print((1, 2) + (3,), (1, 2) * 2)")
p(
    "container",
    "str methods",
    "s = 'Hello World'\n"
    "print(s.upper(), s.lower(), s.split(), s.replace('o', '0'), s.find('World'), s.startswith('He'), s.endswith('ld'))",
)
p("container", "str join/strip/format", "print(','.join(['a', 'b']), '  x '.strip(), '{}-{}'.format(1, 2))")
p("container", "str %-format", "print('%d-%s' % (1, 'a'))")
p("container", "str encode", "print('a'.encode())")
p("container", "heterogeneous list", "xs = [1, 'a', 2.5]\nprint(xs)")
p(
    "container",
    "list of objects sort by key",
    "class P:\n    def __init__(self, v: int) -> None:\n        self.v: int = v\n"
    "ps = [P(2), P(1)]\nps.sort(key=lambda q: q.v)\nprint([q.v for q in ps])",
)

# -------------------------------------------------------------- semantics

p("semantics", "untyped params (inference)", "def f(x):\n    return x + 1\nprint(f(1))")
p("semantics", "duck-typed function", "def f(x):\n    return len(x)\nprint(f([1, 2]), f('abc'))")
p("semantics", "int/float mixing", "print(1 + 2.5, 3 / 2, 7 // 2.0)")
p("semantics", "bool is int", "print(True + True, isinstance(True, int))")
p("semantics", "exception .args", "try:\n    raise ValueError('a')\nexcept ValueError as e:\n    print(e.args)")
p("semantics", "float formatting", "print(0.1 + 0.2, 1 / 3)")
p("semantics", "int division semantics", "print(-7 // 2, -7 % 2, divmod(-7, 2))")
p("semantics", "unicode", "s = 'h\u00e9llo\U0001f642'\nprint(len(s), s[1], s.upper())")
p(
    "semantics",
    "mutable default (module level)",
    "def f(xs: list[int] = []) -> int:\n    xs.append(1)\n    return len(xs)\nprint(f(), f(), f())",
)
p(
    "semantics",
    "mutable default (nested)",
    "def outer() -> str:\n    def f(ys: list[int] = []) -> int:\n        ys.append(1)\n        return len(ys)\n"
    "    return str(f()) + ' ' + str(f())\nprint(outer())",
)
p(
    "semantics",
    "nonlocal augmented assign",
    "def mk():\n    n: int = 0\n    def inc() -> int:\n        nonlocal n\n        n += 1\n"
    "        return n\n    return inc\nc = mk()\nprint(c(), c())",
)
p("semantics", "generator next", "def g():\n    yield 1\n    yield 2\nit = g()\nprint(next(it), next(it))")
p(
    "semantics",
    "generator close",
    "def g():\n    try:\n        yield 1\n    except GeneratorExit:\n        pass\n"
    "it = g()\nnext(it)\nit.close()\nprint('closed')",
)
p("semantics", "nested function defs", "def a() -> int:\n    def b() -> int:\n        return 1\n    return b()\nprint(a())")
p("semantics", "docstring", "def f() -> int:\n    '''doc'''\n    return 1\nprint(f())")
p("semantics", "if __name__ guard", "if __name__ == '__main__':\n    print('main')")
p("semantics", "sys.argv", "import sys\nprint(len(sys.argv) >= 1)")
p("semantics", "uncaught traceback", "raise ValueError('boom')")
p("semantics", "ZeroDivisionError", "print(1 // 0)")
p("semantics", "IndexError", "xs = [1]\nprint(xs[5])")
p("semantics", "KeyError", "d: dict[str, int] = {'a': 1}\nprint(d['k'])")

# ----------------------------------------------------------------- stdlib
#
# Deliberately short: the stdlib is frozen by policy until the language can
# host modules written in PyRs (docs/PRIMITIVES.md section 9), so this only
# records which of the shipped four work, plus a handful of common absences.

for _mod, _expr in [
    ("math", "math.sqrt(2.0)"),
    ("os.path", "os.path.basename('/a/b')"),
    ("json", "json.dumps([1, 2])"),
    ("re", "re.sub('a', 'b', 'aa')"),
    ("collections", "collections.Counter('aab')['a']"),
    ("itertools", "len([1])"),
    ("functools", "functools.reduce(lambda a, b: a + b, [1, 2, 3])"),
    ("datetime", "datetime.date(2020, 1, 1).year"),
    ("pathlib", "pathlib.Path('/a').name"),
    ("random", "random.seed(1) or True"),
    ("time", "time.time() > 0"),
    ("dataclasses", "dataclasses.fields is not None"),
]:
    p("stdlib", f"import {_mod}", f"import {_mod}\nprint({_expr})")


def run_probe(
    item: tuple[str, str, str], pyrs: str, python: str, workdir: Path, timeout: int
) -> dict[str, str]:
    category, name, source = item
    slug = "".join(c if c.isalnum() else "_" for c in name)[:60]
    case_dir = workdir / f"{category}_{slug}"
    case_dir.mkdir(parents=True, exist_ok=True)
    src = case_dir / "prog.py"
    src.write_text(source)

    def run(cmd: list[str]) -> subprocess.CompletedProcess | None:
        try:
            return subprocess.run(
                cmd, capture_output=True, input=b"", timeout=timeout, cwd=case_dir
            )
        except subprocess.TimeoutExpired:
            return None

    oracle = run([python, str(src)])
    if oracle is None:
        return {"category": category, "name": name, "status": "oracle", "detail": "CPython timed out"}
    if b"SyntaxError" in oracle.stderr:
        return {
            "category": category,
            "name": name,
            "status": "oracle",
            "detail": f"SyntaxError under {python}",
        }

    actual = run([pyrs, "run", "--no-cache", "-i", str(src)])
    if actual is None:
        return {"category": category, "name": name, "status": "diverge", "detail": "PyRs timed out"}

    if (
        actual.stdout == oracle.stdout
        and actual.stderr == oracle.stderr
        and actual.returncode == oracle.returncode
    ):
        return {"category": category, "name": name, "status": "ok", "detail": ""}

    stderr = actual.stderr.decode(errors="replace")
    if b"error[" in actual.stderr and not actual.stdout:
        first = next((ln for ln in stderr.splitlines() if ln.startswith("error[")), stderr)
        return {"category": category, "name": name, "status": "reject", "detail": first.strip()[:300]}

    detail = (
        f"cpython {oracle.stdout[:60]!r} rc={oracle.returncode} / "
        f"pyrs {actual.stdout[:60]!r} rc={actual.returncode}"
    )
    if stderr:
        detail += f" :: {stderr.splitlines()[0][:120]}"
    return {"category": category, "name": name, "status": "diverge", "detail": detail}


ORDER = ["keyword", "expression", "class", "builtin", "container", "semantics", "stdlib"]
STATUSES = ["ok", "reject", "diverge", "oracle"]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--pyrs", default="target/release/pyrs", help="PyRs binary to measure")
    ap.add_argument("--python", default=sys.executable, help="CPython oracle")
    ap.add_argument("--category", action="append", choices=ORDER, help="limit to a category")
    ap.add_argument("--jobs", type=int, default=min(8, (os.cpu_count() or 4)))
    ap.add_argument("--timeout", type=int, default=180)
    ap.add_argument("--json", type=Path, help="write the full result table here")
    ap.add_argument("--verbose", action="store_true", help="print every non-ok probe")
    ap.add_argument(
        "--fail-on-diverge",
        action="store_true",
        help="exit non-zero on a divergence that is not in KNOWN_DIVERGENCES, "
        "or on a recorded one that started passing",
    )
    args = ap.parse_args()

    pyrs = os.path.abspath(args.pyrs)
    if not os.path.exists(pyrs):
        print(f"no such binary: {pyrs} (run `make release`)", file=sys.stderr)
        return 2

    probes = [x for x in PROBES if not args.category or x[0] in args.category]

    with tempfile.TemporaryDirectory(prefix="pyrs-coverage-") as tmp:
        workdir = Path(tmp)
        with ThreadPoolExecutor(max_workers=args.jobs) as pool:
            results = list(
                pool.map(lambda it: run_probe(it, pyrs, args.python, workdir, args.timeout), probes)
            )

    if args.json:
        args.json.write_text(json.dumps(results, indent=1) + "\n")

    counts = Counter(r["status"] for r in results)
    width = max(len(c) for c in ORDER)
    print(f"{'category':<{width}}  total  " + "  ".join(f"{s:>7}" for s in STATUSES))
    for category in ORDER:
        rows = [r for r in results if r["category"] == category]
        if not rows:
            continue
        c = Counter(r["status"] for r in rows)
        print(
            f"{category:<{width}}  {len(rows):>5}  "
            + "  ".join(f"{c.get(s, 0):>7}" for s in STATUSES)
        )
    print(
        f"{'total':<{width}}  {len(results):>5}  "
        + "  ".join(f"{counts.get(s, 0):>7}" for s in STATUSES)
    )

    if args.verbose:
        for status in ("diverge", "reject", "oracle"):
            rows = [r for r in results if r["status"] == status]
            if not rows:
                continue
            print(f"\n{status.upper()} ({len(rows)})")
            for r in sorted(rows, key=lambda r: (r["category"], r["name"])):
                print(f"  {r['category']:<11} {r['name']}")
                if r["detail"]:
                    print(f"    {r['detail']}")

    diverged = {r["name"] for r in results if r["status"] == "diverge"}
    ran = {r["name"] for r in results}
    unrecorded = sorted(diverged - set(KNOWN_DIVERGENCES))
    stale = sorted((set(KNOWN_DIVERGENCES) & ran) - diverged)

    if diverged & set(KNOWN_DIVERGENCES):
        print(f"\n{len(diverged & set(KNOWN_DIVERGENCES))} recorded divergence(s), as expected")
    for name in unrecorded:
        print(f"UNRECORDED divergence: {name}", file=sys.stderr)
    for name in stale:
        print(
            f"STALE record: '{name}' now matches CPython — remove it from "
            "KNOWN_DIVERGENCES and from the README list",
            file=sys.stderr,
        )

    if args.fail_on_diverge and (unrecorded or stale):
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
