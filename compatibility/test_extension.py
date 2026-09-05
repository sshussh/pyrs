"""Exercise the real native/CPython boundary, including GC and buffer lifetimes.

Run with the target CPython (3.12+, development headers installed):
    python3 compatibility/test_extension.py --pyrs target/debug/pyrs
NumPy/pandas tests run when both are installed; --require-science forbids skips.
"""

import argparse
import array
import ctypes
import importlib.util
import math
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest

HERE = Path(__file__).resolve().parent
ARGS = None


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class ExtensionTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.work = tempfile.TemporaryDirectory(prefix="pyrs-extension-")
        cls.addClassCleanup(cls.work.cleanup)
        cls.directory = Path(cls.work.name)
        cls.reference = load("extension_reference", HERE / "extension_kernels.py")
        cls.modules = []
        for level in (0, 2, 3):
            name = f"kernels_o{level}"
            path = cls.build(HERE / "extension_kernels.py", name, level)
            cls.modules.append(load(name, path))

    @classmethod
    def build(cls, source, name, level=2):
        path = cls.directory / f"{name}.so"
        result = subprocess.run(
            [ARGS.pyrs, "build-extension", "-i", str(source), "--module", name,
             "--python", sys.executable, "-O", str(level), "-o", str(path)],
            capture_output=True, text=True, timeout=180,
        )
        if result.returncode:
            raise AssertionError(result.stdout + result.stderr)
        return path

    def test_scalar_values_and_gc_roots(self):
        integers = [0, 1, -1, 2**62 - 1, 2**62, -(2**62), -(2**62) - 1,
                    2**63, -(2**63), 2**1000 + 17, -(2**1000 + 17), 1 << 20000]
        for module in self.modules:
            with self.subTest(module=module.__name__):
                for value in integers:
                    self.assertEqual(module.echo_int(value), value)
                for value in (0.0, -0.0, 2.5, math.inf, -math.inf, math.nan):
                    got = module.echo_float(value)
                    if math.isnan(value):
                        self.assertTrue(math.isnan(got))
                    else:
                        self.assertEqual(got, value)
                        self.assertEqual(math.copysign(1, got), math.copysign(1, value))
                self.assertIs(module.invert(True), False)
                self.assertIs(module.invert(False), True)
                self.assertIsNone(module.nothing())
                self.assertEqual(module.answer(), 42)
                self.assertEqual(module.main(9), 10)
                self.assertEqual(module.power(2**300 + 1, 5), (2**300 + 1)**5)
                self.assertEqual(module.product(160), math.factorial(160))
                self.assertTrue(module.larger(2**53 + 1, float(2**53)))
                self.assertFalse(module.larger(1 << 1500, math.inf))
                self.assertFalse(module.larger(2, math.nan))

    def test_binding_and_exact_type_guards(self):
        class IntSubclass(int):
            pass

        class FloatSubclass(float):
            pass

        for module in self.modules:
            with self.subTest(module=module.__name__):
                self.assertEqual(module.divide(b=2.0, a=9.0), 4.5)
                self.assertEqual(module.power(3, exponent=4), 81)
                calls = [lambda: module.answer(1), lambda: module.power(2),
                         lambda: module.power(2, 3, exponent=4),
                         lambda: module.power(2, unexpected=3),
                         lambda: module.echo_int(True), lambda: module.echo_int(IntSubclass(2)),
                         lambda: module.echo_float(2), lambda: module.echo_float(FloatSubclass(2)),
                         lambda: module.invert(1), lambda: module.nothing(0),
                         lambda: module.total([1.0, 2]), lambda: module.total([FloatSubclass(2)]),
                         lambda: module.total((1.0, 2.0))]
                for call in calls:
                    with self.assertRaises(TypeError):
                        call()
                self.assertEqual(module.answer(), 42)

    def test_buffers_and_list_ownership(self):
        for module in self.modules:
            with self.subTest(module=module.__name__):
                values = array.array("d", [1.0, 2.0, 3.0])
                before = sys.getrefcount(values)
                for _ in range(25):
                    self.assertEqual(module.total(values), 6.0)
                    with self.assertRaisesRegex(ValueError, "requested failure"):
                        module.guarded(values, True)
                    with self.assertRaises(TypeError):
                        module.guarded(values, 1)
                self.assertEqual(sys.getrefcount(values), before)
                # Resizing would fail if any native call leaked a buffer lease.
                values.append(4.0)
                values[0] = 5.0
                self.assertEqual(module.total(values), 14.0)
                self.assertEqual(module.at(values, -1), 4.0)
                self.assertEqual(module.recovered(values), 14.0)
                with memoryview(values).toreadonly() as view:
                    self.assertEqual(module.total(view), 14.0)
                self.assertEqual(module.total([]), 0.0)
                self.assertEqual(module.total(array.array("d")), 0.0)
                copied = [1.0, 2.0]
                self.assertEqual(module.total(copied), 3.0)
                self.assertEqual(copied, [1.0, 2.0])
                with self.assertRaises(TypeError):
                    module.total(array.array("f", [1.0]))
                with memoryview(values)[::2] as view:
                    with self.assertRaises(TypeError):
                        module.total(view)
                with memoryview(bytearray(17))[1:].cast("d") as view:
                    with self.assertRaises(TypeError):
                        module.total(view)
                values.append(5.0)

    def test_exception_translation_and_recovery(self):
        for module in self.modules:
            with self.subTest(module=module.__name__):
                for name, args in (("divide", (1.0, 0.0)), ("at", ([2.0], 8)),
                                   ("conditional", (False,))):
                    try:
                        getattr(self.reference, name)(*args)
                    except Exception as reference:
                        with self.assertRaises(type(reference)) as got:
                            getattr(module, name)(*args)
                        if name == "divide":
                            # The native runtime targets 3.14 message wording;
                            # 3.12/3.13 used "float division by zero" here.
                            self.assertEqual(str(got.exception), "division by zero")
                        else:
                            self.assertEqual(str(got.exception), str(reference))
                    else:
                        self.fail("oracle was expected to raise")
                self.assertEqual(module.conditional(True), 0)
                self.assertEqual(module.divide(8.0, 2.0), 4.0)

    def test_thread_and_reentrancy_guards(self):
        module = self.modules[0]
        errors = []

        def worker():
            try:
                module.answer()
            except RuntimeError as error:
                errors.append(str(error))

        thread = threading.Thread(target=worker)
        thread.start()
        thread.join(timeout=10)
        self.assertFalse(thread.is_alive())
        self.assertEqual(len(errors), 1)
        self.assertIn("importing thread", errors[0])

        class Exporter:
            def __init__(self):
                self.storage = array.array("d", [4.0, 5.0])
                self.releases = 0

            def __buffer__(self, flags):
                with self_test.assertRaisesRegex(RuntimeError, "reentrant"):
                    module.answer()
                return memoryview(self.storage)

            def __release_buffer__(self, view):
                self.releases += 1

        self_test = self
        exporter = Exporter()
        self.assertEqual(module.total(exporter), 9.0)
        with self.assertRaisesRegex(ValueError, "requested failure"):
            module.guarded(exporter, True)
        self.assertEqual(exporter.releases, 2)
        exporter.storage.append(6.0)
        self.assertEqual(module.answer(), 42)

    def test_independent_extensions_hide_native_symbols(self):
        source = self.directory / "other.py"
        source.write_text("def answer() -> int:\n    return 99\n")
        path = self.build(source, "other_kernels")
        old_flags = sys.getdlopenflags()
        try:
            sys.setdlopenflags(os.RTLD_NOW | os.RTLD_GLOBAL)
            other = load("other_kernels", path)
            self.assertEqual(other.answer(), 99)
            self.assertEqual(self.modules[0].answer(), 42)
            with self.assertRaises(AttributeError):
                getattr(ctypes.CDLL(str(path)), "pyrs_answer")
            with self.assertRaises(AttributeError):
                getattr(ctypes.CDLL(str(path)), "pyrs_gc_collect")
            # Rebuilding must replace the inode, not overwrite code mapped in
            # this Python process. Existing imports keep their original code.
            old_inode = path.stat().st_ino
            source.write_text("def answer() -> int:\n    return 77\n")
            self.build(source, "other_kernels")
            self.assertNotEqual(path.stat().st_ino, old_inode)
            self.assertEqual(other.answer(), 99)
            result = subprocess.run(
                [sys.executable, "-c", "import other_kernels; assert other_kernels.answer() == 77"],
                cwd=self.directory, capture_output=True, text=True, timeout=30,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
        finally:
            sys.setdlopenflags(old_flags)

    def test_subinterpreter_import_is_rejected(self):
        try:
            import _interpreters as interpreters
        except ImportError:
            import _xxsubinterpreters as interpreters
        module = self.modules[0]
        interpreter = interpreters.create()
        try:
            failure = interpreters.run_string(interpreter, f"""
import importlib.util
spec = importlib.util.spec_from_file_location({module.__name__!r}, {module.__file__!r})
try:
    native = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(native)
except ImportError:
    pass
else:
    raise AssertionError('native module was allowed in a subinterpreter')
""")
            self.assertIsNone(failure, repr(failure))
        finally:
            interpreters.destroy(interpreter)

    def test_reject_unsafe_or_unsupported_source(self):
        bodies = ["import os\n", "print('side effect')\n",
                  "x = 3\ndef f() -> int:\n    return x\n",
                  "def f(a: list[float]) -> float:\n    a[0] = 2.0\n    return a[0]\n",
                  "def f(a: list[float]) -> float:\n    b = a\n    b += a\n    return a[0]\n",
                  "def f(a: list[float]) -> float:\n    a.append(2.0)\n    return a[0]\n",
                  "def f(a: list[float]) -> list[float]:\n    return a\n",
                  "def f(a: int = 1) -> int:\n    return a\n",
                  "def f(a: list[float], b: list[float]) -> bool:\n    return a is b\n"]
        for i, body in enumerate(bodies):
            with self.subTest(source=body):
                source = self.directory / f"rejected{i}.py"
                source.write_text(body)
                output = self.directory / f"rejected{i}.so"
                result = subprocess.run(
                    [ARGS.pyrs, "build-extension", "-i", str(source), "--module", "rejected",
                     "--python", sys.executable, "-o", str(output)],
                    capture_output=True, text=True, timeout=30,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(output.exists())
                self.assertNotIn("side effect\n", result.stdout)

    def test_numpy_and_pandas(self):
        try:
            import numpy as np
            import pandas as pd
        except ImportError:
            if ARGS.require_science:
                self.fail("NumPy and pandas are required in this run")
            self.skipTest("NumPy/pandas are not installed in this interpreter")
        for module in self.modules:
            with self.subTest(module=module.__name__):
                frame = pd.DataFrame({"value": [1.0, 2.0, 3.0]})
                values = frame["value"].to_numpy(copy=False)
                expected = self.reference.total(values)
                count = sys.getrefcount(values)
                self.assertEqual(module.total(values), expected)
                self.assertEqual(sys.getrefcount(values), count)
                values.flags.writeable = False
                self.assertEqual(module.total(values), expected)
                self.assertEqual(module.total(values[:0]), 0.0)
                for invalid in (values[::2], values[::-1], values.reshape(1, 3),
                                values.astype(np.float32), values.astype(np.int64),
                                values.astype(values.dtype.newbyteorder()),
                                np.ndarray(2, dtype="d", buffer=bytearray(17), offset=1)):
                    with self.assertRaises((TypeError, BufferError, ValueError)):
                        module.total(invalid)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pyrs", required=True)
    parser.add_argument("--require-science", action="store_true")
    ARGS, remaining = parser.parse_known_args()
    ARGS.pyrs = str(Path(ARGS.pyrs).resolve())
    # Child extension processes inherit this before their first native call.
    os.environ.setdefault("PYRS_GC_STRESS", "1")
    unittest.main(argv=[sys.argv[0], *remaining], verbosity=2)
