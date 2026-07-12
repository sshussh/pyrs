# v0.9 containers + try/except (parity-checked by make examples)

t: tuple[int, str] = (1, "a")
print(t)
a, b = t
print(a, b)

d: dict[str, int] = {"x": 1, "y": 2}
d["z"] = 3
print(d)
print("x" in d, d.get("q", 0))

s: set[int] = {1, 2}
s.add(3)
print(1 in s, len(s))

try:
    raise ValueError("oops")
except ValueError as e:
    print("caught", e)
finally:
    print("done")
