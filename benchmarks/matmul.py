# Nested lists: 250x250 matrix multiply (deterministic LCG values)
def make(n: int, seed: int) -> list[list[float]]:
    m: list[list[float]] = []
    for i in range(n):
        row: list[float] = []
        for j in range(n):
            seed = (seed * 1103515245 + 12345) % 2147483648
            row.append(float(seed % 1000) / 1000.0)
        m.append(row)
    return m

def matmul(a: list[list[float]], b: list[list[float]], n: int) -> list[list[float]]:
    c: list[list[float]] = []
    for i in range(n):
        row: list[float] = []
        for j in range(n):
            row.append(0.0)
        c.append(row)
    for i in range(n):
        ai = a[i]
        ci = c[i]
        for k in range(n):
            aik = ai[k]
            bk = b[k]
            for j in range(n):
                ci[j] += aik * bk[j]
    return c

n = 250
a = make(n, 1)
b = make(n, 2)
c = matmul(a, b, n)
total = 0.0
for i in range(n):
    for j in range(n):
        total += c[i][j]
print(total)
