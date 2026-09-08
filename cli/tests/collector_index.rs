//! Conservative marking finds the same owners through the granule index.
//!
//! The collector answers "which object contains this address" for every
//! candidate word it scans. That used to be a sort of all live ranges followed
//! by a binary search; it is now a granule-keyed hash table for ordinary
//! objects and a small sorted tier for ranges too wide to file that way.
//!
//! Missing an owner is the failure that matters, and it is silent: a live
//! object is swept, its memory is handed to a later allocation, and the
//! program reads back something else. So every test here keeps values alive
//! across collections and checks them against CPython — under
//! `PYRS_GC_STRESS=1`, which collects on every allocation, and under a tiny
//! threshold, which collects often while still letting the program finish.

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
            .join(format!("pyrs-gcindex-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    (dir, src)
}

/// Compile once, then run under each collector setting and require CPython's
/// output every time.
///
/// `PYRS_GC_STRESS=1` collects on every allocation, so the inputs here stay
/// small — a program that allocates half a million objects under stress is
/// quadratic and would only ever report a timeout.
fn survives_collection(tag: &str, source: &str) {
    let (_dir, src) = write_prog(tag, source);
    let oracle = Command::new("python3")
        .arg(&src)
        .output()
        .expect("failed to spawn CPython");
    assert!(
        oracle.status.success(),
        "CPython failed for {tag}: {}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    let expected = String::from_utf8_lossy(&oracle.stdout);

    for (name, value) in [
        ("PYRS_GC_STRESS", "1"),
        ("PYRS_GC_THRESHOLD", "4096"),
        ("PYRS_GC_THRESHOLD", "65536"),
    ] {
        let actual = Command::new(PYRS)
            .args(["run", "--no-cache", "-O", "2", "-i"])
            .arg(&src)
            .env(name, value)
            .output()
            .expect("failed to spawn PyRs");
        assert!(
            actual.status.success(),
            "{tag} failed under {name}={value}:\n{}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&actual.stdout),
            expected,
            "{tag} lost a live object under {name}={value}"
        );
    }
}

/// Many small objects, all reachable only through one list: every one is found
/// through the hash tier, and one missed owner corrupts a later read.
#[test]
fn many_small_objects_stay_reachable() {
    survives_collection(
        "small",
        r#"
rows: list[list[int]] = []
for i in range(400):
    rows.append([i, i * 2, i * 3])
total = 0
for r in rows:
    total += r[0] + r[1] + r[2]
print(total, len(rows), rows[0], rows[399])
"#,
    );
}

/// A list's `data` buffer is an owned allocation registered separately from
/// its header, and a big one spans more granules than the hash tier files, so
/// it goes to the sorted tier. Both tiers have to be consulted for the same
/// candidate.
#[test]
fn large_owned_buffers_and_small_objects_together() {
    survives_collection(
        "mixed",
        r#"
wide: list[int] = []
for i in range(9000):
    wide.append(i)
small: list[str] = []
for i in range(300):
    small.append("s" + str(i))
print(len(wide), wide[0], wide[8999], sum(wide) % 100003)
print(len(small), small[0], small[299])
"#,
    );
}

/// Strings of varied length land at varied offsets within and across granules,
/// which is where an off-by-one in the span calculation would show up.
#[test]
fn objects_of_many_sizes_land_across_granule_boundaries() {
    survives_collection(
        "sizes",
        r#"
blobs: list[str] = []
for i in range(200):
    blobs.append("x" * (i * 3 + 1))
total = 0
for b in blobs:
    total += len(b)
print(total, len(blobs[0]), len(blobs[199]))
"#,
    );
}

/// Nested containers make the mark phase recurse through the work list, so an
/// owner found in one tier has to be traced for candidates that land in the
/// other.
#[test]
fn nested_containers_are_traced_through_both_tiers() {
    survives_collection(
        "nested",
        r#"
grid: list[list[list[int]]] = []
for i in range(40):
    plane: list[list[int]] = []
    for j in range(20):
        row: list[int] = []
        for k in range(30):
            row.append(i * j * k)
        plane.append(row)
    grid.append(plane)
total = 0
for plane in grid:
    for row in plane:
        total += row[0] + row[29]
print(total, len(grid), len(grid[0]), len(grid[0][0]), grid[39][19][29])
"#,
    );
}

/// Dicts and sets keep the per-slot visitor because their slots are strided,
/// and their tables are owned buffers in the sorted tier. Keys and values must
/// both survive.
#[test]
fn dict_and_set_contents_survive() {
    survives_collection(
        "dictset",
        r#"
d: dict[str, list[int]] = {}
for i in range(150):
    d["k" + str(i)] = [i, i * 2]
s: set[str] = set()
for i in range(150):
    s.add("k" + str(i))
total = 0
for i in range(150):
    v = d["k" + str(i)]
    total += v[0] + v[1]
print(total, len(d), len(s), d["k0"], d["k149"], "k77" in s)
"#,
    );
}

/// Objects held only by a generator's heap frame, and by locals live across a
/// raise, are found by the conservative scan rather than by tracing — the path
/// the index does not change, and so the one most worth re-checking.
#[test]
fn values_held_only_by_frames_and_handlers_survive() {
    survives_collection(
        "frames",
        r#"
from typing import Iterator


def chunks(n: int) -> Iterator[str]:
    held: list[str] = []
    for i in range(n):
        held.append("g" + str(i))
        yield held[i]

out: list[str] = []
for c in chunks(120):
    out.append(c)

def guarded(n: int) -> str:
    kept: list[str] = []
    for i in range(n):
        kept.append("h" + str(i))
    try:
        if n > 0:
            raise ValueError("stop")
    except ValueError:
        return "{} {} {}".format(len(kept), kept[0], kept[n - 1])
    return "none"

print(len(out), out[0], out[119])
print(guarded(100))
"#,
    );
}
