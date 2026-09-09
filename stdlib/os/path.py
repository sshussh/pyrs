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


def _stat_kind(p: str) -> int:
    # Compiler-lowered: 0 nothing, 1 file, 2 directory, 3 other.
    return 0


def exists(p: str) -> bool:
    # CPython follows symlinks and answers False for a broken one.
    return _stat_kind(p) != 0


def isfile(p: str) -> bool:
    return _stat_kind(p) == 1


def isdir(p: str) -> bool:
    return _stat_kind(p) == 2


def splitext(p: str) -> tuple[str, str]:
    # CPython: a leading dot in the basename does not start an extension, so
    # `.bashrc` splits to ('.bashrc', '') rather than ('', '.bashrc').
    base: str = basename(p)
    dot: int = base.rfind(".")
    lead: int = 0
    while lead < len(base) and base[lead] == ".":
        lead += 1
    if dot < lead:
        return (p, "")
    cut: int = len(p) - (len(base) - dot)
    return (p[:cut], p[cut:])


def isabs(p: str) -> bool:
    return p.startswith("/")


def normpath(p: str) -> str:
    # Collapse `.`, `..` and repeated slashes, without touching the filesystem
    # (CPython's normpath is purely lexical, and so is this).
    if p == "":
        return "."
    rooted: bool = p.startswith("/")
    # A path starting with exactly two slashes is implementation-defined in
    # POSIX and CPython preserves it.
    initial: int = 0
    while initial < len(p) and p[initial] == "/":
        initial += 1
    keep_two: bool = initial == 2
    parts: list[str] = []
    for part in p.split("/"):
        if part == "" or part == ".":
            continue
        if part == "..":
            if len(parts) > 0 and parts[len(parts) - 1] != "..":
                parts.pop()
                continue
            if rooted:
                continue
        parts.append(part)
    joined: str = "/".join(parts)
    if rooted:
        prefix: str = "//" if keep_two else "/"
        return prefix + joined
    if joined == "":
        return "."
    return joined


def _getcwd() -> str:
    # Compiler-lowered, same primitive as `os.getcwd`. `os.path` cannot import
    # `os`, which imports it.
    return ""


def _environ() -> dict[str, str]:
    # Compiler-lowered, same primitive as `os._environ`, for the same reason.
    return {}


def abspath(p: str) -> str:
    if isabs(p):
        return normpath(p)
    return normpath(join(_getcwd(), p))


def expanduser(p: str) -> str:
    # `~` and `~/...` only. CPython also expands `~user` by consulting the
    # password database; there is no such lookup here, and a `~user` path is
    # returned unchanged, exactly as CPython does when the user is unknown.
    if not p.startswith("~"):
        return p
    rest: str = p[1:]
    if rest != "" and not rest.startswith("/"):
        return p
    home: str = _environ().get("HOME", "")
    if home == "":
        return p
    if home.endswith("/") and rest.startswith("/"):
        home = home[: len(home) - 1]
    if rest == "":
        return home
    return home + rest
