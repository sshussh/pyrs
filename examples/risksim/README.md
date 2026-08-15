# RiskSim

A packaged Monte Carlo portfolio-risk CLI used as a PyRs language showcase
and a fair AOT vs CPython race.

It loads a multi-asset `.scenario` file, runs GBM paths, and prints
terminal-wealth mean/median/percentiles, historical VaR/CVaR, and a
histogram. Optional second argument writes a text report plus JSON metrics.

```console
# from the repo root
pyrs run -i examples/risksim/main.py -- examples/risksim/data/balanced.scenario
python3 examples/risksim/main.py examples/risksim/data/balanced.scenario

pyrs compile -O2 -i examples/risksim/main.py -o /tmp/risksim
/tmp/risksim examples/risksim/data/stress.scenario

bash examples/risksim/bench.sh
```

`make examples` includes the balanced scenario. Use `stress.scenario` for
wall-clock races (more paths and assets). Paths are streamed; percentile
stats use a capped sample so large runs stay bounded.
