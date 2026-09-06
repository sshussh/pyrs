//! Module-level containers are visible to functions.
//!
//! A module-level scalar could already be read from a function; a list, dict,
//! set or tuple could not, and reported `name 'X' is not defined`. A lookup
//! table or config dict at module scope is ordinary Python, and this was found
//! by running a small graph-traversal program against CPython.
//!
//! Global *storage types* are seeded from the top-level assignments before any
//! function is lowered, and only literals whose type can be determined get
//! seeded — an element the seeder cannot type leaves the global unseeded,
//! which is the safe direction: the name is simply not in scope, as before.

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

/// Differential check at every optimization level, comparing stdout and exit
/// status against the CPython oracle.
fn matches_python(tag: &str, source: &str) {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-globals-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();

    let expected = Command::new("python3")
        .arg(&src)
        .output()
        .expect("failed to spawn CPython");
    assert!(
        expected.status.success(),
        "CPython failed for {tag}: {}",
        String::from_utf8_lossy(&expected.stderr)
    );
    for opt in ["0", "2", "3"] {
        let actual = Command::new(PYRS)
            .args(["run", "-O", opt, "-i"])
            .arg(&src)
            .output()
            .expect("failed to spawn PyRs");
        assert!(
            actual.status.success(),
            "PyRs {tag} at -O{opt} failed:\n{}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&actual.stdout),
            String::from_utf8_lossy(&expected.stdout),
            "stdout differs for {tag} at -O{opt}"
        );
    }
}

// ---------------------------------------------------------------------------
// Reading module-level containers
// ---------------------------------------------------------------------------

#[test]
fn a_function_can_read_every_module_level_container() {
    matches_python(
        "containers",
        r#"
XS = [1, 2, 3]
D = {"a": 1, "b": 2}
S = {1, 2}
T = (1, "a")

def sizes() -> str:
    return "%d %d %d %d" % (len(XS), len(D), len(S), len(T))

def lookup(k: str) -> int:
    return D[k]

print(sizes())
print(lookup("a"), lookup("b"))
print(XS[0], T[0])
"#,
    );
}

#[test]
fn nested_containers_are_visible_too() {
    matches_python(
        "nested",
        r#"
M = [[1, 2], [3]]
G = {"a": ["b", "c"], "b": ["d"], "c": []}

def row(i: int) -> int:
    return len(M[i])

def edges(k: str) -> int:
    return len(G[k])

print(row(0), row(1))
print(edges("a"), edges("b"), edges("c"))
print(sorted(G))
"#,
    );
}

#[test]
fn module_level_scalars_still_work() {
    matches_python(
        "scalars",
        r#"
N = 5
S = "hi"
F = 1.5
B = True

def read() -> str:
    return "{} {} {} {}".format(N, S, F, B)

print(read())

COUNT = 0
def bump() -> int:
    global COUNT
    COUNT = COUNT + 1
    return COUNT

print(bump(), bump(), COUNT)
"#,
    );
}

#[test]
fn the_graph_traversal_that_found_this_works() {
    matches_python(
        "graph",
        r#"
edges = {"a": ["b", "c"], "b": ["d"], "c": ["d"], "d": []}

def reachable(start: str) -> list[str]:
    seen: list[str] = []
    stack = [start]
    while stack:
        node = stack.pop()
        if node in seen:
            continue
        seen.append(node)
        for nxt in edges[node]:
            stack.append(nxt)
    return sorted(seen)

print(reachable("a"))
print(reachable("b"))
counts = {k: len(v) for k, v in edges.items()}
print(sorted(counts.items()))
"#,
    );
}

// ---------------------------------------------------------------------------
// The empty-literal coercion this exposed
// ---------------------------------------------------------------------------

#[test]
fn an_empty_list_nested_in_a_container_takes_the_element_type() {
    // `[]` has no element type of its own, so it is provisionally
    // `list[Any]`. Nested inside a typed container it just takes that type —
    // the runtime value, a length-zero list, is the same either way.
    matches_python(
        "empty-nested",
        r#"
rows = [["a"], []]
print(rows, len(rows[1]))
d = {"a": ["b"], "d": []}
print(len(d["a"]), len(d["d"]))
def f() -> int:
    local = {"x": [1], "y": []}
    return len(local["y"])
print(f())
"#,
    );
}

#[test]
fn empty_literals_that_already_worked_still_do() {
    matches_python(
        "empty-plain",
        r#"
xs: list[str] = []
print(xs, len(xs))
def f(ys: list[str]) -> int:
    return len(ys)
print(f([]))
d: dict[str, int] = {}
print(len(d))
"#,
    );
}

// ---------------------------------------------------------------------------
// Nothing regressed
// ---------------------------------------------------------------------------

#[test]
fn module_globals_are_still_shared_state_not_copies() {
    matches_python(
        "shared",
        r#"
BUF: list[int] = []

def add(v: int) -> int:
    BUF.append(v)
    return len(BUF)

print(add(1), add(2))
print(BUF)
"#,
    );
}

#[test]
fn container_literals_with_mixed_element_types_still_error_the_same_way() {
    matches_python(
        "mixed-numeric",
        r#"
XS = [1, 2.5, 1]
print(XS)
def read() -> int:
    return len(XS)
print(read())
"#,
    );
}
