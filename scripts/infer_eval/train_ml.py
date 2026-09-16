"""Train and evaluate the classical-ML type predictor.

Inputs are the name, default and usage-event features of features.py, hashed
into a fixed-width sparse vector. Candidates: multinomial logistic regression,
a depth-limited decision tree, and gradient-boosted trees; the chosen model is
exported for the compiler (export_ml.py).

Evaluation is out of project: five external projects never seen in training,
and the PyRs corpus, which no model trains on.

usage: train_ml.py [--data DIR] [--out DIR]
"""
import os
import sys

sys.path.insert(0, os.path.dirname(__file__))
import repro  # noqa: E402  -- before torch: it sets CUBLAS_WORKSPACE_CONFIG

import argparse  # noqa: E402
import json  # noqa: E402
import time  # noqa: E402
from collections import Counter  # noqa: E402

import numpy as np  # noqa: E402
import torch  # noqa: E402
import torch.nn.functional as TF  # noqa: E402
from scipy import sparse  # noqa: E402
from sklearn.tree import DecisionTreeClassifier  # noqa: E402
from sklearn.ensemble import HistGradientBoostingClassifier  # noqa: E402

import features as F  # noqa: E402

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
DATA = os.path.join(REPO, "target", "inference-eval")
HELD_OUT = ["black", "flask", "httpx", "rich", "networkx"]
DIM = 1 << 14


def bucket(token):
    """FNV-1a 32-bit, identical in predict.rs."""
    h = 0x811C9DC5
    for b in token.encode():
        h ^= b
        h = (h * 0x01000193) & 0xFFFFFFFF
    return h % DIM


def tokens(row):
    toks = list(row["feats"])
    ev = row["events"]
    toks += [f"ev:{e}" for e in ev]
    # the operation alone, without its operand kind, generalises further
    toks += [f"evop:{e.split(':')[0]}:{e.split(':')[1] if ':' in e else ''}" for e in ev]
    return toks


def vectorize(rows):
    indptr, indices, data = [0], [], []
    for r in rows:
        counts = Counter(bucket(t) for t in tokens(r))
        for k, v in counts.items():
            indices.append(k)
            data.append(1.0 + np.log(v))
        indptr.append(len(indices))
    return sparse.csr_matrix((np.array(data, dtype=np.float32), np.array(indices), np.array(indptr)),
                             shape=(len(rows), DIM))



class LogReg:
    """Multinomial logistic regression, trained on the GPU with L-BFGS.

    scikit-learn's `saga` needed thousands of passes over 290k hashed rows and
    still stopped at `max_iter` with a ConvergenceWarning -- a model whose
    coefficients are wherever the optimiser happened to be when the budget ran
    out. The objective is small and convex (25 x 16384 parameters), so full-batch
    L-BFGS solves it properly, and on the accelerator the whole dataset is one
    `embedding_bag` per step.

    Exposes `coef_` and `intercept_` so export_models.py and the evaluation code
    do not care which optimiser produced them.
    """

    def __init__(self, C=4.0, max_iter=2000, tol=1e-7, device="cpu", dtype=torch.float64):
        self.C, self.max_iter, self.tol, self.device = C, max_iter, tol, device
        # float64: the stopping test is a gradient norm, and in float32 the
        # gradient bottoms out around 1e-5 in rounding noise, so "converged"
        # would mean "as close as float32 can see" rather than converged.
        self.dtype = dtype
        self.coef_ = None
        self.intercept_ = None
        self.iterations = 0
        self.converged = False

    def _bag(self, X):
        """CSR -> the flat (indices, weights, offsets) an embedding_bag reads."""
        return (torch.from_numpy(X.indices.astype(np.int64)).to(self.device),
                torch.from_numpy(X.data).to(self.device, self.dtype),
                torch.from_numpy(X.indptr[:-1].astype(np.int64)).to(self.device))

    def fit(self, X, y, sample_weight=None):
        repro.seed_everything()
        dev = self.device
        idx, val, off = self._bag(X)
        yt = torch.from_numpy(np.asarray(y, dtype=np.int64)).to(dev)
        n, n_labels = X.shape[0], int(yt.max().item()) + 1
        w = torch.ones(n, device=dev, dtype=self.dtype) if sample_weight is None else \
            torch.from_numpy(np.asarray(sample_weight)).to(dev, self.dtype)
        weight = torch.zeros(X.shape[1], n_labels, device=dev, dtype=self.dtype, requires_grad=True)
        bias = torch.zeros(n_labels, device=dev, dtype=self.dtype, requires_grad=True)
        # sklearn minimises C * sum(loss) + |w|^2 / 2; same objective per sample.
        l2 = 1.0 / (self.C * n)
        opt = torch.optim.LBFGS([weight, bias], max_iter=self.max_iter, tolerance_grad=self.tol,
                                tolerance_change=0.0, history_size=10, line_search_fn="strong_wolfe")

        def closure():
            opt.zero_grad(set_to_none=True)
            logits = TF.embedding_bag(idx, weight, off, per_sample_weights=val, mode="sum") + bias
            loss = (TF.cross_entropy(logits, yt, reduction="none") * w).sum() / w.sum()
            loss = loss + 0.5 * l2 * (weight.square().sum() + bias.square().sum())
            loss.backward()
            return loss

        loss = opt.step(closure)
        state = opt.state[opt._params[0]]
        self.iterations = int(state.get("n_iter", 0))
        grad = torch.cat([weight.grad.flatten(), bias.grad.flatten()]).abs().max().item()
        self.converged = grad <= self.tol
        if not self.converged:
            print(f"  logreg: stopped at max_iter={self.max_iter} with |grad|={grad:.2e}; "
                  f"raise --logreg-iters", flush=True)
        self.loss_ = float(loss.detach())
        self.grad_ = grad
        self.coef_ = weight.detach().T.contiguous().cpu().numpy().astype(np.float32)
        self.intercept_ = bias.detach().cpu().numpy().astype(np.float32)
        return self

    @classmethod
    def from_weights(cls, coef, intercept):
        m = cls()
        m.coef_, m.intercept_ = np.asarray(coef), np.asarray(intercept)
        return m

    def decision_function(self, X):
        return X @ self.coef_.T + self.intercept_

    def predict(self, X):
        return np.asarray(self.decision_function(X).argmax(axis=1)).ravel()


def saved(models, labels):
    """What goes in the pickle: the shipped model as plain arrays.

    Pickling the estimator itself stored it as `__main__.LogReg`, so unpickling
    from any other entry point -- export_models.py, for one -- failed. Arrays
    have no such problem, and `LogReg.from_weights` puts them back.
    """
    lr = models["logreg"]
    out = {"logreg": {"coef": lr.coef_, "intercept": lr.intercept_},
           "dim": DIM, "labels": labels}
    baselines = {k: v for k, v in models.items() if k != "logreg"}
    if baselines:
        out["models"] = baselines
    return out


def load(path):
    with open(path) as fh:
        return [json.loads(l) for l in fh]


def accuracy(y_true, y_pred):
    return float(np.mean(np.array(y_true) == np.array(y_pred))) if y_true else 0.0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", default=os.path.join(DATA, "data"))
    ap.add_argument("--out", default=os.path.join(DATA, "models"))
    ap.add_argument("--device", default="auto", help="auto (prefers a GPU), cuda, xpu, mps or cpu")
    ap.add_argument("--logreg-iters", type=int, default=2000,
                    help="L-BFGS iteration budget (1316 were needed at 290k rows)")
    ap.add_argument("--skip-baselines", action="store_true",
                    help="fit only the model that ships, not the tree and boosting candidates")
    a = ap.parse_args()
    repro.seed_everything()
    device = repro.pick_device(a.device)
    os.makedirs(a.out, exist_ok=True)
    ext = [r for r in load(os.path.join(a.data, "ext.jsonl")) if r["label"]]
    corpus = [r for r in load(os.path.join(a.data, "corpus.jsonl")) if r["label"]]
    train = [r for r in ext if r["project"] not in HELD_OUT]
    test = [r for r in ext if r["project"] in HELD_OUT]
    print(f"train {len(train)}  held-out {len(test)}  corpus {len(corpus)}")
    labels = F.LABELS
    yi = lambda rows: np.array([F.LABEL_INDEX[r["label"]] for r in rows])
    Xtr, ytr = vectorize(train), yi(train)
    Xte, yte = vectorize(test), yi(test)
    Xco, yco = vectorize(corpus), yi(corpus)
    majority = Counter(ytr.tolist()).most_common(1)[0][0]
    results = {"environment": repro.environment(device), "majority": {
        "held_out": accuracy(yte.tolist(), [majority] * len(yte)),
        "corpus": accuracy(yco.tolist(), [majority] * len(yco)),
    }}
    models = {}

    # Each project counts about as much as any other, whatever its size: the
    # same weighting the deep model trains with.
    sizes = Counter(r["project"] for r in train)
    wtr = np.array([1.0 / np.sqrt(sizes[r["project"]]) for r in train])
    wtr *= len(wtr) / wtr.sum()
    t0 = time.time()
    lr = LogReg(C=4.0, max_iter=a.logreg_iters, device=device).fit(Xtr, ytr, sample_weight=wtr)
    models["logreg"] = lr
    results["logreg"] = {"train_s": time.time() - t0, "iterations": lr.iterations,
                         "converged": lr.converged, "loss": lr.loss_, "grad_inf_norm": lr.grad_}

    if a.skip_baselines:
        for name, m in models.items():
            results[name]["held_out"] = accuracy(yte.tolist(), m.predict(Xte).tolist())
            results[name]["corpus"] = accuracy(yco.tolist(), m.predict(Xco).tolist())
        print(json.dumps(results, indent=1))
        with open(os.path.join(a.out, "ml_results.json"), "w") as fh:
            json.dump(results, fh, indent=1)
        import pickle
        with open(os.path.join(a.out, "ml_models.pkl"), "wb") as fh:
            pickle.dump(saved(models, labels), fh)
        return

    t0 = time.time()
    tree = DecisionTreeClassifier(max_depth=18, min_samples_leaf=20, random_state=repro.SEED)
    tree.fit(Xtr, ytr, sample_weight=wtr)
    models["tree"] = tree
    results["tree"] = {"train_s": time.time() - t0, "leaves": int(tree.get_n_leaves())}

    # Gradient boosting over the densest columns only (it needs dense input).
    t0 = time.time()
    colsum = np.asarray((Xtr > 0).sum(axis=0)).ravel()
    top = np.argsort(-colsum)[:1500]
    gb = HistGradientBoostingClassifier(max_iter=200, learning_rate=0.1, max_leaf_nodes=31,
                                        random_state=repro.SEED)
    rng = np.random.default_rng(repro.SEED)
    idx = rng.choice(Xtr.shape[0], size=min(120_000, Xtr.shape[0]), replace=False)
    gb.fit(Xtr[idx][:, top].toarray(), ytr[idx], sample_weight=wtr[idx])
    models["gboost"] = (gb, top)
    results["gboost"] = {"train_s": time.time() - t0}

    for name, m in models.items():
        if name == "gboost":
            g, cols = m
            pte = g.predict(Xte[:, cols].toarray())
            pco = g.predict(Xco[:, cols].toarray())
        else:
            pte, pco = m.predict(Xte), m.predict(Xco)
        results[name]["held_out"] = accuracy(yte.tolist(), pte.tolist())
        results[name]["corpus"] = accuracy(yco.tolist(), pco.tolist())
        per = Counter()
        tot = Counter()
        for t, p in zip(yco.tolist(), pco.tolist()):
            tot[labels[t]] += 1
            per[labels[t]] += int(t == p)
        results[name]["corpus_by_label"] = {k: f"{per[k]}/{tot[k]}" for k in tot}
    print(json.dumps(results, indent=1))
    with open(os.path.join(a.out, "ml_results.json"), "w") as fh:
        json.dump(results, fh, indent=1)
    import pickle
    with open(os.path.join(a.out, "ml_models.pkl"), "wb") as fh:
        pickle.dump(saved(models, labels), fh)


if __name__ == "__main__":
    main()
