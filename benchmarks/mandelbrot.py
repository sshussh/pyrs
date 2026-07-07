# Float arithmetic: Mandelbrot escape iterations on a 300x300 grid
def mandel(cx: float, cy: float, limit: int) -> int:
    x = 0.0
    y = 0.0
    i = 0
    while i < limit:
        if x * x + y * y > 4.0:
            return i
        nx = x * x - y * y + cx
        y = 2.0 * x * y + cy
        x = nx
        i += 1
    return limit

total = 0
size = 500
for py in range(size):
    for px in range(size):
        cx = -2.0 + 2.5 * float(px) / float(size)
        cy = -1.25 + 2.5 * float(py) / float(size)
        total += mandel(cx, cy, 80)
print(total)
