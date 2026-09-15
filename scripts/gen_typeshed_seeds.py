#!/usr/bin/env python3
"""Generate semantic/src/typeshed_seeds.rs from typeshed's stubs.

An unannotated parameter passed straight to a library function can take its
type from that function's signature instead of being inferred. Typeshed is
ground truth for those signatures, but its annotations describe Python, not
PyRs, so each is narrowed to what PyRs can represent -- and where that does
not come down to exactly one type, the parameter is left unseeded rather than
guessed.

    python3 scripts/gen_typeshed_seeds.py --typeshed path/to/typeshed

The surface is read from PyRs itself -- the `BUILTINS` list in
semantic/src/lib.rs and the functions defined in stdlib/ -- so the table only
ever names functions PyRs can call. `make hygiene` checks the output is
present and stamped; regeneration needs a typeshed checkout, so it is run by
hand when the surface grows.
"""

from __future__ import annotations

import argparse
import ast
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "semantic" / "src" / "typeshed_seeds.rs"

# PyRs module -> stub file, relative to typeshed/stdlib.
STUBS = {
    "builtins": "builtins.pyi",
    "math": "math/__init__.pyi",
    "os": "os/__init__.pyi",
    "os.path": "posixpath.pyi",
    "json": "json/__init__.pyi",
}
# PyRs module -> the stdlib source whose top-level functions are the surface.
SURFACE_SOURCES = {
    "math": "stdlib/math.py",
    "os": "stdlib/os/__init__.py",
    "os.path": "stdlib/os/path.py",
    "json": "stdlib/json.py",
}

SEEDABLE = {"str": "Str", "int": "Int", "float": "Float", "bool": "Bool", "LiteralString": "Str"}
# Protocols whose only PyRs-representable implementers are one type. bool is an
# int subclass and int promotes into float, so each names a single seed.
PROTOCOL_SEEDS = {"SupportsIndex": "Int", "SupportsInt": "Int", "SupportsFloat": "Float"}
# Concrete Python types PyRs has no representation for. They are dropped from a
# union rather than poisoning it: `str | bytes` still seeds `str`.
UNREPRESENTABLE = {"bytes", "bytearray", "memoryview", "complex", "None", "PathLike"}

# Members of a typeshed annotation that PyRs's own implementation refuses, each
# verified against the compiler. `open(3)` is "open() path must be a str".
EXCLUSIONS = {("open", 0): {"Int"}}


class Poison(Exception):
    """An open set of types -- a protocol, type variable or generic."""


def version_ok(test: ast.expr) -> bool | None:
    """Evaluate a `sys.version_info` / `sys.platform` guard; None if unknown."""
    if not isinstance(test, ast.Compare) or len(test.ops) != 1:
        return None
    left, op, right = test.left, test.ops[0], test.comparators[0]
    src = ast.unparse(left)
    if src == "sys.version_info" and isinstance(right, ast.Tuple):
        want = tuple(ast.literal_eval(right))
        have = sys.version_info[: len(want)]
    elif src == "sys.platform" and isinstance(right, ast.Constant):
        want, have = right.value, sys.platform
    else:
        return None
    return {
        ast.GtE: have >= want,
        ast.Gt: have > want,
        ast.Lt: have < want,
        ast.LtE: have <= want,
        ast.Eq: have == want,
        ast.NotEq: have != want,
    }.get(type(op))


def module_level(body: list[ast.stmt]):
    """Top-level statements, following version guards but never into classes'
    method bodies (a `hex` method on int is not the `hex` builtin)."""
    for node in body:
        if isinstance(node, ast.If):
            verdict = version_ok(node.test)
            if verdict is None:
                yield from module_level(node.body)
                yield from module_level(node.orelse)
            elif verdict:
                yield from module_level(node.body)
            else:
                yield from module_level(node.orelse)
        else:
            yield node


def aliases_in(tree: ast.Module) -> dict[str, str]:
    """TypeAliases, and constrained TypeVars as the union of their constraints.

    `AnyStr = TypeVar("AnyStr", str, bytes)` can only ever be one of its
    constraints, so in PyRs -- which has no bytes -- it is `str`. An
    unconstrained or bound TypeVar stays unresolved, and so poisons."""
    out = {}
    for node in module_level(tree.body):
        if (
            isinstance(node, ast.Assign)
            and len(node.targets) == 1
            and isinstance(node.targets[0], ast.Name)
            and isinstance(node.value, ast.Call)
            and ast.unparse(node.value.func) in ("TypeVar", "typing.TypeVar")
            and len(node.value.args) >= 3
        ):
            out[node.targets[0].id] = " | ".join(ast.unparse(a) for a in node.value.args[1:])
            continue
        if (
            isinstance(node, ast.AnnAssign)
            and isinstance(node.target, ast.Name)
            and ast.unparse(node.annotation) == "TypeAlias"
            and node.value is not None
        ):
            out[node.target.id] = ast.unparse(node.value)
    return out


def split_union(text: str) -> list[str]:
    parts, depth, cur = [], 0, ""
    for ch in text:
        if ch in "[(":
            depth += 1
        elif ch in "])":
            depth -= 1
        if ch == "|" and depth == 0:
            parts.append(cur.strip())
            cur = ""
        else:
            cur += ch
    parts.append(cur.strip())
    return [p for p in parts if p]


def members(text: str, aliases: dict[str, str], depth: int = 0) -> set[str]:
    """The PyRs types an annotation admits, or Poison if the set is open."""
    if depth > 12:
        raise Poison(text)
    out: set[str] = set()
    for part in split_union(text):
        base = part.split("[", 1)[0].strip()
        if part.startswith("Optional["):
            out |= members(part[len("Optional[") : -1], aliases, depth + 1)
        elif part.startswith("Literal["):
            values = ast.literal_eval("[" + part[len("Literal[") : -1] + "]")
            for v in values:
                name = type(v).__name__
                if name not in SEEDABLE:
                    raise Poison(part)
                out.add(SEEDABLE[name])
        elif part in SEEDABLE:
            out.add(SEEDABLE[part])
        elif part in PROTOCOL_SEEDS:
            out.add(PROTOCOL_SEEDS[part])
        elif base in UNREPRESENTABLE:
            continue
        elif part in aliases:
            out |= members(aliases[part], aliases, depth + 1)
        else:
            raise Poison(part)
    return out


# An overload whose parameter admits no PyRs type at all -- `PathLike[AnyStr]`
# -- can never be the one a PyRs call selects, so it abstains rather than
# counting as a disagreement.
UNREACHABLE = "unreachable"


def seed_of(text: str | None, aliases: dict[str, str]) -> str | None:
    if text is None:
        return None
    try:
        admitted = members(text, aliases)
    except Poison:
        return None
    if not admitted:
        return UNREACHABLE
    # typeshed's numeric-tower convention: `float` also accepts int and bool.
    if admitted and admitted <= {"Int", "Float", "Bool"} and "Float" in admitted:
        return "Float"
    if admitted == {"Int", "Bool"}:
        return "Int"
    return next(iter(admitted)) if len(admitted) == 1 else None


def positional(fn: ast.FunctionDef, drop_receiver: bool) -> list[str | None]:
    args = fn.args.posonlyargs + fn.args.args
    if drop_receiver and args:
        args = args[1:]
    return [ast.unparse(a.annotation) if a.annotation else None for a in args]


def overloads(tree: ast.Module, name: str) -> list[list[str | None]]:
    found = []
    for node in module_level(tree.body):
        if isinstance(node, ast.FunctionDef) and node.name == name:
            found.append(positional(node, drop_receiver=False))
        elif isinstance(node, ast.ClassDef) and node.name == name:
            for item in module_level(node.body):
                if isinstance(item, ast.FunctionDef) and item.name in ("__new__", "__init__"):
                    found.append(positional(item, drop_receiver=True))
    return found


def seeds_for(qual: str, sigs: list[list[str | None]], aliases: dict[str, str]) -> list[str]:
    width = max((len(s) for s in sigs), default=0)
    result = []
    for i in range(width):
        per = []
        for s in sigs:
            if i < len(s):
                per.append(seed_of(s[i], aliases))
        per = [p for p in per if p != UNREACHABLE]
        seed = per[0] if per and all(p == per[0] for p in per) else None
        result.append(seed)
    # An exclusion removes a member PyRs refuses before narrowing, so it has to
    # work on the admitted member sets rather than on the already-narrowed seed:
    # `open`'s `int | str` narrows to nothing, and to `str` without the `int`.
    for (fname, i), dropped in EXCLUSIONS.items():
        if fname == qual and i < width:
            admitted = set()
            poisoned = False
            for s in sigs:
                if i < len(s) and s[i] is not None:
                    try:
                        admitted |= members(s[i], aliases)
                    except Poison:
                        poisoned = True
            admitted -= dropped
            result[i] = next(iter(admitted)) if not poisoned and len(admitted) == 1 else None
    while result and result[-1] is None:
        result.pop()
    return [r if r is not None else "Any" for r in result]


def pyrs_builtins() -> list[str]:
    src = (ROOT / "semantic" / "src" / "lib.rs").read_text()
    block = re.search(r"const BUILTINS: \[&str; \d+\] = \[(.*?)\];", src, re.S)
    return re.findall(r'"([a-z_]+)"', block.group(1))


def stdlib_surface(rel: str) -> list[str]:
    tree = ast.parse((ROOT / rel).read_text())
    return [
        n.name
        for n in tree.body
        if isinstance(n, ast.FunctionDef) and not n.name.startswith("_")
    ]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--typeshed", required=True, type=Path)
    args = ap.parse_args()
    stdlib = args.typeshed / "stdlib"
    commit = subprocess.run(
        ["git", "-C", str(args.typeshed), "rev-parse", "HEAD"],
        capture_output=True, text=True, check=True,
    ).stdout.strip()

    shared = {
        **aliases_in(ast.parse((stdlib / "typing.pyi").read_text())),
        **aliases_in(ast.parse((stdlib / "_typeshed" / "__init__.pyi").read_text())),
    }
    rows: list[tuple[str, list[str]]] = []
    surfaces = {"builtins": pyrs_builtins()}
    surfaces.update({mod: stdlib_surface(src) for mod, src in SURFACE_SOURCES.items()})
    for mod, names in surfaces.items():
        tree = ast.parse((stdlib / STUBS[mod]).read_text())
        aliases = {**shared, **aliases_in(tree)}
        for name in sorted(set(names)):
            sigs = overloads(tree, name)
            if not sigs:
                continue
            qual = name if mod == "builtins" else f"{mod}.{name}"
            seeds = seeds_for(qual, sigs, aliases)
            if any(s != "Any" for s in seeds):
                rows.append((qual, seeds))

    lines = [
        "// GENERATED by scripts/gen_typeshed_seeds.py -- do not edit.",
        "//",
        f"// Derived from typeshed {commit} for CPython "
        f"{sys.version_info.major}.{sys.version_info.minor}.{sys.version_info.micro}.",
        "// Each entry seeds the leading positional parameters of a function PyRs",
        "// supports. `Any` marks a parameter typeshed admits more than one PyRs type",
        "// for -- a protocol, a type variable, or a union that does not narrow to",
        "// one -- and is not a seed.",
        "",
        "/// Parameter types for a builtin (by the name a call writes, `open`) or a",
        "/// supported stdlib function (`math.sqrt`), from typeshed.",
        "pub(crate) fn typeshed_params(qual: &str) -> Option<&'static [ir::Ty]> {",
        "    Some(match qual {",
    ]
    for qual, seeds in rows:
        tys = ", ".join(f"ir::Ty::{s}" for s in seeds)
        lines.append(f'        "{qual}" => &[{tys}],')
    lines += ["        _ => return None,", "    })", "}", ""]
    OUT.write_text("\n".join(lines))
    print(f"wrote {OUT.relative_to(ROOT)}: {len(rows)} functions from typeshed {commit[:10]}")
    for qual, seeds in rows:
        print(f"  {qual:22s} {seeds}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
