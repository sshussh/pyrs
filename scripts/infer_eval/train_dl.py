"""Train and evaluate the deep-learning type predictor.

The model reads the same evidence as the linear model, but as structure rather
than a bag: name, default and position tokens are pooled from a hashed
embedding table; each usage event is an embedding summed from its parts
(`binop`, `add`, `l`, `int`), and the sequence of events -- in source order --
goes through a small transformer encoder. The two views meet in an MLP that
scores the labels.

Small enough to run inside the compiler (export_dl.py writes the weights and
semantic/src/predict.rs runs the forward pass on the CPU, deterministically).

usage: train_dl.py [--data DIR] [--out DIR] [--epochs N] [--device auto|cuda|cpu]
"""
import os
import sys

sys.path.insert(0, os.path.dirname(__file__))
import repro  # noqa: E402  -- before torch: it sets CUBLAS_WORKSPACE_CONFIG

import argparse  # noqa: E402
import json  # noqa: E402
import math  # noqa: E402
import time  # noqa: E402
from collections import Counter  # noqa: E402

import numpy as np  # noqa: E402
import torch  # noqa: E402
import torch.nn as nn  # noqa: E402
import torch.nn.functional as TF  # noqa: E402

import features as F  # noqa: E402
from train_ml import HELD_OUT, load  # noqa: E402

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
DATA = os.path.join(REPO, "target", "inference-eval")

BUCKETS = 1 << 14
DIM = 64
MAX_NAME = 24
MAX_EV = F.MAX_EVENTS
PARTS = 4


def fnv(token):
    h = 0x811C9DC5
    for b in token.encode():
        h ^= b
        h = (h * 0x01000193) & 0xFFFFFFFF
    return h


def encode(row):
    """Name token ids, and per-event part ids (0 is padding)."""
    names = [1 + fnv("n:" + t) % (BUCKETS - 1) for t in row["feats"][:MAX_NAME]]
    names += [0] * (MAX_NAME - len(names))
    events = []
    for e in row["events"][:MAX_EV]:
        parts = e.split(":")
        ids = [1 + fnv(f"e{i}:{p}") % (BUCKETS - 1) for i, p in enumerate(parts[:PARTS])]
        ids += [0] * (PARTS - len(ids))
        events.append(ids)
    n_ev = len(events)
    events += [[0] * PARTS] * (MAX_EV - n_ev)
    return names, events, n_ev


class TypePredictor(nn.Module):
    def __init__(self, n_labels, layers=2, heads=4, ff=128):
        super().__init__()
        self.emb = nn.Embedding(BUCKETS, DIM, padding_idx=0)
        self.pos = nn.Embedding(MAX_EV + 1, DIM)
        self.cls = nn.Parameter(torch.zeros(DIM))
        enc = nn.TransformerEncoderLayer(DIM, heads, ff, dropout=0.1, batch_first=True,
                                         activation="relu", norm_first=False)
        self.encoder = nn.TransformerEncoder(enc, layers, enable_nested_tensor=False)
        self.head = nn.Sequential(nn.Linear(2 * DIM, 128), nn.ReLU(), nn.Linear(128, n_labels))

    def forward(self, names, events, n_ev):
        name_mask = (names != 0).float().unsqueeze(-1)
        name_vec = (self.emb(names) * name_mask).sum(1) / name_mask.sum(1).clamp(min=1)
        ev = self.emb(events).sum(2)  # (B, MAX_EV, DIM)
        b = ev.shape[0]
        seq = torch.cat([self.cls.expand(b, 1, DIM), ev], dim=1)
        seq = seq + self.pos.weight.unsqueeze(0)
        pad = torch.arange(MAX_EV + 1, device=names.device).unsqueeze(0) > n_ev.unsqueeze(1)
        out = self.encoder(seq, src_key_padding_mask=pad)
        return self.head(torch.cat([name_vec, out[:, 0]], dim=1))


def tensors(rows, device):
    enc = [encode(r) for r in rows]
    names = torch.tensor([e[0] for e in enc], dtype=torch.long, device=device)
    events = torch.tensor([e[1] for e in enc], dtype=torch.long, device=device)
    n_ev = torch.tensor([e[2] for e in enc], dtype=torch.long, device=device)
    y = torch.tensor([F.LABEL_INDEX[r["label"]] for r in rows], dtype=torch.long, device=device)
    return names, events, n_ev, y


def predict(model, t, batch=4096):
    model.eval()
    out = []
    with torch.no_grad():
        for i in range(0, t[0].shape[0], batch):
            out.append(model(t[0][i:i + batch], t[1][i:i + batch], t[2][i:i + batch]).argmax(1))
    return torch.cat(out).cpu().numpy() if out else np.array([])


def train(rows, device, epochs, weights=None, seed=repro.SEED, init=None, lr=2e-3):
    repro.seed_everything(seed)
    model = init if init is not None else TypePredictor(len(F.LABELS)).to(device)
    t = tensors(rows, device)
    w = torch.tensor(weights if weights is not None else [1.0] * len(rows), device=device)
    opt = torch.optim.AdamW(model.parameters(), lr=lr, weight_decay=0.01)
    n = len(rows)
    bs = 512
    steps = epochs * math.ceil(n / bs)
    sched = torch.optim.lr_scheduler.OneCycleLR(opt, max_lr=lr, total_steps=max(steps, 1))
    for ep in range(epochs):
        model.train()
        perm = torch.randperm(n, device=device)
        total = 0.0
        for i in range(0, n, bs):
            idx = perm[i:i + bs]
            logits = model(t[0][idx], t[1][idx], t[2][idx])
            loss = (TF.cross_entropy(logits, t[3][idx], reduction="none") * w[idx]).sum() / w[idx].sum()
            opt.zero_grad()
            loss.backward()
            opt.step()
            sched.step()
            total += loss.item() * len(idx)
        print(f"  epoch {ep + 1}/{epochs} loss {total / n:.4f}", flush=True)
    return model


def project_weights(rows):
    """Each project counts about as much as any other, whatever its size."""
    sizes = Counter(r["project"] for r in rows)
    return [1.0 / math.sqrt(sizes[r["project"]]) for r in rows]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", default=os.path.join(DATA, "data"))
    ap.add_argument("--out", default=os.path.join(DATA, "models"))
    ap.add_argument("--epochs", type=int, default=6)
    ap.add_argument("--device", default="auto", help="auto (prefers a GPU), cuda, xpu, mps or cpu")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    device = repro.pick_device(a.device)
    ext = [r for r in load(os.path.join(a.data, "ext.jsonl")) if r["label"]]
    corpus = [r for r in load(os.path.join(a.data, "corpus.jsonl")) if r["label"]]
    train_rows = [r for r in ext if r["project"] not in HELD_OUT]
    test_rows = [r for r in ext if r["project"] in HELD_OUT]
    t0 = time.time()
    model = train(train_rows, device, a.epochs, weights=project_weights(train_rows))
    train_s = time.time() - t0
    te, co = tensors(test_rows, device), tensors(corpus, device)
    held = float(np.mean(predict(model, te) == te[3].cpu().numpy()))
    pco = predict(model, co)
    corp = float(np.mean(pco == co[3].cpu().numpy()))
    per, tot = Counter(), Counter()
    for t, p in zip(co[3].cpu().numpy().tolist(), pco.tolist()):
        tot[F.LABELS[t]] += 1
        per[F.LABELS[t]] += int(t == p)
    n_params = sum(p.numel() for p in model.parameters())
    res = {"dl": {"train_s": train_s, "params": n_params, "held_out": held, "corpus": corp,
                  "corpus_by_label": {k: f"{per[k]}/{tot[k]}" for k in tot},
                  "environment": repro.environment(device, epochs=a.epochs)}}
    print(json.dumps(res, indent=1))
    torch.save(model.state_dict(), os.path.join(a.out, "dl_model.pt"))
    with open(os.path.join(a.out, "dl_results.json"), "w") as fh:
        json.dump(res, fh, indent=1)


if __name__ == "__main__":
    main()
