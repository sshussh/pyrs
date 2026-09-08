//! Build caching: runtime objects and whole programs.
//!
//! A one-line program took 2.61s to build, of which 2.39s was compiling
//! `runtime.c` — 92% of the floor, paid again on every `pyrs run` of a
//! program that had not changed.
//!
//! The property worth testing is not "it got faster" (machine-sensitive, and
//! it does not belong in a gate) but "it did not serve a stale artifact". A
//! stale entry is a wrong answer that looks like a right one, so these tests
//! change one input at a time and assert the output follows.
//!
//! Cache reuse itself is asserted by *counting compiler invocations* through
//! a `cc` wrapper, which is what makes "the cache was used" a fact rather
//! than an inference from a stopwatch.

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

/// An isolated project directory and its own cache root, so tests neither
/// see each other's entries nor the developer's real cache.
fn sandbox(tag: &str) -> (TempDir, PathBuf, PathBuf) {
    let dir = TempDir(
        Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-cache-{tag}-{}", std::process::id())),
    );
    let src = dir.0.join("src");
    let cache = dir.0.join("cache");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&cache).unwrap();
    (dir, src, cache)
}

fn write(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
}

fn run_in(cache: &Path, args: &[&str]) -> std::process::Output {
    Command::new(PYRS)
        .args(args)
        .env("PYRS_CACHE_DIR", cache)
        .output()
        .expect("failed to spawn PyRs")
}

fn stdout_of(cache: &Path, args: &[&str]) -> String {
    let out = run_in(cache, args);
    assert!(
        out.status.success(),
        "PyRs failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn entries(cache: &Path, kind: &str) -> usize {
    fs::read_dir(cache.join(kind))
        .map(|d| {
            d.flatten()
                .filter(|e| !e.file_name().to_string_lossy().starts_with(".staging"))
                .count()
        })
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Invalidation — the property that matters
// ---------------------------------------------------------------------------

#[test]
fn editing_the_program_rebuilds_it() {
    let (_d, src, cache) = sandbox("edit-main");
    let prog = src.join("prog.py");
    write(&prog, "print(\"first\")\n");
    assert_eq!(
        stdout_of(&cache, &["run", "-i", prog.to_str().unwrap()]),
        "first\n"
    );
    write(&prog, "print(\"second\")\n");
    assert_eq!(
        stdout_of(&cache, &["run", "-i", prog.to_str().unwrap()]),
        "second\n"
    );
}

#[test]
fn editing_an_imported_module_rebuilds_the_program() {
    // The key covers every module in the resolved import graph, not just the
    // entry point -- otherwise a library edit would be invisible.
    let (_d, src, cache) = sandbox("edit-import");
    write(
        &src.join("helper.py"),
        "def greet() -> str:\n    return \"v1\"\n",
    );
    let prog = src.join("prog.py");
    write(&prog, "import helper\nprint(helper.greet())\n");
    assert_eq!(
        stdout_of(&cache, &["run", "-i", prog.to_str().unwrap()]),
        "v1\n"
    );

    write(
        &src.join("helper.py"),
        "def greet() -> str:\n    return \"v2\"\n",
    );
    assert_eq!(
        stdout_of(&cache, &["run", "-i", prog.to_str().unwrap()]),
        "v2\n"
    );
}

#[test]
fn each_optimization_level_gets_its_own_entry() {
    let (_d, src, cache) = sandbox("opt-levels");
    let prog = src.join("prog.py");
    write(&prog, "print(\"opt\")\n");
    for opt in ["0", "2", "3"] {
        assert_eq!(
            stdout_of(&cache, &["run", "-O", opt, "-i", prog.to_str().unwrap()]),
            "opt\n"
        );
    }
    assert_eq!(entries(&cache, "programs"), 3, "one program entry per -O");
    assert_eq!(
        entries(&cache, "runtime"),
        1,
        "the C runtime is built at -O2 regardless, so one entry serves all"
    );
}

/// A binary built for `native` carries this host's ISA extensions, so it must
/// never be served from a cache entry a `generic` build wrote — on a machine
/// with a different CPU that is an illegal instruction rather than a
/// diagnostic. The key holds the *resolved* model and feature string, not the
/// request, which is what makes a cache shared across microarchitectures safe.
#[test]
fn each_target_cpu_gets_its_own_entry() {
    let (_d, src, cache) = sandbox("target-cpu");
    let prog = src.join("cpu.py");
    write(&prog, "print(\"cpu\")\n");
    for cpu in ["generic", "native"] {
        assert_eq!(
            stdout_of(
                &cache,
                &["run", "--target-cpu", cpu, "-i", prog.to_str().unwrap()]
            ),
            "cpu\n"
        );
    }
    assert_eq!(
        entries(&cache, "programs"),
        2,
        "generic and native must not share a program entry"
    );
}

/// Asking for the same CPU twice is a hit, so the resolved identity has to be
/// stable rather than, say, re-probing into a differently ordered feature
/// string each time.
#[test]
fn the_same_target_cpu_hits_the_cache() {
    let (_d, src, cache) = sandbox("target-cpu-stable");
    let prog = src.join("cpu.py");
    write(&prog, "print(\"stable\")\n");
    for _ in 0..3 {
        assert_eq!(
            stdout_of(
                &cache,
                &[
                    "run",
                    "--target-cpu",
                    "native",
                    "-i",
                    prog.to_str().unwrap()
                ]
            ),
            "stable\n"
        );
    }
    assert_eq!(entries(&cache, "programs"), 1);
}

#[test]
fn two_programs_with_the_same_text_share_an_entry() {
    // The key is content, not path: identical inputs produce an identical
    // binary, so sharing is correct rather than a collision.
    let (_d, src, cache) = sandbox("same-text");
    for name in ["a.py", "b.py"] {
        let p = src.join(name);
        write(&p, "print(\"same\")\n");
        assert_eq!(
            stdout_of(&cache, &["run", "-i", p.to_str().unwrap()]),
            "same\n"
        );
    }
    assert_eq!(entries(&cache, "programs"), 1);
}

// ---------------------------------------------------------------------------
// Damage and bypass
// ---------------------------------------------------------------------------

#[test]
fn a_corrupted_program_entry_is_rebuilt_not_executed() {
    let (_d, src, cache) = sandbox("corrupt-program");
    let prog = src.join("prog.py");
    write(&prog, "print(\"intact\")\n");
    assert_eq!(
        stdout_of(&cache, &["run", "-i", prog.to_str().unwrap()]),
        "intact\n"
    );

    let entry = fs::read_dir(cache.join("programs"))
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path();
    write(&entry.join("program"), "not an executable");
    assert_eq!(
        stdout_of(&cache, &["run", "-i", prog.to_str().unwrap()]),
        "intact\n"
    );
}

#[test]
fn a_corrupted_runtime_object_is_rebuilt_not_linked() {
    let (_d, src, cache) = sandbox("corrupt-runtime");
    let first = src.join("one.py");
    write(&first, "print(\"one\")\n");
    stdout_of(&cache, &["run", "-i", first.to_str().unwrap()]);

    let entry = fs::read_dir(cache.join("runtime"))
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path();
    write(&entry.join("runtime.o"), "truncated");

    let second = src.join("two.py");
    write(&second, "print(\"two\")\n");
    assert_eq!(
        stdout_of(&cache, &["run", "-i", second.to_str().unwrap()]),
        "two\n"
    );
}

#[test]
fn no_cache_neither_reuses_nor_publishes() {
    let (_d, src, cache) = sandbox("no-cache");
    let prog = src.join("prog.py");
    write(&prog, "print(\"bypass\")\n");
    assert_eq!(
        stdout_of(&cache, &["run", "--no-cache", "-i", prog.to_str().unwrap()]),
        "bypass\n"
    );
    assert_eq!(entries(&cache, "programs"), 0, "published nothing");
    assert_eq!(entries(&cache, "runtime"), 0, "published nothing");
}

#[test]
fn a_program_that_fails_to_compile_runs_nothing() {
    let (_d, src, cache) = sandbox("failed-build");
    let prog = src.join("prog.py");
    write(&prog, "print(\"good\")\n");
    assert_eq!(
        stdout_of(&cache, &["run", "-i", prog.to_str().unwrap()]),
        "good\n"
    );

    write(&prog, "print(undefined_name)\n");
    let out = run_in(&cache, &["run", "-i", prog.to_str().unwrap()]);
    assert!(!out.status.success(), "the broken program must not run");
    assert!(
        out.stdout.is_empty(),
        "a stale artifact was executed: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

// ---------------------------------------------------------------------------
// Reuse, asserted by counting compiler invocations
// ---------------------------------------------------------------------------

/// A `cc` wrapper that appends its arguments to a log, so a test can tell
/// compiling the runtime apart from preprocessing and linking.
fn install_cc_wrapper(dir: &Path) -> (PathBuf, PathBuf) {
    let log = dir.join("cc.log");
    let wrapper = dir.join("cc-wrapper");
    write(
        &wrapper,
        &format!(
            "#!/bin/sh\necho \"$@\" >> {}\nexec cc \"$@\"\n",
            log.display()
        ),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    }
    (wrapper, log)
}

/// Invocations that compiled a runtime translation unit (`-c` on a `.c`),
/// as opposed to preprocessing (`-E`) or linking.
fn runtime_compiles(log: &Path) -> usize {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("-c ") && l.contains(".c") && !l.contains("-E"))
        .count()
}

#[test]
fn a_warm_runtime_cache_compiles_no_c_at_all() {
    let (_d, src, cache) = sandbox("count-runtime");
    let (wrapper, log) = install_cc_wrapper(&_d.0);

    let first = src.join("one.py");
    write(&first, "print(1)\n");
    let out = Command::new(PYRS)
        .args(["run", "-i", first.to_str().unwrap()])
        .env("PYRS_CACHE_DIR", &cache)
        .env("CC", &wrapper)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let cold = runtime_compiles(&log);
    assert!(
        cold >= 3,
        "cold build should compile the runtime, saw {cold}"
    );

    // A *different* program: the program cache cannot help, so any saving
    // here is the runtime cache and nothing else.
    let _ = fs::remove_file(&log);
    let second = src.join("two.py");
    write(&second, "print(2)\n");
    let out = Command::new(PYRS)
        .args(["run", "-i", second.to_str().unwrap()])
        .env("PYRS_CACHE_DIR", &cache)
        .env("CC", &wrapper)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        runtime_compiles(&log),
        0,
        "the runtime was recompiled despite a warm cache"
    );
}

#[test]
fn a_warm_program_cache_invokes_the_compiler_not_at_all() {
    let (_d, src, cache) = sandbox("count-program");
    let (wrapper, log) = install_cc_wrapper(&_d.0);
    let prog = src.join("prog.py");
    write(&prog, "print(\"warm\")\n");

    for _ in 0..2 {
        let out = Command::new(PYRS)
            .args(["run", "-i", prog.to_str().unwrap()])
            .env("PYRS_CACHE_DIR", &cache)
            .env("CC", &wrapper)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let total = fs::read_to_string(&log).unwrap_or_default().lines().count();
    let first_build_only = total > 0;
    assert!(first_build_only, "the cold build should have invoked cc");

    let _ = fs::remove_file(&log);
    let out = Command::new(PYRS)
        .args(["run", "-i", prog.to_str().unwrap()])
        .env("PYRS_CACHE_DIR", &cache)
        .env("CC", &wrapper)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(&log).unwrap_or_default().lines().count(),
        0,
        "a warm program cache must not invoke the C compiler"
    );
}

#[test]
fn a_different_compiler_gets_its_own_entries() {
    // The object bytes depend on the toolchain, so its identity is part of
    // the key. Skipped where there is no second compiler to switch to.
    let (_d, src, cache) = sandbox("compiler-change");
    let Some(other) = ["gcc", "clang"]
        .into_iter()
        .find(|c| Command::new(c).arg("--version").output().is_ok())
    else {
        return;
    };
    let prog = src.join("prog.py");
    write(&prog, "print(\"cc\")\n");
    stdout_of(&cache, &["run", "-i", prog.to_str().unwrap()]);
    let before = entries(&cache, "runtime");

    let out = Command::new(PYRS)
        .args(["run", "-i", prog.to_str().unwrap()])
        .env("PYRS_CACHE_DIR", &cache)
        .env("CC", other)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        entries(&cache, "runtime") > before,
        "switching compilers reused another toolchain's objects"
    );
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

#[test]
fn concurrent_builds_publish_one_entry_and_leave_no_staging() {
    let (_d, src, cache) = sandbox("concurrent");
    let prog = src.join("prog.py");
    write(&prog, "print(\"concurrent\")\n");

    let handles: Vec<_> = (0..6)
        .map(|_| {
            let cache = cache.clone();
            let prog = prog.clone();
            std::thread::spawn(move || {
                Command::new(PYRS)
                    .args(["run", "-i", prog.to_str().unwrap()])
                    .env("PYRS_CACHE_DIR", &cache)
                    .output()
                    .unwrap()
            })
        })
        .collect();
    for h in handles {
        let out = h.join().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), "concurrent\n");
    }
    assert_eq!(entries(&cache, "programs"), 1);
    let staging = fs::read_dir(cache.join("programs"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".staging"))
        .count();
    assert_eq!(staging, 0, "a staging directory was left behind");
}

// ---------------------------------------------------------------------------
// compile, and artifacts the cache does not hold
// ---------------------------------------------------------------------------

#[test]
fn compile_reuses_the_cache_and_still_writes_its_output() {
    let (_d, src, cache) = sandbox("compile-output");
    let prog = src.join("prog.py");
    write(&prog, "print(\"compiled\")\n");
    let first = src.join("first.bin");
    let second = src.join("second.bin");

    stdout_of(
        &cache,
        &[
            "compile",
            "-i",
            prog.to_str().unwrap(),
            "-o",
            first.to_str().unwrap(),
        ],
    );
    stdout_of(
        &cache,
        &[
            "compile",
            "-i",
            prog.to_str().unwrap(),
            "-o",
            second.to_str().unwrap(),
        ],
    );
    assert!(
        second.exists(),
        "a cache hit must still produce the output file"
    );

    let out = Command::new(&second).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "compiled\n");
}

#[test]
fn emit_llvm_still_writes_the_ir_on_a_repeat_build() {
    // The cache holds an executable, not the .ll side artifact, so this
    // build must not be short-circuited.
    let (_d, src, cache) = sandbox("emit-llvm");
    let prog = src.join("prog.py");
    write(&prog, "print(\"ir\")\n");
    let out_bin = src.join("out.bin");
    let args = [
        "compile",
        "-i",
        prog.to_str().unwrap(),
        "-o",
        out_bin.to_str().unwrap(),
    ];
    stdout_of(&cache, &args);

    let mut with_llvm = args.to_vec();
    with_llvm.push("--emit-llvm");
    stdout_of(&cache, &with_llvm);
    assert!(
        out_bin.with_extension("ll").exists(),
        "--emit-llvm produced no IR on a cached build"
    );
}

// ---------------------------------------------------------------------------
// Management — inspecting, cleaning and pruning
// ---------------------------------------------------------------------------

/// Total bytes under a layer, so a size assertion does not have to trust the
/// same walk the implementation uses.
fn layer_bytes(cache: &Path, kind: &str) -> u64 {
    fn walk(path: &Path) -> u64 {
        let Ok(meta) = fs::symlink_metadata(path) else {
            return 0;
        };
        if !meta.is_dir() {
            return meta.len();
        }
        fs::read_dir(path)
            .map(|d| d.flatten().map(|e| walk(&e.path())).sum())
            .unwrap_or(0)
    }
    walk(&cache.join(kind))
}

#[test]
fn cache_dir_reports_the_configured_directory() {
    let (_dir, _src, cache) = sandbox("dir");
    let out = stdout_of(&cache, &["cache", "dir"]);
    assert_eq!(out.trim(), cache.to_string_lossy());
}

#[test]
fn cache_info_counts_what_was_built() {
    let (_dir, src, cache) = sandbox("info");
    let prog = src.join("a.py");
    write(&prog, "print(1)\n");
    run_in(&cache, &["run", "-i", prog.to_str().unwrap()]);

    let out = stdout_of(&cache, &["cache", "info"]);
    assert!(out.contains("programs"), "{out}");
    assert!(out.contains("runtime"), "{out}");
    assert!(out.contains("total"), "{out}");
    // One program, one set of runtime objects, both non-empty.
    assert_eq!(entries(&cache, "programs"), 1, "{out}");
    assert!(layer_bytes(&cache, "programs") > 0, "{out}");
}

#[test]
fn cleaning_programs_leaves_the_runtime_objects_alone() {
    let (_dir, src, cache) = sandbox("clean-programs");
    let prog = src.join("a.py");
    write(&prog, "print(1)\n");
    run_in(&cache, &["run", "-i", prog.to_str().unwrap()]);
    assert_eq!(entries(&cache, "programs"), 1);
    let runtime_before = entries(&cache, "runtime");
    assert!(runtime_before > 0);

    let out = stdout_of(&cache, &["cache", "clean", "--programs"]);
    assert!(out.starts_with("removed 1 entry"), "{out}");
    assert_eq!(entries(&cache, "programs"), 0);
    assert_eq!(
        entries(&cache, "runtime"),
        runtime_before,
        "cleaning programs must not throw away the objects every build shares"
    );
}

#[test]
fn a_dry_run_removes_nothing() {
    let (_dir, src, cache) = sandbox("dry-run");
    let prog = src.join("a.py");
    write(&prog, "print(1)\n");
    run_in(&cache, &["run", "-i", prog.to_str().unwrap()]);
    let before = entries(&cache, "programs");
    assert!(before > 0);

    let out = stdout_of(&cache, &["cache", "clean", "--dry-run"]);
    assert!(out.starts_with("would have removed"), "{out}");
    assert_eq!(entries(&cache, "programs"), before);
    assert!(entries(&cache, "runtime") > 0);
}

#[test]
fn a_cleaned_program_is_rebuilt_rather_than_lost() {
    let (_dir, src, cache) = sandbox("clean-rebuild");
    let prog = src.join("a.py");
    write(&prog, "print(7)\n");
    assert_eq!(
        stdout_of(&cache, &["run", "-i", prog.to_str().unwrap()]),
        "7\n"
    );

    stdout_of(&cache, &["cache", "clean"]);
    assert_eq!(entries(&cache, "programs"), 0);
    assert_eq!(entries(&cache, "runtime"), 0);

    // The cache is an optimization; emptying it changes timing, never output.
    assert_eq!(
        stdout_of(&cache, &["run", "-i", prog.to_str().unwrap()]),
        "7\n"
    );
    assert_eq!(entries(&cache, "programs"), 1);
}

#[test]
fn pruning_by_size_evicts_the_least_recently_used_entry() {
    let (_dir, src, cache) = sandbox("prune-lru");
    let old = src.join("old.py");
    let new = src.join("new.py");
    write(&old, "print('old')\n");
    write(&new, "print('new')\n");

    run_in(&cache, &["run", "-i", old.to_str().unwrap()]);
    // Distinct modification times: an LRU policy needs an order to read.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    run_in(&cache, &["run", "-i", new.to_str().unwrap()]);
    // Reuse the older one, which stamps it as the most recently used.
    assert_eq!(
        stdout_of(&cache, &["run", "-i", old.to_str().unwrap()]),
        "old\n"
    );
    assert_eq!(entries(&cache, "programs"), 2);

    // A budget that fits one program but not two.
    let budget = layer_bytes(&cache, "programs") * 3 / 4;
    let out = stdout_of(
        &cache,
        &[
            "cache",
            "prune",
            "--programs",
            "--max-size",
            &budget.to_string(),
        ],
    );
    assert!(out.starts_with("pruned 1 entry"), "{out}");
    assert_eq!(entries(&cache, "programs"), 1);
    // The survivor is the one that was reused, not the one built last.
    assert_eq!(
        stdout_of(&cache, &["run", "-i", old.to_str().unwrap()]),
        "old\n"
    );
}

#[test]
fn pruning_by_age_keeps_entries_that_are_young_enough() {
    let (_dir, src, cache) = sandbox("prune-age");
    let prog = src.join("a.py");
    write(&prog, "print(1)\n");
    run_in(&cache, &["run", "-i", prog.to_str().unwrap()]);
    assert_eq!(entries(&cache, "programs"), 1);

    let out = stdout_of(&cache, &["cache", "prune", "--older-than", "7d"]);
    assert!(out.starts_with("pruned 0 entries"), "{out}");
    assert_eq!(entries(&cache, "programs"), 1);

    // Everything is older than nothing.
    let out = stdout_of(&cache, &["cache", "prune", "--older-than", "0s"]);
    assert!(out.starts_with("pruned"), "{out}");
    assert_eq!(entries(&cache, "programs"), 0);
}

#[test]
fn prune_without_a_budget_is_refused_rather_than_guessed() {
    let (_dir, src, cache) = sandbox("prune-nobudget");
    let prog = src.join("a.py");
    write(&prog, "print(1)\n");
    run_in(&cache, &["run", "-i", prog.to_str().unwrap()]);

    let out = run_in(&cache, &["cache", "prune"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("nothing to prune by"), "{err}");
    // Refusing must not have been a refusal *after* deleting something.
    assert_eq!(entries(&cache, "programs"), 1);
}

#[test]
fn an_unparseable_budget_is_reported_not_ignored() {
    let (_dir, _src, cache) = sandbox("prune-badsize");
    for (flag, value) in [("--max-size", "later"), ("--older-than", "soon")] {
        let out = run_in(&cache, &["cache", "prune", flag, value]);
        assert!(!out.status.success(), "{flag} {value} was accepted");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("invalid"), "{err}");
    }
}

#[test]
fn build_flags_are_part_of_the_key() {
    let (_dir, src, cache) = sandbox("cflags");
    let prog = src.join("a.py");
    write(&prog, "print(1)\n");

    run_in(&cache, &["run", "-i", prog.to_str().unwrap()]);
    let baseline = entries(&cache, "programs");

    // Honored *and* keyed: a flag that can change the emitted bytes must not
    // silently reuse an entry built without it.
    let out = Command::new(PYRS)
        .args(["run", "-i", prog.to_str().unwrap()])
        .env("PYRS_CACHE_DIR", &cache)
        .env("PYRS_CFLAGS", "-DPYRS_CACHE_KEY_PROBE=1")
        .output()
        .expect("failed to spawn PyRs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1\n");
    assert_eq!(
        entries(&cache, "programs"),
        baseline + 1,
        "PYRS_CFLAGS changed but the program entry was reused"
    );
}

#[test]
fn the_ceiling_is_enforced_by_growth_not_only_by_the_clock() {
    // The bug this pins: the first version checked once per day and nothing
    // else, so the development cache reached 3.6 GiB against a 2 GiB limit
    // in the 46 minutes after a check found it compliant. A test suite
    // publishes thousands of entries in an hour; a time interval alone does
    // not bound a cache.
    let (_dir, src, cache) = sandbox("gc-growth");

    // A limit small enough that a handful of programs must exceed it.
    let limit = 400_000u64;
    let build = |n: usize| {
        let prog = src.join(format!("p{n}.py"));
        write(&prog, &format!("print({n})\n"));
        let out = Command::new(PYRS)
            .args(["run", "-i", prog.to_str().unwrap()])
            .env("PYRS_CACHE_DIR", &cache)
            .env("PYRS_CACHE_LIMIT", limit.to_string())
            .output()
            .expect("failed to spawn PyRs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };

    for n in 0..8 {
        build(n);
    }

    // Well inside a day, so only growth can have triggered the prune.
    let held = layer_bytes(&cache, "programs");
    assert!(
        held <= limit + limit / 4,
        "cache held {held} bytes against a {limit} limit; growth did not trigger a prune"
    );
    // Not emptied either: pruning to nothing would make the cache useless.
    assert!(entries(&cache, "programs") > 0, "the cache was emptied");
}

#[test]
fn a_disabled_limit_prunes_nothing() {
    let (_dir, src, cache) = sandbox("gc-disabled");
    for n in 0..4 {
        let prog = src.join(format!("p{n}.py"));
        write(&prog, &format!("print({n})\n"));
        let out = Command::new(PYRS)
            .args(["run", "-i", prog.to_str().unwrap()])
            .env("PYRS_CACHE_DIR", &cache)
            .env("PYRS_CACHE_LIMIT", "0")
            .output()
            .expect("failed to spawn PyRs");
        assert!(out.status.success());
    }
    assert_eq!(
        entries(&cache, "programs"),
        4,
        "PYRS_CACHE_LIMIT=0 must switch the opportunistic prune off"
    );
}
