# Collatz sequence lengths using for/range and lists
def collatz_len(n: int) -> int:
    steps = 0
    while n != 1:
        if n % 2 == 0:
            n = n // 2
        else:
            n = 3 * n + 1
        steps += 1
    return steps

lengths: list[int] = []
for n in range(1, 11):
    lengths.append(collatz_len(n))
print(lengths)
