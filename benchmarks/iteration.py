# zip / enumerate: 2M paired steps. Both materialized lists of tuples before
# 0.119; they now advance in lockstep with no intermediate list.
def build(n: int) -> list[float]:
    xs: list[float] = []
    i = 0
    while i < n:
        xs.append(float(i) * 0.5)
        i += 1
    return xs


xs = build(1000000)
ys = build(1000000)

total = 0.0
for a, b in zip(xs, ys):
    total += a * b

idx_sum = 0.0
for i, v in enumerate(xs):
    idx_sum += v * float(i)

print(f"{total:.1f} {idx_sum:.1f}")
