#!/usr/bin/env python3
"""Failure-path tests for the validation gates themselves.

A gate that cannot fail is not a gate. The previous example-parity recipe
compared command-substitution output, which strips trailing newlines, so a
program emitting the wrong number of them passed. These tests deliberately
break each condition and assert the gate reports it.

Run: python3 -m unittest discover -s scripts -p 'test_*.py'
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent
ROOT = SCRIPTS.parent


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(textwrap.dedent(text))


def fake_compiler(path: Path, body: str) -> str:
    """A stand-in for `pyrs run -O N -i FILE` that emits fixed results."""
    write(
        path,
        f"""\
        import sys
        {body}
        """,
    )
    return f"{sys.executable} {path}"


class ExampleGateTests(unittest.TestCase):
    def run_gate(self, root: Path, pyrs_cmd: str, example: str) -> subprocess.CompletedProcess:
        parts = pyrs_cmd.split()
        # check_examples invokes `<pyrs> run -O n -i <example>`; wrap the
        # interpreter+script pair into a single executable shim.
        shim = root / "pyrs-shim"
        shim.write_text(f'#!/bin/sh\nexec {pyrs_cmd} "$@"\n')
        shim.chmod(0o755)
        self.assertTrue(parts)
        return subprocess.run(
            [
                sys.executable,
                str(SCRIPTS / "check_examples.py"),
                "--pyrs",
                str(shim),
                "--root",
                str(root),
                "--only",
                example,
            ],
            capture_output=True,
            text=True,
        )

    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        write(self.root / "examples/x.py", 'print("hi")\n')

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def test_matching_output_passes(self) -> None:
        cmd = fake_compiler(self.root / "fake.py", 'sys.stdout.write("hi\\n")')
        done = self.run_gate(self.root, cmd, "examples/x.py")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertIn("MATCH", done.stdout)

    def test_missing_trailing_newline_is_caught(self) -> None:
        # The exact case the old command-substitution recipe could not see.
        cmd = fake_compiler(self.root / "fake.py", 'sys.stdout.write("hi")')
        done = self.run_gate(self.root, cmd, "examples/x.py")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("DIFFER", done.stdout)
        self.assertIn("stdout", done.stdout)

    def test_extra_trailing_newline_is_caught(self) -> None:
        cmd = fake_compiler(self.root / "fake.py", 'sys.stdout.write("hi\\n\\n")')
        done = self.run_gate(self.root, cmd, "examples/x.py")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("DIFFER", done.stdout)

    def test_wrong_content_is_caught(self) -> None:
        cmd = fake_compiler(self.root / "fake.py", 'sys.stdout.write("bye\\n")')
        done = self.run_gate(self.root, cmd, "examples/x.py")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("DIFFER", done.stdout)

    def test_nonzero_exit_with_right_stdout_is_caught(self) -> None:
        # Correct bytes but a failed process must not pass.
        cmd = fake_compiler(
            self.root / "fake.py", 'sys.stdout.write("hi\\n")\nsys.exit(3)'
        )
        done = self.run_gate(self.root, cmd, "examples/x.py")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("exit status differs", done.stdout)

    def test_unexpected_stderr_is_caught(self) -> None:
        cmd = fake_compiler(
            self.root / "fake.py",
            'sys.stdout.write("hi\\n")\nsys.stderr.write("warning\\n")',
        )
        done = self.run_gate(self.root, cmd, "examples/x.py")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("stderr", done.stdout)

    def test_failing_oracle_is_reported_not_skipped(self) -> None:
        write(self.root / "examples/x.py", "raise SystemExit(2)\n")
        cmd = fake_compiler(self.root / "fake.py", "pass")
        done = self.run_gate(self.root, cmd, "examples/x.py")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("ORACLE", done.stdout)


class HygieneGateTests(unittest.TestCase):
    def run_gate(self, root: Path, only: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [
                sys.executable,
                str(SCRIPTS / "check_hygiene.py"),
                "--root",
                str(root),
                "--only",
                only,
            ],
            capture_output=True,
            text=True,
        )

    def make_repo(self, version: str = "9.9.9") -> Path:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        for crate, package in (
            ("cli", "pyrs"),
            ("codegen", "codegen"),
            ("common", "common"),
            ("ir", "ir"),
            ("lexer", "lexer"),
            ("parser", "parser"),
            ("semantic", "semantic"),
        ):
            write(
                root / crate / "Cargo.toml",
                f'[package]\nname = "{package}"\nversion = "{version}"\n',
            )
        lock = "".join(
            f'[[package]]\nname = "{p}"\nversion = "{version}"\n\n'
            for p in ("pyrs", "codegen", "common", "ir", "lexer", "parser", "semantic")
        )
        write(root / "Cargo.lock", lock)
        write(
            root / "README.md",
            f"""\
            ## The language (v{version})

            Known limits (v{version}): things

            Release tags: `git tag v{version} && git push origin v{version}`.
            """,
        )
        write(
            root / "docs/SPECIFICATIONS.md",
            f"""\
            **v{version.rsplit('.', 1)[0]}** / `{version}`. Optional release tags: `vX.Y.Z`.

            **Today (v{version} subset):**
            """,
        )
        write(root / "CHANGELOG.md", "# Changelog\n")
        write(root / "docs/superpowers/plans/keep.md", "# history\n")
        return root

    def test_consistent_repo_passes(self) -> None:
        done = self.run_gate(self.make_repo(), "versions")
        self.assertEqual(done.returncode, 0, done.stdout)
        self.assertIn("versions agree", done.stdout)

    def test_crate_version_drift_is_caught(self) -> None:
        root = self.make_repo()
        write(root / "semantic/Cargo.toml", '[package]\nname = "semantic"\nversion = "9.9.8"\n')
        done = self.run_gate(root, "versions")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("semantic/Cargo.toml", done.stdout)

    def test_readme_label_drift_is_caught(self) -> None:
        root = self.make_repo()
        text = (root / "README.md").read_text()
        write(root / "README.md", text.replace("## The language (v9.9.9)", "## The language (v9.9.7)"))
        done = self.run_gate(root, "versions")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("README.md", done.stdout)

    def test_lockfile_drift_is_caught(self) -> None:
        root = self.make_repo()
        lock = (root / "Cargo.lock").read_text()
        write(root / "Cargo.lock", lock.replace('name = "parser"\nversion = "9.9.9"', 'name = "parser"\nversion = "9.9.1"'))
        done = self.run_gate(root, "versions")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("parser", done.stdout)

    def test_broken_relative_link_is_caught(self) -> None:
        root = self.make_repo()
        write(root / "docs/thing.md", "See [the plan](does-not-exist.md).\n")
        done = self.run_gate(root, "links")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("does-not-exist.md", done.stdout)

    def test_code_spans_are_not_read_as_links(self) -> None:
        # `fs[0](args)` is Python, not a markdown link. This false positive
        # was found by running the checker against the real repository.
        root = self.make_repo()
        write(root / "docs/thing.md", "Call with `fs[0](args)` or `t[i](args)`.\n")
        done = self.run_gate(root, "links")
        self.assertEqual(done.returncode, 0, done.stdout)

    def test_real_repository_passes_both_checks(self) -> None:
        done = subprocess.run(
            [sys.executable, str(SCRIPTS / "check_hygiene.py"), "--root", str(ROOT)],
            capture_output=True,
            text=True,
        )
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
