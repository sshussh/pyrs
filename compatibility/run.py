#!/usr/bin/env python3
"""Differential regression runner. Only execute trusted workload sources."""
import argparse
import base64
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time


def run(command, cwd, env, timeout, stdin=""):
    start = time.monotonic()
    child = subprocess.Popen(
        command, cwd=cwd, env=env, stdin=subprocess.PIPE,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        start_new_session=os.name == "posix",
    )
    timed_out = False
    try:
        stdout, stderr = child.communicate(stdin.encode(), timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        if os.name == "posix":
            os.killpg(child.pid, signal.SIGKILL)
        else:
            child.kill()
        stdout, stderr = child.communicate()
    return {
        "command": command,
        "returncode": child.returncode,
        "timeout": timed_out,
        "stdout": stdout.decode("utf-8", errors="replace"),
        "stderr": stderr.decode("utf-8", errors="replace"),
        "stdout_base64": base64.b64encode(stdout).decode(),
        "stderr_base64": base64.b64encode(stderr).decode(),
        "elapsed_seconds": round(time.monotonic() - start, 6),
    }


def effects(root):
    result = {}
    for path in sorted(root.rglob("*")):
        rel = path.relative_to(root).as_posix()
        if rel in {"program.py", "__pyrs_binary"} or "__pycache__" in path.parts:
            continue
        if path.is_symlink():
            result[rel] = {"symlink": os.readlink(path)}
        elif path.is_file():
            result[rel] = {"sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
        elif path.is_dir():
            result[rel] = {"directory": True}
    return result


def reset(root, source):
    for path in root.iterdir():
        if path.is_dir() and not path.is_symlink():
            shutil.rmtree(path)
        else:
            path.unlink()
    (root / "program.py").write_bytes(source)


def same_result(got, want):
    return all(got[k] == want[k] for k in (
        "returncode", "stdout_base64", "stderr_base64", "files",
    ))


def classify(case, mode, oracle, compile_result, actual):
    if oracle["timeout"] or oracle["returncode"] != 0:
        return "oracle_error"
    expected = case["native"] if mode == "native" else "pass"
    if compile_result is not None and (compile_result["timeout"] or compile_result["returncode"] != 0):
        if (expected == "unsupported" and not compile_result["timeout"]
                and compile_result["returncode"] == 1
                and not compile_result["stdout_base64"]
                and case["diagnostic"] in compile_result["stderr"]):
            return "known_gap"
        return "regression"
    if actual is None or actual["timeout"]:
        return "regression"
    matches = same_result(actual, oracle)
    if expected == "pass":
        return "pass" if matches else "regression"
    if matches:
        return "unexpected_pass"
    if (expected == "mismatch" and actual["returncode"] == 0
            and actual["stdout_base64"] == base64.b64encode(case["native_stdout"].encode()).decode()
            and actual["stderr_base64"] == oracle["stderr_base64"]
            and actual["files"] == oracle["files"]):
        return "known_gap"
    return "regression"


def executable(value):
    resolved = shutil.which(value)
    if not resolved:
        raise ValueError(f"executable not found: {value}")
    # Keep virtualenv executable paths: resolving their symlinks loses the venv.
    return os.path.abspath(resolved)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=Path(__file__).with_name("manifest.json"))
    parser.add_argument("--pyrs", default="target/debug/pyrs")
    parser.add_argument("--python", default=sys.executable)
    parser.add_argument("--group", choices=["core", "science", "all"], default="core")
    parser.add_argument("--mode", choices=["native", "compat", "both"], default="both")
    parser.add_argument("--opt-levels", nargs="+", type=int, choices=range(4), default=[2])
    parser.add_argument("--timeout", type=float, default=60)
    parser.add_argument("--gc-stress", action="store_true")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    try:
        pyrs, python = executable(args.pyrs), executable(args.python)
        manifest = json.loads(args.manifest.read_text())
        if manifest["schema_version"] != 1:
            raise ValueError("unsupported manifest schema")
        cases = [case for case in manifest["cases"] if args.group in ("all", case["group"])]
        if not cases or len({case["id"] for case in cases}) != len(cases):
            raise ValueError("manifest must select nonempty cases with unique ids")
        for case in cases:
            if case["native"] not in {"pass", "mismatch", "unsupported"}:
                raise ValueError(f"invalid native expectation: {case['id']}")
            if case["native"] == "unsupported" and not case.get("diagnostic"):
                raise ValueError(f"missing expected diagnostic: {case['id']}")
            if case["native"] == "mismatch" and "native_stdout" not in case:
                raise ValueError(f"missing recorded native output: {case['id']}")
        env = os.environ.copy()
        for key in list(env):
            if key.startswith("PYRS_GC"):
                del env[key]
        env.update(PYTHONHASHSEED="0", OPENBLAS_NUM_THREADS="1", OMP_NUM_THREADS="1")
        packages = sorted({p for case in cases for p in case.get("requires", [])})
        metadata_code = (
            "import importlib.metadata as m, json, sys; "
            "print(json.dumps({'version': sys.version, 'implementation': sys.implementation.name, "
            "'executable': sys.executable, 'packages': {p: m.version(p) for p in sys.argv[1:]}}))"
        )
        with tempfile.TemporaryDirectory(prefix="pyrs-compatibility-") as tmp:
            root = Path(tmp)
            metadata = run([python, "-c", metadata_code, *packages], root, env, args.timeout)
            if metadata["returncode"] != 0 or metadata["timeout"]:
                raise ValueError(f"oracle dependencies unavailable; install {packages} in --python's environment:\n{metadata['stderr']}")
            python_info = json.loads(metadata["stdout"])
            if python_info["implementation"] != "cpython":
                raise ValueError("the differential oracle must be CPython")
            version = run([pyrs, "--version"], root, env, args.timeout)
            if version["returncode"] != 0 or version["timeout"]:
                raise ValueError("cannot read compiler version")
            report = {
                "schema_version": 1, "kind": manifest["kind"],
                "python": python_info, "pyrs": version["stdout"].strip(),
                "compiler_sha256": hashlib.sha256(Path(pyrs).read_bytes()).hexdigest(),
                "manifest_sha256": hashlib.sha256(args.manifest.read_bytes()).hexdigest(),
                "environment": {key: env.get(key) for key in ("CC", "ASAN_OPTIONS", "UBSAN_OPTIONS", "OPENBLAS_NUM_THREADS", "OMP_NUM_THREADS", "PYTHONHASHSEED")},
                "gc_stress": args.gc_stress, "results": [],
            }
            modes = ["native", "compat"] if args.mode == "both" else [args.mode]
            for case in cases:
                source = (args.manifest.parent / case["file"]).read_bytes()
                program_args = case.get("args", [])
                reset(root, source)
                oracle = run([python, "program.py", *program_args], root, env, args.timeout, case.get("stdin", ""))
                oracle["files"] = effects(root)
                for mode in modes:
                    for opt in sorted(set(args.opt_levels)) if mode == "native" else [None]:
                        reset(root, source)
                        compiled, actual = None, None
                        if mode == "native":
                            compiled = run([pyrs, "compile", "-O", str(opt), "-i", "program.py", "-o", "__pyrs_binary"], root, env, args.timeout)
                            if compiled["returncode"] == 0 and not compiled["timeout"]:
                                native_env = dict(env)
                                if args.gc_stress:
                                    native_env["PYRS_GC_STRESS"] = "1"
                                actual = run([str(root / "__pyrs_binary"), *program_args], root, native_env, args.timeout, case.get("stdin", ""))
                        else:
                            actual = run([pyrs, "--compat", "--python", python, "program.py", *program_args], root, env, args.timeout, case.get("stdin", ""))
                        if actual is not None:
                            actual["files"] = effects(root)
                        status = classify(case, mode, oracle, compiled, actual)
                        report["results"].append({
                            "id": case["id"], "source": source.decode(),
                            "source_sha256": hashlib.sha256(source).hexdigest(),
                            "mode": mode, "opt_level": opt, "status": status,
                            "expected_native": case["native"], "reason": case.get("reason"),
                            "oracle": oracle, "compile": compiled, "actual": actual,
                        })
                        suffix = f" O{opt}" if opt is not None else ""
                        print(f"{status:16} {mode}{suffix}: {case['id']}")
            report["summary"] = {
                mode: dict(Counter(r["status"] for r in report["results"] if r["mode"] == mode))
                for mode in modes
            }
            print(json.dumps(report["summary"], sort_keys=True))
            if args.output:
                args.output.parent.mkdir(parents=True, exist_ok=True)
                args.output.write_text(json.dumps(report, indent=2) + "\n")
            return int(any(r["status"] not in {"pass", "known_gap"} for r in report["results"]))
    except (OSError, ValueError, KeyError) as error:
        print(f"compatibility runner: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
