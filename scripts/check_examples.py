#!/usr/bin/env python3
"""Byte-exact example parity gate.

Every example is compiled, then the resulting binary is run, and its
observable results are compared against CPython: stdout bytes, stderr bytes
and exit status.

Two design points, both learned from failures:

* **Comparisons are on raw bytes.** The original shell recipe used command
  substitution (`got=$(...)`), which strips trailing newlines and so could
  not detect a program emitting the wrong number of them.

* **Compiling and running are separate steps.** `pyrs run` writes build
  diagnostics to its own stderr -- the C toolchain's warnings, which differ
  between compilers and CI images. Comparing that against an interpreter's
  stderr is meaningless. Building first isolates *program* output from
  *toolchain* output, so program stderr can be compared strictly while build
  noise is reported only when the build actually fails.

Exit status is 0 only when every example matches.
"""

from __future__ import annotations

import argparse
import difflib
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

GREEN = "\033[32m"
RED = "\033[31m"
RESET = "\033[0m"

# Examples needing arguments, passed to the compiled binary and to CPython.
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


def build(pyrs: str, root: Path, example: str, opt: str, out: Path) -> tuple[bool, bytes]:
    """Compile `example`. Returns (succeeded, build output)."""
    done = subprocess.run(
        [pyrs, "compile", "-O", opt, "-i", example, "-o", str(out)],
        cwd=root,
        capture_output=True,
    )
    return done.returncode == 0, done.stdout + done.stderr


def run_binary(binary: Path, root: Path, example: str) -> Run:
    done = subprocess.run(
        [str(binary), *EXTRA_ARGS.get(example, [])], cwd=root, capture_output=True
    )
    return Run(done.stdout, done.stderr, done.returncode)


def run_python(python: str, root: Path, example: str) -> Run:
    done = subprocess.run(
        [python, example, *EXTRA_ARGS.get(example, [])], cwd=root, capture_output=True
    )
    return Run(done.stdout, done.stderr, done.returncode)


def describe(label: str, expected: bytes, actual: bytes) -> list[str]:
    """A diff that still makes a trailing-newline-only change visible."""
    if expected == actual:
        return []
    exp = expected.decode("utf-8", "replace").splitlines(keepends=True)
    act = actual.decode("utf-8", "replace").splitlines(keepends=True)
    lines = [f"    {label} differs ({len(expected)} vs {len(actual)} bytes):"]
    diff = difflib.unified_diff(exp, act, "cpython", "pyrs", n=1)
    lines.extend("    " + line.rstrip("\n") for line in list(diff)[:40])
    return lines


def compare(expected: Run, actual: Run, opt: str) -> list[str]:
    problems: list[str] = []
    if expected.status != actual.status:
        problems.append(
            f"    exit status differs at -O{opt}: "
            f"cpython={expected.status} pyrs={actual.status}"
        )
    problems.extend(describe(f"stdout at -O{opt}", expected.stdout, actual.stdout))
    problems.extend(describe(f"stderr at -O{opt}", expected.stderr, actual.stderr))
    return problems


def check(
    pyrs: str, python: str, root: Path, example: str, opts: list[str], workdir: Path
) -> list[str]:
    expected = run_python(python, root, example)
    if expected.status != 0:
        return [
            "    CPython itself failed, so there is no oracle: "
            + expected.stderr.decode(errors="replace").strip()
        ]
    problems: list[str] = []
    for opt in opts:
        binary = workdir / f"prog-O{opt}"
        ok, output = build(pyrs, root, example, opt, binary)
        if not ok:
            problems.append(f"    build failed at -O{opt}:")
            problems.extend(
                "      " + line
                for line in output.decode(errors="replace").splitlines()[:20]
            )
            continue
        problems.extend(compare(expected, run_binary(binary, root, example), opt))
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
    with tempfile.TemporaryDirectory(prefix="pyrs-example-parity-") as tmp:
        workdir = Path(tmp)
        for example in examples:
            problems = check(pyrs, args.python, root, example, args.opt_levels, workdir)
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
