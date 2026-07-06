//! Driver: orchestrates the pipeline (lex → parse → semantic → codegen →
//! link) and renders diagnostics against the original source.

use clap::Parser;
use common::{Diagnostic, Span};
use std::{
    fs, io,
    io::Write,
    path::{Path, PathBuf},
    process,
};

mod cli;

fn main() {
    let args = cli::Cli::parse();
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
            let tokens = lexer::lex(&source)
                .map_err(|d| render_diag(&d, &cmd.input, &source))?;
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
            let module = parser::parse(&source)
                .map_err(|d| render_diag(&d, &cmd.input, &source))?;
            write_output(&format!("{module:#?}\n"), cmd.output.as_deref())?;
            Ok(0)
        }
        cli::Command::Compile(cmd) => {
            compile(&cmd.input, &cmd.output, cmd.opt_level, cmd.emit_llvm)?;
            Ok(0)
        }
        cli::Command::Run(cmd) => {
            let workdir = temp_workdir()?;
            let exe = workdir.join("program");
            let result = compile(&cmd.input, &exe, cmd.opt_level, false)
                .and_then(|()| {
                    process::Command::new(&exe)
                        .status()
                        .map_err(|e| format!("failed to run compiled program: {e}"))
                });
            let _ = fs::remove_dir_all(&workdir);
            let status = result?;
            Ok(status.code().unwrap_or(1))
        }
    }
}

/// The full pipeline: source file in, linked native executable out.
fn compile(input: &Path, output: &Path, opt_level: u8, emit_llvm: bool) -> Result<(), String> {
    let source = read_source(input)?;

    let module = parser::parse(&source).map_err(|d| render_diag(&d, input, &source))?;
    let ir_module = semantic::analyze(&module).map_err(|d| render_diag(&d, input, &source))?;
    let llvm_ir = codegen::emit_llvm_ir(&ir_module);

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

        // the C runtime is compiled and linked in the same cc invocation
        let runtime = workdir.join("runtime.c");
        fs::write(&runtime, codegen::RUNTIME_C)
            .map_err(|e| format!("failed to write runtime: {e}"))?;

        let status = process::Command::new("cc")
            .arg(&object)
            .arg(&runtime)
            .arg("-O2")
            .arg("-lm")
            .arg("-o")
            .arg(output)
            .status()
            .map_err(|e| format!("failed to invoke the system linker 'cc': {e}"))?;
        if !status.success() {
            return Err("linking failed (see 'cc' output above)".to_string());
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&workdir);
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
        Some(path) => fs::write(path, text)
            .map_err(|e| format!("failed to write {}: {e}", path.display())),
        None => io::stdout()
            .write_all(text.as_bytes())
            .map_err(|e| format!("failed to write to stdout: {e}")),
    }
}

fn temp_workdir() -> Result<PathBuf, String> {
    // nanos make concurrent invocations (e.g. parallel tests) collision-free
    let unique = format!(
        "pyrs-{}-{}",
        process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    );
    let dir = std::env::temp_dir().join(unique);
    fs::create_dir_all(&dir).map_err(|e| format!("failed to create temp dir: {e}"))?;
    Ok(dir)
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
