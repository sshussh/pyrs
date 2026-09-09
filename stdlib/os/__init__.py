# Minimal `os` package: re-export `path` so `import os` then `os.path` works
# (same pattern as CPython’s package layout for this subset).
# `getcwd` and `_environ` are replaced by the compiler with C runtime calls;
# everything else here is ordinary PyRs written on top of them.
from . import path


def getcwd() -> str:
    return ""


def _environ() -> dict[str, str]:
    # Compiler-lowered: reads the process environment once.
    return {}


# CPython's `os.environ` is a live mapping that also writes through to the
# process environment. This is a snapshot taken when the module initialises,
# which supports `in`, `[]`, `.get(...)` and iteration identically. Assigning
# into it changes only this dict; there is no subprocess surface here for the
# difference to reach.
environ: dict[str, str] = _environ()


def getenv(key: str, default: str = "") -> str:
    # CPython returns None when the variable is unset and no default is given.
    # A `str` return keeps every caller's type concrete; pass a default, or
    # test with `key in os.environ` when "unset" and "empty" must differ.
    return environ.get(key, default)
