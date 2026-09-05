def dot(a: list[float], b: list[float]) -> float:
    total = 0.0
    for i in range(len(a)):
        total += a[i] * b[i]
    return total


def matmul(a: list[list[float]], b: list[list[float]]) -> list[list[float]]:
    result = [[0.0 for j in range(len(b[0]))] for i in range(len(a))]
    for i in range(len(a)):
        for j in range(len(b[0])):
            for k in range(len(b)):
                result[i][j] += a[i][k] * b[k][j]
    return result


print(dot([1.0, 2.0, 3.0], [0.5, 1.5, 2.5]))
print(matmul([[1.0, 2.0], [3.0, 4.0]], [[5.0, 6.0], [7.0, 8.0]]))
xs = [1.0, 2.0, 4.0, 5.0]
mean = sum(xs) / len(xs)
variance = sum([(x - mean) ** 2 for x in xs]) / len(xs)
print(mean, variance)
