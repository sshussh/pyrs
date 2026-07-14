# Minimal `os` package: re-export `path` so `import os` then `os.path` works
# (same pattern as CPython’s package layout for this subset).
# `getcwd` body is replaced by the compiler with a C runtime call.
from . import path


def getcwd() -> str:
    return ""
