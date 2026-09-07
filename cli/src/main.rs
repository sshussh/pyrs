//! Driver: orchestrates the pipeline (lex → parse → semantic → codegen →
//! link) and renders diagnostics against the original source.

use common::{Diagnostic, Span};
use std::{
    ffi::OsString,
    fs, io,
    io::{Read, Write},
    path::{Path, PathBuf},
    process,
};

mod cache;
mod cli;
mod diagnostics;
mod extension;
mod hash;
mod interpreter;
mod manifest;
mod modules;
mod testing;

use diagnostics::{Failure, Format};

fn main() {
    let args = cli::Cli::parse_env();
    let code = run(args).unwrap_or_else(|err| {
        eprintln!("{err}");
        1
    });
    process::exit(code);
}

fn run(args: cli::Cli) -> Result<i32, String> {
    match args.command {
        cli::Command::Lex(cmd) => {
            let source = read_source(&cmd.input)?;
            let tokens = lexer::lex(&source).map_err(|d| render_diag(&d, &cmd.input, &source))?;
            let mut text = String::new();
            for (token, _) in &tokens {
                match token {
                    lexer::Token::Newline | lexer::Token::EOF => {
                        text.push_str(&format!("{token:?}\n"));
                    }
                    _ => text.push_str(&format!("{token:?} ")),
                }
            }
            write_output(&text, cmd.output.as_deref())?;
            Ok(0)
        }
        cli::Command::Parse(cmd) => {
            let source = read_source(&cmd.input)?;
            let module =
                parser::parse(&source).map_err(|d| render_diag(&d, &cmd.input, &source))?;
            write_output(&format!("{module:#?}\n"), cmd.output.as_deref())?;
            Ok(0)
        }
        cli::Command::Compile(cmd) => compile_command(cmd),
        cli::Command::Run(cmd) => run_program(cmd),
        cli::Command::Check(cmd) => check_program(cmd),
        cli::Command::BuildExtension(cmd) => {
            extension::build(cmd)?;
            Ok(0)
        }
        cli::Command::Init(cmd) => init_project(cmd),
        cli::Command::Cache(cmd) => manage_cache(cmd),
        cli::Command::Clean(cmd) => clean_project(cmd),
        cli::Command::Tree(cmd) => show_tree(cmd),
        cli::Command::Test(cmd) => run_tests(cmd),
        cli::Command::Doctor => doctor(),
        cli::Command::Completions(cmd) => {
            use clap::CommandFactory;
            let mut command = cli::Cli::command();
            clap_complete::generate(cmd.shell, &mut command, "pyrs", &mut std::io::stdout());
            Ok(0)
        }
    }
}

fn run_program(mut cmd: cli::RunCommand) -> Result<i32, String> {
    let mut input = cmd.input.take().or_else(|| {
        if cmd.code.is_none() && cmd.module.is_none() && !cmd.args.is_empty() {
            Some(PathBuf::from(cmd.args.remove(0)))
        } else {
            None
        }
    });

    // An explicit source bypasses discovery entirely; the manifest only fills
    // in what was not asked for on the command line.
    // An explicit source, `-c` or `-m` bypasses discovery entirely.
    let project = if input.is_none() && cmd.code.is_none() && cmd.module.is_none() {
        match std::env::current_dir() {
            Ok(dir) => manifest::discover(&dir)?,
            Err(_) => None,
        }
    } else {
        None
    };
    let mut compat = cmd.compat;
    let mut import_root: Option<PathBuf> = None;
    let mut manifest_python: Option<PathBuf> = None;
    if let Some(path) = &project {
        let m = manifest::load(path)?;
        import_root = Some(m.root_path());
        if let Some(entry) = m.entry_path() {
            input = Some(entry);
        }
        if cmd.opt_level.is_none() {
            cmd.opt_level = m.opt_level;
        }
        // Declared, never inferred -- and `--no-compat` exists so a project
        // can test whether its program has become natively compilable
        // without editing the file.
        if m.execution == manifest::Execution::Compat && !cmd.no_compat {
            compat = true;
        }
        if cmd.python.is_none() {
            manifest_python = m.python.map(|p| m.dir.join(p));
        }
    }

    // `--python` selects the interpreter for compatibility mode, which the
    // manifest may enable -- so it no longer requires `--compat` on the
    // command line. It is still worth saying when it cannot matter, rather
    // than swallowing a flag the user expected to have an effect.
    if !compat && cmd.python.is_some() {
        eprintln!(
            "warning: --python has no effect without compatibility mode; \
             pass --compat or set execution = \"compat\" in [tool.pyrs]"
        );
    }

    if compat {
        // Explicit whole-program execution. Never execute part of a program
        // natively and then retry its side effects in another engine.
        let (python, source) = interpreter::resolve(cmd.python, manifest_python);
        if std::env::var_os("PYRS_QUIET").is_none() {
            interpreter::warn_on_version_mismatch(&python);
        }
        let _ = source;
        let mut process = process::Command::new(&python);
        if let Some(code) = cmd.code {
            process.args(["-c", &code]);
        } else if let Some(module) = cmd.module {
            process.args(["-m", &module]);
        } else if let Some(path) = input {
            process.arg("--").arg(path);
        } else {
            process.arg("-");
        }
        process.args(cmd.args);
        return execute(&mut process).map_err(|e| {
            format!(
                "failed to run CPython compatibility mode using {}: {e}; select an installed interpreter with --python or PYRS_PYTHON",
                python.display()
            )
        });
    }

    let format = cmd.message_format;
    let (loaded, argv0) = if let Some(code) = cmd.code {
        (
            modules::load_inline(code, "<string>").map_err(|e| fail(e, format))?,
            OsString::from("-c"),
        )
    } else if let Some(path) = input.filter(|p| p != Path::new("-")) {
        let loaded = match &import_root {
            Some(root) => modules::load_program_in_project(&path, root),
            None => modules::load_program(&path),
        };
        (loaded.map_err(|e| fail(e, format))?, path.into_os_string())
    } else {
        let mut source = String::new();
        io::stdin()
            .read_to_string(&mut source)
            .map_err(|e| format!("failed to read stdin: {e}"))?;
        (
            modules::load_inline(source, "<stdin>").map_err(|e| fail(e, format))?,
            OsString::from("-"),
        )
    };
    let opt_level = cmd.opt_level.unwrap_or(2);
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let key =
        (!cmd.no_cache).then(|| cache::program_key(&program_sources(&loaded), opt_level, &cc));

    // A hit skips analysis and code generation as well as the C compile:
    // the program is unchanged, so there is nothing left to decide about it.
    if let Some(key) = &key
        && let Some(cached) = cache::program_lookup(key)
    {
        let mut process = process::Command::new(&cached);
        process.args(&cmd.args);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            process.arg0(&argv0);
        }
        return process
            .status()
            .map(exit_code)
            .map_err(|e| format!("failed to run compiled program: {e}"));
    }

    let module = analyze(loaded).map_err(|f| f.render(format))?;
    let workdir = temp_workdir()?;
    let exe = workdir.join("program");
    let result = compile_module(&module, &exe, opt_level, false, !cmd.no_cache).and_then(|()| {
        if let Some(key) = &key {
            cache::program_store(key, &exe);
            cache::maintain();
        }
        let mut process = process::Command::new(&exe);
        process.args(&cmd.args);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            process.arg0(&argv0);
        }
        #[cfg(not(unix))]
        let _ = argv0;
        // Keep the parent alive to clean up the native executable afterwards.
        process
            .status()
            .map(exit_code)
            .map_err(|e| format!("failed to run compiled program: {e}"))
    });
    drop(workdir);
    result
}

fn exit_code(status: process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
    }
    #[cfg(not(unix))]
    status.code().unwrap_or(1)
}

fn execute(command: &mut process::Command) -> Result<i32, io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // No temporary native artifact in compatibility mode: replacing the
        // process preserves signal, stdin and exit behavior exactly.
        Err(command.exec())
    }
    #[cfg(not(unix))]
    command.status().map(exit_code)
}

/// `pyrs compile` (and its `build` alias).
///
/// Project-aware in the same way `run` is: with no `-i` it builds the
/// manifest's entry through the declared import root, and with no `-o` it
/// writes `target/NAME` rather than `./a.out` in whatever directory the
/// command happened to run from. `pyrs build` in a project should mean the
/// same kind of thing `cargo build` does.
fn compile_command(cmd: cli::CompileCommand) -> Result<i32, String> {
    let project = if cmd.input.is_none() {
        match std::env::current_dir() {
            Ok(dir) => manifest::discover(&dir)?,
            Err(_) => None,
        }
    } else {
        None
    };
    let m = match &project {
        Some(path) => Some(manifest::load(path)?),
        None => None,
    };

    let input = match (&cmd.input, m.as_ref().and_then(|m| m.entry_path())) {
        (Some(i), _) => i.clone(),
        (None, Some(entry)) => entry,
        (None, None) => {
            return Err(
                "no input: pass -i, or run inside a project with [tool.pyrs] entry".to_string(),
            );
        }
    };
    let output = match (&cmd.output, m.as_ref().and_then(|m| m.default_output())) {
        (Some(o), _) => o.clone(),
        (None, Some(default)) => default,
        (None, None) => PathBuf::from("a.out"),
    };
    let opt_level = cmd
        .opt_level
        .or_else(|| m.as_ref().and_then(|m| m.opt_level))
        .unwrap_or(2);

    compile(
        &input,
        &output,
        m.as_ref().map(|m| m.root_path()),
        opt_level,
        cmd.emit_llvm,
        !cmd.no_cache,
        cmd.message_format,
    )?;
    Ok(0)
}

/// The full pipeline: source file(s) in, linked native executable out.
#[allow(clippy::too_many_arguments)]
fn compile(
    input: &Path,
    output: &Path,
    import_root: Option<PathBuf>,
    opt_level: u8,
    emit_llvm: bool,
    use_cache: bool,
    format: Format,
) -> Result<(), String> {
    let loaded = match &import_root {
        Some(root) => modules::load_program_in_project(input, root),
        None => modules::load_program(input),
    }
    .map_err(|e| fail(e, format))?;
    // A default output under `target/` may name a directory nothing created.
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    // `--emit-llvm` asks for a side artifact the cache does not hold, so it
    // always rebuilds rather than silently not producing the .ll file.
    let key = (use_cache && !emit_llvm)
        .then(|| cache::program_key(&program_sources(&loaded), opt_level, &cc));
    if let Some(key) = &key
        && let Some(cached) = cache::program_lookup(key)
    {
        fs::copy(&cached, output)
            .map_err(|e| format!("failed to write {}: {e}", output.display()))?;
        return Ok(());
    }
    let module = analyze(loaded).map_err(|f| f.render(format))?;
    compile_module(&module, output, opt_level, emit_llvm, use_cache)?;
    if let Some(key) = &key {
        cache::program_store(key, output);
        cache::maintain();
    }
    Ok(())
}

fn analyze(loaded: Vec<modules::Loaded>) -> Result<ir::Module, Box<Failure>> {
    let inputs: Vec<semantic::ModuleInput> = loaded
        .iter()
        .map(|m| semantic::ModuleInput {
            name: m.name.clone(),
            ast: &m.ast,
        })
        .collect();

    semantic::analyze_program(&inputs).map_err(|d| {
        let m = &loaded[d.file.min(loaded.len() - 1)];
        Box::new(Failure::located(&d, &m.display, &m.source))
    })
}

/// Render a failure in the requested format.
///
/// Everything the driver reports goes through here, so a consumer that asked
/// for JSON never receives a line of prose it cannot parse.
fn fail(failure: impl Into<Failure>, format: Format) -> String {
    failure.into().render(format)
}

fn compile_module(
    module: &ir::Module,
    output: &Path,
    opt_level: u8,
    emit_llvm: bool,
    use_cache: bool,
) -> Result<(), String> {
    let llvm_ir = codegen::emit_llvm_ir(module);

    if emit_llvm {
        let ll_path = output.with_extension("ll");
        fs::write(&ll_path, &llvm_ir)
            .map_err(|e| format!("failed to write {}: {e}", ll_path.display()))?;
    }

    let workdir = temp_workdir()?;
    let result = (|| {
        // LLVM: optimize + emit the object file
        let object = workdir.join("program.o");
        codegen::compile_ir_to_object(&llvm_ir, &object, opt_level)
            .map_err(|e| format!("error[codegen]: {e}"))?;

        // The C runtime and collector are embedded in the compiler binary so
        // produced executables do not depend on a PyRs installation at run
        // time. They are written out here both to compile and to key the
        // cache on their preprocessed content.
        let runtime = workdir.join("runtime.c");
        fs::write(&runtime, codegen::RUNTIME_C)
            .map_err(|e| format!("failed to write runtime: {e}"))?;
        let gc = workdir.join("gc.c");
        fs::write(&gc, codegen::GC_C).map_err(|e| format!("failed to write collector: {e}"))?;
        let gc_header = workdir.join("gc.h");
        fs::write(&gc_header, codegen::GC_H)
            .map_err(|e| format!("failed to write collector header: {e}"))?;
        let unicode = workdir.join("unicode_data.c");
        fs::write(&unicode, codegen::UNICODE_DATA_C)
            .map_err(|e| format!("failed to write Unicode tables: {e}"))?;
        fs::write(workdir.join("unicode_data.h"), codegen::UNICODE_DATA_H)
            .map_err(|e| format!("failed to write Unicode table header: {e}"))?;

        // Honor `CC` so CI clang is used instead of Ubuntu's gcc. gcc's
        // `-Wformat-truncation` on `runtime.c` otherwise leaks onto stderr
        // and breaks GC-stat parsing plus `make examples` (which diffs 2>&1).
        let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());

        let runtime_build = cache::runtime_objects(&cc, &workdir, &workdir, use_cache)?;
        let mut link = process::Command::new(&cc);
        link.arg(&object);
        match &runtime_build {
            cache::Runtime::Cached(o) | cache::Runtime::Separate(o) => {
                link.args(&o.objects);
            }
            cache::Runtime::Inline => {
                // One invocation, not three: these objects are discarded, so
                // splitting the compile buys nothing and measurably costs.
                link.arg(&runtime).arg(&gc).arg(&unicode);
            }
        }
        if matches!(runtime_build, cache::Runtime::Inline) {
            link.args(cache::extra_flags("PYRS_CFLAGS"));
        }
        let status = link
            .arg("-O2")
            .arg("-Wno-format-truncation")
            .args(cache::extra_flags("PYRS_LDFLAGS"))
            .arg("-lm")
            .arg("-o")
            .arg(output)
            .status()
            .map_err(|e| format!("failed to invoke the C compiler '{cc}': {e}"))?;
        if !status.success() {
            return Err("linking failed (see 'cc' output above)".to_string());
        }
        Ok(())
    })();
    drop(workdir);
    result
}

/// The inputs a program's identity is built from: every module in the
/// resolved import graph, which the resolver has already determined exactly.
fn program_sources(loaded: &[modules::Loaded]) -> Vec<(String, String)> {
    loaded
        .iter()
        .map(|m| (m.name.clone(), m.source.clone()))
        .collect()
}

/// `pyrs check`, and the report of what the project resolved to.
///
/// Which entry point, import root, mode and interpreter are in play should
/// never be a guess -- especially the interpreter, whose default now depends
/// on whether uv is installed and whether this is a uv project.
fn check_program(cmd: cli::CheckCommand) -> Result<i32, String> {
    let project = match std::env::current_dir().ok().map(|d| manifest::discover(&d)) {
        Some(found) => found?,
        None => None,
    };
    let m = match &project {
        Some(path) => Some(manifest::load(path)?),
        None => None,
    };

    let input = match (cmd.input, m.as_ref().and_then(|m| m.entry_path())) {
        (Some(i), _) => i,
        (None, Some(e)) => e,
        (None, None) => {
            return Err(
                "no input: pass -i, or run inside a project with [tool.pyrs] entry".to_string(),
            );
        }
    };

    if let Some(m) = &m {
        let path = project.as_ref().expect("a manifest implies its path");
        println!("project:     {}", path.display());
        println!("entry:       {}", input.display());
        println!("import root: {}", m.root_path().display());
        println!(
            "execution:   {}",
            match m.execution {
                manifest::Execution::Native => "native",
                manifest::Execution::Compat => "compat",
            }
        );
        let (python, source) = interpreter::resolve(None, m.python.as_ref().map(|p| m.dir.join(p)));
        let version = interpreter::version_of(&python).unwrap_or_else(|| "unavailable".into());
        println!(
            "interpreter: {} ({}, Python {version}; PyRs targets {})",
            python.display(),
            source.describe(),
            codegen::oracle_python_minor()
        );
    }

    let loaded = match m.as_ref().map(|m| m.root_path()) {
        Some(root) => modules::load_program_in_project(&input, &root),
        None => modules::load_program(&input),
    };
    let format = cmd.message_format;
    analyze(loaded.map_err(|e| fail(e, format))?).map_err(|f| f.render(format))?;
    Ok(0)
}

/// `pyrs cache`: inspect, clean and prune the build cache.
///
/// The cache is otherwise a directory that only grows, with no way to see
/// what is in it and no remedy short of deleting the whole thing — which
/// also throws away the runtime objects that make every build on the machine
/// fast, to reclaim space held by programs.
fn manage_cache(cmd: cli::CacheCommand) -> Result<i32, String> {
    let root = cache::root().ok_or_else(|| {
        "no cache directory: set PYRS_CACHE_DIR, XDG_CACHE_HOME or HOME".to_string()
    })?;

    match cmd.action {
        cli::CacheAction::Dir => println!("{}", root.display()),

        cli::CacheAction::Info => {
            println!("cache: {}", root.display());
            let stats = cache::stats(&root);
            let total: u64 = stats.iter().map(|s| s.bytes).sum();
            for layer in &stats {
                println!(
                    "  {:<10} {:>6} entries  {:>10}",
                    layer.name,
                    layer.entries,
                    cache::human_size(layer.bytes)
                );
            }
            println!(
                "  {:<10} {:>6}           {:>10}",
                "total",
                "",
                cache::human_size(total)
            );
        }

        cli::CacheAction::Clean(opts) => {
            let layers = selected_layers(opts.programs, opts.runtime);
            let removed = cache::clean(&root, &layers, opts.dry_run);
            report_removed("removed", &removed, opts.dry_run);
        }

        cli::CacheAction::Prune(opts) => {
            if opts.older_than.is_none() && opts.max_size.is_none() {
                return Err(
                    "nothing to prune by: pass --older-than, --max-size, or both \
                     (use 'pyrs cache clean' to remove everything)"
                        .to_string(),
                );
            }
            let older_than = match &opts.older_than {
                Some(text) => Some(cache::parse_duration(text).ok_or_else(|| {
                    format!("invalid --older-than '{text}': expected a form like 7d, 24h or 30m")
                })?),
                None => None,
            };
            let max_size = match &opts.max_size {
                Some(text) => Some(cache::parse_size(text).ok_or_else(|| {
                    format!("invalid --max-size '{text}': expected a form like 500MB or 2GiB")
                })?),
                None => None,
            };
            let layers = selected_layers(opts.programs, opts.runtime);
            let removed = cache::prune(
                &root,
                &layers,
                &cache::PruneOptions {
                    older_than,
                    max_size,
                    dry_run: opts.dry_run,
                },
            );
            report_removed("pruned", &removed, opts.dry_run);
        }
    }
    Ok(0)
}

/// Which layers a `--programs`/`--runtime` pair selects. Neither flag means
/// both, which is what someone typing the bare command wants.
///
/// `toolchain` is never included: its entries are 64 bytes each and losing
/// one costs two subprocesses on the next build for no space worth having.
fn selected_layers(programs: bool, runtime: bool) -> Vec<&'static str> {
    match (programs, runtime) {
        (false, false) => vec!["programs", "runtime"],
        _ => {
            let mut layers = Vec::new();
            if programs {
                layers.push("programs");
            }
            if runtime {
                layers.push("runtime");
            }
            layers
        }
    }
}

fn report_removed(verb: &str, removed: &cache::Removed, dry_run: bool) {
    let what = format!(
        "{} {} ({})",
        removed.entries,
        if removed.entries == 1 {
            "entry"
        } else {
            "entries"
        },
        cache::human_size(removed.bytes)
    );
    if dry_run {
        println!("would have {verb} {what}");
    } else {
        println!("{verb} {what}");
    }
}

/// `pyrs test`: compile the project's tests and run them natively.
///
/// pytest under CPython already tests whether your logic is right. What it
/// cannot do is tell you whether the *compiled* program agrees with it,
/// which is precisely the failure mode of a compiler for a Python subset.
fn run_tests(cmd: cli::TestCommand) -> Result<i32, String> {
    let project = match std::env::current_dir() {
        Ok(dir) => manifest::discover(&dir)?,
        Err(_) => None,
    };
    let m = match &project {
        Some(path) => Some(manifest::load(path)?),
        None => None,
    };

    // Where to look, and what those directories are roots *for*. A test
    // module has to be importable to be runnable, so the search roots and
    // the import roots are the same set by construction.
    let mut roots: Vec<PathBuf> = Vec::new();
    match (&cmd.input, &m) {
        (Some(input), _) if input.is_file() => {
            roots.push(input.parent().unwrap_or(Path::new(".")).to_path_buf());
        }
        (Some(input), _) => roots.push(input.clone()),
        (None, Some(m)) => {
            roots.push(m.root_path());
            // pytest's `tests/` sits outside a `src/` import root, so it is
            // only importable if it is a root of its own.
            let tests = m.dir.join("tests");
            if tests.is_dir() {
                roots.push(tests);
            }
        }
        (None, None) => roots.push(PathBuf::from(".")),
    }

    let single_file = cmd.input.as_ref().filter(|i| i.is_file());
    let mut discovered: Vec<(String, PathBuf)> = Vec::new();
    match single_file {
        Some(file) => {
            let stem = file
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .ok_or_else(|| format!("{} has no module name", file.display()))?;
            discovered.push((stem, file.clone()));
        }
        None => {
            for root in &roots {
                for found in testing::discover(root) {
                    if !discovered.iter().any(|(name, _)| *name == found.0) {
                        discovered.push(found);
                    }
                }
            }
        }
    }

    let mut modules = Vec::new();
    let mut total = 0usize;
    for (name, path) in discovered {
        let source = read_source(&path)?;
        let ast = parser::parse(&source).map_err(|d| render_diag(&d, &path, &source))?;
        let tests: Vec<String> = testing::collect_tests(&ast)
            .into_iter()
            .filter(|test| match &cmd.filter {
                Some(needle) => test.contains(needle.as_str()) || name.contains(needle.as_str()),
                None => true,
            })
            .collect();
        if tests.is_empty() {
            continue;
        }
        total += tests.len();
        modules.push(testing::TestModule { name, tests });
    }

    if cmd.list {
        for module in &modules {
            for test in &module.tests {
                println!("{}::{test}", module.name);
            }
        }
        return Ok(0);
    }

    if total == 0 {
        println!("no tests found");
        // Not a failure: a project with no tests yet is a normal project,
        // and failing here would make `pyrs test` unusable in CI from day
        // one. `--filter` matching nothing is reported as such below.
        if cmd.filter.is_some() {
            println!("(no test matched the filter)");
        }
        return Ok(0);
    }

    let workdir = temp_workdir()?;
    let results = workdir.join("results.tsv");
    let source = testing::generate(&modules, &results);
    let loaded =
        modules::load_inline_with_roots(source, "<pyrs test>", &roots).map_err(|e| e.message)?;
    let ir = analyze(loaded).map_err(|f| f.message)?;

    let exe = workdir.join("harness");
    let opt_level = cmd
        .opt_level
        .or_else(|| m.as_ref().and_then(|m| m.opt_level))
        .unwrap_or(2);
    compile_module(&ir, &exe, opt_level, false, !cmd.no_cache)?;

    println!("running {total} test{}", if total == 1 { "" } else { "s" });
    let status = process::Command::new(&exe)
        .status()
        .map_err(|e| format!("failed to run the test harness: {e}"))?;

    let outcomes = testing::read_results(&results);
    for outcome in &outcomes {
        println!(
            "test {} ... {}",
            outcome.name,
            if outcome.passed { "ok" } else { "FAILED" }
        );
    }

    let failed: Vec<&testing::Outcome> = outcomes.iter().filter(|o| !o.passed).collect();
    if !failed.is_empty() {
        println!("\nfailures:");
        for outcome in &failed {
            println!("    {}", outcome.name);
            if !outcome.message.is_empty() {
                println!("        {}", outcome.message);
            }
        }
    }

    // A harness that died mid-suite recorded fewer results than there are
    // tests. Reporting the survivors as the whole run would turn a crash
    // into a pass.
    let missing = total.saturating_sub(outcomes.len());
    if missing > 0 {
        println!(
            "\nthe harness stopped after {} of {total} tests{}",
            outcomes.len(),
            match status.code() {
                Some(code) => format!(" (exit status {code})"),
                None => " (killed by a signal)".to_string(),
            }
        );
    }

    let passed = outcomes.len() - failed.len();
    let ok = failed.is_empty() && missing == 0;
    println!(
        "\ntest result: {}. {passed} passed; {} failed{}",
        if ok { "ok" } else { "FAILED" },
        failed.len(),
        if missing > 0 {
            format!("; {missing} not run")
        } else {
            String::new()
        }
    );
    Ok(if ok { 0 } else { 1 })
}

/// `pyrs tree`: the import graph, as the resolver actually resolved it.
///
/// PyRs is closed-world, so this graph is a *fact* rather than an estimate:
/// it is exactly the set of modules that will be compiled into the program,
/// which is what makes it worth printing. `cargo tree` is the shape.
fn show_tree(cmd: cli::TreeCommand) -> Result<i32, String> {
    let project = if cmd.input.is_none() {
        match std::env::current_dir() {
            Ok(dir) => manifest::discover(&dir)?,
            Err(_) => None,
        }
    } else {
        None
    };
    let m = match &project {
        Some(path) => Some(manifest::load(path)?),
        None => None,
    };
    let input = match (&cmd.input, m.as_ref().and_then(|m| m.entry_path())) {
        (Some(i), _) => i.clone(),
        (None, Some(entry)) => entry,
        (None, None) => {
            return Err(
                "no input: pass -i, or run inside a project with [tool.pyrs] entry".to_string(),
            );
        }
    };

    let loaded = match m.as_ref().map(|m| m.root_path()) {
        Some(root) => modules::load_program_in_project(&input, &root),
        None => modules::load_program(&input),
    }
    .map_err(|e| e.message)?;

    let mut printer = TreePrinter {
        by_name: loaded.iter().map(|m| (m.name.as_str(), m)).collect(),
        seen: std::collections::HashSet::new(),
        paths: cmd.paths,
        max_depth: cmd.depth,
    };
    printer.walk(modules::ROOT_NAME, "", true, true, 0);

    // The count is the point of the exercise as often as the shape is.
    println!();
    println!(
        "{} module{}",
        loaded.len(),
        if loaded.len() == 1 { "" } else { "s" }
    );
    Ok(0)
}

/// State the tree walk carries, so the recursion passes a position rather
/// than re-threading the whole graph at every level.
struct TreePrinter<'a> {
    by_name: std::collections::HashMap<&'a str, &'a modules::Loaded>,
    seen: std::collections::HashSet<String>,
    paths: bool,
    max_depth: Option<usize>,
}

impl TreePrinter<'_> {
    fn walk(&mut self, name: &str, prefix: &str, is_last: bool, is_root: bool, depth: usize) {
        let module = self.by_name.get(name).copied();
        // A module reached twice is printed once and marked, the way cargo
        // does: repeating a shared subtree turns a graph into an unreadable
        // expansion.
        let repeat = !self.seen.insert(name.to_string());
        let label = match (module, self.paths) {
            (Some(m), true) => format!("{name} ({})", m.display),
            _ => name.to_string(),
        };
        let connector = match (is_root, is_last) {
            (true, _) => "",
            (false, true) => "\u{2514}\u{2500}\u{2500} ",
            (false, false) => "\u{251c}\u{2500}\u{2500} ",
        };
        println!(
            "{prefix}{connector}{label}{}",
            if repeat { " (*)" } else { "" }
        );

        if repeat || self.max_depth.is_some_and(|max| depth >= max) {
            return;
        }
        let Some(module) = module else { return };
        // Only edges to modules actually in the graph.
        let children: Vec<String> = module
            .deps
            .iter()
            .filter(|d| self.by_name.contains_key(d.as_str()))
            .cloned()
            .collect();

        // A child's guide line continues its parent's unless the parent was
        // the last of its siblings, in which case the column is closed off.
        let child_prefix = match (is_root, is_last) {
            (true, _) => String::new(),
            (false, true) => format!("{prefix}    "),
            (false, false) => format!("{prefix}\u{2502}   "),
        };
        for (i, child) in children.iter().enumerate() {
            self.walk(
                child,
                &child_prefix,
                i + 1 == children.len(),
                false,
                depth + 1,
            );
        }
    }
}

/// `pyrs clean`: remove this project's build output directory.
///
/// Scoped to the project, not the machine. The global build cache is
/// `pyrs cache clean` — conflating the two would mean that clearing one
/// project's outputs slowed down every build on the system, which is
/// exactly the confusion `cargo clean` avoids by owning only `target/`.
fn clean_project(cmd: cli::CleanCommand) -> Result<i32, String> {
    let project = match std::env::current_dir() {
        Ok(dir) => manifest::discover(&dir)?,
        Err(_) => None,
    };
    let Some(path) = project else {
        return Err(
            "not in a PyRs project: 'clean' removes the [tool.pyrs] target directory \
             (for the shared build cache, use 'pyrs cache clean')"
                .to_string(),
        );
    };
    let target = manifest::load(&path)?.target_path();
    if !target.exists() {
        println!("nothing to clean: {} does not exist", target.display());
        return Ok(0);
    }
    if cmd.dry_run {
        println!("would remove {}", target.display());
        return Ok(0);
    }
    fs::remove_dir_all(&target)
        .map_err(|e| format!("failed to remove {}: {e}", target.display()))?;
    println!("removed {}", target.display());
    Ok(0)
}

/// `pyrs doctor`: what PyRs found, and whether it is enough to build.
///
/// The contributor-facing `make doctor` checks the machine can build the
/// *compiler*. This answers the different question a user has — can this
/// binary compile my program, and which interpreter will `--compat` use —
/// and it answers it from the same resolution code the build actually runs,
/// so the report cannot describe a different toolchain than the one used.
fn doctor() -> Result<i32, String> {
    let mut problems = 0;

    println!("pyrs {}", env!("CARGO_PKG_VERSION"));
    println!(
        "  target       {}-{}",
        std::env::consts::ARCH,
        std::env::consts::OS
    );

    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    match process::Command::new(&cc).arg("--version").output() {
        Ok(out) if out.status.success() => {
            let first = String::from_utf8_lossy(&out.stdout);
            let first = first.lines().next().unwrap_or("").trim();
            println!("  C compiler   {cc} ({first})");
        }
        _ => {
            println!("  C compiler   {cc}: not found");
            println!("               PyRs links every program with a C compiler; install one");
            problems += 1;
        }
    }
    for var in ["PYRS_CFLAGS", "PYRS_LDFLAGS"] {
        let flags = cache::extra_flags(var);
        if !flags.is_empty() {
            println!("  {var:<12} {}", flags.join(" "));
        }
    }

    let (python, source) = interpreter::resolve(None, None);
    match interpreter::version_of(&python) {
        Some(version) => {
            let want = codegen::oracle_python_minor();
            let matches = version.starts_with(want);
            println!(
                "  interpreter  {} ({}, Python {version}){}",
                python.display(),
                source.describe(),
                if matches {
                    String::new()
                } else {
                    format!("  [PyRs targets {want}]")
                }
            );
            if !matches {
                println!(
                    "               --compat and the differential oracle use this \
                     interpreter; a different minor version can disagree with PyRs"
                );
            }
        }
        None => {
            println!("  interpreter  {}: not runnable", python.display());
            println!("               only --compat and build-extension need one");
        }
    }

    match cache::root() {
        Some(root) => {
            let total: u64 = cache::stats(&root).iter().map(|s| s.bytes).sum();
            println!(
                "  cache        {} ({})",
                root.display(),
                cache::human_size(total)
            );
        }
        None => println!("  cache        none (set PYRS_CACHE_DIR, XDG_CACHE_HOME or HOME)"),
    }

    match std::env::current_dir().ok().map(|d| manifest::discover(&d)) {
        Some(Ok(Some(path))) => {
            println!("  project      {}", path.display());
            match manifest::load(&path) {
                Ok(m) => {
                    println!(
                        "               entry {}, root {}, target {}",
                        m.entry
                            .as_ref()
                            .map(|e| e.display().to_string())
                            .unwrap_or_else(|| "<none>".into()),
                        m.root_path().display(),
                        m.target_path().display()
                    );
                }
                Err(e) => {
                    println!("               {e}");
                    problems += 1;
                }
            }
        }
        Some(Err(e)) => {
            println!("  project      {e}");
            problems += 1;
        }
        _ => println!("  project      none in this directory or its parents"),
    }

    if problems == 0 {
        println!("\nno problems found");
        Ok(0)
    } else {
        // A non-zero status so a setup script can act on this.
        println!("\n{problems} problem(s) found");
        Ok(1)
    }
}

/// `pyrs init`: scaffold a PyRs project, or add the one table PyRs needs to
/// a project that already exists.
///
/// There is still no `pyrs new`. The split is by *what is already there*,
/// not by which command was typed: a directory with a `pyproject.toml`
/// belongs to a project someone else created — `uv init`, most likely — and
/// gets exactly one table added and nothing else touched. A directory
/// without one gets the layout cargo and uv both scaffold, because a user
/// starting from nothing should not have to assemble it by hand just because
/// PyRs declined to own project creation.
fn init_project(cmd: cli::InitCommand) -> Result<i32, String> {
    let dir = cmd.path.unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    let path = dir.join("pyproject.toml");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if existing.contains("[tool.pyrs]") {
        return Err(format!(
            "{} already has a [tool.pyrs] table; edit it rather than re-running init",
            path.display()
        ));
    }

    let name = match &cmd.name {
        Some(name) => name.clone(),
        None => project_name(&dir),
    };
    let module = module_name(&name);
    let scaffolding = existing.trim().is_empty();

    // Where the program lives. `src/<module>/main.py` is uv's layout (and
    // cargo's `src/`); `--script` is uv's older flat one, which is still the
    // right shape for a single-file program.
    let entry = match (&cmd.entry, cmd.script, scaffolding) {
        (Some(entry), _, _) => entry.clone(),
        (None, true, _) => PathBuf::from("main.py"),
        (None, false, true) => PathBuf::from("src").join(&module).join("main.py"),
        // An existing project: adopt whatever it already has rather than
        // inventing a second entry point beside it.
        (None, false, false) => discover_entry(&dir, &module),
    };
    let in_src = entry.starts_with("src");

    let mut text = existing.clone();
    if scaffolding {
        text = format!(
            "[project]\nname = \"{name}\"\nversion = \"0.1.0\"\n\
             description = \"Add your description here\"\nreadme = \"README.md\"\n\
             requires-python = \">={}\"\ndependencies = []\n",
            codegen::oracle_python_minor()
        );
    } else if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("\n[tool.pyrs]\n");
    text.push_str(&format!("entry = \"{}\"\n", slashed(&entry)));
    if in_src {
        text.push_str("root = \"src\"\n");
    }
    text.push_str("opt-level = 2\nexecution = \"native\"\n");
    fs::write(&path, &text).map_err(|e| format!("failed to write {}: {e}", path.display()))?;

    let mut wrote = vec![path.clone()];
    if scaffolding {
        // `requires-python` states the floor; `.python-version` is what uv
        // actually reads when it provisions the environment, and PyRs's
        // Unicode tables and differential oracle come from a specific
        // CPython. Writing only the first would leave uv free to pick 3.12.
        wrote.extend(write_new(
            &dir.join(".python-version"),
            &format!("{}\n", codegen::oracle_python_minor()),
        ));
        wrote.extend(write_new(
            &dir.join("README.md"),
            &format!("# {name}\n\nBuilt with [PyRs](https://github.com/sshussh/pyrs).\n\n```console\npyrs run\npyrs build\n```\n"),
        ));
    }
    // Even in an existing project, build output must not be committed.
    wrote.extend(ensure_ignored(&dir)?);

    if in_src {
        wrote.extend(write_new(
            &dir.join("src").join(&module).join("__init__.py"),
            "",
        ));
    }
    let entry_path = dir.join(&entry);
    wrote.extend(write_new(
        &entry_path,
        &format!("def main() -> None:\n    print(\"Hello from {name}!\")\n\n\nmain()\n"),
    ));

    if cmd.vcs == cli::Vcs::Git && scaffolding {
        init_git(&dir);
    }

    println!("initialized project `{name}` at {}", dir.display());
    for file in &wrote {
        println!("  {}", file.display());
    }
    Ok(0)
}

/// Write `text` only when nothing is there. Every file `init` produces is a
/// starting point, so overwriting one would destroy work to supply a
/// template — and `init` on an existing project is a normal thing to do.
fn write_new(path: &Path, text: &str) -> Option<PathBuf> {
    if path.exists() {
        return None;
    }
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(path, text).ok().map(|()| path.to_path_buf())
}

/// The entry an existing project already has, or the conventional one.
fn discover_entry(dir: &Path, module: &str) -> PathBuf {
    for candidate in [
        PathBuf::from("src").join(module).join("main.py"),
        PathBuf::from("src").join(module).join("__main__.py"),
        PathBuf::from("main.py"),
        PathBuf::from("__main__.py"),
    ] {
        if dir.join(&candidate).is_file() {
            return candidate;
        }
    }
    PathBuf::from("main.py")
}

/// Project name from a directory, falling back when the directory has no
/// usable name of its own (`.`, `/`, a name of only punctuation).
fn project_name(dir: &Path) -> String {
    let raw = dir
        .canonicalize()
        .ok()
        .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() {
        "app".to_string()
    } else {
        cleaned
    }
}

/// Import name for a project name. A package directory has to be a Python
/// identifier, so `my-app` becomes `my_app` — the same mapping uv uses.
fn module_name(name: &str) -> String {
    let mut module: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    if module
        .chars()
        .next()
        .is_none_or(|c| !(c.is_alphabetic() || c == '_'))
    {
        module.insert(0, '_');
    }
    module
}

/// Manifest paths are `/`-separated regardless of platform, so a project
/// written on Windows still resolves on Linux.
fn slashed(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Make sure the build output directory is ignored, without clobbering an
/// existing `.gitignore`.
fn ensure_ignored(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let path = dir.join(".gitignore");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|l| {
        let l = l.trim();
        l == "/target" || l == "target" || l == "target/" || l == "/target/"
    }) {
        return Ok(Vec::new());
    }
    let mut text = existing.clone();
    if text.is_empty() {
        text.push_str(
            "# Python-generated files\n__pycache__/\n*.py[oc]\nbuild/\ndist/\nwheels/\n\
             *.egg-info\n\n# Virtual environments\n.venv\n",
        );
    } else if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("\n# PyRs build output\n/target\n");
    fs::write(&path, &text).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(vec![path])
}

/// `git init`, the way cargo and uv both do it.
///
/// Best-effort in every direction: no git on the machine, a directory
/// already inside a repository, or a git that fails for its own reasons must
/// none of them turn a successful scaffold into a failure.
fn init_git(dir: &Path) {
    let inside = process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(dir)
        .output();
    if matches!(&inside, Ok(out) if out.status.success()) {
        return;
    }
    let _ = process::Command::new("git")
        .arg("init")
        .arg("--quiet")
        .current_dir(dir)
        .status();
}

impl From<modules::LoadError> for Failure {
    fn from(e: modules::LoadError) -> Self {
        match e.located {
            Some((phase, message, span, file, source)) => Failure {
                message: e.message,
                located: Some(diagnostics::Located {
                    phase: phase.to_string(),
                    message,
                    span,
                    file,
                    source,
                }),
            },
            None => Failure::plain(e.message),
        }
    }
}

fn read_source(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("failed to read {}: {e}", path.display()))
}

/// Render a diagnostic with a source snippet; synthesized spans (0,0) carry
/// no useful location, so print the message alone.
fn render_diag(diag: &Diagnostic, path: &Path, source: &str) -> String {
    if diag.span == Span::default() {
        format!("{diag}")
    } else {
        diag.render(&path.display().to_string(), source)
    }
}

fn write_output(text: &str, output: Option<&Path>) -> Result<(), String> {
    match output {
        Some(path) => {
            fs::write(path, text).map_err(|e| format!("failed to write {}: {e}", path.display()))
        }
        None => io::stdout()
            .write_all(text.as_bytes())
            .map_err(|e| format!("failed to write to stdout: {e}")),
    }
}

struct TempWorkdir(PathBuf);

impl std::ops::Deref for TempWorkdir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempWorkdir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn temp_workdir() -> Result<TempWorkdir, String> {
    temp_workdir_in(&std::env::temp_dir())
}

fn temp_workdir_in(parent: &Path) -> Result<TempWorkdir, String> {
    // Atomic creation refuses an existing path/symlink; private permissions
    // protect the source, runtime objects and executable while linking.
    for attempt in 0..100 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = parent.join(format!("pyrs-{}-{nanos}-{attempt}", process::id()));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&dir) {
            Ok(()) => return Ok(TempWorkdir(dir)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("failed to create temp dir: {e}")),
        }
    }
    Err("failed to create a unique temporary directory".to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn initialization_lexer_test() {
        assert_eq!(lexer::ping(), String::from("pong"));
    }

    #[test]
    fn initialization_parser_test() {
        assert_eq!(parser::ping(), String::from("pong"));
    }

    #[test]
    fn initialization_semantic_test() {
        assert_eq!(semantic::ping(), String::from("pong"));
    }

    #[test]
    fn initialization_ir_test() {
        assert_eq!(ir::ping(), String::from("pong"));
    }

    #[test]
    fn initialization_codegen_test() {
        assert_eq!(codegen::ping(), String::from("pong"));
    }

    #[test]
    fn initialization_common_test() {
        assert_eq!(common::ping(), String::from("pong"));
    }
}
