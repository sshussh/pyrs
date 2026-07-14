# Minimal `os` package: re-export `path` so `import os` then `os.path` works
# (same pattern as CPython’s package layout for this subset).
from . import path
