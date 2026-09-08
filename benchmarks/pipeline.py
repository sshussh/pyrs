# map / filter over a lazy chain: 2M elements, no intermediate list.
def scale(v: float) -> float:
    return v * 1.5


def big(v: float) -> bool:
    return v > 250000.0


def build(n: int) -> list[float]:
    xs: list[float] = []
    i = 0
    while i < n:
        xs.append(float(i) * 0.5)
        i += 1
    return xs


xs = build(2000000)
total = 0.0
for v in map(scale, filter(big, xs)):
    total += v
print(f"{total:.1f}")
