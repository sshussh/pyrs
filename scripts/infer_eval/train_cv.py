"""In-domain adaptation, measured without leakage.

The external projects are library and application code; the PyRs corpus is
the kind of program PyRs compiles. This trains both predictors on the external
data *plus* four fifths of the corpus and predicts the remaining fifth, five
times, grouping corpus programs by source file so no file is on both sides.
The corpus is one domain whose total weight equals the largest external
project's -- fixed in advance, not tuned on the folds.

Writes out-of-fold accuracy, and with --ship trains the final models on
everything and replaces the exported training artifacts.

usage: train_cv.py [--folds 5] [--ship]
"""
import os
import sys

sys.path.insert(0, os.path.dirname(__file__))
import repro  # noqa: E402  -- before torch: it sets CUBLAS_WORKSPACE_CONFIG

import argparse  # noqa: E402
import json  # noqa: E402
import math  # noqa: E402
import pickle  # noqa: E402
from collections import Counter  # noqa: E402

import numpy as np  # noqa: E402
import torch  # noqa: E402

import features as F  # noqa: E402
import train_dl as D  # noqa: E402
import train_ml as M  # noqa: E402

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
DATA = os.path.join(REPO, "target", "inference-eval")


def weights(rows):
    sizes = Counter(r["project"] for r in rows)
    w = np.array([1.0 / math.sqrt(sizes[r["project"]]) for r in rows])
    largest = max(math.sqrt(n) for p, n in sizes.items() if p != "pyrs-corpus")
    n_corpus = sizes.get("pyrs-corpus", 0)
    if n_corpus:
        for i, r in enumerate(rows):
            if r["project"] == "pyrs-corpus":
                w[i] = largest / n_corpus
    return w * len(w) / w.sum()


def fit_ml(rows, device, max_iter=2000):
    X = M.vectorize(rows)
    y = np.array([F.LABEL_INDEX[r["label"]] for r in rows])
    return M.LogReg(C=4.0, max_iter=max_iter, device=device).fit(X, y, sample_weight=weights(rows))


def fit_dl(rows, device, epochs):
    return D.train(rows, device, epochs, weights=list(weights(rows)))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--folds", type=int, default=5)
    ap.add_argument("--epochs", type=int, default=6)
    ap.add_argument("--ship", action="store_true")
    ap.add_argument("--logreg-iters", type=int, default=2000)
    ap.add_argument("--device", default="auto", help="auto (prefers a GPU), cuda, xpu, mps or cpu")
    a = ap.parse_args()
    repro.seed_everything()
    device = repro.pick_device(a.device)
    data = os.path.join(DATA, "data")
    ext = [r for r in M.load(os.path.join(data, "ext.jsonl")) if r["label"] and r["project"] not in M.HELD_OUT]
    held = [r for r in M.load(os.path.join(data, "ext.jsonl")) if r["label"] and r["project"] in M.HELD_OUT]
    corpus = [r for r in M.load(os.path.join(data, "corpus.jsonl")) if r["label"]]
    groups = sorted({r["origin"].split("#")[0] for r in corpus})
    fold_of = {g: i % a.folds for i, g in enumerate(sorted(groups, key=repro.stable_hash))}
    y = np.array([F.LABEL_INDEX[r["label"]] for r in corpus])
    oof_ml = np.zeros(len(corpus), dtype=int)
    oof_dl = np.zeros(len(corpus), dtype=int)
    for k in range(a.folds):
        test_idx = [i for i, r in enumerate(corpus) if fold_of[r["origin"].split("#")[0]] == k]
        train_rows = ext + [r for i, r in enumerate(corpus) if i not in set(test_idx)]
        test_rows = [corpus[i] for i in test_idx]
        print(f"fold {k}: train {len(train_rows)} test {len(test_rows)}", flush=True)
        lr = fit_ml(train_rows, device, a.logreg_iters)
        oof_ml[test_idx] = lr.predict(M.vectorize(test_rows))
        model = fit_dl(train_rows, device, a.epochs)
        oof_dl[test_idx] = D.predict(model, D.tensors(test_rows, device))
    report = {
        "ml_corpus_out_of_fold": float(np.mean(oof_ml == y)),
        "dl_corpus_out_of_fold": float(np.mean(oof_dl == y)),
        "folds": a.folds,
        "groups": len(groups),
        "environment": repro.environment(device, epochs=a.epochs),
    }
    print(json.dumps(report, indent=1))
    out = os.path.join(DATA, "models")
    os.makedirs(out, exist_ok=True)
    with open(os.path.join(out, "cv_results.json"), "w") as fh:
        json.dump(report, fh, indent=1)
    if a.ship:
        rows = ext + corpus
        lr = fit_ml(rows, device, a.logreg_iters)
        with open(os.path.join(out, "ml_models.pkl"), "wb") as fh:
            pickle.dump(M.saved({"logreg": lr}, F.LABELS), fh)
        model = fit_dl(rows, device, a.epochs)
        torch.save(model.state_dict(), os.path.join(out, "dl_model.pt"))
        yh = np.array([F.LABEL_INDEX[r["label"]] for r in held])
        report["ship_held_out_ml"] = float(np.mean(lr.predict(M.vectorize(held)) == yh))
        report["ship_held_out_dl"] = float(np.mean(D.predict(model, D.tensors(held, device)) == yh))
        print(json.dumps(report, indent=1))
        with open(os.path.join(out, "cv_results.json"), "w") as fh:
            json.dump(report, fh, indent=1)


if __name__ == "__main__":
    main()
