//! A dynamic value is a register pair, not a heap allocation.
//!
//! `Ty::Any` used to lower to an `i64` holding a pointer to a GC box
//! `{ i32 print_tag, i64 payload }`, so every dynamic value cost an
//! allocation: a 20M-iteration loop through `object` allocated 320 MB and ran
//! 28x slower than the same loop through a two-member union, which carries
//! the same two fields in registers.
//!
//! It now lowers to that same `{ i32, i64 }` pair. The box survives at exactly
//! two boundaries, both one word wide and neither carrying a tag of its own: a
//! container slot, and the C runtime ABI. These tests pin the behaviour across
//! both boundaries and the shapes that cross them.

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

fn temp_source(tag: &str, source: &str) -> TempDir {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-dynrepr-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    fs::write(dir.0.join("prog.py"), source).unwrap();
    dir
}

/// Differential check against CPython at every optimization level, and again
/// under collection pressure: the payload word of an inline pair can be the
/// only live reference to an object, so GC stress is the gate that matters.
fn matches_python(tag: &str, source: &str) {
    let dir = temp_source(tag, source);
    let expected = Command::new("python3")
        .arg("prog.py")
        .current_dir(&dir.0)
        .output()
        .expect("failed to spawn CPython");
    assert!(
        expected.status.success(),
        "CPython failed for {tag}: {}",
        String::from_utf8_lossy(&expected.stderr)
    );
    for opt in ["0", "2", "3"] {
        for stress in ["0", "1"] {
            let actual = Command::new(PYRS)
                .args(["run", "-O", opt, "-i", "prog.py"])
                .env("PYRS_GC_STRESS", stress)
                .current_dir(&dir.0)
                .output()
                .expect("failed to spawn PyRs");
            assert_eq!(
                String::from_utf8_lossy(&actual.stdout),
                String::from_utf8_lossy(&expected.stdout),
                "{tag}: stdout differs at -O{opt} (GC stress {stress})\nstderr: {}",
                String::from_utf8_lossy(&actual.stderr)
            );
            assert_eq!(
                actual.status.code(),
                expected.status.code(),
                "{tag}: exit status differs at -O{opt} (GC stress {stress})"
            );
        }
    }
}

/// Compile to LLVM text so the emitted representation itself can be asserted.
fn emit_llvm(tag: &str, source: &str) -> String {
    let dir = temp_source(tag, source);
    let out = Command::new(PYRS)
        .args([
            "compile",
            "--emit-llvm",
            "-O",
            "0",
            "-i",
            "prog.py",
            "-o",
            "out",
        ])
        .current_dir(&dir.0)
        .output()
        .expect("failed to spawn PyRs");
    assert!(
        out.status.success(),
        "{tag} failed to compile: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::read_to_string(dir.0.join("out.ll")).expect("no LLVM output")
}

// ---------------------------------------------------------------------------
// the representation itself
// ---------------------------------------------------------------------------

#[test]
fn a_dynamic_local_does_not_allocate() {
    let ll = emit_llvm(
        "no-alloc",
        "def run(n: int) -> int:\n\
         \x20   total: int = 0\n\
         \x20   i: int = 0\n\
         \x20   while i < n:\n\
         \x20       v: object = i\n\
         \x20       if isinstance(v, int):\n\
         \x20           total += v * 2\n\
         \x20       i += 1\n\
         \x20   return total\n\
         print(run(10))\n",
    );
    let body = ll
        .split("define")
        .find(|f| f.contains("@pyrs_run"))
        .expect("no run function");
    assert!(
        !body.contains("@pyrs_union_box_new"),
        "a dynamic local that never enters a container or the runtime ABI \
         must not allocate:\n{body}"
    );
}

#[test]
fn a_dynamic_global_is_registered_as_a_pair() {
    let ll = emit_llvm(
        "global-pair",
        "dynamic: object = \"root\"\nprint(dynamic)\n",
    );
    assert!(
        ll.contains("@g.dynamic = internal global { i32, i64 }"),
        "{ll}"
    );
    // 16, not 8: registering only the tag word would hide the payload from
    // the collector, and the payload can be the only live reference.
    assert!(
        ll.contains("call void @pyrs_gc_add_root_range(ptr @g.dynamic, i64 16)"),
        "{ll}"
    );
}

#[test]
fn entering_a_container_still_boxes() {
    let ll = emit_llvm(
        "boundary-box",
        "v: object = 5\nxs: list[object] = []\nxs.append(v)\nprint(xs)\n",
    );
    assert!(
        ll.contains("call ptr @pyrs_union_box_new(i32 "),
        "a container slot is one word and carries no tag, so the pair has to \
         be boxed to enter one:\n{ll}"
    );
    assert!(!ll.contains("call ptr @malloc"), "{ll}");
}

// ---------------------------------------------------------------------------
// behaviour, against CPython
// ---------------------------------------------------------------------------

#[test]
fn none_inside_a_dynamic_value_stays_none() {
    // A zero-initialised pair reads as tag 0, which is `int`; None is tag -1.
    // Nothing may confuse the two.
    matches_python(
        "none-tag",
        "n: object = None\nprint(n, n is None, bool(n))\n\
         m: object = 0\nprint(m, m is None, bool(m))\n",
    );
}

#[test]
fn a_dynamic_value_reads_back_through_a_container() {
    matches_python(
        "roundtrip",
        "xs: list[object] = [1, \"two\", 3.5, True, None]\n\
         for x in xs:\n\
         \x20   print(x, bool(x))\n\
         print(len(xs), xs)\n\
         d: dict[str, object] = {\"a\": 1, \"b\": \"two\"}\n\
         print(d, d[\"a\"], d[\"b\"], len(d))\n\
         for k in d:\n\
         \x20   print(k, d[k])\n",
    );
}

#[test]
fn a_dynamic_value_crosses_a_generator_suspension() {
    // A suspended frame stores its locals as words, so the pair is boxed going
    // in and unboxed coming back out.
    matches_python(
        "generator",
        "def gen(k: int) -> object:\n\
         \x20   i: int = 0\n\
         \x20   while i < k:\n\
         \x20       v: object = \"g\" + str(i)\n\
         \x20       yield v\n\
         \x20       i += 1\n\
         for g in gen(3):\n\
         \x20   print(g)\n",
    );
}

#[test]
fn a_dynamic_value_survives_a_closure_capture() {
    matches_python(
        "closure",
        "def outer() -> str:\n\
         \x20   cap: object = \"captured\"\n\
         \x20   def inner() -> str:\n\
         \x20       return str(cap)\n\
         \x20   return inner()\n\
         print(outer())\n",
    );
}

#[test]
fn a_dynamic_value_passes_through_calls() {
    matches_python(
        "params",
        "def ident(x: object) -> object:\n\
         \x20   return x\n\
         print(ident(\"through\"), ident(7), ident(None), ident(2.5))\n",
    );
}

#[test]
fn narrowing_a_dynamic_value_still_works() {
    matches_python(
        "narrowing",
        "def show(v: object) -> str:\n\
         \x20   if isinstance(v, int):\n\
         \x20       return \"int \" + str(v + 1)\n\
         \x20   if isinstance(v, str):\n\
         \x20       return \"str \" + v.upper()\n\
         \x20   if isinstance(v, float):\n\
         \x20       return \"float \" + str(v)\n\
         \x20   return \"other\"\n\
         print(show(1))\nprint(show(\"a\"))\nprint(show(2.5))\nprint(show(None))\n",
    );
}

#[test]
fn dynamic_values_under_collection_pressure() {
    // Every shape at once, with enough churn to force real collections.
    matches_python(
        "churn",
        "def churn(n: int) -> int:\n\
         \x20   total: int = 0\n\
         \x20   i: int = 0\n\
         \x20   while i < n:\n\
         \x20       v: object = \"s\" + str(i % 7)\n\
         \x20       xs: list[object] = [v, i, 3.5, True, None]\n\
         \x20       d: dict[str, object] = {\"a\": v, \"b\": xs}\n\
         \x20       got: object = d[\"a\"]\n\
         \x20       if isinstance(got, str):\n\
         \x20           total += len(got)\n\
         \x20       inner: object = d[\"b\"]\n\
         \x20       total += len(inner)\n\
         \x20       first: object = inner[0]\n\
         \x20       if isinstance(first, str):\n\
         \x20           total += len(first)\n\
         \x20       for k in d:\n\
         \x20           total += len(k)\n\
         \x20       i += 1\n\
         \x20   return total\n\
         print(churn(400))\n",
    );
}

// ---------------------------------------------------------------------------
// an unwritten dynamic slot
// ---------------------------------------------------------------------------
//
// These two crashed (SIGSEGV) during development. `zeroinitializer` is
// `{ 0, 0 }`: tag 0 is `int`, and payload 0 is not a tagged small integer, so
// the runtime dereferenced address 0 as a heap `PyrsInt*`. Locals are saved by
// the definite-assignment flags, but a class field and a module global are read
// without one.

fn run_at(dir: &TempDir, opt: &str) -> std::process::Output {
    Command::new(PYRS)
        .args(["run", "-O", opt, "-i", "prog.py"])
        .current_dir(&dir.0)
        .output()
        .expect("failed to spawn PyRs")
}

/// A field assigned on only one branch reads as None rather than crashing.
#[test]
fn an_unwritten_dynamic_field_reads_as_none() {
    let src = concat!(
        "class Box:\n",
        "    def __init__(self, flag: bool) -> None:\n",
        "        if flag:\n",
        "            self.x: object = \"hello\"\n",
        "    def show(self) -> None:\n",
        "        print(self.x)\n",
        "Box(False).show()\n",
        "Box(True).show()\n",
    );
    let dir = temp_source("unwritten-field", src);
    for opt in ["0", "2", "3"] {
        let out = run_at(&dir, opt);
        assert_eq!(
            out.status.code(),
            Some(0),
            "-O{opt}: stderr {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "None\nhello\n",
            "-O{opt}"
        );
    }
}

/// A global assigned only on an untaken branch reads as None rather than
/// crashing.
#[test]
fn an_unwritten_dynamic_global_reads_as_none() {
    let src = concat!("if False:\n", "    x: object = \"hello\"\n", "print(x)\n");
    let dir = temp_source("unwritten-global", src);
    for opt in ["0", "2", "3"] {
        let out = run_at(&dir, opt);
        assert_eq!(
            out.status.code(),
            Some(0),
            "-O{opt}: stderr {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), "None\n", "-O{opt}");
    }
}

/// A dynamic local read before assignment still raises, so the None-init above
/// never masks a genuine unbound read.
#[test]
fn an_unassigned_dynamic_local_still_raises() {
    let src = concat!(
        "def f(flag: bool) -> str:\n",
        "    if flag:\n",
        "        v: object = \"set\"\n",
        "    return str(v)\n",
        "print(f(False))\n",
    );
    let dir = temp_source("unbound-local", src);
    let out = run_at(&dir, "0");
    assert_ne!(out.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("UnboundLocalError"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
