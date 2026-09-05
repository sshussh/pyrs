# Compatibility evidence

These are **synthetic regression probes**, not a representative sample of Python
projects. Passing them does not establish 1.0 readiness or a percentage of Python
workloads supported. The independent scientific corpus is still required by the
[1.0 plan](../docs/ROADMAP-1.0.md).

Build the compiler, then compare native and explicit CPython execution:

```sh
cargo build -p pyrs
python3 compatibility/run.py --group core --mode both --opt-levels 0 2 3 \
  --gc-stress --output target/compatibility/core.json
```

The science suite needs NumPy and pandas in the selected interpreter. A separate
CPython 3.12 environment reproduces the package versions tested here:

```sh
python3.12 -m venv target/science-venv
target/science-venv/bin/python -m pip install -r compatibility/requirements-science.txt
python3 compatibility/run.py --group all --mode both \
  --python target/science-venv/bin/python \
  --output target/compatibility/science.json
```

`--python` chooses both the reference interpreter and compatibility-mode
interpreter. The report records its actual Python and package versions, compiler
binary hash, manifest/source hashes, source text, commands, optimization levels,
raw output bytes, return codes, timings and file-content hashes. Each execution
gets a fresh fixture in the same temporary directory; output files from the
reference never leak into the compiler execution. Timeouts kill the process
group on POSIX, including compiler/linker children. Missing packages and oracle
failures fail the run instead of silently skipping cases. Run trusted code only;
temporary directories are not a security sandbox.

Native compilation and program execution are separate observations. A known
unsupported case must fail with the recorded compiler diagnostic. A known output
mismatch must still compile and exit successfully; crashes, timeouts and linker
failures are regressions. A newly passing gap fails as `unexpected_pass` until
its manifest expectation is updated. Compatibility mode must match every probe,
including the ones with native gaps. No native gap exemption applies to it.

The `pass`/`known_gap` expectations are regression gates, not release gates. Known
gaps remain failures of native compatibility. Results contain separate counts per
mode; repeated optimization runs are not additional independent workloads.

The initial probes cover native arithmetic/matrix loops, exact numeric
comparisons, exception bindings, file effects, and six scientific workflows:
NumPy broadcasting/indexing/reductions, linear algebra, seeded random sampling,
pandas grouping/joins, CSV round trips and time series. NumPy and pandas currently
execute entirely in CPython through `--compat`; there is no native array ABI or
compiler speedup for that mode.

The separate experimental native extension target has its own boundary tests:

```sh
python3 compatibility/test_extension.py --pyrs target/debug/pyrs
target/science-venv/bin/python compatibility/test_extension.py \
  --pyrs target/debug/pyrs --require-science
target/science-venv/bin/python examples/interop/demo.py \
  --pyrs target/debug/pyrs --output target/compatibility/interop.json
```

These compile and import real extensions at O0/O2/O3 with GC stress. The suite
checks values, exceptions, keyword binding, exact type guards, buffer layouts,
reference/export cleanup, runtime symbol isolation and thread/reentrancy guards.
The standalone run skips the scientific case when packages are missing;
`--require-science` makes missing packages a failure. The scientific CI job uses
that flag. Native extensions require CPython 3.12+ development headers on Linux.
`cargo test --workspace` also runs the boundary suite using `python3`, or
`PYRS_TEST_PYTHON` when specified. This proves a bounded numerical interface; it
does not count arbitrary NumPy/pandas code as native compatibility.

The runner's result-classification tests are part of `make ci`:

```sh
python3 -m unittest discover -s compatibility -p test_runner.py
```
