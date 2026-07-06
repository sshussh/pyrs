use clap::{Args, Parser, Subcommand};
use std::path;

#[derive(Debug, Parser)]
#[command(name = "PyRs", version = "0.1.0", about = "PyRs compiler")]
pub struct Cli {
    /// Command to execute
    #[clap(subcommand)]
    pub command: Command,
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
    #[arg(short = 'O', long = "opt-level", default_value_t = 2)]
    pub opt_level: u8,

    /// Also write the generated LLVM IR next to the output (<output>.ll)
    #[arg(long)]
    pub emit_llvm: bool,
}

#[derive(Debug, Args)]
pub struct RunCommand {
    /// Input file path
    #[arg(short, long)]
    pub input: path::PathBuf,

    /// Optimization level (0-3)
    #[arg(short = 'O', long = "opt-level", default_value_t = 2)]
    pub opt_level: u8,
}
