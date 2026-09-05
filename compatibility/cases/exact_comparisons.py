for n in [0, 1, -1, 2**53 + 1, -(2**53 + 1), 2**1024]:
    for f in [0.0, 0.5, -1.5, 9007199254740992.0, float("inf"), float("nan")]:
        print(n == f, n != f, n < f, n <= f, n > f, n >= f)
        print(f == n, f != n, f < n, f <= n, f > n, f >= n)
