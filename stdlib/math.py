# Subset of CPython's math module.
# Constants are pure floats; function bodies are replaced by the compiler
# with libm / LLVM intrinsics when this module is analyzed.

pi: float = 3.141592653589793
e: float = 2.718281828459045


def sqrt(x: float) -> float:
    return x


def sin(x: float) -> float:
    return x


def cos(x: float) -> float:
    return x


def tan(x: float) -> float:
    return x


def log(x: float) -> float:
    return x


def log10(x: float) -> float:
    return x


def exp(x: float) -> float:
    return x


def floor(x: float) -> int:
    return 0


def ceil(x: float) -> int:
    return 0


def fabs(x: float) -> float:
    return x
