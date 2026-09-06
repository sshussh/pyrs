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

mod cli;
mod extension;
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
            compile(&cmd.input, &cmd.output, cmd.opt_level, cmd.emit_llvm)?;
            Ok(0)
        }
        cli::Command::Run(cmd) => run_program(cmd),
        cli::Command::Check(cmd) => {
            analyze(modules::load_program(&cmd.input).map_err(|e| e.0)?)?;
            Ok(0)
        }
        cli::Command::BuildExtension(cmd) => {
            extension::build(cmd)?;
            Ok(0)
        }
    }
}

fn run_program(mut cmd: cli::RunCommand) -> Result<i32, String> {
    let input = cmd.input.take().or_else(|| {
        if cmd.code.is_none() && cmd.module.is_none() && !cmd.args.is_empty() {
            Some(PathBuf::from(cmd.args.remove(0)))
        } else {
            None
        }
    });
    if cmd.compat {
        // Explicit whole-program execution. Never execute part of a program
        // natively and then retry its side effects in another engine.
        let python = cmd.python.unwrap_or_else(|| {
            std::env::var_os("PYRS_PYTHON")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("python3"))
        });
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
        (
            modules::load_program(&path).map_err(|e| e.0)?,
            path.into_os_string(),
        )
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
    let module = analyze(loaded)?;
    let workdir = temp_workdir()?;
    let exe = workdir.join("program");
    let result = compile_module(&module, &exe, cmd.opt_level, false).and_then(|()| {
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
fn compile(input: &Path, output: &Path, opt_level: u8, emit_llvm: bool) -> Result<(), String> {
    let module = analyze(modules::load_program(input).map_err(|e| e.0)?)?;
    compile_module(&module, output, opt_level, emit_llvm)
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

        // The C runtime and collector are compiled and linked in the same cc
        // invocation.  They are embedded in the compiler binary so produced
        // executables do not depend on a PyRs installation at run time.
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
        let status = process::Command::new(&cc)
            .arg(&object)
            .arg(&runtime)
            .arg(&gc)
            .arg(&unicode)
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
