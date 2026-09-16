"""Export the trained predictors as the compiler's model files.

Weights are written in a small tensor container (format below) with int8
per-row quantization for the large matrices, and checked here: the quantized
model, run by the numpy reference implementation in this file, must agree with
the original on the held-out and corpus sets. semantic/src/predict.rs runs the
same arithmetic.

Container: b"PYRSMDL1", u32 tensor count, then per tensor:
  u16 name length, name (utf-8), u8 kind (0 = f32, 1 = int8 rows + f32 scale
  per row), u8 rank, u32 dims[rank], payload (little-endian).

usage: export_models.py [--models DIR] [--out semantic/models]
"""
import argparse
import datetime
import hashlib
import json
import math
import os
import pickle
import struct
import subprocess
import sys

import numpy as np

sys.path.insert(0, os.path.dirname(__file__))
import features as F  # noqa: E402
import train_dl as D  # noqa: E402
import train_ml as M  # noqa: E402

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
DATA = os.path.join(REPO, "target", "inference-eval")


def write_container(path, tensors):
    with open(path, "wb") as fh:
        fh.write(b"PYRSMDL1")
        fh.write(struct.pack("<I", len(tensors)))
        for name, arr, quantize in tensors:
            arr = np.asarray(arr, dtype=np.float32)
            nb = name.encode()
            fh.write(struct.pack("<H", len(nb)))
            fh.write(nb)
            fh.write(struct.pack("<BB", 1 if quantize else 0, arr.ndim))
            for d in arr.shape:
                fh.write(struct.pack("<I", d))
            if quantize:
                rows = arr.reshape(arr.shape[0], -1)
                scale = np.maximum(np.abs(rows).max(axis=1), 1e-12) / 127.0
                q = np.clip(np.round(rows / scale[:, None]), -127, 127).astype(np.int8)
                fh.write(scale.astype("<f4").tobytes())
                fh.write(q.tobytes())
            else:
                fh.write(arr.astype("<f4").tobytes())


def read_container(path):
    with open(path, "rb") as fh:
        data = fh.read()
    assert data[:8] == b"PYRSMDL1"
    off = 8
    (n,) = struct.unpack_from("<I", data, off)
    off += 4
    out = {}
    for _ in range(n):
        (ln,) = struct.unpack_from("<H", data, off)
        off += 2
        name = data[off:off + ln].decode()
        off += ln
        kind, rank = struct.unpack_from("<BB", data, off)
        off += 2
        dims = struct.unpack_from("<" + "I" * rank, data, off)
        off += 4 * rank
        count = int(np.prod(dims))
        if kind == 1:
            rows = dims[0]
            scale = np.frombuffer(data, "<f4", rows, off)
            off += 4 * rows
            q = np.frombuffer(data, np.int8, count, off).astype(np.float32)
            off += count
            arr = (q.reshape(rows, -1) * scale[:, None]).reshape(dims)
        else:
            arr = np.frombuffer(data, "<f4", count, off).reshape(dims)
            off += 4 * count
        out[name] = arr
    return out


# ---- the reference forward passes -------------------------------------------------

def ml_scores(w, row):
    x = np.zeros(M.DIM, dtype=np.float32)
    counts = {}
    for t in M.tokens(row):
        b = M.bucket(t)
        counts[b] = counts.get(b, 0) + 1
    for b, c in counts.items():
        x[b] = 1.0 + math.log(c)
    return w["coef"] @ x + w["intercept"]


def layer_norm(x, g, b, eps=1e-5):
    mu = x.mean(-1, keepdims=True)
    var = ((x - mu) ** 2).mean(-1, keepdims=True)
    return (x - mu) / np.sqrt(var + eps) * g + b


def dl_scores(w, row, layers=2, heads=4):
    names, events, n_ev = D.encode(row)
    emb = w["emb"]
    name_ids = [i for i in names if i != 0]
    name_vec = emb[name_ids].mean(0) if name_ids else np.zeros(D.DIM, np.float32)
    seq = np.zeros((n_ev + 1, D.DIM), np.float32)
    seq[0] = w["cls"]
    for j in range(n_ev):
        for pid in events[j]:
            if pid:
                seq[j + 1] += emb[pid]
    seq += w["pos"][: n_ev + 1]
    x = seq
    hd = D.DIM // heads
    for l in range(layers):
        p = f"layer{l}."
        qkv = x @ w[p + "in_proj_weight"].T + w[p + "in_proj_bias"]
        q, k, v = np.split(qkv, 3, axis=-1)
        t = x.shape[0]
        q = q.reshape(t, heads, hd).transpose(1, 0, 2)
        k = k.reshape(t, heads, hd).transpose(1, 0, 2)
        v = v.reshape(t, heads, hd).transpose(1, 0, 2)
        att = q @ k.transpose(0, 2, 1) / math.sqrt(hd)
        att = np.exp(att - att.max(-1, keepdims=True))
        att /= att.sum(-1, keepdims=True)
        o = (att @ v).transpose(1, 0, 2).reshape(t, D.DIM)
        o = o @ w[p + "out_proj_weight"].T + w[p + "out_proj_bias"]
        x = layer_norm(x + o, w[p + "norm1_weight"], w[p + "norm1_bias"])
        ff = np.maximum(x @ w[p + "linear1_weight"].T + w[p + "linear1_bias"], 0.0)
        ff = ff @ w[p + "linear2_weight"].T + w[p + "linear2_bias"]
        x = layer_norm(x + ff, w[p + "norm2_weight"], w[p + "norm2_bias"])
    h = np.concatenate([name_vec, x[0]])
    h = np.maximum(h @ w["head0_weight"].T + w["head0_bias"], 0.0)
    return h @ w["head2_weight"].T + w["head2_bias"]


def git_commits(ext_dir):
    """What is actually checked out -- read from the checkouts, not from a list."""
    out = {}
    for d in sorted(os.listdir(ext_dir)):
        p = os.path.join(ext_dir, d)
        if os.path.isdir(os.path.join(p, ".git")):
            r = subprocess.run(["git", "-C", p, "rev-parse", "HEAD"], capture_output=True, text=True)
            url = subprocess.run(["git", "-C", p, "remote", "get-url", "origin"], capture_output=True, text=True)
            out[d] = {"commit": r.stdout.strip(), "url": url.stdout.strip()}
    return out


def provenance(ext_dir):
    """Everything needed to run this training again and get these weights.

    The training data is not pinned -- a retrain takes each project's newest
    commit -- so what makes a run reproducible is this record: the commits that
    were used, the compiler revision, and the content of every script in the
    pipeline. `fetch_projects.py --pinned` replays the commits.
    """
    here = os.path.dirname(os.path.abspath(__file__))
    rev = subprocess.run(["git", "-C", REPO, "rev-parse", "HEAD"], capture_output=True, text=True)
    dirty = subprocess.run(["git", "-C", REPO, "status", "--porcelain"], capture_output=True, text=True)
    out = {
        "pyrs_commit": rev.stdout.strip(),
        "pyrs_worktree_clean": not dirty.stdout.strip(),
        "scripts": {f: sha256(os.path.join(here, f))
                    for f in sorted(os.listdir(here)) if f.endswith(".py")},
        "replay": "scripts/infer_eval/fetch_projects.py --pinned semantic/models/MANIFEST.json",
    }
    fetched = os.path.join(ext_dir, "fetched.json")
    if os.path.exists(fetched):
        rec = json.load(open(fetched))
        out["fetched"] = rec.get("fetched")
        out["fetch_pinned"] = rec.get("pinned")
    return out


def sha256(path):
    with open(path, "rb") as fh:
        return hashlib.sha256(fh.read()).hexdigest()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--models", default=os.path.join(DATA, "models"))
    ap.add_argument("--data", default=os.path.join(DATA, "data"))
    ap.add_argument("--out", default=os.path.join(REPO, "semantic", "models"))
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)

    with open(os.path.join(a.models, "ml_models.pkl"), "rb") as fh:
        ml = pickle.load(fh)
    lr = M.LogReg.from_weights(ml["logreg"]["coef"], ml["logreg"]["intercept"])
    write_container(os.path.join(a.out, "ml.bin"), [
        ("coef", lr.coef_, True),
        ("intercept", lr.intercept_, False),
    ])

    import torch
    sd = torch.load(os.path.join(a.models, "dl_model.pt"), map_location="cpu")
    t = []
    t.append(("emb", sd["emb.weight"].numpy(), True))
    t.append(("pos", sd["pos.weight"].numpy(), False))
    t.append(("cls", sd["cls"].numpy(), False))
    for l in range(2):
        src = f"encoder.layers.{l}."
        for part in ["self_attn.in_proj_weight", "self_attn.in_proj_bias", "self_attn.out_proj.weight",
                     "self_attn.out_proj.bias", "linear1.weight", "linear1.bias", "linear2.weight",
                     "linear2.bias", "norm1.weight", "norm1.bias", "norm2.weight", "norm2.bias"]:
            name = f"layer{l}." + part.replace("self_attn.", "").replace(".", "_")
            t.append((name, sd[src + part].numpy(), False))
    for part in ["0.weight", "0.bias", "2.weight", "2.bias"]:
        t.append(("head" + part.replace(".", "_"), sd["head." + part].numpy(), False))
    write_container(os.path.join(a.out, "dl.bin"), t)

    # Check the quantized models against the originals.
    ext = [r for r in M.load(os.path.join(a.data, "ext.jsonl")) if r["label"]]
    held = [r for r in ext if r["project"] in M.HELD_OUT]
    corpus = [r for r in M.load(os.path.join(a.data, "corpus.jsonl")) if r["label"]]
    wml = read_container(os.path.join(a.out, "ml.bin"))
    wdl = read_container(os.path.join(a.out, "dl.bin"))
    model = D.TypePredictor(len(F.LABELS))
    model.load_state_dict(sd)
    report = {}
    for name, rows in [("held_out", held), ("corpus", corpus)]:
        y = np.array([F.LABEL_INDEX[r["label"]] for r in rows])
        X = M.vectorize(rows)
        p_lr = lr.predict(X)
        p_mlq = np.array([int(np.argmax(ml_scores(wml, r))) for r in rows])
        tens = D.tensors(rows, "cpu")
        p_dl = D.predict(model, tens)
        p_dlq = np.array([int(np.argmax(dl_scores(wdl, r))) for r in rows])
        report[name] = {
            "n": len(rows),
            "ml_accuracy": float(np.mean(p_lr == y)), "ml_quantized_accuracy": float(np.mean(p_mlq == y)),
            "ml_quantized_agreement": float(np.mean(p_lr == p_mlq)),
            "dl_accuracy": float(np.mean(p_dl == y)), "dl_quantized_accuracy": float(np.mean(p_dlq == y)),
            "dl_quantized_agreement": float(np.mean(p_dl == p_dlq)),
        }
    print(json.dumps(report, indent=1))
    manifest = {
        "generated": datetime.date.today().isoformat(),
        "labels": F.LABELS,
        "features": {"script": "scripts/infer_eval/features.py",
                     "sha256": sha256(os.path.join(os.path.dirname(__file__), "features.py"))},
        "ml": {"model": "multinomial logistic regression over hashed features",
               "buckets": M.DIM, "file": "ml.bin", "sha256": sha256(os.path.join(a.out, "ml.bin"))},
        "dl": {"model": "hashed embeddings + 2-layer transformer encoder over usage events + MLP",
               "buckets": D.BUCKETS, "dim": D.DIM, "layers": 2, "heads": 4, "ff": 128,
               "file": "dl.bin", "sha256": sha256(os.path.join(a.out, "dl.bin"))},
        "training_data": git_commits(os.path.join(DATA, "ext")),
        "held_out_projects": M.HELD_OUT,
        "evaluation": report,
        "provenance": provenance(os.path.join(DATA, "ext")),
    }
    # How the shipped weights were trained: device, versions, seed, epochs.
    for source in ("cv_results.json", "dl_results.json"):
        path = os.path.join(a.models, source)
        if os.path.exists(path):
            env = json.load(open(path))
            env = env.get("environment") or env.get("dl", {}).get("environment")
            if env:
                manifest["environment"] = env
                break
    cv = os.path.join(a.models, "cv_results.json")
    if os.path.exists(cv):
        # Shipped models trained on the corpus too: their corpus accuracy
        # above is on training data, and the honest figure is out of fold.
        manifest["trained_on_corpus"] = True
        manifest["corpus_cross_validation"] = json.load(open(cv))
    with open(os.path.join(a.out, "MANIFEST.json"), "w") as fh:
        json.dump(manifest, fh, indent=1)
        fh.write("\n")


if __name__ == "__main__":
    main()
