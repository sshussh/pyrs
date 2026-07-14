# Package demo: dotted imports, re-exports from __init__, and relative imports.
import geom.shapes
from geom import circle_area, PI, VERSION
from geom.shapes import rect_area

print("VERSION", VERSION, geom.package_version())
print("circle", circle_area(2.0))
print("rect", rect_area(3.0, 4.0))
print("PI", PI, geom.shapes.PI)
