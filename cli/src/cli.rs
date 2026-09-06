use clap::{Args, Parser, Subcommand};
use std::{ffi::OsString, path};

#[derive(Debug, Parser)]
#[command(name = "PyRs", version = env!("CARGO_PKG_VERSION"), about = "PyRs compiler")]
pub struct Cli {
    /// Command to execute
    #[clap(subcommand)]
    pub command: Command,
}

impl Cli {
    /// Retain compiler subcommands while accepting `pyrs script.py args` and
    /// Python-style execution options. Everything after the script is a program
    /// argument, even if it happens to look like a compiler option.
    pub fn parse_env() -> Self {
        Self::parse_from(Self::execution_args(std::env::args_os().collect()))
    }

    fn execution_args(mut args: Vec<OsString>) -> Vec<OsString> {
        if let Some(first) = args.get(1)
            && !matches!(
                first.to_str(),
                Some(
                    "compile"
                        | "build-extension"
                        | "run"
                        | "check"
                        | "lex"
                        | "parse"
                        | "help"
                        | "-h"
                        | "--help"
                        | "-V"
                        | "--version"
                )
            )
        {
            args.insert(1, OsString::from("run"));
        }
        if args.get(1).is_some_and(|arg| arg == "run") {
            let mut i = 2;
            while i < args.len() {
                match args[i].to_str() {
                    Some("-c" | "-m") if i + 1 < args.len() => {
                        // CPython stops parsing its own options at -c/-m.
                        if args.get(i + 2).is_some() {
                            args.insert(i + 2, OsString::from("--"));
                        }
                        break;
                    }
                    Some("--python" | "-O" | "--opt-level" | "-i" | "--input") => i += 2,
                    Some("--") => break,
                    Some(arg)
                        if (arg.starts_with("-c") || arg.starts_with("-m")) && arg.len() > 2 =>
                    {
                        args.insert(i + 1, OsString::from("--"));
                        break;
                    }
                    Some(arg) if arg.starts_with('-') && arg != "-" => i += 1,
                    _ => break,
                }
            }
        }
        args
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Tokenize the input file and dump the tokens
    Lex(LexCommand),

    /// Parse the input file and dump the AST
    Parse(ParseCommand),

    /// Compile the input file to a native executable
    Compile(CompileCommand),

    /// Compile the input file and run it immediately
    Run(RunCommand),

    /// Check native compatibility without generating or running a program
    Check(CheckCommand),

    /// Build an experimental CPython extension from native numerical functions
    BuildExtension(ExtensionCommand),
}

#[derive(Debug, Args)]
pub struct LexCommand {
    /// Input file path
    #[arg(short, long)]
    pub input: path::PathBuf,

    /// Output file path (defaults to stdout)
    #[arg(short, long)]
    pub output: Option<path::PathBuf>,
}

#[derive(Debug, Args)]
pub struct ParseCommand {
    /// Input file path
    #[arg(short, long)]
    pub input: path::PathBuf,

    /// Output file path (defaults to stdout)
    #[arg(short, long)]
    pub output: Option<path::PathBuf>,
}

#[derive(Debug, Args)]
pub struct CompileCommand {
    /// Input file path
    #[arg(short, long)]
    pub input: path::PathBuf,

    /// Output executable path
    #[arg(short, long, default_value = "a.out")]
    pub output: path::PathBuf,

    /// Optimization level (0-3)
    #[arg(short = 'O', long = "opt-level", default_value_t = 2, value_parser = clap::value_parser!(u8).range(0..=3))]
    pub opt_level: u8,

    /// Also write the generated LLVM IR next to the output (<output>.ll)
    #[arg(long)]
    pub emit_llvm: bool,

    /// Recompile from scratch, reusing and publishing nothing
    #[arg(long)]
    pub no_cache: bool,
}

#[derive(Debug, Args)]
pub struct RunCommand {
    /// Input file path
    #[arg(short, long, group = "source")]
    pub input: Option<path::PathBuf>,

    /// Execute a string as Python source
    #[arg(short = 'c', group = "source")]
    pub code: Option<String>,

    /// Run an installed module (requires --compat)
    #[arg(short = 'm', group = "source", requires = "compat")]
    pub module: Option<String>,

    /// Execute the whole program with CPython, including installed packages
    #[arg(long)]
    pub compat: bool,

    /// CPython executable for --compat (default: PYRS_PYTHON or python3)
    #[arg(long, requires = "compat")]
    pub python: Option<path::PathBuf>,

    /// Optimization level (0-3)
    #[arg(short = 'O', long = "opt-level", default_value_t = 2, value_parser = clap::value_parser!(u8).range(0..=3))]
    pub opt_level: u8,

    /// Recompile from scratch, reusing and publishing nothing
    #[arg(long)]
    pub no_cache: bool,

    /// Script and arguments, or just arguments with -i/-c/-m; '-' reads stdin
    #[arg(trailing_var_arg = true)]
    pub args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct CheckCommand {
    /// Input file path
    #[arg(short, long)]
    pub input: path::PathBuf,
}

#[derive(Debug, Args)]
pub struct ExtensionCommand {
    /// Source containing numerical function definitions
    #[arg(short, long)]
    pub input: path::PathBuf,

    /// Import name of the resulting extension (an ASCII identifier)
    #[arg(long)]
    pub module: String,

    /// CPython executable whose headers and ABI to target
    #[arg(long, default_value = "python3")]
    pub python: path::PathBuf,

    /// Output extension path (defaults to MODULE plus Python's extension suffix)
    #[arg(short, long)]
    pub output: Option<path::PathBuf>,

    /// Optimization level (0–3)
    #[arg(short = 'O', long = "opt-level", default_value_t = 2, value_parser = clap::value_parser!(u8).range(0..=3))]
    pub opt_level: u8,
}
