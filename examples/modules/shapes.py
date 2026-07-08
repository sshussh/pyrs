# Command-line area calculator, split across modules.
import geometry
from geometry import PI

def describe(name: str, area: float) -> str:
    return name + " area = " + str(area)

print(describe("circle r=2", geometry.circle_area(2.0)))
print(describe("rect 3x4", geometry.rect_area(3.0, 4.0)))
print("using PI =", PI)

radii = [1.0, 2.0, 3.0]
areas = [geometry.circle_area(r) for r in radii]
print("areas:", areas)
