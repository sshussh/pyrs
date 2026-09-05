import numpy as np

a = np.array([[3.0, 1.0], [1.0, 2.0]])
b = np.array([9.0, 8.0])
x = np.linalg.solve(a, b)
print(np.round(x, 10).tolist())
print(bool(np.allclose(a @ x, b)))
u, s, vh = np.linalg.svd(a)
print(np.round(s, 10).tolist())
print(bool(np.allclose((u * s) @ vh, a)))
