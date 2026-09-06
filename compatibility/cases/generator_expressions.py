# Generator expressions: consumers, filters, laziness and short-circuiting.
def loud(n: int):
    for i in range(n):
        print("gen", i)
        yield i


def trace(v: int) -> int:
    print("made", v)
    return v


xs = [1, 2, 3, 4]
print(sum(x for x in xs))
print(max(x for x in xs), min(x for x in xs))
print(list(x for x in xs if x % 2 == 0))
print(",".join(str(x) for x in xs))
print(sum(x * y for x in [1, 2] for y in [10, 20]))
print(sum(x for x in range(5)))
print(any(x > 1 for x in loud(4)))
print(all(x < 1 for x in loud(4)))
g = (trace(x) for x in [1, 2])
print("created")
print(list(g))
