# v0.135 operator overloading: numeric types written in PyRs
# (parity-checked by make examples)
#
# Nothing here is a compiler builtin. `Vec3` and `Matrix` are ordinary classes,
# and `+`, `*`, `@`, `-x` and `+=` reach them through the dunders CPython uses.
# The same file runs under python3 and produces the same bytes.


class Vec3:
    def __init__(self, x: float, y: float, z: float) -> None:
        self.x: float = x
        self.y: float = y
        self.z: float = z

    def __add__(self, o: "Vec3") -> "Vec3":
        return Vec3(self.x + o.x, self.y + o.y, self.z + o.z)

    def __sub__(self, o: "Vec3") -> "Vec3":
        return Vec3(self.x - o.x, self.y - o.y, self.z - o.z)

    # `v * 2.0` and `2.0 * v` are different slots: the second has no float
    # implementation to find, so it reflects onto the right operand.
    def __mul__(self, k: float) -> "Vec3":
        return Vec3(self.x * k, self.y * k, self.z * k)

    def __rmul__(self, k: float) -> "Vec3":
        return Vec3(self.x * k, self.y * k, self.z * k)

    def __truediv__(self, k: float) -> "Vec3":
        return Vec3(self.x / k, self.y / k, self.z / k)

    def __neg__(self) -> "Vec3":
        return Vec3(-self.x, -self.y, -self.z)

    # The dot product, spelled the way NumPy spells it.
    def __matmul__(self, o: "Vec3") -> float:
        return self.x * o.x + self.y * o.y + self.z * o.z

    def __eq__(self, o: "Vec3") -> bool:
        return self.x == o.x and self.y == o.y and self.z == o.z

    def __repr__(self) -> str:
        return "Vec3(" + str(self.x) + ", " + str(self.y) + ", " + str(self.z) + ")"


class Matrix:
    """A dense row-major matrix over floats."""

    def __init__(self, rows: list[list[float]]) -> None:
        self.rows: list[list[float]] = rows

    def shape(self) -> tuple[int, int]:
        return (len(self.rows), len(self.rows[0]))

    def __add__(self, o: "Matrix") -> "Matrix":
        out: list[list[float]] = []
        for i in range(len(self.rows)):
            row: list[float] = []
            for j in range(len(self.rows[i])):
                row.append(self.rows[i][j] + o.rows[i][j])
            out.append(row)
        return Matrix(out)

    def __mul__(self, k: float) -> "Matrix":
        out: list[list[float]] = []
        for row in self.rows:
            scaled: list[float] = []
            for v in row:
                scaled.append(v * k)
            out.append(scaled)
        return Matrix(out)

    def __matmul__(self, o: "Matrix") -> "Matrix":
        n = len(self.rows)
        inner = len(o.rows)
        cols = len(o.rows[0])
        out: list[list[float]] = []
        for i in range(n):
            row: list[float] = []
            for j in range(cols):
                total = 0.0
                for k in range(inner):
                    total += self.rows[i][k] * o.rows[k][j]
                row.append(total)
            out.append(row)
        return Matrix(out)

    def __repr__(self) -> str:
        return "Matrix(" + str(self.rows) + ")"


class Accumulator:
    """`__iadd__` mutates and returns self, so `+=` rebinds to the same object."""

    def __init__(self) -> None:
        self.total: float = 0.0
        self.count: int = 0

    def __iadd__(self, v: float) -> "Accumulator":
        self.total += v
        self.count += 1
        return self

    def mean(self) -> float:
        if self.count == 0:
            return 0.0
        return self.total / float(self.count)

    def __repr__(self) -> str:
        return "Accumulator(" + str(self.total) + ", " + str(self.count) + ")"


a = Vec3(1.0, 2.0, 3.0)
b = Vec3(4.0, 5.0, 6.0)

print(a + b)
print(a - b)
print(a * 2.0)
print(2.0 * a)
print(b / 2.0)
print(-a)
print(a @ b)
print(a == Vec3(1.0, 2.0, 3.0), a == b)

# A reduction over a list of them, which is where the dispatch gets exercised
# in a loop rather than once.
points: list[Vec3] = [Vec3(1.0, 0.0, 0.0), Vec3(0.0, 2.0, 0.0), Vec3(0.0, 0.0, 3.0)]
centroid = Vec3(0.0, 0.0, 0.0)
for p in points:
    centroid = centroid + p
print(centroid / float(len(points)))

m = Matrix([[1.0, 2.0], [3.0, 4.0]])
n = Matrix([[5.0, 6.0], [7.0, 8.0]])
print(m.shape())
print(m + n)
print(m * 3.0)
print(m @ n)
print(m @ n @ m)

# The identity matrix is a right unit, which is the cheapest check that the
# inner loop indexes the way it claims to.
ident = Matrix([[1.0, 0.0], [0.0, 1.0]])
print((m @ ident).rows == m.rows)

acc = Accumulator()
same = acc
for v in [1.5, 2.5, 3.5, 4.5]:
    acc += v
print(acc, acc.mean(), acc is same)
