# Core Monte Carlo engine: multi-asset geometric Brownian motion.
# Hot path: float + nested loops — where PyRs shows large AOT speedups.
#
# Memory: stream path results and keep only a fixed-size sample for
# percentiles/histogram (no list of all terminals).

from math import exp, sqrt
from risksim.models import Portfolio, SimConfig, RunResult, Asset
from risksim.rng import Rng

# Cap for order-statistic sample (VaR / median / histogram). Independent of path count.
_SAMPLE_CAP: int = 2048


def _step_return(a: Asset, z: float, dt: float) -> float:
    # GBM simple return over dt.
    sig = a.sigma
    drift = (a.mu - 0.5 * sig * sig) * dt
    shock = sig * sqrt(dt) * z
    return exp(drift + shock) - 1.0


def simulate_path(portfolio: Portfolio, cfg: SimConfig, rng: Rng) -> float:
    wealth = portfolio.capital
    dt = cfg.dt
    steps = cfg.steps
    assets = portfolio.assets
    n = len(assets)
    step = 0
    while step < steps:
        r = 0.0
        i = 0
        while i < n:
            z = rng.next_gauss()
            r = r + assets[i].weight * _step_return(assets[i], z, dt)
            i = i + 1
        wealth = wealth * (1.0 + r)
        if wealth <= 0.0:
            return 0.0
        step = step + 1
    return wealth


def run_simulation(portfolio: Portfolio, cfg: SimConfig) -> RunResult:
    p = portfolio.normalized()
    rng = Rng(cfg.seed)

    sample: list[float] = []
    sum_w = 0.0
    best = -1.0e300
    worst = 1.0e300
    k = 0
    while k < cfg.paths:
        w = simulate_path(p, cfg, rng)
        sum_w = sum_w + w
        if w > best:
            best = w
        if w < worst:
            worst = w
        # Reservoir-style: keep first SAMPLE_CAP, then skip (simple prefix sample).
        # Prefix of a long MC run is fine for demo quantiles at fixed seed.
        if k < _SAMPLE_CAP:
            sample.append(w)
        k = k + 1

    n = cfg.paths
    mean = sum_w / float(n)
    ordered = sorted(sample)
    sn = len(ordered)
    mid = sn // 2
    if sn % 2 == 0 and sn >= 2:
        median = 0.5 * (ordered[mid - 1] + ordered[mid])
    else:
        median = ordered[mid]
    i05 = int(float(sn) * 0.05)
    if i05 >= sn:
        i05 = sn - 1
    if i05 < 0:
        i05 = 0
    i95 = int(float(sn) * 0.95)
    if i95 >= sn:
        i95 = sn - 1
    p05 = ordered[i05]
    p95 = ordered[i95]
    var95 = p.capital - p05
    tail_sum = 0.0
    t = 0
    while t <= i05:
        tail_sum = tail_sum + ordered[t]
        t = t + 1
    tail_n = i05 + 1
    cvar95 = p.capital - (tail_sum / float(tail_n))

    return RunResult(
        sample,
        mean,
        median,
        var95,
        cvar95,
        p05,
        p95,
        best,
        worst,
        0.0,
        cfg.paths,
        cfg.steps,
    )


def histogram(sample: list[float], bins: int, paths: int) -> list[tuple[float, int]]:
    if bins < 1:
        raise ValueError("bins must be >= 1")
    lo = min(sample)
    hi = max(sample)
    if hi <= lo:
        return [(lo, paths)]
    width = (hi - lo) / float(bins)
    counts: list[int] = [0 for _ in range(bins)]
    for v in sample:
        idx = int((v - lo) / width)
        if idx >= bins:
            idx = bins - 1
        if idx < 0:
            idx = 0
        counts[idx] = counts[idx] + 1
    # Scale sample bin counts up to full path count for display.
    scale = float(paths) / float(len(sample))
    out: list[tuple[float, int]] = []
    i = 0
    while i < bins:
        edge = lo + width * float(i)
        scaled = int(float(counts[i]) * scale + 0.5)
        out.append((edge, scaled))
        i = i + 1
    return out
