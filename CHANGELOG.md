# Changelog

## 0.86.0 — CPython interoperability and stranded correctness fixes

Native compilation is the default; the target workload family is scientific/data
Python, including NumPy and pandas through optional CPython compatibility
execution. Reaching 0.86.0 does not establish 1.0 readiness; see the
[roadmap](docs/ROADMAP.md) for the remaining gates.

- Borrowed list headers from the CPython bridge carry a non-owned capacity
  marker, so every runtime growth site raises `BufferError` instead of calling
  the libc allocator on exporter- or `PyMem`-owned memory. Previously a resize
  would free a foreign pointer, corrupt the heap and invalidate a buffer lease
  that still had to be released. Read-only enforcement no longer rests solely on
  the extension frontend's allowlist.

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
- Extension functions can return strings: native UTF-8 is copied into a Python
  string while the result remains rooted. Invalid UTF-8 raises UnicodeDecodeError
  with input buffers released normally. String arguments remain unsupported.
- Exact scalar boundary guards, arbitrary-size integer conversion, keyword
  binding, native exception translation and read-only 1D float64 buffer borrowing.
  Python lists of floats use temporary copies; NumPy/pandas buffers can pass
  without copying. Buffer leases are released on ordinary success and failure.
- Native extension boundary tests at O0/O2/O3 under GC stress, including lifetime,
  thread/reentrancy, symbol isolation, and scientific package checks. The
  `examples/interop` demo verifies results and measures full call overhead.
- The bridge remains an explicit numerical API; automatic mixed execution,
  general objects/arrays and a stable standalone library ABI remain planned
  work. See the [interoperability contract](docs/INTEROPERABILITY.md) for
  current restrictions.
- Known gaps recorded rather than claimed fixed: string length/indexing still
  operate on UTF-8 bytes, and mixed numeric list literals still promote ints to
  floats. Both are reported as `known_gap` by the compatibility probes.
- CI wiring for the compatibility probes is deferred: the required
  `scientific-compatibility` job would have made every run depend on installing
  NumPy/pandas from PyPI. `make ci` runs the probes locally in the meantime.
