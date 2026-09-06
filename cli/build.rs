//! Embed every `stdlib/**/*.py` into the `pyrs` binary at compile time so a
//! relocated compiler works without a companion `stdlib/` directory, and
//! compute the compiler's own fingerprint for the build cache.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

// The build script needs only part of this module; the crate proper uses the
// rest, and the tests use the one-shot helper.
#[path = "src/hash.rs"]
#[allow(dead_code)]
mod hash;

fn main() {
    emit_build_fingerprint();
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let stdlib = manifest_dir.join("..").join("stdlib");
    println!("cargo:rerun-if-changed={}", stdlib.display());

    let mut files: Vec<(String, PathBuf)> = Vec::new();
    if stdlib.is_dir() {
        collect_py_files(&stdlib, &stdlib, &mut files);
    } else {
        println!(
            "cargo:warning=PyRs stdlib directory missing at {}; embed would be empty",
            stdlib.display()
        );
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    if files.is_empty() {
        let allow = env::var("PYRS_ALLOW_EMPTY_STDLIB").ok().as_deref() == Some("1");
        if allow {
            println!(
                "cargo:warning=PyRs stdlib embed is empty (no .py under {}); \
                 PYRS_ALLOW_EMPTY_STDLIB=1 set",
                stdlib.display()
            );
        } else {
            panic!(
                "PyRs stdlib embed is empty (no .py files under {}). \
                 Add stdlib sources or set PYRS_ALLOW_EMPTY_STDLIB=1 to allow.",
                stdlib.display()
            );
        }
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let dest = out_dir.join("stdlib_embed_data.rs");

    let mut body = String::from(
        "/// Look up an embedded stdlib source by POSIX-style relative path\n\
         /// (e.g. `os/path.py`, `os/__init__.py`).\n\
         pub fn embedded_source(rel: &str) -> Option<&'static str> {\n\
             match rel {\n",
    );
    for (rel, abs) in &files {
        // Absolute path for include_str! so the generated file in OUT_DIR works.
        // Debug formatting escapes rare path characters safely into Rust string literals.
        let abs_s = abs.display().to_string().replace('\\', "/");
        let rel_s = rel.replace('\\', "/");
        body.push_str(&format!(
            "        {rel_s:?} => Some(include_str!({abs_s:?})),\n"
        ));
        // Also rebuild when any individual file changes.
        println!("cargo:rerun-if-changed={}", abs.display());
    }
    body.push_str(
        "        _ => None,\n\
             }\n\
         }\n\
         \n\
         /// Relative paths of all embedded stdlib modules (for tests).\n\
         #[allow(dead_code)]\n\
         pub fn embedded_paths() -> &'static [&'static str] {\n\
             &[\n",
    );
    for (rel, _) in &files {
        let rel_s = rel.replace('\\', "/");
        body.push_str(&format!("                 {rel_s:?},\n"));
    }
    body.push_str("             ]\n         }\n");

    fs::write(&dest, body).expect("write stdlib_embed_data.rs");
}

fn collect_py_files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_py_files(root, &path, out);
        } else if path.extension().is_some_and(|e| e == "py") {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, path));
        }
    }
}

/// Fingerprint of this compiler build, for the build cache to key on.
///
/// Computed here rather than by hashing the 5 MB executable at startup: that
/// would cost more than some cache hits save. Covering every workspace source
/// and embedded asset means a changed compiler cannot serve a stale artifact
/// — which is the failure that matters, since a stale binary is a wrong
/// answer that looks like a right one.
fn emit_build_fingerprint() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace = manifest_dir.join("..");

    let mut inputs: Vec<(String, PathBuf)> = Vec::new();
    for crate_dir in [
        "cli", "codegen", "common", "ir", "lexer", "parser", "semantic",
    ] {
        let dir = workspace.join(crate_dir);
        collect_files(
            &dir,
            &dir,
            &["rs", "c", "h", "toml"],
            crate_dir,
            &mut inputs,
        );
    }
    inputs.sort_by(|a, b| a.0.cmp(&b.0));

    let mut h = hash::Sha256::new();
    h.field(env!("CARGO_PKG_VERSION").as_bytes());
    for (name, path) in &inputs {
        println!("cargo:rerun-if-changed={}", path.display());
        h.field(name.as_bytes());
        match fs::read(path) {
            Ok(bytes) => h.field(&bytes),
            Err(_) => h.field(b"<unreadable>"),
        }
    }

    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("build_fingerprint.txt");
    fs::write(&out, h.hex()).expect("write build_fingerprint.txt");
}

fn collect_files(
    root: &Path,
    dir: &Path,
    extensions: &[&str],
    prefix: &str,
    out: &mut Vec<(String, PathBuf)>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // `target/` holds build output, not input.
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            collect_files(root, &path, extensions, prefix, out);
        } else if path
            .extension()
            .is_some_and(|e| extensions.iter().any(|w| e == *w))
        {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((format!("{prefix}/{rel}"), path));
        }
    }
}
