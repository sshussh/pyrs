"""Build unannotated numerical kernels as a CPython extension with each engine.

For each engine: does the stripped kernel file build, which kernels get the
signature the annotated file has, do their results match pure Python on sample
inputs, and how fast are they called from CPython compared with the Python
function. Nothing in an extension calls its kernels, so this is the setting
where only a learned predictor has evidence.

usage: extension_bench.py
"""
import ast
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
PYRS = os.environ.get("PYRS", os.path.join(REPO, "target", "release", "pyrs"))
DATA = os.path.join(REPO, "target", "inference-eval")

INPUTS = {
    "dot": ([1.5 * i for i in range(2000)], [0.5 * i for i in range(2000)]),
    "norm": ([0.25 * i for i in range(2000)],),
    "mean": ([float(i % 17) for i in range(3000)],),
    "variance": ([float(i % 13) for i in range(3000)],),
    "fib": (80,),
    "count_primes": (4000,),
    "gcd": (1071 * 997, 462 * 997),
    "collatz_steps": (837799,),
    "mandelbrot_escape": (-0.7436, 0.1318, 2000),
    "trapezoid": (0.0, 3.0, 20000),
    "polynomial": ([1.0, -2.0, 0.5, 3.0, -1.25], 1.75),
    "clamp": (5.5, 0.0, 3.0),
    "is_leap_year": (2000,),
    "digit_sum": (982451653,),
    "power_mod": (7, 1000003, 1000000007),
    "celsius_to_fahrenheit": (36.6,),
    "bmi": (70.0, 1.75),
    "compound_interest": (1000.0, 0.05, 30),
    "running_max": ([float((i * 7919) % 1000) for i in range(3000)],),
    "label_length": ("  kernel  ",),
}


class Strip(ast.NodeTransformer):
    def visit_FunctionDef(self, n):
        for a in n.args.args:
            a.annotation = None
        n.returns = None
        return n


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def build(source, name, engine, work):
    out = os.path.join(work, f"{name}.so")
    args = [PYRS, "build-extension", "-i", source, "--module", name, "--python", sys.executable,
            "-O", "2", "-o", out]
    if engine:
        args += ["--infer", engine]
    r = subprocess.run(args, capture_output=True, text=True, timeout=600)
    return (out if r.returncode == 0 else None), (r.stdout + r.stderr).strip()


def build_each(source_text, engine, work, tag):
    """Build every kernel on its own, so one untypable kernel does not hide the rest."""
    tree = ast.parse(source_text)
    results = {}
    for fn in [n for n in tree.body if isinstance(n, ast.FunctionDef)]:
        src = os.path.join(work, f"{tag}_{fn.name}.py")
        with open(src, "w") as fh:
            fh.write(ast.unparse(fn) + "\n")
        so, log = build(src, f"{tag}_{fn.name}", engine, work)
        results[fn.name] = (so, log, f"{tag}_{fn.name}")
    return results


def timed(f, args, repeat=5, number=None):
    if number is None:
        start = time.perf_counter()
        f(*args)
        once = time.perf_counter() - start
        number = max(1, int(0.05 / max(once, 1e-7)))
    best = float("inf")
    for _ in range(repeat):
        start = time.perf_counter()
        for _ in range(number):
            f(*args)
        best = min(best, (time.perf_counter() - start) / number)
    return best


def main():
    source = os.path.join(HERE, "kernels.py")
    text = open(source).read()
    reference = load("kernels_reference", source)
    stripped = Strip().visit(ast.parse(text))
    ast.fix_missing_locations(stripped)
    stripped_text = ast.unparse(stripped) + "\n"
    report = {}
    with tempfile.TemporaryDirectory(prefix="pyrs-ext-bench-", dir=DATA) as work:
        annotated = build_each(text, None, work, "ann")
        for label, engine, body in [("annotated", None, None), ("static", "static", stripped_text),
                                    ("ml", "ml", stripped_text), ("dl", "dl", stripped_text)]:
            built = annotated if body is None else build_each(body, engine, work, label)
            rows = {}
            for name, (so, log, module_name) in built.items():
                row = {"built": so is not None}
                if so is None:
                    row["error"] = next((l for l in log.splitlines() if "error" in l or "requires" in l), log[:160])
                else:
                    mod = load(module_name, so)
                    f = getattr(mod, name)
                    py = getattr(reference, name)
                    args = INPUTS[name]
                    try:
                        got = f(*args)
                        row["matches"] = got == py(*args) or (
                            isinstance(got, float) and abs(got - py(*args)) <= 1e-9 * max(1.0, abs(got)))
                    except Exception as exc:  # a mispredicted signature refuses the argument
                        row["matches"] = False
                        row["error"] = f"{type(exc).__name__}: {exc}"
                    if row["matches"]:
                        row["speedup"] = timed(py, args) / timed(f, args)
                rows[name] = row
            report[label] = rows
    summary = {}
    for label, rows in report.items():
        built = sum(r["built"] for r in rows.values())
        ok = [r for r in rows.values() if r.get("matches")]
        speed = sorted(r["speedup"] for r in ok)
        geo = (2.718281828 ** (sum(__import__("math").log(s) for s in speed) / len(speed))) if speed else 0
        summary[label] = {"kernels": len(rows), "built": built, "correct": len(ok),
                          "geomean_speedup_of_correct": round(geo, 2)}
    print(json.dumps(summary, indent=1))
    for label, rows in report.items():
        for name, r in rows.items():
            if not r.get("matches"):
                print(f"  {label:9s} {name:22s} {r.get('error', 'wrong result')}")
    os.makedirs(os.path.join(DATA, "results"), exist_ok=True)
    with open(os.path.join(DATA, "results", "extension_bench.json"), "w") as fh:
        json.dump({"summary": summary, "report": report}, fh, indent=1)


if __name__ == "__main__":
    main()
