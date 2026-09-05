"""Boundary fixtures; these functions also serve as independent Python oracles."""


def echo_int(value: int) -> int:
    return value


def echo_float(value: float) -> float:
    return value


def invert(value: bool) -> bool:
    return not value


def nothing() -> None:
    pass


def answer() -> int:
    return 42


def ping() -> str:
    return "pong"


def unicode_text() -> str:
    return "Hello, مرحبا 🌍\0done"


def number_text(value: int) -> str:
    return str(value)


def joined_text(count: int) -> str:
    text = ""
    for i in range(count):
        text = text + "ab"
    return text


def invalid_utf8(values: list[float]) -> str:
    if len(values) > 0:
        return "é"[0]
    return ""


def main(value: int) -> int:
    return value + 1


def power(value: int, exponent: int) -> int:
    return value ** exponent


def larger(value: int, other: float) -> bool:
    return value > other


def divide(a: float, b: float) -> float:
    return a / b


def at(values: list[float], index: int) -> float:
    return values[index]


def total(values: list[float]) -> float:
    result = 0.0
    for value in values:
        result += float(value)
    return result


def guarded(values: list[float], fail: bool) -> float:
    if fail:
        raise ValueError("requested failure")
    return total(values)


def recovered(values: list[float]) -> float:
    try:
        return at(values, 100)
    except IndexError:
        return total(values)


def product(n: int) -> int:
    result = 1
    for i in range(1, n + 1):
        result *= i
    return result


def conditional(flag: bool) -> int:
    if flag:
        value = 0
    return value
