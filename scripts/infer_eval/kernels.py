"""Numerical kernels for the extension experiment, annotated.

extension_bench.py strips these annotations and builds the file as a CPython
extension with each inference engine: nothing inside an extension calls its
kernels, so the static engine has no evidence and the learned predictors are
the only source of types.
"""


def dot(xs: list[float], ys: list[float]) -> float:
    total = 0.0
    for i in range(len(xs)):
        total += xs[i] * ys[i]
    return total


def norm(xs: list[float]) -> float:
    total = 0.0
    for x in xs:
        total += x * x
    return total ** 0.5


def mean(values: list[float]) -> float:
    if len(values) == 0:
        return 0.0
    total = 0.0
    for v in values:
        total += v
    return total / len(values)


def variance(values: list[float]) -> float:
    n = len(values)
    if n < 2:
        return 0.0
    m = 0.0
    for v in values:
        m += v
    m = m / n
    acc = 0.0
    for v in values:
        acc += (v - m) * (v - m)
    return acc / (n - 1)


def fib(n: int) -> int:
    a = 0
    b = 1
    for _ in range(n):
        a, b = b, a + b
    return a


def count_primes(limit: int) -> int:
    count = 0
    for candidate in range(2, limit):
        is_prime = True
        d = 2
        while d * d <= candidate:
            if candidate % d == 0:
                is_prime = False
                break
            d += 1
        if is_prime:
            count += 1
    return count


def gcd(a: int, b: int) -> int:
    while b != 0:
        a, b = b, a % b
    return a


def collatz_steps(start: int) -> int:
    steps = 0
    value = start
    while value != 1:
        if value % 2 == 0:
            value = value // 2
        else:
            value = 3 * value + 1
        steps += 1
    return steps


def mandelbrot_escape(real: float, imag: float, max_iter: int) -> int:
    zr = 0.0
    zi = 0.0
    for i in range(max_iter):
        if zr * zr + zi * zi > 4.0:
            return i
        zr, zi = zr * zr - zi * zi + real, 2.0 * zr * zi + imag
    return max_iter


def trapezoid(lo: float, hi: float, steps: int) -> float:
    width = (hi - lo) / steps
    total = 0.0
    for i in range(steps):
        x0 = lo + i * width
        x1 = x0 + width
        total += (x0 * x0 + x1 * x1) * width / 2.0
    return total


def polynomial(coefficients: list[float], x: float) -> float:
    result = 0.0
    for c in coefficients:
        result = result * x + c
    return result


def clamp(value: float, lower: float, upper: float) -> float:
    if value < lower:
        return lower
    if value > upper:
        return upper
    return value


def is_leap_year(year: int) -> bool:
    return (year % 4 == 0 and year % 100 != 0) or year % 400 == 0


def digit_sum(number: int) -> int:
    total = 0
    n = abs(number)
    while n > 0:
        total += n % 10
        n //= 10
    return total


def power_mod(base: int, exponent: int, modulus: int) -> int:
    result = 1
    b = base % modulus
    e = exponent
    while e > 0:
        if e % 2 == 1:
            result = result * b % modulus
        b = b * b % modulus
        e //= 2
    return result


def celsius_to_fahrenheit(celsius: float) -> float:
    return celsius * 9.0 / 5.0 + 32.0


def bmi(weight_kg: float, height_m: float) -> float:
    return weight_kg / (height_m * height_m)


def compound_interest(principal: float, rate: float, years: int) -> float:
    amount = principal
    for _ in range(years):
        amount = amount * (1.0 + rate)
    return amount


def running_max(values: list[float]) -> float:
    best = values[0]
    for v in values:
        if v > best:
            best = v
    return best


def label_length(label: str) -> int:
    return len(label.strip())
