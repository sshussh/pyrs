"""Measure a type-inference engine on the corpus with its annotations stripped.

Every parameter and return annotation is removed; variable annotations stay.
Per program: does the stripped program compile, and does it still match
CPython. Per parameter and return: the type the engine chose, against the type
PyRs compiles when the annotations are present (`pyrs check --show-types` on
the original). Buckets: exact, dynamic (Any), wrong (a different concrete
type), lost (the program does not compile).

usage: evaluate.py LABEL [--corpus DIR] [--engine static|ml|dl] [--library]

--library removes every module-level statement except definitions and imports
before stripping, so no function has a call site: the setting of a library or
extension, where only the body and the names are evidence.
"""
import argparse
import ast
import collections
import json
import os
import re
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
PYRS = os.environ.get("PYRS", os.path.join(REPO, "target", "release", "pyrs"))
DATA = os.path.join(REPO, "target", "inference-eval")


class Strip(ast.NodeTransformer):
    def visit_FunctionDef(self, n):
        self.generic_visit(n)
        for a in n.args.posonlyargs + n.args.args + n.args.kwonlyargs:
            a.annotation = None
        if n.args.vararg:
            n.args.vararg.annotation = None
        if n.args.kwarg:
            n.args.kwarg.annotation = None
        n.returns = None
        return n


def strip(src, library=False):
    t = ast.parse(src)
    if library:
        # Definitions, imports, and module globals set without calling anything
        # the program defines: what a library module leaves behind.
        defined = {s.name for s in t.body if isinstance(s, (ast.FunctionDef, ast.ClassDef))}

        def calls_user_code(node):
            return any(isinstance(c, ast.Call) and isinstance(c.func, ast.Name) and c.func.id in defined
                       for c in ast.walk(node))

        keep = (ast.FunctionDef, ast.ClassDef, ast.Import, ast.ImportFrom)
        t.body = [s for s in t.body if isinstance(s, keep)
                  or (isinstance(s, (ast.Assign, ast.AnnAssign)) and not calls_user_code(s))]
        t.body.append(ast.parse("print('library')").body[0])
    t = Strip().visit(t)
    ast.fix_missing_locations(t)
    return ast.unparse(t) + "\n"


SIG = re.compile(r"^(?P<name>[^(]+)\((?P<params>.*)\) -> (?P<ret>.*)$")


def split_top(s):
    out, depth, cur = [], 0, ""
    for c in s:
        if c in "[(":
            depth += 1
        elif c in "])":
            depth -= 1
        if c == "," and depth == 0:
            out.append(cur.strip())
            cur = ""
        else:
            cur += c
    if cur.strip():
        out.append(cur.strip())
    return out


def sigs(text):
    res = {}
    for line in text.splitlines():
        m = SIG.match(line)
        if not m:
            continue
        ps = {}
        for p in split_top(m.group("params")):
            n, _, t = p.partition(": ")
            ps[n] = t
        ps["<return>"] = m.group("ret")
        res[m.group("name")] = ps
    return res


def run(cmd, cwd, timeout):
    try:
        r = subprocess.run(cmd, capture_output=True, cwd=cwd, timeout=timeout, stdin=subprocess.DEVNULL)
        return r.returncode, r.stdout.decode(errors="replace"), r.stderr.decode(errors="replace")
    except subprocess.TimeoutExpired:
        return None, "", "timeout"


def evaluate(rows, label, engine_args, library, workers):
    def one(row):
        d = os.path.join(DATA, "eval", label, row["hash"])
        os.makedirs(d, exist_ok=True)
        orig = os.path.join(d, "annotated.py")
        with open(orig, "w") as fh:
            fh.write(row["src"])
        stripped = os.path.join(d, "prog.py")
        with open(stripped, "w") as fh:
            fh.write(strip(row["src"], library))
        rc, out, _ = run([PYRS, "check", "--show-types", "-i", orig], d, 120)
        truth = sigs(out) if rc == 0 else {}
        t0 = time.perf_counter()
        rc, out, err = run([PYRS, "check", "--show-types", *engine_args, "-i", stripped], d, 120)
        check_s = time.perf_counter() - t0
        ok = rc == 0
        got = sigs(out) if ok else {}
        diag = "" if ok else next((l for l in err.splitlines() if "error" in l), err.strip()[:200])
        same = False
        if ok and not library:
            prc, pout, perr = run([PYRS, "run", "--no-cache", "-O", "0", *engine_args, "-i", stripped], d, 300)
            same = prc == 0 and pout == row["stdout"]
            if not same:
                diag = "MISMATCH " + next((l for l in perr.splitlines() if l.strip()), "")[:200]
        params = []
        for fn, ps in truth.items():
            for pn, tt in ps.items():
                if pn in ("self", "cls"):
                    continue
                g = got.get(fn, {}).get(pn)
                if not ok or g is None:
                    b = "lost"
                elif g == tt:
                    b = "exact"
                elif g == "Any":
                    b = "dynamic"
                else:
                    b = "wrong"
                params.append({"func": fn, "param": pn, "truth": tt, "got": g, "bucket": b})
        return {"hash": row["hash"], "origin": row["origin"], "compiles": ok, "matches": same,
                "diag": diag, "params": params, "check_s": check_s}

    with ThreadPoolExecutor(workers) as ex:
        return list(ex.map(one, rows))


def report(res, label, library):
    n = len(res)
    c = sum(r["compiles"] for r in res)
    s = sum(r["matches"] for r in res)
    head = f"[{label}] programs: {n}  compile stripped: {c} ({c / n:.1%})"
    if not library:
        head += f"  match CPython: {s} ({s / n:.1%})"
    print(head)
    for kind in ("param", "return"):
        ps = [p for r in res for p in r["params"] if (p["param"] == "<return>") == (kind == "return")]
        b = collections.Counter(p["bucket"] for p in ps)
        tot = len(ps) or 1
        print(f"  {kind}s: {len(ps)}  " + "  ".join(
            f"{k} {b[k]} ({b[k] / tot:.1%})" for k in ("exact", "dynamic", "wrong", "lost")))
    diags = collections.Counter(re.sub(r"'[^']*'", "'_'", r["diag"].split("]:", 1)[-1].strip())[:110]
                                for r in res if r["diag"])
    if diags:
        print("  failures:")
        for k, v in diags.most_common(15):
            print(f"   {v:4d}  {k}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("label")
    ap.add_argument("--corpus", default=os.path.join(DATA, "corpus"))
    ap.add_argument("--engine", default=None, help="passed as --infer ENGINE")
    ap.add_argument("--library", action="store_true")
    ap.add_argument("--workers", type=int, default=os.cpu_count() or 4)
    a = ap.parse_args()
    rows = json.load(open(os.path.join(a.corpus, "index.json")))
    engine_args = ["--infer", a.engine] if a.engine else []
    res = evaluate(rows, a.label, engine_args, a.library, a.workers)
    os.makedirs(os.path.join(DATA, "results"), exist_ok=True)
    with open(os.path.join(DATA, "results", a.label + ".json"), "w") as fh:
        json.dump(res, fh, indent=1)
    report(res, a.label, a.library)


if __name__ == "__main__":
    main()
