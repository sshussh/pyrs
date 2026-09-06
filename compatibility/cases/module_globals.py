edges = {"a": ["b", "c"], "b": ["d"], "c": ["d"], "d": []}

def reachable(start: str) -> list[str]:
    seen: list[str] = []
    stack = [start]
    while stack:
        node = stack.pop()
        if node in seen:
            continue
        seen.append(node)
        for nxt in edges[node]:
            stack.append(nxt)
    return sorted(seen)

print(reachable("a"))
print(reachable("b"))
counts = {k: len(v) for k, v in edges.items()}
print(sorted(counts.items()))

TABLE = {"a": 1, "b": 2}
NAMES = ["x", "y"]
PAIR = (1, "z")


def totals() -> str:
    return "%d %d %d" % (len(TABLE), len(NAMES), len(PAIR))


print(totals(), TABLE["a"], NAMES[0], PAIR[1])
print([["a"], []], {"a": ["b"], "d": []})
