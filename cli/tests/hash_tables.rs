//! Dict and set lookups survive a changed hash function and a stored tag.
//!
//! Two things changed under these containers. String keys are hashed eight
//! bytes at a time rather than one, with a `fmix64` finalizer — the table index
//! is `h & mask`, so without a finalizer a word-at-a-time hash leaves the low
//! bits barely stirred and everything collides (measured: 314 ms to 1817 ms).
//! And each slot caches the top byte of its key's hash, so a probe rejects a
//! colliding slot with one compare instead of a full key comparison.
//!
//! Both are silent when wrong. A bad hash still returns *an* answer, just from
//! the wrong bucket; a stale tag makes a present key look absent. So these
//! tests are about lookups finding what was inserted across every shape that
//! moves entries: growth, tombstones, reuse after deletion, and the boundaries
//! of the eight-byte blocking.

use std::fs;
use std::path::PathBuf;
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
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-hash-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    (dir, src)
}

/// Differential against CPython at every optimization level, and under GC
/// stress — a table's `data` buffer is an owned allocation, so a rehash during
/// a collection is a shape worth exercising.
fn matches_python(tag: &str, source: &str) {
    let (_dir, src) = write_prog(tag, source);
    let expected = Command::new("python3").arg(&src).output().unwrap();
    assert!(
        expected.status.success(),
        "CPython failed for {tag}: {}",
        String::from_utf8_lossy(&expected.stderr)
    );
    let want = String::from_utf8_lossy(&expected.stdout);

    for (opt, env) in [
        ("0", None),
        ("2", None),
        ("3", None),
        ("2", Some(("PYRS_GC_STRESS", "1"))),
        ("2", Some(("PYRS_GC_THRESHOLD", "4096"))),
    ] {
        let mut cmd = Command::new(PYRS);
        cmd.args(["run", "--no-cache", "-O", opt, "-i"]).arg(&src);
        if let Some((k, v)) = env {
            cmd.env(k, v);
        }
        let actual = cmd.output().unwrap();
        let label = env.map(|(k, _)| k).unwrap_or("default");
        assert!(
            actual.status.success(),
            "PyRs {tag} at -O{opt} ({label}) failed:\n{}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&actual.stdout),
            want,
            "{tag} differs from CPython at -O{opt} ({label})"
        );
    }
}

/// Enough keys to force many resizes. Each resize rehashes every key — the
/// slot tag is only a byte, so it cannot be reused to re-place an entry — and
/// a key that lands in the wrong bucket afterwards reads as absent.
#[test]
fn every_key_survives_repeated_growth() {
    matches_python(
        "growth",
        r#"
d: dict[str, int] = {}
for i in range(3000):
    d["k" + str(i)] = i
print(len(d), d["k0"], d["k1499"], d["k2999"])
missing = 0
for i in range(3000):
    if ("k" + str(i)) not in d:
        missing += 1
print("missing", missing)
"#,
    );
}

/// Deletion leaves tombstones, which a probe must skip without reading their
/// stale tag, and which the next insert should reuse.
#[test]
fn tombstones_are_skipped_and_reused() {
    matches_python(
        "tombstones",
        r#"
d: dict[str, int] = {}
for i in range(3000):
    d["k" + str(i)] = i
for i in range(0, 3000, 3):
    del d["k" + str(i)]
print(len(d), "k0" in d, "k1" in d)
for i in range(0, 3000, 3):
    d["k" + str(i)] = i * 10
print(len(d), d["k0"], d["k3"], d["k2997"])
missing = 0
for i in range(3000):
    if ("k" + str(i)) not in d:
        missing += 1
print("missing", missing)
"#,
    );
}

/// Insertion order comes from a side array rather than the table, so it must
/// be untouched by rehashing. If it ever started depending on the hash, this
/// is where a changed hash function would show.
#[test]
fn insertion_order_is_independent_of_the_hash() {
    matches_python(
        "order",
        r#"
d: dict[str, int] = {}
for i in range(500):
    d["k" + str(i)] = i
for i in range(0, 500, 7):
    del d["k" + str(i)]
for i in range(500, 700):
    d["k" + str(i)] = i
keys = list(d.keys())
vals = list(d.values())
print(len(keys), keys[0], keys[1], keys[-1])
print(len(vals), vals[0], vals[-1])
for k in keys:
    print(k, d[k])
"#,
    );
}

/// Keys of every length from 0 to 40 straddle the eight-byte blocking: the
/// full-block loop, the short tail, and an exact multiple with no tail at all.
#[test]
fn key_lengths_across_the_block_boundary() {
    matches_python(
        "blocks",
        r#"
edge: dict[str, int] = {}
for n in range(41):
    edge["x" * n] = n
print(len(edge))
for n in range(41):
    print(n, edge["x" * n], ("x" * n) in edge)
"#,
    );
}

/// The tail of a partial block is read into a zero-padded word, so the length
/// has to participate in the hash — otherwise `"ab"` and a shorter prefix of a
/// longer key could hash alike and, worse, compare alike if the key comparison
/// were ever relaxed.
#[test]
fn keys_that_differ_only_in_length_stay_distinct() {
    matches_python(
        "tails",
        r#"
tricky: dict[str, int] = {}
tricky[""] = 0
tricky["a"] = 1
tricky["ab"] = 2
tricky["ab "] = 3
tricky["abc"] = 4
tricky["abcdefg"] = 5
tricky["abcdefgh"] = 6
tricky["abcdefghi"] = 7
tricky["abcdefgh\x00"] = 8
print(len(tricky))
for k in list(tricky.keys()):
    print(len(k), tricky[k])
"#,
    );
}

/// Sets use the same probe with their own slot layout, and their tag also has
/// to fit the existing padding.
#[test]
fn sets_grow_and_discard_correctly() {
    matches_python(
        "sets",
        r#"
s: set[str] = set()
for i in range(3000):
    s.add("s" + str(i))
print(len(s), "s0" in s, "s2999" in s, "s3000" in s)
for i in range(0, 3000, 2):
    s.discard("s" + str(i))
print(len(s), "s0" in s, "s1" in s)
for i in range(0, 3000, 2):
    s.add("s" + str(i))
print(len(s), "s0" in s, "s2999" in s)
so = sorted(list(s))
print(len(so), so[0], so[-1])
"#,
    );
}

/// Int and tuple keys take different branches of `hash_key`; only the string
/// branch changed, so these are the control.
#[test]
fn int_and_tuple_keys_are_unaffected() {
    matches_python(
        "other-keys",
        r#"
ints: dict[int, str] = {}
for i in range(-500, 500):
    ints[i] = "v" + str(i)
print(len(ints), ints[-500], ints[0], ints[499])
big: dict[int, int] = {}
for i in range(200):
    big[i * 1000000007] = i
print(len(big), big[0], big[199 * 1000000007])
tups: dict[tuple[int, str], int] = {}
for i in range(200):
    tups[(i, "t" + str(i))] = i
print(len(tups), tups[(0, "t0")], tups[(199, "t199")])
seen: set[tuple[int, str]] = set()
for i in range(200):
    seen.add((i, "t" + str(i)))
print(len(seen), (0, "t0") in seen, (200, "t200") in seen)
"#,
    );
}
