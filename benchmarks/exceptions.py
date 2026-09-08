# raise / catch through a call boundary: 400k round trips.
def checked(v: float) -> float:
    if v < 0.0:
        raise ValueError("negative")
    return v * 2.0


total = 0.0
caught = 0
i = 0
while i < 400000:
    try:
        total += checked(float(i % 7) - 3.0)
    except ValueError:
        caught += 1
    i += 1
print(f"{total:.1f} {caught}")
