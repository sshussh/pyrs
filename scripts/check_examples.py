#!/usr/bin/env python3
"""Byte-exact example parity gate.

Every example runs under both PyRs and CPython, and all observable results
are compared: stdout bytes, stderr bytes and exit status.

This replaces a shell recipe that used command substitution (`got=$(...)`),
which strips trailing newlines and so could not detect a program emitting
the wrong number of them. Comparisons here are on raw bytes.

Exit status is 0 only when every example matches.
"""

from __future__ import annotations

import argparse
import difflib
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

GREEN = "\033[32m"
RED = "\033[31m"
RESET = "\033[0m"

# Examples needing arguments. PyRs takes them after `--`; CPython does not.
EXTRA_ARGS: dict[str, list[str]] = {
    "examples/risksim/main.py": ["examples/risksim/data/balanced.scenario"],
}

GLOBS = ("examples/*.py", "examples/modules/*.py")
EXPLICIT = ("examples/packages/main.py", "examples/risksim/main.py")


@dataclass
class Run:
    stdout: bytes
    stderr: bytes
    status: int


def discover(root: Path) -> list[str]:
    found: list[str] = []
    for pattern in GLOBS:
        found.extend(sorted(str(p.relative_to(root)) for p in root.glob(pattern)))
    for extra in EXPLICIT:
        if (root / extra).is_file() and extra not in found:
            found.append(extra)
    return found


def run_pyrs(pyrs: str, root: Path, example: str, opt: str) -> Run:
    cmd = [pyrs, "run", "-O", opt, "-i", example]
    args = EXTRA_ARGS.get(example)
    if args:
        cmd.append("--")
        cmd.extend(args)
    done = subprocess.run(cmd, cwd=root, capture_output=True)
    return Run(done.stdout, done.stderr, done.returncode)


def run_python(python: str, root: Path, example: str) -> Run:
    cmd = [python, example, *EXTRA_ARGS.get(example, [])]
    done = subprocess.run(cmd, cwd=root, capture_output=True)
    return Run(done.stdout, done.stderr, done.returncode)


def describe(label: str, expected: bytes, actual: bytes) -> list[str]:
    """A readable diff that still makes trailing-newline changes visible."""
    if expected == actual:
        return []
    exp = expected.decode("utf-8", "replace").splitlines(keepends=True)
    act = actual.decode("utf-8", "replace").splitlines(keepends=True)
    lines = [f"    {label} differs ({len(expected)} vs {len(actual)} bytes):"]
    diff = difflib.unified_diff(exp, act, "cpython", "pyrs", n=1)
    lines.extend("    " + line.rstrip("\n") for line in list(diff)[:40])
    return lines


def compare(example: str, expected: Run, actual: Run, opt: str) -> list[str]:
    problems: list[str] = []
    if expected.status != actual.status:
        problems.append(
            f"    exit status differs at -O{opt}: "
            f"cpython={expected.status} pyrs={actual.status}"
        )
    problems.extend(describe(f"stdout at -O{opt}", expected.stdout, actual.stdout))
    problems.extend(describe(f"stderr at -O{opt}", expected.stderr, actual.stderr))
    return problems


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--pyrs", default="target/release/pyrs")
    ap.add_argument("--python", default="python3")
    ap.add_argument("--root", default=".", help="repository root")
    ap.add_argument(
        "--opt-levels",
        nargs="+",
        default=["2"],
        help="PyRs optimization levels to check (default: 2)",
    )
    ap.add_argument("--only", help="check a single example path")
    args = ap.parse_args()

    root = Path(args.root).resolve()
    pyrs = str(Path(args.pyrs).resolve()) if Path(args.pyrs).exists() else args.pyrs

    examples = [args.only] if args.only else discover(root)
    if not examples:
        print("no examples found", file=sys.stderr)
        return 1

    failures = 0
    for example in examples:
        expected = run_python(args.python, root, example)
        if expected.status != 0:
            print(f"  {RED}ORACLE{RESET} {example}")
            print(f"    CPython itself failed: {expected.stderr.decode(errors='replace')}")
            failures += 1
            continue
        problems: list[str] = []
        for opt in args.opt_levels:
            problems.extend(compare(example, expected, run_pyrs(pyrs, root, example, opt), opt))
        if problems:
            print(f"  {RED}DIFFER{RESET} {example}")
            print("\n".join(problems))
            failures += 1
        else:
            print(f"  {GREEN}MATCH{RESET}  {example}")

    if failures:
        print(f"\n{failures} of {len(examples)} examples differ from CPython")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
