//! Build cache: compiled C runtime objects, and whole programs.
//!
//! A one-line program takes 2.61 s to build on this machine, of which 2.39 s
//! is `cc -O2 -c runtime.c`. That is 92% of the floor, paid again on every
//! invocation, including every `pyrs run` of a program that has not changed.
//!
//! Two layers close that:
//!
//! 1. **Runtime objects** — `runtime.c`, `gc.c` and `unicode_data.c` compiled
//!    once and reused. Takes any build to roughly the link time.
//! 2. **Programs** — a whole executable keyed on its inputs, so an unchanged
//!    `pyrs run` skips compilation entirely.
//!
//! The cache is global rather than per-project (`$XDG_CACHE_HOME/pyrs`),
//! because the runtime objects depend only on the compiler and the embedded
//! sources: every project on the machine wants the same ones. It is also what
//! lets an explicit `pyrs run -i prog.py`, which has no project, still hit.
//!
//! **A stale entry is worse than a slow build.** It is a wrong answer that
//! looks like a right one, so: keys cover everything that can change the
//! output bytes, entries are checksum-verified before reuse rather than
//! trusted, and publication is by atomic rename so a killed process can leave
//! a staging directory but never a half-written entry.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;

use crate::hash::Sha256;

/// Fingerprint of the compiler itself, computed by `build.rs` over every
/// workspace source and embedded asset. Hashing the 5 MB executable at
/// startup would cost more than some cache hits save.
const BUILD_FINGERPRINT: &str = include_str!(concat!(env!("OUT_DIR"), "/build_fingerprint.txt"));

/// Cache format. Bump to invalidate every entry when the layout changes.
const FORMAT: u32 = 1;

/// Root of the cache, or `None` when no cache directory can be determined
/// (a cache is an optimization; not having one is not an error).
pub fn root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("PYRS_CACHE_DIR") {
        return Some(PathBuf::from(dir));
    }
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(dir).join("pyrs"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache").join("pyrs"))
}

/// Cheap stand-in for "is this the same compiler binary": its name, and the
/// size and mtime of whatever it resolves to. Used only to decide whether the
/// recorded identity below can be reused, never as the identity itself.
fn toolchain_stamp(cc: &str) -> String {
    let mut h = Sha256::new();
    h.field(cc.as_bytes());
    let resolved = which(cc);
    if let Some(path) = &resolved {
        h.field(path.as_os_str().as_encoded_bytes());
        if let Ok(meta) = fs::metadata(path) {
            h.field(&meta.len().to_le_bytes());
            if let Ok(mtime) = meta.modified()
                && let Ok(since) = mtime.duration_since(std::time::UNIX_EPOCH)
            {
                h.field(&since.as_nanos().to_le_bytes());
            }
        }
    }
    h.hex()
}

/// First match for `cc` on `PATH`, or the path itself when it has one.
fn which(cc: &str) -> Option<PathBuf> {
    let direct = Path::new(cc);
    if direct.components().count() > 1 {
        return Some(direct.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(cc))
        .find(|candidate| candidate.is_file())
}

/// Identity of the C toolchain: which compiler, and what it reports about
/// itself. A compiler upgrade changes `--version`, and a cross-compiler
/// changes `-dumpmachine`; either changes the object bytes.
///
/// Asking the compiler costs two subprocesses, which is more than a warm
/// build should spend on anything, so the answer is recorded under a cheap
/// stamp of the binary and only recomputed when that binary changes.
fn toolchain_identity(cc: &str) -> String {
    let stamp = toolchain_stamp(cc);
    let recorded = root().map(|r| r.join("toolchain").join(&stamp));
    if let Some(path) = &recorded
        && let Ok(cached) = fs::read_to_string(path)
        && cached.len() == 64
    {
        return cached;
    }

    let mut h = Sha256::new();
    h.field(cc.as_bytes());
    for arg in ["--version", "-dumpmachine"] {
        let out = process::Command::new(cc).arg(arg).output();
        match out {
            Ok(o) => {
                h.field(&o.stdout);
                h.field(&o.stderr);
            }
            Err(_) => h.field(b"<unavailable>"),
        }
    }
    let identity = h.hex();
    if let Some(path) = &recorded
        && let Some(parent) = path.parent()
        && fs::create_dir_all(parent).is_ok()
    {
        // A concurrent writer produces the same bytes, so a lost race is fine.
        let _ = fs::write(path, &identity);
    }
    identity
}

/// The compiled C sources, preprocessed.
///
/// Preprocessing is the point: a key over the embedded `runtime.c` bytes
/// alone would happily reuse an object built against a different `stdint.h`,
/// a different include path or different predefined macros. The exact
/// preprocessed bytes are also what gets compiled on a miss, so the key and
/// the object cannot describe different things.
fn preprocessed(cc: &str, dir: &Path, sources: &[&str]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for name in sources {
        // `-P` suppresses line markers. Without it the preprocessed text
        // embeds the absolute path of the temporary directory the sources
        // were written to, which differs on every invocation -- so the key
        // would never repeat and the cache would silently never hit.
        let result = process::Command::new(cc)
            .arg("-E")
            .arg("-P")
            .arg(dir.join(name))
            .output()
            .ok()?;
        if !result.status.success() {
            return None;
        }
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&result.stdout);
    }
    Some(out)
}

/// Compiled runtime objects ready to link.
pub struct RuntimeObjects {
    pub objects: Vec<PathBuf>,
}

/// How the C runtime reaches the link step.
pub enum Runtime {
    /// Project-style build: reuse verified objects from the cache.
    Cached(RuntimeObjects),
    /// Cache bypassed: same separate-object shape, publish nothing. Keeping
    /// the shape is deliberate — `--no-cache` is a bypass, not a different
    /// build strategy, and collapsing it would make it untestable as one.
    Separate(RuntimeObjects),
    /// No cache is possible or wanted: compile the C sources as part of the
    /// link, in one `cc` invocation. Splitting this path into three processes
    /// buys nothing (the objects are discarded) and measurably costs time.
    Inline,
}

/// Compile the runtime sources into separate objects under `out_dir`.
fn compile_objects(cc: &str, src_dir: &Path, out_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut objects = Vec::new();
    for name in ["runtime.c", "gc.c", "unicode_data.c"] {
        let object = out_dir.join(format!("{}.o", name.trim_end_matches(".c")));
        let status = process::Command::new(cc)
            .arg("-c")
            .arg(src_dir.join(name))
            .arg("-O2")
            .arg("-Wno-format-truncation")
            .arg("-o")
            .arg(&object)
            .status()
            .map_err(|e| format!("failed to invoke the C compiler '{cc}': {e}"))?;
        if !status.success() {
            return Err(format!("compiling {name} failed (see 'cc' output above)"));
        }
        objects.push(object);
    }
    Ok(objects)
}

/// Digest of a file's bytes, or `None` if it cannot be read.
fn file_digest(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Some(h.hex())
}

/// Publish `staging` at `final_path` by rename, tolerating a concurrent
/// builder that got there first — both produced the same bytes, by
/// construction of the key.
fn publish(staging: &Path, final_path: &Path) -> io::Result<()> {
    match fs::rename(staging, final_path) {
        Ok(()) => Ok(()),
        Err(_) if final_path.exists() => {
            let _ = fs::remove_dir_all(staging);
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Obtain the runtime objects, from the cache when possible.
///
/// `src_dir` holds the runtime sources already written out by the caller.
pub fn runtime_objects(
    cc: &str,
    src_dir: &Path,
    work_dir: &Path,
    use_cache: bool,
) -> Result<Runtime, String> {
    if !use_cache {
        let objects = compile_objects(cc, src_dir, work_dir)?;
        return Ok(Runtime::Separate(RuntimeObjects { objects }));
    }
    let Some(root) = root() else {
        return Ok(Runtime::Inline);
    };
    let Some(pp) = preprocessed(cc, src_dir, &["runtime.c", "gc.c", "unicode_data.c"]) else {
        // Preprocessing failed: fall back rather than fail the build, since
        // the compile itself will report the real error.
        return Ok(Runtime::Inline);
    };

    let mut h = Sha256::new();
    h.field(&FORMAT.to_le_bytes());
    h.field(BUILD_FINGERPRINT.trim().as_bytes());
    h.field(toolchain_identity(cc).as_bytes());
    h.field(b"-O2 -Wno-format-truncation");
    h.field(&pp);
    let key = h.hex();

    let entry = root.join("runtime").join(&key);
    let names = ["runtime.o", "gc.o", "unicode_data.o"];
    let checksums = entry.join("checksums");

    // Verify before reuse: a truncated or corrupted object must be rebuilt,
    // not linked.
    if let Ok(recorded) = fs::read_to_string(&checksums) {
        let expected: Vec<&str> = recorded.lines().collect();
        let objects: Vec<PathBuf> = names.iter().map(|n| entry.join(n)).collect();
        let intact = expected.len() == names.len()
            && objects
                .iter()
                .zip(&expected)
                .all(|(p, want)| file_digest(p).as_deref() == Some(*want));
        if intact {
            return Ok(Runtime::Cached(RuntimeObjects { objects }));
        }
        let _ = fs::remove_dir_all(&entry);
    }

    let staging =
        root.join("runtime")
            .join(format!(".staging-{}-{}", std::process::id(), &key[..16]));
    fs::create_dir_all(&staging)
        .map_err(|e| format!("failed to create {}: {e}", staging.display()))?;
    let built = compile_objects(cc, src_dir, &staging)?;

    let mut recorded = String::new();
    for object in &built {
        let Some(d) = file_digest(object) else {
            let _ = fs::remove_dir_all(&staging);
            return Ok(Runtime::Separate(RuntimeObjects { objects: built }));
        };
        recorded.push_str(&d);
        recorded.push('\n');
    }
    // Name the objects as the entry expects before publishing the directory.
    for (object, name) in built.iter().zip(names) {
        let target = staging.join(name);
        if object != &target {
            let _ = fs::rename(object, &target);
        }
    }
    let _ = fs::write(staging.join("checksums"), &recorded);
    if publish(&staging, &entry).is_err() {
        let objects = names.iter().map(|n| staging.join(n)).collect();
        return Ok(Runtime::Separate(RuntimeObjects { objects }));
    }
    let objects = names.iter().map(|n| entry.join(n)).collect();
    Ok(Runtime::Cached(RuntimeObjects { objects }))
}

/// Key for a whole compiled program.
///
/// Covers every module in the resolved import graph — which the module
/// resolver has already computed exactly, so the input set is a fact rather
/// than a guess — plus everything else that changes the output bytes.
pub fn program_key(sources: &[(String, String)], opt_level: u8, cc: &str) -> String {
    let mut h = Sha256::new();
    h.field(&FORMAT.to_le_bytes());
    h.field(BUILD_FINGERPRINT.trim().as_bytes());
    h.field(&[opt_level]);
    h.field(toolchain_identity(cc).as_bytes());
    h.field(std::env::consts::ARCH.as_bytes());
    h.field(std::env::consts::OS.as_bytes());
    for (name, source) in sources {
        h.field(name.as_bytes());
        h.field(source.as_bytes());
    }
    h.hex()
}

/// Path of a cached program, if one is present and intact.
pub fn program_lookup(key: &str) -> Option<PathBuf> {
    let entry = root()?.join("programs").join(key);
    let binary = entry.join("program");
    let recorded = fs::read_to_string(entry.join("checksum")).ok()?;
    if file_digest(&binary).as_deref() == Some(recorded.trim()) {
        return Some(binary);
    }
    let _ = fs::remove_dir_all(&entry);
    None
}

/// Publish a freshly built program. Failure is silent: the build succeeded,
/// and an unwritable cache must not turn that into an error.
pub fn program_store(key: &str, built: &Path) -> Option<PathBuf> {
    let root = root()?;
    let dir = root.join("programs");
    let staging = dir.join(format!(".staging-{}-{}", std::process::id(), &key[..16]));
    fs::create_dir_all(&staging).ok()?;
    let binary = staging.join("program");
    fs::copy(built, &binary).ok()?;
    let digest = file_digest(&binary)?;
    fs::write(staging.join("checksum"), &digest).ok()?;
    let entry = dir.join(key);
    if publish(&staging, &entry).is_err() {
        let _ = fs::remove_dir_all(&staging);
        return None;
    }
    Some(entry.join("program"))
}
