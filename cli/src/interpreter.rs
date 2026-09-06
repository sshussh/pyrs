//! Which CPython to use for `--compat` and `build-extension`.
//!
//! Both paths need a real interpreter, and `build-extension` needs one that
//! ships development headers. Defaulting to `python3` on `PATH` makes that
//! depend on the system interpreter having a `python3-dev` style package
//! installed, which is a separate install on most distributions and a
//! confusing failure when it is missing.
//!
//! uv provisions interpreters that always carry headers, so it is preferred
//! when present — but never required. PyRs compiles natively with no uv, no
//! virtual environment and no manifest; a native compiler that needs a
//! package manager to emit a binary has given up the thing that makes it
//! worth having.

use std::path::PathBuf;
use std::process::Command;

/// Where an interpreter came from, for `pyrs check` to report. Which
/// interpreter is in use should never be a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Flag,
    Manifest,
    Uv,
    Environment,
    Path,
}

impl Source {
    pub fn describe(self) -> &'static str {
        match self {
            Source::Flag => "--python",
            Source::Manifest => "[tool.pyrs] python",
            Source::Uv => "uv (project environment)",
            Source::Environment => "PYRS_PYTHON",
            Source::Path => "python3 on PATH",
        }
    }
}

/// Resolve an interpreter, most specific source first.
pub fn resolve(flag: Option<PathBuf>, manifest: Option<PathBuf>) -> (PathBuf, Source) {
    if let Some(p) = flag {
        return (p, Source::Flag);
    }
    if let Some(p) = manifest {
        return (p, Source::Manifest);
    }
    if let Some(p) = uv_python() {
        return (p, Source::Uv);
    }
    if let Some(p) = std::env::var_os("PYRS_PYTHON") {
        return (PathBuf::from(p), Source::Environment);
    }
    (PathBuf::from("python3"), Source::Path)
}

/// Convenience for callers that only need the path.
pub fn discover() -> PathBuf {
    resolve(None, None).0
}

/// The interpreter of the uv **project environment**, if there is one.
///
/// Only the stable CLI is used: `uv.lock` and `uv init`'s template shape are
/// internals that move.
///
/// Restricted to a project venv on purpose. Outside a project `uv python
/// find` still answers, with whatever managed interpreter uv defaults to —
/// which is not the project's choice and, in testing, was 3.12 where the
/// system interpreter was the 3.14 PyRs is built against. Letting that win
/// would silently retarget every compat run on the machine.
pub fn uv_python() -> Option<PathBuf> {
    let out = Command::new("uv").arg("python").arg("find").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8(out.stdout).ok()?;
    let path = PathBuf::from(path.trim());
    if !path.exists() {
        return None;
    }
    let in_project_venv = path
        .components()
        .any(|c| c.as_os_str() == ".venv" || c.as_os_str() == "venv");
    in_project_venv.then_some(path)
}

/// `major.minor` an interpreter reports, or `None` if it cannot be run.
pub fn version_of(python: &std::path::Path) -> Option<String> {
    let out = Command::new(python)
        .args(["-c", "import sys; print('%d.%d' % sys.version_info[:2])"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Warn when the interpreter's minor version differs from the one PyRs was
/// built against.
///
/// This is not pedantry: the Unicode tables and the differential oracle are
/// generated from a specific CPython, and `uv init` picks its own default
/// (3.12 in testing, against PyRs's 3.14). A mismatch otherwise surfaces only
/// when something Unicode- or compatibility-shaped disagrees.
pub fn warn_on_version_mismatch(python: &std::path::Path) {
    let Some(found) = version_of(python) else {
        return;
    };
    let expected = codegen::oracle_python_minor();
    if found != expected {
        eprintln!(
            "warning: {} is Python {found}, but PyRs was built against {expected}; \
             set requires-python or pass --python to align them",
            python.display()
        );
    }
}
