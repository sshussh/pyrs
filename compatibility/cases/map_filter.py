"""map and filter, lazy and composed."""
from typing import Iterator


def counted(n: int, tag: str) -> Iterator[int]:
    i: int = 0
    while i < n:
        print(tag, i)
        i = i + 1
        yield i - 1


def double(n: int) -> int:
    return n * 2


def odd(n: int) -> bool:
    return n % 2 == 1


xs: list[int] = [1, 2, 3, 4, 5]
print(list(map(str, xs)))
print(list(map(double, xs)))
print(list(map(lambda n: n + 1, xs)))
print(list(filter(odd, xs)))
print(list(filter(None, [1, 0, 2, 0, 3])))
print(list(filter(None, ["", "a", "", "b"])))

# Composition, and the consumers.
for v in map(double, filter(odd, xs)):
    print("v", v)
print(sorted(map(double, xs)))
print(sum(map(double, xs)))
print(any(map(odd, xs)), all(map(odd, xs)))
print(list(enumerate(filter(odd, xs), 1)))

# Lazy: a filtered element must not be paired, and neither side is drained.
print(list(zip(map(double, counted(9, "m")), [10, 20])))
print(list(zip(filter(odd, counted(9, "f")), [10, 20])))

# Edges.
empty: list[int] = []
print(list(filter(lambda n: n > 100, xs)))
print(list(filter(None, empty)), list(map(str, empty)))
