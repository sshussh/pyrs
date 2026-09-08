"""zip advances its inputs in lockstep instead of draining them first."""
from typing import Iterator


def counted(n: int, tag: str) -> Iterator[int]:
    i: int = 0
    while i < n:
        print(tag, i)
        i = i + 1
        yield i - 1


def endless() -> Iterator[int]:
    i: int = 0
    while True:
        print("endless", i)
        i = i + 1
        yield i - 1


# The recorded defect: this did not terminate.
print(list(zip(endless(), [10])))

# Advance order, and the count of pulls from each side.
for a, b in zip(counted(3, "L"), counted(5, "R")):
    print("pair", a, b)

# Composition, and every iterable kind through one protocol.
print(list(enumerate(zip([1, 2], "ab"), 5)))
print(list(zip(range(3), {"k": 1}, counted(9, "g"))))

# range evaluates its operands left to right.
def lo() -> int:
    print("lo")
    return 0


def hi() -> int:
    print("hi")
    return 2


for i in range(lo(), hi()):
    print("i", i)


# A generator expression evaluates its outermost iterable at creation.
def edge() -> int:
    print("edge")
    return 3


gen = (x for x in range(edge()))
print("created")
print(list(gen))

unused = (x for x in range(edge()))
print("unused made")
