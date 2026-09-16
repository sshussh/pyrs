"""Check that the compiler's feature extractor agrees with features.py.

The models are trained on features.py's output and run on the compiler's, so
any disagreement is a silent accuracy loss. For every corpus program, compare
`pyrs check --show-features` with the Python extractor, parameter by parameter.

usage: parity.py [--corpus DIR] [--show N]
"""
import argparse
import json
import os
import subprocess
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor

sys.path.insert(0, os.path.dirname(__file__))
import features as F  # noqa: E402

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
DATA = os.path.join(REPO, "target", "inference-eval")
PYRS = os.environ.get("PYRS", os.path.join(REPO, "target", "release", "pyrs"))


def compare(row):
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "prog.py")
        with open(path, "w") as fh:
            fh.write(row["src"])
        r = subprocess.run([PYRS, "check", "--show-features", "-i", path], capture_output=True, text=True)
    if r.returncode != 0:
        return [("rust-failed", row["origin"], r.stderr.strip()[:200], None)]
    rust = {}
    for line in r.stdout.splitlines():
        o = json.loads(line)
        rust[(o["func"], o["param"])] = (o["feats"], o["events"])
    out = []
    py = {(e["func"], e["param"]): (e["feats"], e["events"]) for e in F.examples_from_source(row["src"], "", "")}
    for k in sorted(set(py) | set(rust)):
        if k not in rust or k not in py:
            out.append(("missing", row["origin"], k, (py.get(k), rust.get(k))))
        elif py[k] != rust[k]:
            out.append(("differs", row["origin"], k, (py[k], rust[k])))
        else:
            out.append(("same", row["origin"], k, None))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", default=os.path.join(DATA, "corpus"))
    ap.add_argument("--show", type=int, default=12)
    a = ap.parse_args()
    rows = json.load(open(os.path.join(a.corpus, "index.json")))
    with ThreadPoolExecutor(os.cpu_count() or 4) as ex:
        results = [x for xs in ex.map(compare, rows) for x in xs]
    counts = {}
    for kind, *_ in results:
        counts[kind] = counts.get(kind, 0) + 1
    total = sum(v for k, v in counts.items() if k != "rust-failed")
    print(f"parameters: {total}  " + "  ".join(f"{k} {v}" for k, v in sorted(counts.items())))
    if total:
        print(f"parity: {counts.get('same', 0) / total:.1%}")
    shown = 0
    for kind, origin, key, detail in results:
        if kind != "same" and shown < a.show:
            print(f"  {kind} {origin} {key}")
            if detail:
                print(f"    py:   {detail[0]}")
                print(f"    rust: {detail[1]}")
            shown += 1


if __name__ == "__main__":
    main()
