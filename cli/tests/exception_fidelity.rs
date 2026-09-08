//! Exception display and the builtin hierarchy.
//!
//! Two things this pins. First, an argument that was *given* is distinct from
//! an argument that happens to be empty: CPython reprs `raise E` as `E()` and
//! `raise E("")` as `E('')`, and PyRs printed `E()` for both because it stored
//! only the message and an empty message looked like no message at all.
//!
//! Second, the builtin bases real code catches on. `except LookupError` around
//! an index or key miss, and `except ArithmeticError` around a division, are
//! how the idiom is spelled; neither type existed.
//!
//! These are display-shaped differences, so a byte comparison against CPython
//! is the whole test.

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

/// Differential check at every optimization level, against CPython.
fn matches_python(tag: &str, source: &str) {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-exc-{tag}-{}", std::process::id())),
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
// An argument given is not an argument that is empty
// ---------------------------------------------------------------------------

#[test]
fn no_argument_and_an_empty_argument_repr_differently() {
    // The recorded defect. Both stored an empty message, so both printed
    // `RuntimeError()`.
    matches_python(
        "empty-vs-absent",
        "try:\n    raise RuntimeError\nexcept RuntimeError as e:\n    print(repr(e))\n\n\
         try:\n    raise RuntimeError()\nexcept RuntimeError as e:\n    print(repr(e))\n\n\
         try:\n    raise RuntimeError(\"\")\nexcept RuntimeError as e:\n    print(repr(e))\n\n\
         try:\n    raise RuntimeError(\"x\")\nexcept RuntimeError as e:\n    print(repr(e))\n",
    );
}

#[test]
fn the_same_distinction_shows_through_a_container() {
    // A container element reprs its exception, so this is the same rule
    // reached by a different path.
    matches_python(
        "container-repr",
        "try:\n    raise ValueError\nexcept ValueError as e:\n    print([e])\n\n\
         try:\n    raise ValueError(\"\")\nexcept ValueError as e:\n    print([e])\n",
    );
}

#[test]
fn args_has_one_element_for_an_empty_argument_and_none_for_no_argument() {
    // `len(e.args)` and `e.args[0]` match CPython even though the *shape*
    // differs (list, not tuple) — so this asserts the length, not the repr.
    matches_python(
        "args-length",
        "try:\n    raise ValueError\nexcept ValueError as e:\n    print(len(e.args))\n\n\
         try:\n    raise ValueError(\"\")\nexcept ValueError as e:\n    \
         print(len(e.args), repr(e.args[0]))\n\n\
         try:\n    raise ValueError(\"x\")\nexcept ValueError as e:\n    \
         print(len(e.args), repr(e.args[0]))\n",
    );
}

#[test]
fn str_of_an_exception_is_unaffected() {
    // `str(e)` is args[0] as the type displays it, and was already right.
    matches_python(
        "str-unchanged",
        "try:\n    raise ValueError\nexcept ValueError as e:\n    print(repr(str(e)))\n\n\
         try:\n    raise ValueError(\"boom\")\nexcept ValueError as e:\n    print(str(e))\n\n\
         d: dict[str, int] = {}\n\
         try:\n    print(d[\"z\"])\nexcept KeyError as e:\n    print(str(e), repr(e))\n",
    );
}

#[test]
fn assert_follows_the_same_rule() {
    // `assert x` supplies no argument; `assert x, ""` supplies an empty one.
    matches_python(
        "assert-args",
        "try:\n    assert False\nexcept AssertionError as e:\n    \
         print(repr(e), len(e.args))\n\n\
         try:\n    assert False, \"\"\nexcept AssertionError as e:\n    \
         print(repr(e), len(e.args))\n\n\
         try:\n    assert False, \"why\"\nexcept AssertionError as e:\n    \
         print(repr(e), len(e.args))\n",
    );
}

#[test]
fn a_bare_reraise_keeps_the_argument_count() {
    // Re-raising must carry the original count, not reset it. `raise <name>`
    // on a caught object is outside the subset, so bare `raise` is the form.
    matches_python(
        "reraise-args",
        "def outer() -> None:\n    \
         try:\n        \
         raise RuntimeError\n    \
         except RuntimeError:\n        \
         raise\n\n\n\
         def inner() -> None:\n    \
         try:\n        \
         raise RuntimeError(\"\")\n    \
         except RuntimeError:\n        \
         raise\n\n\n\
         try:\n    outer()\nexcept RuntimeError as e:\n    print(repr(e), len(e.args))\n\
         try:\n    inner()\nexcept RuntimeError as e:\n    print(repr(e), len(e.args))\n",
    );
}

// ---------------------------------------------------------------------------
// The builtin types real code catches on
// ---------------------------------------------------------------------------

#[test]
fn the_new_exception_types_raise_and_repr() {
    matches_python(
        "new-types",
        "for kind in [\"attr\", \"notimpl\", \"imp\", \"mod\", \"lookup\", \"arith\"]:\n    \
         try:\n        \
         if kind == \"attr\":\n            \
         raise AttributeError(\"no attribute\")\n        \
         elif kind == \"notimpl\":\n            \
         raise NotImplementedError(\"later\")\n        \
         elif kind == \"imp\":\n            \
         raise ImportError(\"no module\")\n        \
         elif kind == \"mod\":\n            \
         raise ModuleNotFoundError(\"missing\")\n        \
         elif kind == \"lookup\":\n            \
         raise LookupError(\"nope\")\n        \
         else:\n            \
         raise ArithmeticError(\"bad\")\n    \
         except Exception as e:\n        \
         print(kind, repr(e))\n",
    );
}

#[test]
fn lookup_error_catches_index_and_key_misses() {
    // How the idiom is actually written.
    matches_python(
        "lookup-hierarchy",
        "xs: list[int] = []\n\
         try:\n    print(xs[3])\nexcept LookupError as e:\n    print(repr(e))\n\n\
         d: dict[str, int] = {}\n\
         try:\n    print(d[\"k\"])\nexcept LookupError as e:\n    print(repr(e))\n",
    );
}

#[test]
fn arithmetic_error_catches_division_by_zero() {
    matches_python(
        "arithmetic-hierarchy",
        "try:\n    print(1 // 0)\nexcept ArithmeticError as e:\n    print(repr(e))\n",
    );
}

#[test]
fn runtime_error_catches_not_implemented_and_import_catches_module_not_found() {
    matches_python(
        "other-hierarchy",
        "try:\n    raise NotImplementedError(\"x\")\n\
         except RuntimeError as e:\n    print(repr(e))\n\n\
         try:\n    raise ModuleNotFoundError(\"m\")\n\
         except ImportError as e:\n    print(repr(e))\n",
    );
}

#[test]
fn the_new_types_do_not_widen_what_a_narrower_handler_catches() {
    // The failure mode a hierarchy change invites: `except IndexError` must
    // not start catching KeyError just because both are LookupErrors.
    matches_python(
        "no-overcatch",
        "d: dict[str, int] = {}\n\
         try:\n    \
         try:\n        \
         print(d[\"k\"])\n    \
         except IndexError as e:\n        \
         print(\"wrong handler\", repr(e))\n\
         except KeyError as e:\n    \
         print(\"right handler\", repr(e))\n\n\
         try:\n    \
         try:\n        \
         raise ImportError(\"i\")\n    \
         except ModuleNotFoundError as e:\n        \
         print(\"wrong handler\", repr(e))\n\
         except ImportError as e:\n    \
         print(\"right handler\", repr(e))\n",
    );
}

#[test]
fn every_new_type_is_still_caught_by_bare_exception() {
    matches_python(
        "under-exception",
        "for kind in [1, 2, 3, 4, 5, 6]:\n    \
         try:\n        \
         if kind == 1:\n            \
         raise AttributeError(\"a\")\n        \
         elif kind == 2:\n            \
         raise NotImplementedError(\"b\")\n        \
         elif kind == 3:\n            \
         raise ImportError(\"c\")\n        \
         elif kind == 4:\n            \
         raise ModuleNotFoundError(\"d\")\n        \
         elif kind == 5:\n            \
         raise LookupError(\"e\")\n        \
         else:\n            \
         raise ArithmeticError(\"f\")\n    \
         except Exception as e:\n        \
         print(kind, repr(e))\n",
    );
}
