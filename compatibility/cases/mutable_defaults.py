# A mutable default argument, at module level and nested.
#
# CPython evaluates a default once, when the `def` executes, so the list is
# shared across calls. PyRs freezes nested and lambda defaults the same way
# but re-evaluates module-level ones, so the two halves of this program
# disagree with each other. Recorded as a mismatch rather than fixed: a
# module-level freeze needs the default stored as a module global evaluated
# in `def` source order, not as a frame temp.


def accumulate(xs: list[int] = []) -> int:
    xs.append(1)
    return len(xs)


def nested() -> str:
    def inner(ys: list[int] = []) -> int:
        ys.append(1)
        return len(ys)

    return str(inner()) + " " + str(inner()) + " " + str(inner())


print("module-level:", accumulate(), accumulate(), accumulate())
print("nested:      ", nested())

# Literal defaults are indistinguishable either way.
def scaled(n: int, factor: int = 2) -> int:
    return n * factor


print("literal:     ", scaled(3), scaled(3, 5))
