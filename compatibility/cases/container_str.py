# str()/repr() of containers, and the repr()/ascii() builtins.
xs = [1, 2, 3]
t = (1, "a", 2.5)
d = {"a": 1, "b": 2}
s = {1}
print(str(xs), str(t), str(d), str(s))
print(repr(xs), repr(t), repr(d))
print(f"{xs} {d} {t}")
print("%s and %s" % (xs, (4, 5)))
print("{} then {}".format(xs, {"k": 1}))
print(str([[1], [2, 3], []]), str({"a": [1, 2], "b": []}))
print(str(["a'b", 'c"d', "e\nf"]))
print(str(["héllo", "🐍"]))

empty: list[int] = []
ed: dict[str, int] = {}
es: set[int] = set()
print(str(empty), str(ed), str(es), str(()))

print(repr("a'b"), repr(42), repr(2.5), repr(True), ascii("héllo"))


def summarize(rows: list[int]) -> str:
    evens = [r for r in rows if r % 2 == 0]
    return f"{len(rows)} rows, evens {evens}"


print(summarize([1, 2, 3, 4]))
print(len(str(xs)), str(xs).split(", "))
