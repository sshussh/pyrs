# The eager builtins over every iterable shape, including range.
print(sorted([3, 1, 2]), sorted((3, 1, 2)), sorted({3, 1, 2}))
print(sorted({"b": 1, "a": 2}), sorted("cab"), sorted(range(3, 0, -1)))
print(sum(range(5)), sum((1, 2, 3)), sum({1, 2, 3}))
print(max(range(5)), min((3, 1, 2)), max("abc"))
print(list(range(4)), list((1, 2)), list("ab"))
print(len(set(range(3))), len(set((1, 2, 2))))
print(",".join(sorted("cab")), ",".join(("a", "b")))
d = {"a": 1}
print(any(d), all(d))
