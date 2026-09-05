# Changelog

## Unreleased — toward 1.0

The release remains 0.82.0 while the [1.0 readiness plan](docs/ROADMAP-1.0.md)
is implemented. Native compilation is the default; the target workload family
is scientific/data Python, including NumPy and pandas through optional CPython
compatibility execution.

- Python-style script, `-c` and stdin invocation, with script arguments and
  native `sys.argv[0]` preserved on Unix. Existing compiler subcommands remain.
- Explicit `--compat` executes a whole script, command or `-m` package using
  CPython. Select an environment with `--python` or `PYRS_PYTHON`. This mode uses
  installed Python packages and preserves their semantics; it does not compile
  them or accelerate them. It is never an automatic retry of native execution.
- `pyrs check -i script.py` checks the native frontend/import graph without
  linking or executing code. Optimization levels outside 0–3 are rejected.
- Private, atomically created temporary build directories with automatic cleanup
  on normal completion and ordinary compiler errors.
- Exact mixed integer/float comparisons, including bigints, fractional values,
  NaN and infinities, and numeric equality through boxed list values.
- Correct bigint-to-float ties-to-even rounding and overflow, and float-to-int
  conversion at the positive small-integer boundary (`2**62`).
- Binding checks for conditionally assigned locals, including zero/False/None
  values and generator suspension. Locals modified inside `try` survive exception
  handling at optimized compilation levels. Module globals, deletion and static
  use-before-assignment diagnostics remain separate work.
- Preserve side effects and unbound reads in None identity comparisons and
  `print(sep=.../end=...)` expressions with a None result.
- Versioned compatibility probes and reports that distinguish native gaps from
  compatibility passes and compare streams, exit status and generated files.
- Experimental `build-extension` target: compile numerical function modules into
  CPython extensions on Linux. Native library analysis/emission is separate from
  the Python adapter and does not require or automatically call `main()`.
- Exact scalar boundary guards, arbitrary-size integer conversion, keyword
  binding, native exception translation and read-only 1D float64 buffer borrowing.
  Python lists of floats use temporary copies; NumPy/pandas buffers can pass
  without copying. Buffer leases are released on ordinary success and failure.
- Native extension boundary tests at O0/O2/O3 under GC stress, including lifetime,
  thread/reentrancy, symbol isolation, and scientific package checks. The
  `examples/interop` demo verifies results and measures full call overhead.
- Development is isolated on `v1.0-development`. The bridge remains an explicit
  numerical API; automatic mixed execution, general objects/arrays and a stable
  standalone library ABI remain planned work. See the
  [interoperability contract](docs/INTEROPERABILITY.md) for current restrictions.
