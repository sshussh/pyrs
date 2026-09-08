"""Exception display: an argument given is not an argument that is empty."""

try:
    raise RuntimeError
except RuntimeError as e:
    print(repr(e), len(e.args))

try:
    raise RuntimeError("")
except RuntimeError as e:
    print(repr(e), len(e.args), repr(e.args[0]))

try:
    raise RuntimeError("x")
except RuntimeError as e:
    print(repr(e), str(e), len(e.args))

try:
    assert False
except AssertionError as e:
    print(repr(e), len(e.args))

try:
    assert False, ""
except AssertionError as e:
    print(repr(e), len(e.args))

# The builtin bases real code catches on.
xs: list[int] = []
try:
    print(xs[3])
except LookupError as e:
    print("lookup", repr(e))

d: dict[str, int] = {}
try:
    print(d["k"])
except LookupError as e:
    print("lookup", repr(e))

try:
    print(1 // 0)
except ArithmeticError as e:
    print("arithmetic", repr(e))

try:
    raise NotImplementedError("soon")
except RuntimeError as e:
    print("runtime", repr(e))

try:
    raise ModuleNotFoundError("m")
except ImportError as e:
    print("import", repr(e))

try:
    raise AttributeError("a")
except Exception as e:
    print("exception", repr(e))
