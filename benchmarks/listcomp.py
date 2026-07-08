# List comprehensions: build/map/filter over 3M elements
n = 3000000
xs = [i * 3 for i in range(n)]
ys = [x + 1 for x in xs]
evens = [y for y in ys if y % 2 == 0]

total = 0
for e in [v % 97 for v in evens]:
    total += e
print(len(xs), len(evens), total)
