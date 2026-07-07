# In-place bubble sort with nested for/range and index assignment
def sort(xs: list[int]) -> list[int]:
    n = len(xs)
    for i in range(n):
        for j in range(0, n - i - 1):
            if xs[j] > xs[j + 1]:
                tmp = xs[j]
                xs[j] = xs[j + 1]
                xs[j + 1] = tmp
    return xs

values = [42, 7, 19, 3, 99, 1, 56]
print("before:", values)
print("after: ", sort(values))
print("powers:", [2 ** 0, 2 ** 2, 2 ** 4, 2 ** 6])
