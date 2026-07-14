# POSIX path helpers (subset of CPython `posixpath`).
# `join` takes exactly two arguments (no *args).


def join(a: str, b: str) -> str:
    # Join two pathname components, inserting '/' as needed.
    # If `b` is absolute, it replaces `a`. An empty `a` yields `b`.
    # An empty `b` with non-empty `a` yields a path ending with '/'.
    if b.startswith("/") or a == "":
        return b
    if a.endswith("/"):
        return a + b
    return a + "/" + b


def dirname(p: str) -> str:
    # Directory component of a pathname (POSIX `os.path.dirname`).
    i: int = p.rfind("/") + 1
    head: str = p[:i]
    # CPython: if head and head != '/'*len(head): head = head.rstrip('/')
    # PyRs `rstrip` is whitespace-only, so strip trailing slashes by hand.
    if head != "" and head != "/" * len(head):
        j: int = len(head)
        while j > 0 and head[j - 1] == "/":
            j = j - 1
        head = head[:j]
    return head


def basename(p: str) -> str:
    # Final component of a pathname (POSIX `os.path.basename`).
    i: int = p.rfind("/") + 1
    return p[i:]
