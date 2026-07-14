# POSIX path helpers (subset of CPython `posixpath`).
# `join` accepts a first segment plus optional further segments (`*parts`).


def join(a: str, *parts: str) -> str:
    # Join pathname components, inserting '/' as needed (POSIX).
    # An absolute later segment replaces earlier ones.
    result: str = a
    for b in parts:
        if b.startswith("/") or result == "":
            result = b
        elif result.endswith("/"):
            result = result + b
        else:
            result = result + "/" + b
    return result


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
