"""Determinism and device selection, shared by the training scripts.

Import this *before* torch. cuBLAS reads CUBLAS_WORKSPACE_CONFIG when the CUDA
context is created, and without it `torch.use_deterministic_algorithms` refuses
to run some matmuls on the GPU -- so the variable has to be set before anything
touches torch.

What determinism means here: the same commits, the same scripts and the same
seed produce the same weights, on the same device. GPU and CPU results are not
bit-identical to each other and are not meant to be; what is recorded in the
manifest is which one ran.
"""
import os

# Must precede the CUDA context. `:4096:8` is the setting cuBLAS documents for
# deterministic reductions.
os.environ.setdefault("CUBLAS_WORKSPACE_CONFIG", ":4096:8")

import platform  # noqa: E402
import random  # noqa: E402
import zlib  # noqa: E402

SEED = 0


def stable_hash(text: str) -> int:
    """A hash that does not move between interpreters.

    `hash()` on a str is salted per process (PYTHONHASHSEED), so using it to
    assign cross-validation folds made the folds -- and the accuracy they
    report -- different on every run.
    """
    return zlib.crc32(text.encode())


def seed_everything(seed: int = SEED) -> None:
    random.seed(seed)
    try:
        import numpy as np

        np.random.seed(seed)
    except ImportError:
        pass
    try:
        import torch
    except ImportError:
        return
    torch.manual_seed(seed)
    torch.cuda.manual_seed_all(seed)
    torch.backends.cudnn.deterministic = True
    torch.backends.cudnn.benchmark = False
    # warn_only: an op with no deterministic kernel should degrade to a warning
    # in a training run, not abort it hours in.
    torch.use_deterministic_algorithms(True, warn_only=True)
    # The fused attention backends pick non-deterministic reductions in their
    # backward pass. The sequences here are at most 49 tokens of 64 dims, so the
    # math backend costs almost nothing and is reproducible.
    if hasattr(torch.backends, "cuda"):
        torch.backends.cuda.enable_mem_efficient_sdp(False)
        torch.backends.cuda.enable_flash_sdp(False)
        torch.backends.cuda.enable_math_sdp(True)


def pick_device(requested: str = "auto") -> str:
    """The device to train on. `auto` prefers a GPU and falls back to the CPU.

    An explicitly named device that is not there is an error: a run asked for
    `--device cuda` should fail loudly rather than quietly take 40x longer.
    """
    import torch

    def available(name: str) -> bool:
        if name == "cuda":
            return torch.cuda.is_available()
        if name == "mps":
            return getattr(torch.backends, "mps", None) is not None and torch.backends.mps.is_available()
        if name == "xpu":
            return hasattr(torch, "xpu") and torch.xpu.is_available()
        return name == "cpu"

    if requested != "auto":
        if not available(requested):
            raise SystemExit(f"--device {requested} is not available in this torch build")
        chosen = requested
    else:
        chosen = next((d for d in ("cuda", "xpu", "mps") if available(d)), "cpu")
        if chosen == "cpu":
            print("  no GPU visible to torch; training on the CPU")
    print(f"  device: {chosen}{' — ' + describe(chosen) if chosen != 'cpu' else ''}", flush=True)
    return chosen


def describe(device: str) -> str:
    import torch

    if device == "cuda":
        return f"{torch.cuda.get_device_name(0)}, CUDA {torch.version.cuda}"
    return device


def environment(device: str, **extra) -> dict:
    """What the run was: recorded in the model manifest, for reproducing it."""
    info = {
        "python": platform.python_version(),
        "platform": platform.platform(),
        "device": device,
        "seed": SEED,
        **extra,
    }
    for name in ("numpy", "scipy", "sklearn", "torch"):
        try:
            info[name] = __import__(name).__version__
        except ImportError:
            pass
    try:
        import torch

        if device == "cuda" and torch.cuda.is_available():
            info["gpu"] = torch.cuda.get_device_name(0)
            info["cuda"] = torch.version.cuda
    except ImportError:
        pass
    return info
