#!/usr/bin/env python3
"""Documentation and version-agreement checks.

The CI hygiene job verified that required files exist and that workflow YAML
parses. It did not check two things that had already drifted in practice:

1. **Version agreement.** Seven crate manifests, `Cargo.lock`, the README
   language label and limits line, `docs/SPECIFICATIONS.md`, and the compiled
   binary's `--version` must all state the same version. A release whose
   artifacts disagree about what they are is not releasable.

2. **Relative link targets.** Markdown links to repository files must resolve.
   Renaming or moving a document silently broke inbound links, which is how
   `AGENTS.md` and `../SPECIFICATIONS.md` references went stale.

Run with no arguments to check everything. Exit status is 0 only when all
selected checks pass.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import urllib.parse
from pathlib import Path

# Directory -> package name. The driver lives in `cli/` but publishes as `pyrs`.
CRATES: dict[str, str] = {
    "cli": "pyrs",
    "codegen": "codegen",
    "common": "common",
    "ir": "ir",
    "lexer": "lexer",
    "parser": "parser",
    "semantic": "semantic",
}

# Files whose version strings must agree, as (path, label, regex) triples.
# Each regex must expose the version in group 1. The label is what a failure
# report names, so it has to be readable on its own.
VERSION_SITES: tuple[tuple[str, str, str], ...] = (
    # The README used to carry the version in three places, two of them inside
    # a chronological feature list that has since moved to the changelog. One
    # explicit line is enough to catch drift and does not invite the list back.
    ("README.md", "milestone line", r"^Current milestone: \*\*v([0-9]+\.[0-9]+\.[0-9]+)\*\*"),
    ("README.md", "release tag example", r"^Release tags: `git tag v([0-9]+\.[0-9]+\.[0-9]+)"),
    (
        "docs/SPECIFICATIONS.md",
        "version line",
        r"^\*\*v[0-9.]+\*\* / `([0-9]+\.[0-9]+\.[0-9]+)`",
    ),
    (
        "docs/SPECIFICATIONS.md",
        "today-subset heading",
        r"^\*\*Today \(v([0-9]+\.[0-9]+\.[0-9]+) subset\)",
    ),
)

# Documents that record what a past milestone shipped. Their version strings
# are history and must not be rewritten to match the current version.
HISTORICAL = ("docs/superpowers/plans/", "CHANGELOG.md")

LINK = re.compile(r"\[[^\]]*\]\(([^)]+)\)")
FENCED = re.compile(r"```.*?```", re.S)
INLINE_CODE = re.compile(r"`[^`\n]*`")


def strip_code(text: str) -> str:
    """Remove code so subscript-then-call expressions are not read as links.

    `fs[0](args)` and `t[i](args)` are Python, but match markdown link syntax
    exactly. Blanking code spans keeps line structure for error messages.
    """
    text = FENCED.sub(lambda m: "\n" * m.group(0).count("\n"), text)
    return INLINE_CODE.sub("", text)


def crate_version(root: Path, crate: str) -> str | None:
    text = (root / crate / "Cargo.toml").read_text()
    m = re.search(r'^version = "([^"]+)"', text, re.M)
    return m.group(1) if m else None


def check_versions(root: Path, binary: str | None) -> list[str]:
    problems: list[str] = []
    found: dict[str, str] = {}

    for crate in CRATES:
        version = crate_version(root, crate)
        if version is None:
            problems.append(f"{crate}/Cargo.toml: no version field")
        else:
            found[f"{crate}/Cargo.toml"] = version

    if not found:
        return problems + ["no crate versions found; cannot compare"]

    expected = max(set(found.values()), key=list(found.values()).count)

    # Cargo.lock must carry the same version for each workspace member.
    lock = (root / "Cargo.lock").read_text()
    for crate, package in CRATES.items():
        entry = re.search(
            rf'^\[\[package\]\]\nname = "{re.escape(package)}"\nversion = "([^"]+)"',
            lock,
            re.M,
        )
        if entry is None:
            problems.append(f"Cargo.lock: no entry for package {package} ({crate}/)")
        else:
            found[f"Cargo.lock[{package}]"] = entry.group(1)

    for rel, label, pattern in VERSION_SITES:
        text = (root / rel).read_text()
        m = re.search(pattern, text, re.M)
        if m is None:
            problems.append(f"{rel} ({label}): no version matching {pattern!r}")
        else:
            found[f"{rel} ({label})"] = m.group(1)

    if binary:
        try:
            out = subprocess.run(
                [binary, "--version"], capture_output=True, text=True, timeout=60
            )
            m = re.search(r"([0-9]+\.[0-9]+\.[0-9]+)", out.stdout)
            if m is None:
                problems.append(f"{binary} --version: no version in {out.stdout!r}")
            else:
                found[f"{binary} --version"] = m.group(1)
        except (OSError, subprocess.SubprocessError) as exc:
            problems.append(f"{binary} --version failed: {exc}")

    for where, version in sorted(found.items()):
        if version != expected:
            problems.append(f"{where}: {version} != {expected}")

    if not problems:
        print(f"  versions agree at {expected} ({len(found)} sites)")
    return problems


def heading_slugs(text: str) -> set[str]:
    """GitHub's heading anchors: lowercased, markup and punctuation dropped,
    spaces to hyphens."""
    slugs = set()
    for match in re.finditer(r"^#{1,6}\s+(.*)$", text, re.M):
        title = match.group(1).strip()
        title = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", title)  # links
        title = re.sub(r"[`*_]", "", title)  # inline markup
        title = re.sub(r"[^\w\s-]", "", title)  # punctuation
        slugs.add(re.sub(r"\s", "-", title.lower()))
    return slugs


def check_links(root: Path) -> list[str]:
    """Every relative link resolves, and every `#anchor` names a real heading.

    Anchors were unchecked until the documentation overhaul, which is how a
    table of contents came to list sections that had been renamed and omit
    three that existed.
    """
    problems: list[str] = []
    checked = 0
    anchors = 0
    slugs: dict[Path, set[str]] = {}
    docs = [
        md
        for md in sorted(root.rglob("*.md"))
        if "target" not in md.parts and ".git" not in md.parts
    ]
    for md in docs:
        slugs[md.resolve()] = heading_slugs(md.read_text())

    for md in docs:
        rel = md.relative_to(root)
        for raw in LINK.findall(strip_code(md.read_text())):
            target = raw.strip()
            if target.startswith(("http://", "https://", "mailto:")):
                continue
            path_part = urllib.parse.unquote(target.split("#", 1)[0])
            fragment = urllib.parse.unquote(target.split("#", 1)[1]) if "#" in target else ""
            if path_part:
                checked += 1
                resolved = (md.parent / path_part).resolve()
                if not resolved.exists():
                    problems.append(f"{rel}: broken link -> {target}")
                    continue
            else:
                resolved = md.resolve()
            if fragment and resolved.suffix == ".md" and resolved in slugs:
                anchors += 1
                if fragment not in slugs[resolved]:
                    problems.append(f"{rel}: link to a heading that does not exist -> {target}")
    if not problems:
        print(f"  all {checked} relative documentation links resolve")
        print(f"  all {anchors} document anchors name a real heading")
    return problems


def check_unicode_tables(root: Path) -> list[str]:
    """The committed Unicode tables must match the interpreter that generated
    them, so a regeneration on a different CPython cannot land unnoticed."""
    header = root / "codegen" / "runtime" / "unicode_data.h"
    if not header.exists():
        return ["codegen/runtime/unicode_data.h is missing; run scripts/gen_unicode_tables.py"]
    text = header.read_text()
    stamped = re.search(r'#define PYRS_UNIDATA_VERSION "([^"]+)"', text)
    if not stamped:
        return ["unicode_data.h has no PYRS_UNIDATA_VERSION stamp"]
    import unicodedata

    running = unicodedata.unidata_version
    if stamped.group(1) != running:
        return [
            f"unicode_data.h was generated for Unicode {stamped.group(1)} but this "
            f"CPython provides {running}; regenerate with "
            f"scripts/gen_unicode_tables.py or use a matching interpreter"
        ]
    # The UCD version alone does not pin the generator: different CPython
    # releases can ship the same UCD, while the tables come from that
    # interpreter's own str methods. Compare the CPython stamp too -- but only
    # to the minor, since a patch bump does not change casing and pinning it
    # would fail the gate on any 3.14.x other than the generating one.
    cpython = re.search(r'#define PYRS_UNIDATA_CPYTHON "([^"]+)"', text)
    if not cpython:
        return ["unicode_data.h has no PYRS_UNIDATA_CPYTHON stamp"]
    import sys

    stamped_minor = ".".join(cpython.group(1).split(".")[:2])
    running_minor = ".".join(sys.version.split()[0].split(".")[:2])
    if stamped_minor != running_minor:
        return [
            f"unicode_data.h was generated by CPython {cpython.group(1)} but this "
            f"interpreter is {sys.version.split()[0]}; regenerate with "
            f"scripts/gen_unicode_tables.py or use a matching interpreter"
        ]
    print(
        f"  Unicode tables generated for {stamped.group(1)} by CPython "
        f"{cpython.group(1)}, matching this interpreter"
    )
    return []


def check_historical_untouched(root: Path) -> list[str]:
    """Milestone records should not be rewritten to the current version."""
    missing = [p for p in HISTORICAL if not (root / p).exists()]
    return [f"missing historical record: {p}" for p in missing]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--root", default=".")
    ap.add_argument(
        "--binary",
        default=None,
        help="compiled pyrs to query for --version (skipped when absent)",
    )
    ap.add_argument("--only", choices=["versions", "links"], help="run one check")
    args = ap.parse_args()

    root = Path(args.root).resolve()
    binary = args.binary
    if binary and not Path(binary).exists():
        print(f"  note: {binary} not built; skipping --version agreement")
        binary = None

    problems: list[str] = []
    if args.only in (None, "versions"):
        print("version agreement:")
        problems += check_versions(root, binary)
        problems += check_historical_untouched(root)
        problems += check_unicode_tables(root)
    if args.only in (None, "links"):
        print("documentation links:")
        problems += check_links(root)

    if problems:
        print("\nhygiene failures:")
        for p in problems:
            print(f"  - {p}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
