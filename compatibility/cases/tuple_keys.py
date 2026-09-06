# Tuple dict/set keys, and the d[i, j] subscript they unblock.
TRANSITIONS = {
    ("idle", "go"): "running",
    ("running", "stop"): "idle",
    ("running", "pause"): "paused",
    ("paused", "go"): "running",
}


def run(events: list[str]) -> list[str]:
    state = "idle"
    trace = [state]
    for e in events:
        key = (state, e)
        if key in TRANSITIONS:
            state = TRANSITIONS[key]
        trace.append(state)
    return trace


print(run(["go", "pause", "go", "stop", "bogus"]))

grid: dict[tuple[int, int], int] = {}
for i in range(3):
    for j in range(3):
        grid[i, j] = i * 3 + j
print(grid[1, 2], grid[(2, 2)], (0, 0) in grid, (3, 0) in grid, len(grid))

nested: dict[tuple[str, tuple[int, int]], int] = {}
nested[("cell", (1, 2))] = 12
print(nested[("cell", (1, 2))], ("cell", (2, 1)) in nested)

seen: set[tuple[str, int]] = set()
for w in ["ab", "c", "ab"]:
    seen.add((w, len(w)))
print(len(seen), sorted(seen), ("c", 1) in seen)

squares = {(i, i + 1): i * i for i in range(3)}
print(sorted(squares.items()))
