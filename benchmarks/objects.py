# Many small live objects: 200k four-element lists and 200k strings, kept alive
# so the collector has to trace and sweep them rather than reclaim them. This
# is what the allocator and the mark phase govern -- the rest of the corpus
# builds a handful of very large objects and never exercises either.
def build(n: int) -> int:
    rows: list[list[int]] = []
    for i in range(n):
        rows.append([i, i * 2, i * 3, i * 4])
    total = 0
    for r in rows:
        total += r[0] + r[3]
    names: list[str] = []
    for i in range(n):
        names.append("row-" + str(i))
    for s in names:
        total += len(s)
    return total


print(build(200000))
