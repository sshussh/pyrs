import numpy as np

a = np.arange(12, dtype=np.int64).reshape(3, 4)
print((a + np.array([10, 20, 30, 40])).tolist())
print(a[:, ::-1].tolist())
print(a[a % 3 == 0].tolist())
print(a.sum(axis=0).tolist(), a.mean(axis=1).tolist())
b = np.array([1.0, np.nan, 3.0])
print(np.isnan(b).tolist(), float(np.nanmean(b)))
view = a[:, 1:3]
view[0, 0] = 99
print(a.tolist())
print(a.astype(np.float64).dtype.name)
