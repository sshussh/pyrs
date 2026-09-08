# Dict and set with string keys: the path nothing else in the corpus touches.
# Every lookup rehashes its key, so this measures hashing as much as probing.
def word_counts(n: int) -> int:
    counts: dict[str, int] = {}
    for i in range(n):
        key = "key-" + str(i % 5000)
        if key in counts:
            counts[key] = counts[key] + 1
        else:
            counts[key] = 1
    total = 0
    for i in range(n):
        key = "key-" + str(i % 5000)
        total += counts[key]
    return total


def unique(n: int) -> int:
    seen: set[str] = set()
    for i in range(n):
        seen.add("item-" + str(i % 20000))
    hits = 0
    for i in range(n):
        if "item-" + str(i % 20000) in seen:
            hits += 1
    return len(seen) + hits


def numeric(n: int) -> int:
    table: dict[int, int] = {}
    for i in range(n):
        table[i % 30000] = i
    total = 0
    for i in range(n):
        total += table[i % 30000]
    return total


print(word_counts(400000), unique(400000), numeric(400000))
