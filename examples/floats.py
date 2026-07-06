# Floating point follows Python semantics
print(7 / 2)            # true division is always float
print(-7 // 2)          # floor division rounds toward -inf
print(0.1 + 0.2)        # shortest round-trip printing
print(int(-2.9))        # int() truncates toward zero

def mean(a: float, b: float, c: float) -> float:
    return (a + b + c) / 3

print(mean(1.0, 2.0, 4.0))
