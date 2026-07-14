# Shape helpers living inside the geom package.
VERSION = 1
PI = 3.141592653589793

def circle_area(r: float) -> float:
    return PI * r * r

def rect_area(w: float, h: float) -> float:
    return w * h
