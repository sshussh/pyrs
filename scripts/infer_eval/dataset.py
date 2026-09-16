"""Build the predictor datasets.

ext.jsonl    every parameter in the external projects, labelled from its
             annotation (features.label_of). Training and held-out evaluation.
corpus.jsonl every parameter in the PyRs corpus, labelled from what PyRs
             compiles with the annotations present (`pyrs check --show-types`),
             so the label is in the compiler's vocabulary exactly.

usage: dataset.py [--ext DIR] [--corpus DIR] [--out DIR]
"""
import argparse
import ast
import json
import os
import subprocess
import sys
from collections import Counter
from concurrent.futures import ProcessPoolExecutor, ThreadPoolExecutor

sys.path.insert(0, os.path.dirname(__file__))
import features as F  # noqa: E402

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
DATA = os.path.join(REPO, "target", "inference-eval")
PYRS = os.environ.get("PYRS", os.path.join(REPO, "target", "release", "pyrs"))


PROJECT_CLASSES = {}


def read_source(path):
    try:
        with open(path, encoding="utf-8") as fh:
            src = fh.read()
    except (UnicodeDecodeError, OSError):
        return None
    return None if len(src) > 400_000 else src


def file_classes(args):
    path, root = args
    import warnings
    src = read_source(path)
    if src is None:
        return path, set()
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        return path, F.class_names(src)


def init_worker(classes):
    PROJECT_CLASSES.update(classes)


def file_examples(args):
    path, root = args
    project = os.path.relpath(path, root).split(os.sep)[0]
    src = read_source(path)
    if src is None:
        return []
    import warnings
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        return F.examples_from_source(src, os.path.relpath(path, root), project,
                                      PROJECT_CLASSES.get(project))


def build_ext(root, out):
    files = []
    for dp, dirs, fs in os.walk(root):
        dirs[:] = [d for d in dirs if d not in (".git", "node_modules")]
        for f in fs:
            if f.endswith(".py"):
                files.append((os.path.join(dp, f), root))
    # os.walk yields directory order, which is the filesystem's business and not
    # stable between machines. Row order reaches the solver, so fix it here.
    files.sort()
    classes = {}
    with ProcessPoolExecutor(os.cpu_count() or 4) as ex:
        for path, names in ex.map(file_classes, files, chunksize=64):
            project = os.path.relpath(path, root).split(os.sep)[0]
            classes.setdefault(project, set()).update(names)
    n = 0
    labels = Counter()
    with open(out, "w") as fh, ProcessPoolExecutor(os.cpu_count() or 4, initializer=init_worker,
                                                   initargs=(classes,)) as ex:
        for rows in ex.map(file_examples, files, chunksize=64):
            for r in rows:
                fh.write(json.dumps(r) + "\n")
                n += 1
                labels[r["label"]] += 1
    print(f"ext: {len(files)} files, {n} parameters, {n - labels[None]} labelled")
    print("  ", labels.most_common())


def corpus_rows(row):
    src = row["src"]
    d = os.path.join(DATA, "work-dataset", row["hash"])
    os.makedirs(d, exist_ok=True)
    path = os.path.join(d, "prog.py")
    with open(path, "w") as fh:
        fh.write(src)
    r = subprocess.run([PYRS, "check", "--show-types", "-i", path], capture_output=True, text=True, cwd=d)
    if r.returncode != 0:
        return []
    tree = ast.parse(src)
    class_names = {n.name for n in ast.walk(tree) if isinstance(n, ast.ClassDef)}
    compiled = {}
    for line in r.stdout.splitlines():
        head, _, _ret = line.rpartition(" -> ")
        name, _, params = head.partition("(")
        params = params[:-1] if params.endswith(")") else params
        ps = {}
        for p in split_params(params):
            pn, _, ty = p.partition(": ")
            ps[pn] = ty
        compiled[name] = ps
    out = []
    for ex in F.examples_from_source(src, row["origin"], "pyrs-corpus"):
        ty = compiled.get(ex["func"], {}).get(ex["param"])
        ex["hash"] = row["hash"]
        ex["pyrs_type"] = ty
        ex["label"] = F.pyrs_type_label(ty, class_names) if ty else None
        out.append(ex)
    return out


def split_params(s):
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


def build_corpus(corpus_dir, out):
    rows = json.load(open(os.path.join(corpus_dir, "index.json")))
    n = 0
    labels = Counter()
    with open(out, "w") as fh, ThreadPoolExecutor(os.cpu_count() or 4) as ex:
        for exs in ex.map(corpus_rows, rows):
            for e in exs:
                fh.write(json.dumps(e) + "\n")
                n += 1
                labels[e["label"]] += 1
    print(f"corpus: {len(rows)} programs, {n} parameters, {n - labels[None]} labelled")
    print("  ", labels.most_common())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ext", default=os.path.join(DATA, "ext"))
    ap.add_argument("--corpus", default=os.path.join(DATA, "corpus"))
    ap.add_argument("--out", default=os.path.join(DATA, "data"))
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    build_corpus(a.corpus, os.path.join(a.out, "corpus.jsonl"))
    build_ext(a.ext, os.path.join(a.out, "ext.jsonl"))


if __name__ == "__main__":
    main()
