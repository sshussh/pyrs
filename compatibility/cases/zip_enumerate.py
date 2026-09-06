# zip over any number of iterables; enumerate with a start.
def gen():
    yield 10
    yield 20


names = ["a", "b", "c"]
vals = [1, 2, 3]
print(list(zip(names)))
print(list(zip(names, vals)))
print(list(zip(names, vals, [True, False, True])))
print(list(zip([1, 2, 3], [4, 5])))
print(list(zip(range(3), "ab")))
print(list(zip(gen(), vals)))
for i, (n, v) in enumerate(zip(names, vals), 1):
    print(i, n, v)
for j, c in enumerate("ab", start=5):
    print(j, c)
for k, m in enumerate(range(3), 10):
    print(k, m)
