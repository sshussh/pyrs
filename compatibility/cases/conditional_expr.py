# Conditional expressions: selection, laziness, and per-branch value types.
def side(tag: str) -> int:
    print("ran", tag)
    return 1


x = 5
print("big" if x > 3 else "small")
print(1 if True else 2.5)
print(side("then") if True else side("else"))
n = 0
print(1 // n if n else -1)
print([("even" if i % 2 == 0 else "odd") for i in range(4)])
print("a" if x == 1 else "b" if x == 5 else "c")
