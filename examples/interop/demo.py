"""Build native kernels, use pandas/NumPy buffers, and measure complete calls."""

import argparse
import hashlib
import importlib.util
import json
import math
from pathlib import Path
import platform
import statistics
import subprocess
import sys
import tempfile
import time

import numpy as np
import pandas as pd

import kernels as reference


def median_call(function, repeats):
    function()  # Warm caches before measuring complete Python-to-native calls.
    samples = []
    for _ in range(repeats):
        start = time.perf_counter()
        function()
        samples.append(time.perf_counter() - start)
    return statistics.median(samples)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pyrs", default="target/release/pyrs")
    parser.add_argument("--size", type=int, default=100_000)
    parser.add_argument("--repeats", type=int, default=7)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.size < 1 or args.repeats < 1:
        parser.error("size and repeats must be positive")
    compiler = Path(args.pyrs).resolve()
    source = Path(__file__).with_name("kernels.py")
    with tempfile.TemporaryDirectory(prefix="pyrs-interop-demo-") as directory:
        library = Path(directory) / "kernels_native.so"
        subprocess.run(
            [str(compiler), "build-extension", "-i", str(source), "--module", "kernels_native",
             "--python", sys.executable, "-O", "2", "-o", str(library)],
            check=True, capture_output=True, text=True, timeout=180,
        )
        spec = importlib.util.spec_from_file_location("kernels_native", library)
        native = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(native)

        frame = pd.DataFrame({
            "value": np.linspace(-2.0, 3.0, args.size),
            "weight": np.linspace(0.25, 1.25, args.size),
        })
        values = frame["value"].to_numpy(copy=False)
        weights = frame["weight"].to_numpy(copy=False)
        original = frame.copy(deep=True)
        rows = []
        cases = [
            ("energy", (values,), lambda: float(np.sum(values * values))),
            ("weighted_sum", (values, weights), lambda: float(np.sum(values * weights))),
            ("count_above", (values, 0.5), lambda: int(np.count_nonzero(values > 0.5))),
        ]
        for name, inputs, vectorized in cases:
            python_call = lambda: getattr(reference, name)(*inputs)
            native_call = lambda: getattr(native, name)(*inputs)
            expected = python_call()
            got = native_call()
            if got != expected:
                raise AssertionError(f"{name}: native {got} differs from Python {expected}")
            if not math.isclose(vectorized(), expected, rel_tol=1e-11, abs_tol=1e-10):
                raise AssertionError(f"{name}: NumPy result differs beyond summation tolerance")
            python_seconds = median_call(python_call, args.repeats)
            native_seconds = median_call(native_call, args.repeats)
            numpy_seconds = median_call(vectorized, args.repeats)
            rows.append({
                "kernel": name, "result": got, "cpython_seconds": python_seconds,
                "native_seconds": native_seconds, "numpy_seconds": numpy_seconds,
                "speedup_vs_python_loop": python_seconds / native_seconds,
            })
            print(f"{name}: Python {python_seconds * 1e3:.3f} ms; "
                  f"PyRs {native_seconds * 1e3:.3f} ms; NumPy {numpy_seconds * 1e3:.3f} ms; "
                  f"{python_seconds / native_seconds:.1f}x vs Python loop")
        pd.testing.assert_frame_equal(frame, original)
        evidence = {
            "status": "experimental", "python": sys.version, "platform": platform.platform(),
            "numpy": np.__version__, "pandas": pd.__version__,
            "compiler_sha256": hashlib.sha256(compiler.read_bytes()).hexdigest(),
            "source_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
            "size": args.size, "repeats": args.repeats, "optimization": 2,
            "measurement": "median complete warmed calls; buffer acquisition and scalar result conversion included; build excluded",
            "results": rows,
        }
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(evidence, indent=2) + "\n")


if __name__ == "__main__":
    main()
