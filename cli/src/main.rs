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
mod extension;
mod hash;
mod interpreter;
mod manifest;
mod modules;

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
        cli::Command::Compile(cmd) => {
            compile(
                &cmd.input,
                &cmd.output,
                cmd.opt_level,
                cmd.emit_llvm,
                !cmd.no_cache,
            )?;
            Ok(0)
        }
        cli::Command::Run(cmd) => run_program(cmd),
        cli::Command::Check(cmd) => check_program(cmd),
        cli::Command::BuildExtension(cmd) => {
            extension::build(cmd)?;
            Ok(0)
        }
        cli::Command::Init(cmd) => init_project(cmd),
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

    let (loaded, argv0) = if let Some(code) = cmd.code {
        (
            modules::load_inline(code, "<string>").map_err(|e| e.0)?,
            OsString::from("-c"),
        )
    } else if let Some(path) = input.filter(|p| p != Path::new("-")) {
        let loaded = match &import_root {
            Some(root) => modules::load_program_in_project(&path, root),
            None => modules::load_program(&path),
        };
        (loaded.map_err(|e| e.0)?, path.into_os_string())
    } else {
        let mut source = String::new();
        io::stdin()
            .read_to_string(&mut source)
            .map_err(|e| format!("failed to read stdin: {e}"))?;
        (
            modules::load_inline(source, "<stdin>").map_err(|e| e.0)?,
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

    let module = analyze(loaded)?;
    let workdir = temp_workdir()?;
    let exe = workdir.join("program");
    let result = compile_module(&module, &exe, opt_level, false, !cmd.no_cache).and_then(|()| {
        if let Some(key) = &key {
            cache::program_store(key, &exe);
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

/// The full pipeline: source file(s) in, linked native executable out.
fn compile(
    input: &Path,
    output: &Path,
    opt_level: u8,
    emit_llvm: bool,
    use_cache: bool,
) -> Result<(), String> {
    let loaded = modules::load_program(input).map_err(|e| e.0)?;
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
    let module = analyze(loaded)?;
    compile_module(&module, output, opt_level, emit_llvm, use_cache)?;
    if let Some(key) = &key {
        cache::program_store(key, output);
    }
    Ok(())
}

fn analyze(loaded: Vec<modules::Loaded>) -> Result<ir::Module, String> {
    let inputs: Vec<semantic::ModuleInput> = loaded
        .iter()
        .map(|m| semantic::ModuleInput {
            name: m.name.clone(),
            ast: &m.ast,
        })
        .collect();

    semantic::analyze_program(&inputs).map_err(|d| {
        let m = &loaded[d.file.min(loaded.len() - 1)];
        if d.span == Span::default() {
            format!("{d}")
        } else {
            d.render(&m.display, &m.source)
        }
    })
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
        let status = link
            .arg("-O2")
            .arg("-Wno-format-truncation")
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
    analyze(loaded.map_err(|e| e.0)?)?;
    Ok(0)
}

/// `pyrs init`: record a `[tool.pyrs]` table for this project.
///
/// Project *creation* is `uv init`'s job — this only adds the one table PyRs
/// needs, and writes a minimal `pyproject.toml` when there is not one yet so
/// the command works without uv installed.
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

    let entry = cmd.entry.unwrap_or_else(|| PathBuf::from("main.py"));
    let mut text = existing.clone();
    if text.trim().is_empty() {
        let name = dir
            .canonicalize()
            .ok()
            .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "app".to_string());
        let name: String = name
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect();
        // `requires-python` is pinned to the interpreter PyRs was built
        // against: uv otherwise picks its own default, and a mismatch shows up
        // only when something Unicode- or compat-shaped disagrees.
        text = format!(
            "[project]\nname = \"{name}\"\nversion = \"0.1.0\"\nrequires-python = \">={}\"\n",
            codegen::oracle_python_minor()
        );
    } else if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!(
        "\n[tool.pyrs]\nentry = \"{}\"\nopt-level = 2\nexecution = \"native\"\n",
        entry.display()
    ));
    fs::write(&path, &text).map_err(|e| format!("failed to write {}: {e}", path.display()))?;

    let entry_path = dir.join(&entry);
    if !entry_path.exists() {
        if let Some(parent) = entry_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(
            &entry_path,
            "def main() -> None:\n    print(\"Hello from PyRs!\")\n\n\nmain()\n",
        );
    }
    println!("wrote [tool.pyrs] to {}", path.display());
    Ok(0)
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
