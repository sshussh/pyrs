# Lambda parameter types inferred from key= context and from body usage.
xs = [3, 1, 2]
ss = ["aa", "b", "ccc"]
ps = [(1, "b"), (2, "a")]

print(sorted(xs, key=lambda v: -v))
print(sorted(ss, key=lambda s: len(s)))
print(max(ss, key=lambda s: len(s)), min(ss, key=lambda s: len(s)))
print(sorted(ss, key=lambda s: s.upper()))
print(min(ps, key=lambda p: p[1]))
xs.sort(key=lambda v: -v)
print(xs)

add1 = lambda a: a + 1
print(add1(1))


def outer() -> int:
    n = 5
    shift = lambda a: a + n
    return shift(1)


print(outer())
