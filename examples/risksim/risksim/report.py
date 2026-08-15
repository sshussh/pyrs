# Human-readable and machine-readable reports.
# Exercises: f-strings, json.dumps, sorted(key=), set algebra, comprehensions,
# decorators, nested functions, open write/append.

import json
from risksim.models import Portfolio, SimConfig, RunResult, Equity, Bond, Asset
from risksim.sim import histogram


def format_report(portfolio: Portfolio, cfg: SimConfig, result: RunResult) -> str:
    lines: list[str] = []
    lines.append("=== PyRs RiskSim — Monte Carlo portfolio risk ===")
    lines.append(str(portfolio))
    lines.append(str(cfg))
    lines.append("--- assets ---")
    # Sort holdings by weight descending
    holdings = list(portfolio.assets)

    def wkey(a: Asset) -> float:
        return -a.weight

    for a in sorted(holdings, key=wkey):
        tag = "asset"
        if isinstance(a, Equity):
            tag = f"equity/{a.sector}"
        elif isinstance(a, Bond):
            tag = f"bond/dur={a.duration:.1f}"
        lines.append(f"  [{tag}] {a}")
    lines.append("--- risk metrics (terminal wealth) ---")
    for s in result.summary_lines():
        lines.append(s)
    # Sector / type sets
    equity_syms: set[str] = set()
    bond_syms: set[str] = set()
    for a in portfolio.assets:
        if isinstance(a, Equity):
            equity_syms.add(a.symbol)
        elif isinstance(a, Bond):
            bond_syms.add(a.symbol)
    all_syms: set[str] = equity_syms | bond_syms
    def join_names(xs: list[str]) -> str:
        if len(xs) == 0:
            return "[]"
        parts: list[str] = []
        for x in sorted(xs):
            parts.append(x)
        out = parts[0]
        i = 1
        while i < len(parts):
            out = out + "," + parts[i]
            i = i + 1
        return "[" + out + "]"

    lines.append("equity_names=" + join_names(list(equity_syms)))
    lines.append("bond_names=" + join_names(list(bond_syms)))
    lines.append("all_names=" + join_names(list(all_syms)))
    # Compact histogram of terminals
    lines.append("--- terminal wealth histogram (12 bins, scaled sample) ---")
    for edge, count in histogram(result.terminals, 12, result.paths):
        bar_n = count * 40 // max(1, result.paths)
        bar = "#" * bar_n
        lines.append(f"  {edge:12.2f} | {count:6d} {bar}")
    return "\n".join(lines) + "\n"


def write_report(path: str, text: str) -> None:
    with open(path, "w") as f:
        f.write(text)


def write_metrics_json(path: str, result: RunResult) -> None:
    # json.dumps is polymorphic on a dict[str, float] summary.
    payload: dict[str, float] = {
        "mean": result.mean,
        "median": result.median,
        "p05": result.p05,
        "p95": result.p95,
        "var95": result.var95,
        "cvar95": result.cvar95,
        "best": result.best,
        "worst": result.worst,
        "elapsed_s": result.elapsed_s,
        "paths": float(result.paths),
        "steps": float(result.steps),
    }
    with open(path, "w") as f:
        f.write(json.dumps(payload))
        f.write("\n")
