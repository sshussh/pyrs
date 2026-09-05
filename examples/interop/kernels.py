"""Numerical kernels usable from Python or the experimental native extension."""


def energy(values: list[float]) -> float:
    total = 0.0
    for value in values:
        number = float(value)
        total += number * number
    return total


def weighted_sum(values: list[float], weights: list[float]) -> float:
    if len(values) != len(weights):
        raise ValueError("values and weights must have the same length")
    total = 0.0
    for i in range(len(values)):
        total += float(values[i]) * float(weights[i])
    return total


def count_above(values: list[float], threshold: float) -> int:
    count = 0
    for value in values:
        if float(value) > threshold:
            count += 1
    return count
