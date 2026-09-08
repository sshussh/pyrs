//! Inline string equality and ASCII indexing agree with the runtime.
//!
//! `==`/`!=` on `str` and `s[i]` now decide most cases without leaving the
//! caller. Both are optimizations, so their only contract is that they give
//! the same answer as the call they replaced — including where the shortcuts
//! could go wrong:
//!
//! * pointer identity may only decide equality *positively*, because a string
//!   literal is a module global while `s[i]` returns the runtime's interned
//!   singleton, so equal one-character strings routinely differ in address;
//! * a length mismatch decides inequality, but only in byte count, which is
//!   not code-point count for non-ASCII;
//! * indexing takes its fast path only when `cplen == len`, and must fall back
//!   for anything multi-byte or out of range.
//!
//! `PYRS_INLINE_INT=0` reverts the whole family to plain runtime calls, so the
//! same program compiled both ways isolates the new code with no CPython
//! semantics in the way. It is read at emit time and is not in the cache key,
//! hence `--no-cache` throughout.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const PYRS: &str = env!("CARGO_BIN_EXE_pyrs");

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("retaining failure artifacts in {}", self.0.display());
            return;
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_prog(tag: &str, source: &str) -> (TempDir, PathBuf) {
    let dir = TempDir(
        Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-strfast-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    (dir, src)
}

fn run(src: &Path, opt: &str, inline: bool) -> String {
    let out = Command::new(PYRS)
        .args(["run", "--no-cache", "-O", opt, "-i"])
        .arg(src)
        .env("PYRS_INLINE_INT", if inline { "1" } else { "0" })
        .output()
        .expect("failed to spawn PyRs");
    assert!(
        out.status.success(),
        "PyRs failed at -O{opt} (inline={inline}):\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Inline and runtime must agree, and both must match CPython.
fn parity(tag: &str, source: &str) {
    let (_dir, src) = write_prog(tag, source);
    let oracle = Command::new("python3").arg(&src).output().unwrap();
    assert!(
        oracle.status.success(),
        "CPython failed for {tag}: {}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    let expected = String::from_utf8_lossy(&oracle.stdout);
    for opt in ["0", "2", "3"] {
        let inlined = run(&src, opt, true);
        assert_eq!(
            inlined,
            run(&src, opt, false),
            "{tag} at -O{opt}: the inline path and the runtime disagree"
        );
        assert_eq!(inlined, expected, "{tag} at -O{opt} differs from CPython");
    }
}

/// Every pair from a set chosen so each shortcut is both taken and refused:
/// equal and unequal one-byte strings, same byte length but different content,
/// same code-point count but different byte length, and empties.
#[test]
fn equality_over_a_pairwise_cross() {
    parity(
        "eq-cross",
        r#"
words: list[str] = ["", "a", "b", "A", "ab", "ba", "aa", "abc", "abcd",
                    "hello", "Hello", "hell", "hello!",
                    "é", "éx", "xé", "café", "cafe", "中", "中文",
                    "z", "zz", "\t", "\n", " ", "0", "9"]
n = len(words)
for i in range(n):
    for j in range(n):
        a = words[i]
        b = words[j]
        print(a == b, a != b, a < b, a <= b, a > b, a >= b)
"#,
    );
}

/// The shape the fast path exists for, and the one where pointer identity is
/// most tempting: a character taken from a string compared against a literal.
/// Those are different objects with equal contents.
#[test]
fn a_character_compares_equal_to_a_literal() {
    parity(
        "char-vs-literal",
        r#"
text = "the quick brown fox jumps over the lazy dog"
vowels = 0
for i in range(len(text)):
    c = text[i]
    if c == "a" or c == "e" or c == "i" or c == "o" or c == "u":
        vowels += 1
    print(c, c == "o", c != "o", c == text[i], c == c)
print(vowels)
"#,
    );
}

/// Strings built at runtime have yet another provenance — a fresh allocation
/// each — so none of the identity shortcuts may fire for them.
///
/// Content only. `is` on `str` is address identity, and which equal strings
/// share an address is a pre-existing divergence from CPython (which interns
/// short literals on its own schedule); it does not go through these helpers.
#[test]
fn strings_of_every_provenance_compare_by_content() {
    parity(
        "provenance",
        r#"
lit = "a"
idx = "abc"[0]
sliced = "abc"[0:1]
built = "" + "a"
repeated = "a" * 1
upper = "A".lower()
converted = str(1)[0:0] + "a"
made = chr(97)
group: list[str] = [lit, idx, sliced, built, repeated, upper, converted, made]
for i in range(len(group)):
    for j in range(len(group)):
        print(group[i] == group[j], group[i] != group[j])
"#,
    );
}

/// Indexing takes its fast path only when every code point is one byte.
/// Mixed-width strings must reach the runtime, and negative indices resolve
/// against the code-point count, not the byte count.
#[test]
fn indexing_across_ascii_and_multibyte() {
    parity(
        "index",
        r#"
samples: list[str] = ["a", "hello", "café", "中文字", "aé中b", "z" * 70,
                      "\t\n ", "0123456789", "ééé", "a中a"]
for s in samples:
    n = len(s)
    print(n, s)
    for i in range(n):
        print(i, s[i], s[-(i + 1)], len(s[i]), s[i] == s[i])
    out = ""
    for i in range(n):
        out = out + s[i]
    print(out == s)
"#,
    );
}

/// Out of range must still raise `IndexError` with the runtime's message: the
/// fast path falls through rather than trapping, so the text stays in one
/// place.
#[test]
fn out_of_range_indexing_still_raises() {
    parity(
        "index-error",
        r#"
cases: list[str] = ["", "a", "abc", "中文"]
for s in cases:
    for i in [-9, -4, -1, 0, 1, 3, 9]:
        try:
            print(s, i, s[i])
        except IndexError as e:
            print(s, i, "IndexError", e)
"#,
    );
}

/// A byte that is not valid UTF-8 is counted as one code point by the runtime
/// (`utf8_next` falls back to latin-1), which makes such a string satisfy the
/// one-byte-per-code-point test. Indexing it used to intern a byte >= 0x80 in
/// a 128-entry table — an out-of-bounds write about 2.5 KiB past the end.
///
/// CPython rejects the file outright, so there is no oracle here; the
/// assertion is PyRs's own consistency: indexing yields one-code-point
/// strings that re-join into the original.
#[test]
fn a_non_utf8_file_indexes_within_bounds() {
    let (dir, src) = write_prog(
        "latin1",
        r#"
import sys
f = open(sys.argv[1], "r")
text = f.read()
f.close()
parts: list[str] = []
for i in range(len(text)):
    parts.append(text[i])
joined = ""
for p in parts:
    joined = joined + p
widths = 0
for p in parts:
    widths += len(p)
print(len(text), len(parts), widths, joined == text)
"#,
    );
    // "café naïve\nrésumé\n" as latin-1: 0xE9 and 0xEF are lone high bytes.
    let data = dir.0.join("latin1.txt");
    fs::write(&data, b"caf\xe9 na\xefve\nr\xe9sum\xe9\n".as_slice()).unwrap();

    for opt in ["0", "2", "3"] {
        let out = Command::new(PYRS)
            .args(["run", "--no-cache", "-O", opt, "-i"])
            .arg(&src)
            .arg(&data)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "latin-1 indexing failed at -O{opt}:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "18 18 18 True",
            "at -O{opt}: a lone high byte must index to a one-code-point string \
             that re-joins into the original"
        );
    }
}

/// Equality feeds `in`, dict and set membership, `sorted`, `count` and
/// `index`, none of which go through the inlined operator — so a divergence
/// between them would show up as two answers to the same question.
#[test]
fn container_operations_agree_with_the_operator() {
    parity(
        "containers",
        r#"
text = "mississippi"
seen: dict[str, int] = {}
for i in range(len(text)):
    c = text[i]
    if c in seen:
        seen[c] = seen[c] + 1
    else:
        seen[c] = 1
keys: list[str] = []
for i in range(len(text)):
    c = text[i]
    if c not in keys:
        keys.append(c)
keys.sort()
print(keys)
for k in keys:
    print(k, seen[k], text.count(k), text.find(k), k == "s", k in text)
uniq: set[str] = set()
for i in range(len(text)):
    uniq.add(text[i])
print(len(uniq), "s" in uniq, "z" in uniq)
"#,
    );
}
