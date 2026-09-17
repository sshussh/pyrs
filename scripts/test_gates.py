#!/usr/bin/env python3
"""Failure-path tests for the validation gates themselves.

A gate that cannot fail is not a gate. The previous example-parity recipe
compared command-substitution output, which strips trailing newlines, so a
program emitting the wrong number of them passed. These tests deliberately
break each condition and assert the gate reports it.

Run: python3 -m unittest discover -s scripts -p 'test_*.py'
"""

from __future__ import annotations

import hashlib
import json
import platform
import subprocess
import sys
import tempfile
import textwrap
import unicodedata
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent
ROOT = SCRIPTS.parent


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(textwrap.dedent(text))


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class ExampleGateTests(unittest.TestCase):
    """The gate compiles, then runs the produced binary.

    The stand-in "compiler" therefore has to *emit an executable* at the `-o`
    path rather than print anything itself. That mirrors the real contract and
    keeps toolchain output separate from program output -- the distinction the
    gate exists to preserve, after comparing `pyrs run` stderr against CPython
    turned every example red on CI because the C compiler emitted a warning.
    """

    def fake_compiler(self, emit: str, *, build_fails: bool = False) -> Path:
        """Write a shim accepting `compile -O n -i SRC -o OUT`.

        `emit` is the shell body of the produced program.
        """
        shim = self.root / "pyrs-shim"
        if build_fails:
            shim.write_text(
                "#!/bin/sh\n"
                "echo 'runtime.c:500: warning: some toolchain noise' >&2\n"
                "echo 'error: deliberate build failure' >&2\n"
                "exit 1\n"
            )
        else:
            shim.write_text(
                "#!/bin/sh\n"
                # Toolchain chatter on the *build* step must never be compared
                # against CPython's stderr.
                "echo 'runtime.c:500: warning: some toolchain noise' >&2\n"
                'out=""\n'
                "while [ $# -gt 0 ]; do\n"
                '  case "$1" in -o) out="$2"; shift 2;; *) shift;; esac\n'
                "done\n"
                'printf "%s\\n" "#!/bin/sh" > "$out"\n'
                f'cat >> "$out" <<\'PROG\'\n{emit}\nPROG\n'
                'chmod +x "$out"\n'
            )
        shim.chmod(0o755)
        return shim

    def run_gate(self, shim: Path, example: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [
                sys.executable,
                str(SCRIPTS / "check_examples.py"),
                "--pyrs",
                str(shim),
                "--root",
                str(self.root),
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
        done = self.run_gate(self.fake_compiler('printf "hi\\n"'), "examples/x.py")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertIn("MATCH", done.stdout)

    def test_build_stderr_is_not_compared_against_cpython(self) -> None:
        # Regression for the CI failure this gate caused: the shim always
        # writes toolchain noise to stderr during the build step, and that must
        # not be mistaken for program output.
        done = self.run_gate(self.fake_compiler('printf "hi\\n"'), "examples/x.py")
        self.assertEqual(done.returncode, 0, done.stdout)
        self.assertNotIn("toolchain noise", done.stdout)

    def test_missing_trailing_newline_is_caught(self) -> None:
        # The exact case the old command-substitution recipe could not see.
        done = self.run_gate(self.fake_compiler('printf "hi"'), "examples/x.py")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("DIFFER", done.stdout)
        self.assertIn("stdout", done.stdout)

    def test_extra_trailing_newline_is_caught(self) -> None:
        done = self.run_gate(self.fake_compiler('printf "hi\\n\\n"'), "examples/x.py")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("DIFFER", done.stdout)

    def test_wrong_content_is_caught(self) -> None:
        done = self.run_gate(self.fake_compiler('printf "bye\\n"'), "examples/x.py")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("DIFFER", done.stdout)

    def test_nonzero_exit_with_right_stdout_is_caught(self) -> None:
        # Correct bytes but a failed process must not pass.
        done = self.run_gate(
            self.fake_compiler('printf "hi\\n"\nexit 3'), "examples/x.py"
        )
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("exit status differs", done.stdout)

    def test_program_stderr_is_still_compared(self) -> None:
        # Build noise is ignored, but what the *program* writes is not.
        done = self.run_gate(
            self.fake_compiler('printf "hi\\n"\nprintf "boom\\n" >&2'),
            "examples/x.py",
        )
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("stderr", done.stdout)

    def test_build_failure_is_reported_with_its_output(self) -> None:
        done = self.run_gate(
            self.fake_compiler("", build_fails=True), "examples/x.py"
        )
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("build failed", done.stdout)
        self.assertIn("deliberate build failure", done.stdout)

    def test_failing_oracle_is_reported_not_skipped(self) -> None:
        write(self.root / "examples/x.py", "raise SystemExit(2)\n")
        done = self.run_gate(self.fake_compiler('printf ""'), "examples/x.py")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("no oracle", done.stdout)


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
            Current milestone: **v{version}**.

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
        write(
            root / "codegen/runtime/unicode_data.h",
            f'#define PYRS_UNIDATA_VERSION "{unicodedata.unidata_version}"\n'
            f'#define PYRS_UNIDATA_CPYTHON "{platform.python_version()}"\n',
        )
        write(
            root / "semantic/src/typeshed_seeds.rs",
            "// GENERATED by scripts/gen_typeshed_seeds.py -- do not edit.\n"
            "//\n"
            f"// Derived from typeshed {'a' * 40} for CPython 3.14.7.\n",
        )
        self.write_models(root)
        return root

    def write_models(self, root: Path, labels: tuple[str, ...] = ("int", "str")) -> None:
        """A stand-in for the shipped predictors: two weight files, the feature
        extractor they were trained on, and a manifest stamping all three."""
        write(root / "semantic/models/ml.bin", "weights\n")
        write(root / "semantic/models/dl.bin", "weights\n")
        write(root / "scripts/infer_eval/features.py", "LABELS = []\n")
        table = ", ".join(f'"{l}"' for l in labels)
        write(
            root / "semantic/src/predict.rs",
            f"pub(crate) const LABELS: [&str; {len(labels)}] = [{table}];\n",
        )
        manifest = {
            "labels": list(labels),
            "features": {
                "script": "scripts/infer_eval/features.py",
                "sha256": sha256(root / "scripts/infer_eval/features.py"),
            },
            "ml": {"file": "ml.bin", "sha256": sha256(root / "semantic/models/ml.bin")},
            "dl": {"file": "dl.bin", "sha256": sha256(root / "semantic/models/dl.bin")},
            "training_data": {"flask": {"commit": "a" * 40, "url": "https://example/flask"}},
        }
        write(root / "semantic/models/MANIFEST.json", json.dumps(manifest, indent=1) + "\n")

    def test_typeshed_seeds_must_exist(self) -> None:
        root = self.make_repo()
        (root / "semantic/src/typeshed_seeds.rs").unlink()
        done = self.run_gate(root, "versions")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("typeshed_seeds.rs is missing", done.stdout)

    def test_typeshed_seeds_must_not_be_hand_edited(self) -> None:
        # The table is only trustworthy as the generator's output: a hand edit
        # is how `abs` once came to be seeded as an int.
        root = self.make_repo()
        write(
            root / "semantic/src/typeshed_seeds.rs",
            f"// Derived from typeshed {'a' * 40} for CPython 3.14.7.\n",
        )
        done = self.run_gate(root, "versions")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("not marked as generated", done.stdout)

    def test_typeshed_seeds_must_name_their_commit(self) -> None:
        root = self.make_repo()
        write(
            root / "semantic/src/typeshed_seeds.rs",
            "// GENERATED by scripts/gen_typeshed_seeds.py -- do not edit.\n",
        )
        done = self.run_gate(root, "versions")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("no typeshed commit stamp", done.stdout)

    def test_unicode_table_version_must_match_the_interpreter(self) -> None:
        # The tables are generated from whatever CPython ran the generator, so
        # a regeneration on a different interpreter must not land unnoticed.
        root = self.make_repo()
        write(
            root / "codegen/runtime/unicode_data.h",
            '#define PYRS_UNIDATA_VERSION "1.0.0"\n',
        )
        done = self.run_gate(root, "versions")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("generated for Unicode 1.0.0", done.stdout)

    def test_unicode_table_cpython_must_match_the_interpreter(self) -> None:
        # The UCD version alone does not pin the generator: the tables come
        # from that interpreter's own str methods, and two CPython releases
        # can ship the same UCD.
        root = self.make_repo()
        write(
            root / "codegen/runtime/unicode_data.h",
            f'#define PYRS_UNIDATA_VERSION "{unicodedata.unidata_version}"\n'
            '#define PYRS_UNIDATA_CPYTHON "3.9.1"\n',
        )
        done = self.run_gate(root, "versions")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("generated by CPython 3.9.1", done.stdout)

    def test_unicode_table_cpython_patch_difference_is_allowed(self) -> None:
        # A patch bump does not change casing; pinning it would fail the gate
        # on any 3.x.y but the generating one.
        root = self.make_repo()
        minor = ".".join(platform.python_version().split(".")[:2])
        write(
            root / "codegen/runtime/unicode_data.h",
            f'#define PYRS_UNIDATA_VERSION "{unicodedata.unidata_version}"\n'
            f'#define PYRS_UNIDATA_CPYTHON "{minor}.999"\n',
        )
        done = self.run_gate(root, "versions")
        self.assertEqual(done.returncode, 0, done.stdout)

    def test_missing_unicode_cpython_stamp_is_caught(self) -> None:
        root = self.make_repo()
        write(
            root / "codegen/runtime/unicode_data.h",
            f'#define PYRS_UNIDATA_VERSION "{unicodedata.unidata_version}"\n',
        )
        done = self.run_gate(root, "versions")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("PYRS_UNIDATA_CPYTHON", done.stdout)

    def test_missing_unicode_tables_are_caught(self) -> None:
        root = self.make_repo()
        (root / "codegen/runtime/unicode_data.h").unlink()
        done = self.run_gate(root, "versions")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("gen_unicode_tables.py", done.stdout)

    def test_model_weights_must_match_their_manifest(self) -> None:
        # Weights are the one shipped artifact nothing else validates: a
        # truncated or stale file still loads and still predicts.
        root = self.make_repo()
        write(root / "semantic/models/ml.bin", "other weights\n")
        done = self.run_gate(root, "versions")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("does not match its MANIFEST.json sha256", done.stdout)

    def test_missing_model_weights_are_caught(self) -> None:
        root = self.make_repo()
        (root / "semantic/models/dl.bin").unlink()
        done = self.run_gate(root, "versions")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("export_models.py", done.stdout)

    def test_feature_extractor_drift_is_caught(self) -> None:
        # The models read features.py's output. Editing the extractor without
        # retraining leaves the compiler predicting from a vector the model
        # never saw -- silently, and only slightly wrong.
        root = self.make_repo()
        write(root / "scripts/infer_eval/features.py", "LABELS = []\n# a new feature\n")
        done = self.run_gate(root, "versions")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("has changed since the models were trained", done.stdout)

    def test_label_vocabulary_drift_is_caught(self) -> None:
        # A model's output is an index into this list; reorder it in one place
        # only and every prediction means a different type.
        root = self.make_repo()
        self.write_models(root, labels=("int", "str"))
        write(
            root / "semantic/src/predict.rs",
            'pub(crate) const LABELS: [&str; 2] = ["str", "int"];\n',
        )
        done = self.run_gate(root, "versions")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("LABELS and MANIFEST.json labels disagree", done.stdout)

    def test_incomplete_environment_record_is_caught(self) -> None:
        # Training data is not pinned, so a run is only reproducible through
        # what it recorded; a half-written record is worse than none.
        root = self.make_repo()
        path = root / "semantic/models/MANIFEST.json"
        manifest = json.loads(path.read_text())
        manifest["environment"] = {"device": "cuda", "seed": 0}
        write(path, json.dumps(manifest) + "\n")
        done = self.run_gate(root, "versions")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("device, seed and torch version", done.stdout)

    def test_provenance_must_name_the_compiler_revision(self) -> None:
        root = self.make_repo()
        path = root / "semantic/models/MANIFEST.json"
        manifest = json.loads(path.read_text())
        manifest["provenance"] = {"scripts": {"features.py": "a" * 64}}
        write(path, json.dumps(manifest) + "\n")
        done = self.run_gate(root, "versions")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("compiler revision", done.stdout)

    def test_a_complete_record_passes(self) -> None:
        root = self.make_repo()
        path = root / "semantic/models/MANIFEST.json"
        manifest = json.loads(path.read_text())
        manifest["environment"] = {"device": "cuda", "seed": 0, "torch": "2.14.0+cu130"}
        manifest["provenance"] = {"pyrs_commit": "b" * 40, "scripts": {"features.py": "a" * 64}}
        write(path, json.dumps(manifest) + "\n")
        done = self.run_gate(root, "versions")
        self.assertEqual(done.returncode, 0, done.stdout)

    def test_models_must_record_their_training_data(self) -> None:
        root = self.make_repo()
        manifest = json.loads((root / "semantic/models/MANIFEST.json").read_text())
        del manifest["training_data"]
        write(root / "semantic/models/MANIFEST.json", json.dumps(manifest) + "\n")
        done = self.run_gate(root, "versions")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("projects the models were trained on", done.stdout)

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
        write(
            root / "README.md",
            text.replace("Current milestone: **v9.9.9**", "Current milestone: **v9.9.7**"),
        )
        done = self.run_gate(root, "versions")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("README.md", done.stdout)

    def test_dead_document_anchor_is_caught(self) -> None:
        # A table of contents that points at renamed sections is the exact
        # rot this check exists for.
        root = self.make_repo()
        write(
            root / "docs/GUIDE.md",
            "# Guide\n\n[jump](#a-real-heading)\n[broken](#not-a-heading)\n\n## A real heading\n",
        )
        done = self.run_gate(root, "links")
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("not-a-heading", done.stdout)

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
