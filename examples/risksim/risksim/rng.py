# Lightweight deterministic RNG.


class Rng:
    def __init__(self, start: int):
        self.s0 = start

    def next_unit(self) -> float:
        x = self.s0
        if x <= 0:
            x = 1
        x = (x * 48271) % 2147483647
        self.s0 = x
        return float(x) * (1.0 / 2147483647.0)

    def next_gauss(self) -> float:
        from math import log, cos, pi, sqrt

        u1 = self.next_unit()
        u2 = self.next_unit()
        if u1 < 1e-300:
            u1 = 1e-300
        return sqrt(-2.0 * log(u1)) * cos(2.0 * pi * u2)
