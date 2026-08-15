# RiskSim — Monte Carlo portfolio risk CLI (PyRs showcase + useful tool).
#
# What it does: load a multi-asset portfolio scenario, run GBM Monte Carlo
# paths, report terminal-wealth mean/median/percentiles, historical VaR/CVaR,
# and a histogram. Optionally write a text report + JSON metrics.
#
# Performance: hot path is pure float nested loops (paths × steps × assets).
# Fair race (compile once, time the native binary):
#   cargo build -p pyrs --release
#   bash examples/risksim/bench.sh
#   # or:
#   pyrs compile -O2 -i examples/risksim/main.py -o /tmp/risksim
#   time python3 examples/risksim/main.py examples/risksim/data/stress.scenario
#   time /tmp/risksim examples/risksim/data/stress.scenario
#
# Usage:
#   pyrs run -i examples/risksim/main.py -- examples/risksim/data/balanced.scenario
#   pyrs run -i examples/risksim/main.py -- examples/risksim/data/stress.scenario /tmp/out.txt
#   python3 examples/risksim/main.py examples/risksim/data/balanced.scenario
#
# Memory note: paths are streamed and the percentile sample is capped so large
# path counts stay bounded (the collector is mark–sweep, not a reason to hoard).
#
# Features exercised (non-exhaustive): packages, classes + inheritance + super,
# classmethod/staticmethod/property, __str__/__len__/__bool__/__contains__,
# isinstance peels, match, generators, with/files, exceptions, assert, walrus,
# closures, sorted(key=), set algebra, f-strings, math/os.path/json, sys.argv.

import sys
from os.path import join, dirname, basename
from risksim import VERSION
from risksim.parse import parse_scenario, load_directive_lines, count_lines_gen
from risksim.sim import run_simulation
from risksim.report import format_report, write_report, write_metrics_json
from risksim.models import Portfolio


def usage() -> None:
    print("RiskSim v" + VERSION)
    print("usage: risksim SCENARIO [REPORT_PATH]")
    print("  SCENARIO   path to .scenario file")
    print("  REPORT_PATH  optional text report; also writes REPORT_PATH.json metrics")


def main() -> int:
    argv = sys.argv
    if len(argv) < 2:
        usage()
        return 1
    # Walrus + truthiness
    if (cmd := argv[1]) == "-h" or cmd == "--help":
        usage()
        return 0

    scenario = argv[1]
    report_path: str | None = None
    if len(argv) >= 3:
        report_path = argv[2]

    # Load directives (validates readability) + exercise a small generator.
    n_dir = 0
    try:
        n_dir = len(load_directive_lines(scenario))
        gen_n = 0
        for _i in count_lines_gen(scenario):
            gen_n = gen_n + 1
        if gen_n != n_dir:
            raise RuntimeError("generator count mismatch")
    except FileNotFoundError as e:
        print(f"error: cannot read scenario: {e}")
        return 1
    except OSError as e:
        print(f"error: os error: {e}")
        return 1

    try:
        portfolio, cfg = parse_scenario(scenario)
    except ValueError as e:
        print(f"error: {e}")
        return 1

    portfolio = portfolio.normalized()
    if "SPY" in portfolio:
        print(f"note: portfolio includes SPY (broad equity sleeve)")

    print(f"RiskSim {VERSION} | scenario={basename(scenario)} directives={n_dir}")
    print(f"running {cfg.paths} paths × {cfg.steps} steps × {len(portfolio)} assets …")

    # --- timed region (same boundary for pyrs and python3) ---
    result = run_simulation(portfolio, cfg)
    # Main does not have high-res timers in the language subset; callers
    # should wrap the whole process with `time`. elapsed_s left 0 here;
    # process wall time is what benchmarks use.
    # --- end timed region ---

    text = format_report(portfolio, cfg, result)
    print(text)

    if report_path is not None:
        write_report(report_path, text)
        # metrics beside report: /path/out.txt -> /path/out.txt.json is odd;
        # write basename.json in same dir when possible.
        metrics = report_path + ".json"
        write_metrics_json(metrics, result)
        print(f"wrote report {report_path}")
        print(f"wrote metrics {metrics}")

    # Demo join/dirname on report location (os.path)
    if report_path is not None:
        d = dirname(report_path)
        if d == "":
            d = "."
        print(f"report_dir={join(d, '.')}")

    return 0


# Script entry: top-level statements run; no need for if __name__
code = main()
if code != 0:
    # Nonzero via raise so `pyrs run` surfaces failure similarly
    raise RuntimeError(f"exit {code}")
