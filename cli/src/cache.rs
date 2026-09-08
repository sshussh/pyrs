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
use std::time::{Duration, SystemTime};

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

/// Extra flags for the C compile step (`PYRS_CFLAGS`) and the link step
/// (`PYRS_LDFLAGS`), split on whitespace.
///
/// Deliberately *not* `CFLAGS`. That variable is a make convention, is
/// routinely set machine-wide for unrelated builds, and `cc` does not read
/// it on its own — silently adopting it would change PyRs's output because
/// of a setting aimed at something else. A PyRs-specific name makes the
/// opt-in explicit, and both are part of the cache key, so changing one
/// invalidates rather than silently reuses.
pub fn extra_flags(var: &str) -> Vec<String> {
    std::env::var(var)
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect()
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
            .args(extra_flags("PYRS_CFLAGS"))
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
    h.field(extra_flags("PYRS_CFLAGS").join(" ").as_bytes());
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
            touch(&entry);
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
pub fn program_key(
    sources: &[(String, String)],
    opt_level: u8,
    cc: &str,
    target_cpu: &str,
) -> String {
    let mut h = Sha256::new();
    h.field(&FORMAT.to_le_bytes());
    h.field(BUILD_FINGERPRINT.trim().as_bytes());
    h.field(&[opt_level]);
    h.field(toolchain_identity(cc).as_bytes());
    h.field(std::env::consts::ARCH.as_bytes());
    // The *resolved* CPU and feature string, not the request: two hosts both
    // asking for "native" resolve it differently, and a cache shared between
    // them would otherwise hand one machine the other's illegal instructions.
    h.field(codegen::target_identity(target_cpu).as_bytes());
    h.field(std::env::consts::OS.as_bytes());
    h.field(extra_flags("PYRS_CFLAGS").join(" ").as_bytes());
    h.field(extra_flags("PYRS_LDFLAGS").join(" ").as_bytes());
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
        touch(&entry);
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

// ---------------------------------------------------------------------------
// Management
// ---------------------------------------------------------------------------
//
// A cache with no way to inspect or bound it is a directory that only grows.
// Measured on this machine before any of the below existed: 998 MB across
// 3736 program entries, accumulated in about a day of test runs, with a
// deleted directory as the only remedy. `uv cache dir/info/clean/prune` is
// the shape this borrows.

/// The cache layers, in the order `pyrs cache info` reports them.
pub const LAYERS: [&str; 3] = ["programs", "runtime", "toolchain"];

/// Default ceiling for the opportunistic prune, in bytes. Generous enough
/// that an ordinary week of work never reaches it, small enough that an
/// unattended machine does not lose a tenth of its disk to build artifacts.
const DEFAULT_LIMIT: u64 = 2 * 1024 * 1024 * 1024;

/// How often the opportunistic prune is allowed to walk the cache when
/// nothing much has been added.
const GC_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Prune early once this fraction of the limit has been added since the last
/// one — an eighth, so the cache can exceed its ceiling by at most about
/// 12% before the walk happens regardless of the clock.
const GC_GROWTH_FRACTION: u64 = 8;

/// Resolution of the reuse clock. An entry's `used` stamp is rewritten at
/// most this often, so a hot cache does not pay a write on every hit while
/// still ordering entries finely enough for an age- or size-based policy.
const TOUCH_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// What one layer holds.
pub struct LayerStats {
    pub name: &'static str,
    pub entries: usize,
    pub bytes: u64,
}

/// One cache entry, for pruning.
struct Entry {
    path: PathBuf,
    used: SystemTime,
    bytes: u64,
}

/// Recursive byte total, not following symlinks.
fn dir_size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    let Ok(read) = fs::read_dir(path) else {
        return 0;
    };
    read.flatten().map(|e| dir_size(&e.path())).sum()
}

/// Whether a directory name is a half-published entry rather than a real one.
fn is_staging(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with(".staging")
}

/// Record that `entry` was just reused, so pruning can distinguish the
/// program you build every hour from the one you built once in March.
///
/// Rewritten only every [`TOUCH_INTERVAL`], and failure is ignored: a
/// read-only cache should still be *read*.
fn touch(entry: &Path) {
    let stamp = entry.join("used");
    if let Ok(meta) = fs::metadata(&stamp)
        && let Ok(mtime) = meta.modified()
        && mtime.elapsed().is_ok_and(|age| age < TOUCH_INTERVAL)
    {
        return;
    }
    let _ = fs::write(&stamp, b"");
}

/// When `entry` was last reused. Falls back to the directory's own
/// modification time, which for an entry never reused since it was published
/// is when it was built.
fn last_used(entry: &Path) -> SystemTime {
    let stamp = entry.join("used");
    fs::metadata(&stamp)
        .or_else(|_| fs::metadata(entry))
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Every real entry in `layer`, with its size and last reuse.
fn entries_of(root: &Path, layer: &str) -> Vec<Entry> {
    let Ok(read) = fs::read_dir(root.join(layer)) else {
        return Vec::new();
    };
    read.flatten()
        .filter(|e| !is_staging(&e.file_name()))
        .map(|e| {
            let path = e.path();
            Entry {
                used: last_used(&path),
                bytes: dir_size(&path),
                path,
            }
        })
        .collect()
}

/// Per-layer entry counts and sizes.
pub fn stats(root: &Path) -> Vec<LayerStats> {
    LAYERS
        .iter()
        .map(|&name| {
            let entries = entries_of(root, name);
            LayerStats {
                name,
                entries: entries.len(),
                bytes: entries.iter().map(|e| e.bytes).sum(),
            }
        })
        .collect()
}

/// What a clean or prune did, so the caller can report it without
/// re-walking the directory.
pub struct Removed {
    pub entries: usize,
    pub bytes: u64,
}

/// Remove every entry in `layers`. `dry_run` reports without deleting.
pub fn clean(root: &Path, layers: &[&str], dry_run: bool) -> Removed {
    let mut removed = Removed {
        entries: 0,
        bytes: 0,
    };
    for layer in layers {
        for entry in entries_of(root, layer) {
            if !dry_run && fs::remove_dir_all(&entry.path).is_err() {
                continue;
            }
            removed.entries += 1;
            removed.bytes += entry.bytes;
        }
    }
    removed
}

/// What a prune is allowed to keep.
#[derive(Default)]
pub struct PruneOptions {
    /// Drop entries not reused within this long.
    pub older_than: Option<Duration>,
    /// Drop least-recently-used entries until the total fits.
    pub max_size: Option<u64>,
    pub dry_run: bool,
}

/// Prune `layers` down to `opts`.
///
/// Age is applied first, then the size ceiling to whatever survived, so
/// `--older-than 7d --max-size 500MB` means both rather than whichever runs
/// last. Eviction is least-recently-used: the entry you rebuild every day
/// should be the last one to go, not an arbitrary one.
///
/// `toolchain` is never pruned. Its entries are 64 bytes each and losing one
/// costs two subprocesses on the next build for no space worth reclaiming.
pub fn prune(root: &Path, layers: &[&str], opts: &PruneOptions) -> Removed {
    let mut removed = Removed {
        entries: 0,
        bytes: 0,
    };
    let mut survivors: Vec<Entry> = Vec::new();

    for layer in layers {
        for entry in entries_of(root, layer) {
            let expired = opts
                .older_than
                .is_some_and(|max| entry.used.elapsed().is_ok_and(|age| age > max));
            if expired {
                if opts.dry_run || fs::remove_dir_all(&entry.path).is_ok() {
                    removed.entries += 1;
                    removed.bytes += entry.bytes;
                }
            } else {
                survivors.push(entry);
            }
        }
    }

    let Some(limit) = opts.max_size else {
        return removed;
    };
    let mut total: u64 = survivors.iter().map(|e| e.bytes).sum();
    if total <= limit {
        return removed;
    }
    // Oldest first: the least-recently-used entry is the cheapest to lose.
    survivors.sort_by_key(|e| e.used);
    for entry in survivors {
        if total <= limit {
            break;
        }
        if !opts.dry_run && fs::remove_dir_all(&entry.path).is_err() {
            continue;
        }
        total = total.saturating_sub(entry.bytes);
        removed.entries += 1;
        removed.bytes += entry.bytes;
    }
    removed
}

/// The ceiling the opportunistic prune enforces: `PYRS_CACHE_LIMIT`, or
/// [`DEFAULT_LIMIT`]. `0` (or an unparseable value) disables it.
fn configured_limit() -> Option<u64> {
    match std::env::var("PYRS_CACHE_LIMIT") {
        Ok(text) => parse_size(text.trim()).filter(|&n| n > 0),
        Err(_) => Some(DEFAULT_LIMIT),
    }
}

/// Keep the cache under its ceiling.
///
/// Called after publishing a program, which is the only moment the cache
/// grows, with the size of what was just published.
///
/// **A time interval alone does not bound a cache.** The first version of
/// this checked once per [`GC_INTERVAL`] and nothing else, which let the
/// development cache reach 3.6 GiB against a 2 GiB ceiling in the 46 minutes
/// after a check found it compliant — a test suite publishes thousands of
/// entries in an hour. So growth is tracked too: once
/// [`GC_GROWTH_FRACTION`] of the limit has been added since the last prune,
/// the walk happens regardless of the clock, which bounds the overshoot by
/// construction rather than by hoping builds are spread out.
///
/// Silent and best-effort throughout: a build that succeeded must not be
/// reported as failed because housekeeping could not run.
pub fn maintain(added: u64) {
    let Some(limit) = configured_limit() else {
        return;
    };
    let Some(root) = root() else {
        return;
    };

    let pending = root.join("gc-pending");
    let grown = fs::read_to_string(&pending)
        .ok()
        .and_then(|t| t.trim().parse::<u64>().ok())
        .unwrap_or(0)
        .saturating_add(added);

    let overdue = fs::metadata(root.join("last-gc"))
        .and_then(|m| m.modified())
        .map_or(true, |mtime| {
            mtime.elapsed().is_ok_and(|age| age >= GC_INTERVAL)
        });
    // A lost update between concurrent builders delays a prune, never
    // corrupts one, so the counter needs no locking.
    if !overdue && grown < limit / GC_GROWTH_FRACTION {
        let _ = fs::write(&pending, grown.to_string());
        return;
    }

    // Stamp first: a prune that is killed must not make every later build
    // retry the same walk.
    let _ = fs::write(root.join("last-gc"), b"");
    let _ = fs::write(&pending, "0");
    prune(
        &root,
        &["programs", "runtime"],
        &PruneOptions {
            max_size: Some(limit),
            ..PruneOptions::default()
        },
    );
}

/// Parse `7d`, `24h`, `30m`, `90s`, or a bare number of seconds.
pub fn parse_duration(text: &str) -> Option<Duration> {
    let text = text.trim();
    let (digits, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => text.split_at(i),
        None => (text, "s"),
    };
    let n: u64 = digits.parse().ok()?;
    let secs = match unit {
        "s" | "" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        "w" => 7 * 24 * 60 * 60,
        _ => return None,
    };
    n.checked_mul(secs).map(Duration::from_secs)
}

/// Parse `500MB`, `1GiB`, `2G`, or a bare number of bytes.
///
/// `MB` means 1024², not 1000², matching what `du -h` prints — a size that
/// disagrees with the tool the user just checked the directory with would be
/// worse than no unit suffix at all.
pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let (digits, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => text.split_at(i),
        None => (text, "B"),
    };
    let n: u64 = digits.parse().ok()?;
    let scale: u64 = match unit.trim().to_ascii_uppercase().as_str() {
        "B" | "" => 1,
        "K" | "KB" | "KIB" => 1024,
        "M" | "MB" | "MIB" => 1024 * 1024,
        "G" | "GB" | "GIB" => 1024 * 1024 * 1024,
        "T" | "TB" | "TIB" => 1024u64.pow(4),
        _ => return None,
    };
    n.checked_mul(scale)
}

/// Human-readable byte count, matching `du -h`'s units.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_in_the_units_people_type() {
        assert_eq!(parse_duration("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("15m"), Some(Duration::from_secs(900)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("7d"), Some(Duration::from_secs(604_800)));
        assert_eq!(parse_duration("1w"), Some(Duration::from_secs(604_800)));
        assert_eq!(parse_duration(" 7d "), Some(Duration::from_secs(604_800)));
    }

    #[test]
    fn an_unrecognized_duration_is_rejected_rather_than_rounded() {
        // Silently reading "7 years" as 7 seconds would delete the cache.
        for text in ["", "7y", "d", "soon", "-1d", "7dd", "1.5h"] {
            assert_eq!(parse_duration(text), None, "accepted {text:?}");
        }
    }

    #[test]
    fn sizes_parse_in_the_units_du_prints() {
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size("2K"), Some(2048));
        assert_eq!(parse_size("1MB"), Some(1024 * 1024));
        assert_eq!(parse_size("1MiB"), Some(1024 * 1024));
        assert_eq!(parse_size("2GiB"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("1gb"), Some(1024 * 1024 * 1024));
    }

    #[test]
    fn an_unrecognized_size_is_rejected() {
        for text in ["", "MB", "lots", "-5M", "1.5G", "5Z"] {
            assert_eq!(parse_size(text), None, "accepted {text:?}");
        }
    }

    #[test]
    fn a_size_that_would_overflow_is_rejected_rather_than_wrapped() {
        // Wrapping would produce a small budget from a huge request, and
        // then delete almost everything.
        assert_eq!(parse_size("99999999999999999999"), None);
        assert_eq!(parse_size(&format!("{}T", u64::MAX)), None);
    }

    #[test]
    fn sizes_render_the_way_du_h_does() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(1024 * 1024), "1.0 MiB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn flags_split_the_way_a_shell_would_on_whitespace() {
        // Safety: these tests share a process, so the variable is set and
        // read without an intervening await point.
        unsafe { std::env::set_var("PYRS_TEST_FLAGS", "  -DA=1   -O0 ") };
        assert_eq!(extra_flags("PYRS_TEST_FLAGS"), vec!["-DA=1", "-O0"]);
        unsafe { std::env::remove_var("PYRS_TEST_FLAGS") };
        assert!(extra_flags("PYRS_TEST_FLAGS").is_empty());
    }
}
