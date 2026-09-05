# Native libraries and CPython interoperability

Status: experimental development on `v1.0-development`. This is a design contract
and incremental implementation plan, not a stable ABI or a 1.0 release.

## Product direction

PyRs remains an ahead-of-time native compiler. Standalone executables have no
CPython dependency. Compatibility builds may use CPython and its installed
packages in the same process as compiled code. Native coverage, interoperability
coverage and speed must be reported separately.

The long-term library target is a Python package compiled into native object
files or archives, linked into a native executable at build time. This requires
library initialization, explicit exports, dependency manifests, a versioned ABI,
and whole-program optimization. The CPython adapter must remain a separate layer
over those native functions rather than becoming the compiler's only ABI.

## Layers

1. **Frontend and semantic IR:** Python names, types, control flow, effects and
   layouts are resolved before code generation.
2. **Native library emission:** emit ordinary native functions without a C main
   function. Executable and library targets share the same implementation.
3. **Foreign adapters:** a CPython extension wrapper validates arguments, owns
   Python references/buffer leases, calls native functions, translates exceptions,
   and constructs Python results. Future C/archive and embedding adapters can use
   the same native functions with their own initialization contracts.
4. **Compatibility orchestration:** eventual function selection and fallback must
   occur before entering a native body. A native exception propagates as an
   exception; it never triggers replay of a function's side effects in Python.

## First bridge: explicit numerical extensions

The initial target is `pyrs build-extension`: a real shared native library
importable from the selected CPython environment. Unlike `--compat` whole-script
execution, its exported functions execute compiled machine code inside Python.
It is an explicit typed kernel API, not automatic compilation of arbitrary
NumPy/pandas source or a transparent Python-function replacement.

Initial scope:

- Linux, GIL-enabled CPython 3.12 and later; target headers must be installed.
- Module source contains function definitions and optional docstrings only.
- Public function arguments: exact Python int, float, bool, or a
  `list[float]` numerical sequence. Public results: int, float, bool, None, str.
- String results are copied from native UTF-8 storage into Python-owned strings.
  String arguments are not supported yet. Invalid UTF-8 produced by the native
  runtime's byte-based string operations raises UnicodeDecodeError; general
  Python Unicode indexing/semantics remain separate compiler work.
- A `list[float]` argument accepts an exact Python list of exact floats (copied
  into temporary native slots), or an aligned, contiguous, one-dimensional,
  native-endian float64 buffer (borrowed without copying). NumPy arrays,
  `array.array('d')` and compatible memoryviews can supply that buffer.
- Buffer inputs are read-only to the kernel. Mutation, escaping views, methods,
  imports, callbacks, globals and external effects are rejected in kernel source.
  The caller retains its array; every buffer lease is released on success/error.
- Unsupported argument types/layouts raise TypeError before native execution;
  errors raised by a buffer exporter propagate unchanged.
  Python scalar subclasses are rejected rather than bypassing their behavior.
- Calls retain the GIL and are restricted to the importing thread and main
  interpreter while the existing runtime/collector has process-local state.
- Native exceptions are caught at the C boundary and raised in CPython. A Python
  traceback includes the call boundary, not native source frames yet. Messages
  use native runtime wording (targeting CPython 3.14); earlier Python versions
  can differ, for example `float division by zero` versus `division by zero`.
  Runtime out-of-memory handling remains a release blocker because it can exit
  the host.
- Runtime symbols are private to each extension so independently compiled modules
  cannot accidentally share collectors or exception stacks.

The bridge's sequence conversion is explicit: buffer elements become native
float values. It does not preserve NumPy scalar subclasses as iteration results.
Use pure Python references that convert elements with `float(value)` when testing
equivalent numerical kernels. General Python/NumPy coercion and dtype semantics
require a richer array IR and guarded specialization in a later stage.

The native compiler's documented language differences still apply inside kernel
bodies. In particular, annotations fix native storage, mixed numeric assignments
can promote values, and integer powers with negative exponents are not yet
Python-compatible. Successful compilation is not a proof of Python equivalence;
each accelerated kernel needs differential testing against its Python reference.

## Try the bridge

```sh
cargo build --release -p pyrs
# Run this using the Python environment that has NumPy and pandas installed.
python3 examples/interop/demo.py --pyrs target/release/pyrs \
  --output target/compatibility/interop.json

# Or build an importable module directly in the current directory.
target/release/pyrs build-extension -i examples/interop/kernels.py \
  --module kernels_native --python python3
python3 -c 'import numpy as np; import kernels_native; print(kernels_native.energy(np.arange(10, dtype="d")))'
```

The shared library is built for the selected Python version and platform; it
requires that CPython environment when imported. It is not a standalone native
executable or a stable ABI artifact. Import the extension explicitly from Python
or from a script launched with `pyrs --compat`. Choosing that function executes
native machine code while surrounding NumPy/pandas orchestration stays in Python.
There is no automatic selection or fallback in this first target.

## Gates before automatic mixed execution

1. Differential and exception tests across optimization levels and GC stress;
   exact reference-count/buffer cleanup tests; array alias/layout tests.
2. Measured speedup for native kernels compared with equivalent Python, with
   conversion/call overhead included and no promised universal speedup.
3. Per-interpreter/runtime ownership, thread handling, recoverable allocation
   failure, cancellation and callback/reentrancy rules.
4. Function signatures/defaults, import/init semantics, reflection and global
   mutation policy; preserve source/fallback functions without duplicated effects.
5. A native array type covering shapes, strides, dtypes, mutation and aliasing;
   specialization guards must run before native code and cannot silently copy
   where a caller expects shared mutation.
6. Native library manifests and an explicit initialization/export ABI for linking
   entire pure-Python packages into native executables. CPython-dependent imports
   remain visibly classified dependencies, not standalone-native coverage.

References: [CPython extension modules](https://docs.python.org/3/c-api/module.html)
and the [buffer protocol](https://docs.python.org/3/c-api/buffer.html).
