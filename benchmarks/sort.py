# List indexing: bubble sort 2000 pseudo-random ints (LCG, no overflow)
def lcg_fill(n: int) -> list[int]:
    xs: list[int] = []
    seed = 42
    for i in range(n):
        seed = (seed * 1103515245 + 12345) % 2147483648
        xs.append(seed % 100000)
    return xs

def sort(xs: list[int]) -> list[int]:
    n = len(xs)
    for i in range(n):
        for j in range(0, n - i - 1):
            if xs[j] > xs[j + 1]:
                tmp = xs[j]
                xs[j] = xs[j + 1]
                xs[j + 1] = tmp
    return xs

xs = sort(lcg_fill(5000))
total = 0
for x in xs:
    total += x
print(xs[0], xs[2499], xs[4999], total)
