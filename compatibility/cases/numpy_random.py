import numpy as np

rng = np.random.default_rng(20260905)
samples = rng.normal(size=(8, 3))
print(np.round(samples.mean(axis=0), 8).tolist())
print(rng.integers(0, 10, size=8).tolist())
print(np.histogram(samples, bins=[-4, -1, 0, 1, 4])[0].tolist())
