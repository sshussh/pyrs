# typing imports, and Iterator[T] for generator parameters.
from typing import Iterator, Optional


def numbers(n: int) -> Iterator[int]:
    for i in range(n):
        yield i


def evens(src: Iterator[int]) -> Iterator[int]:
    for v in src:
        if v % 2 == 0:
            yield v


def squares(src: Iterator[int]) -> Iterator[int]:
    for v in src:
        yield v * v


def words() -> Iterator[str]:
    yield "a"
    yield "b"


def upper(src: Iterator[str]) -> Iterator[str]:
    for v in src:
        yield v.upper()


def maybe(x: Optional[int]) -> int:
    if x is None:
        return -1
    return x


print(list(squares(evens(numbers(10)))))
print(sum(squares(evens(numbers(10)))))
print(",".join(upper(words())))
print(maybe(None), maybe(4))
