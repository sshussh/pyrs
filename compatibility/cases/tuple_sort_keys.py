text = "the quick brown fox jumps over the lazy dog the fox"
counts: dict[str, int] = {}
for w in text.split():
    counts[w] = counts.get(w, 0) + 1
pairs = sorted(counts.items(), key=lambda p: (-p[1], p[0]))
for word, n in pairs[:3]:
    print("%-8s %d" % (word, n))

items = [("a", 2), ("b", 1), ("c", 2)]
print(sorted(items, key=lambda p: (-p[1], p[0])))
print(min(items, key=lambda p: (p[1], p[0])), max(items, key=lambda p: (p[1], p[0])))
names = ["bb", "a", "cc"]
names.sort(key=lambda s: (len(s), s))
print(names)
