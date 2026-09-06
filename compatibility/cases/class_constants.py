class Color:
    RED = "red"
    GREEN = "green"
    COUNT = 2

print(Color.RED, Color.GREEN, Color.COUNT)

class Circle:
    PI = 3.14159
    def __init__(self, r: float) -> None:
        self.r = r
    def area(self) -> float:
        return Circle.PI * self.r * self.r

print("{:.3f}".format(Circle(2.0).area()))


class Base:
    LIMIT = 10
    OFFSET = -3
    KIND = "base"

    def check(self, v: int) -> bool:
        return v < self.LIMIT


class Child(Base):
    KIND = "child"


print(Base.LIMIT, Base.OFFSET, Base.KIND)
print(Child.LIMIT, Child.KIND, Child().check(5), Child().check(50))
