"""Build the inference-evaluation corpus.

Every Python program in the repository -- examples/, compatibility/cases/,
benchmarks/, and the programs embedded as string literals in cli/tests/*.rs --
that PyRs compiles *with* its annotations and that matches CPython byte for
byte. A program admitted here fails after stripping its annotations only for
lack of type information, never for an unsupported feature.

usage: corpus.py [OUT_DIR]   (default: target/inference-eval/corpus)
"""
import ast
import glob
import hashlib
import json
import os
import re
import subprocess
import sys
import textwrap
from concurrent.futures import ThreadPoolExecutor

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
PYRS = os.environ.get("PYRS", os.path.join(REPO, "target", "release", "pyrs"))
OUT = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else os.path.join(REPO, "target", "inference-eval", "corpus"))

RAW = re.compile(r'r(#*)"(.*?)"\1', re.S)
PLAIN = re.compile(r'(?<![r#\w])"((?:[^"\\]|\\.)*)"', re.S)


def rust_unescape(s):
    out, i = [], 0
    while i < len(s):
        c = s[i]
        if c == "\\" and i + 1 < len(s):
            n = s[i + 1]
            if n == "\n":  # line continuation: skip the newline and indentation
                i += 2
                while i < len(s) and s[i] in " \t":
                    i += 1
                continue
            simple = {"n": "\n", "t": "\t", "\\": "\\", '"': '"', "'": "'", "0": "\0", "r": "\r"}
            if n in simple:
                out.append(simple[n])
                i += 2
                continue
            if n == "u":
                j = s.index("}", i)
                out.append(chr(int(s[i + 3:j], 16)))
                i = j + 1
                continue
            if n == "x":
                out.append(chr(int(s[i + 2:i + 4], 16)))
                i += 4
                continue
        out.append(c)
        i += 1
    return "".join(out)


def candidates():
    files = sorted(glob.glob(f"{REPO}/examples/*.py") + glob.glob(f"{REPO}/compatibility/cases/*.py")
                   + glob.glob(f"{REPO}/benchmarks/*.py"))
    for p in files:
        yield os.path.relpath(p, REPO), open(p).read()
    for rs in sorted(glob.glob(f"{REPO}/cli/tests/*.rs")):
        text = open(rs).read()
        seen = 0
        for m in RAW.finditer(text):
            body = m.group(2)
            if "def " in body or "print(" in body:
                seen += 1
                yield f"{os.path.relpath(rs, REPO)}#raw{seen}", textwrap.dedent(body).strip("\n") + "\n"
        for m in PLAIN.finditer(text):
            body = m.group(1)
            if "\\n" in body and ("def " in body or "print(" in body):
                seen += 1
                try:
                    yield (f"{os.path.relpath(rs, REPO)}#str{seen}",
                           textwrap.dedent(rust_unescape(body)).strip("\n") + "\n")
                except (ValueError, IndexError):
                    pass


def run(cmd, cwd, timeout):
    try:
        r = subprocess.run(cmd, capture_output=True, cwd=cwd, timeout=timeout, stdin=subprocess.DEVNULL)
        return r.returncode, r.stdout, r.stderr
    except subprocess.TimeoutExpired:
        return None, b"", b""


def admit(item):
    origin, src = item
    try:
        tree = ast.parse(src)
    except SyntaxError:
        return None
    if not any(isinstance(n, ast.FunctionDef) for n in ast.walk(tree)):
        return None
    h = hashlib.sha1(src.encode()).hexdigest()[:12]
    d = os.path.join(OUT, "work", h)
    os.makedirs(d, exist_ok=True)
    f = os.path.join(d, "prog.py")
    with open(f, "w") as fh:
        fh.write(src)
    rc, out, _ = run([sys.executable, f], d, 20)
    if rc != 0:
        return None
    prc, pout, _ = run([PYRS, "run", "--no-cache", "-O", "0", "-i", f], d, 120)
    if prc != 0 or pout != out:
        return None
    return {"hash": h, "origin": origin, "src": src, "stdout": out.decode(errors="replace")}


def main():
    uniq = {}
    for origin, src in candidates():
        uniq.setdefault(src, origin)
    items = [(o, s) for s, o in uniq.items()]
    print(f"candidate programs: {len(items)}", flush=True)
    os.makedirs(OUT, exist_ok=True)
    with ThreadPoolExecutor(os.cpu_count() or 4) as ex:
        rows = [r for r in ex.map(admit, items) if r]
    rows.sort(key=lambda r: r["origin"])
    with open(os.path.join(OUT, "index.json"), "w") as fh:
        json.dump(rows, fh, indent=1)
    print(f"admitted: {len(rows)} programs -> {OUT}")


if __name__ == "__main__":
    main()
