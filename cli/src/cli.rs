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
    /// Lex the input file
    Lex(LexCommand),

    /// Parse the input file
    Parse(ParseCommand),

    /// Compile the input file
    Compile(CompileCommand),
}

#[derive(Debug, Args)]
pub struct LexCommand {
    #[command(flatten)]
    pub io: Io,
}

#[derive(Debug, Args)]
pub struct ParseCommand {
    #[command(flatten)]
    pub io: Io,
}

#[derive(Debug, Args)]
pub struct CompileCommand {
    #[command(flatten)]
    pub io: Io,
}

#[derive(Debug, Args)]
pub struct Io {
    /// Input file path
    #[arg(short, long)]
    pub input: path::PathBuf,

    /// Output file path
    #[arg(short, long, default_value = "a.out")]
    pub output: path::PathBuf,
}
