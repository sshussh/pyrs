use clap::{Args, Parser, Subcommand};
use std::{ffi::OsString, path, process};

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
        let args: Vec<OsString> = std::env::args_os().collect();
        if let Some((typo, meant)) = Self::mistyped_subcommand(&args) {
            // Without this the shim below turns the typo into a script path,
            // and the user is told `failed to read comple` — a message about
            // a file they never meant to name.
            eprintln!("error: unrecognized subcommand '{typo}'");
            eprintln!();
            eprintln!("  tip: a similar subcommand exists: '{meant}'");
            eprintln!();
            eprintln!("Usage: pyrs [COMMAND] | pyrs <SCRIPT> [ARGS]...");
            eprintln!("For more information, try '--help'.");
            process::exit(2);
        }
        Self::parse_from(Self::execution_args(args))
    }

    /// Every subcommand name and alias, taken from the parser itself.
    ///
    /// Derived rather than listed: a hardcoded copy silently breaks the
    /// moment a subcommand is added — the new name falls through to the
    /// script shim and the command becomes an attempt to read a file of that
    /// name. The test suite pins this to every `Command` variant.
    pub fn subcommand_names() -> Vec<String> {
        use clap::CommandFactory;
        Self::command()
            .get_subcommands()
            .flat_map(|c| {
                std::iter::once(c.get_name().to_string())
                    .chain(c.get_all_aliases().map(str::to_string))
            })
            .collect()
    }

    /// A first argument that is almost a subcommand: not one, not a file that
    /// exists, not obviously a script path, and one or two edits away from a
    /// real name.
    fn mistyped_subcommand(args: &[OsString]) -> Option<(String, String)> {
        let first = args.get(1)?.to_str()?;
        if first.starts_with('-')
            || first.contains(path::MAIN_SEPARATOR)
            || first.contains('/')
            || first.ends_with(".py")
            || path::Path::new(first).exists()
        {
            return None;
        }
        let names = Self::subcommand_names();
        if names.iter().any(|n| n == first) {
            return None;
        }
        names
            .into_iter()
            .filter(|n| n.len() >= 3)
            .map(|n| (edit_distance(first, &n), n))
            .filter(|(d, _)| *d <= 2)
            .min()
            .map(|(_, n)| (first.to_string(), n))
    }

    fn execution_args(mut args: Vec<OsString>) -> Vec<OsString> {
        let known = Self::subcommand_names();
        if let Some(first) = args.get(1)
            && !first
                .to_str()
                .is_some_and(|a| known.iter().any(|n| n == a) || GLOBAL_FLAGS.contains(&a))
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
                    Some(
                        "--python" | "-O" | "--opt-level" | "-i" | "--input" | "--message-format",
                    ) => i += 2,
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

/// Top-level options clap handles itself, which must not be mistaken for a
/// script name.
const GLOBAL_FLAGS: [&str; 5] = ["help", "-h", "--help", "-V", "--version"];

/// Levenshtein distance, for "did you mean". Two rows rather than a full
/// matrix; the inputs are subcommand-sized either way.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Tokenize the input file and dump the tokens
    Lex(LexCommand),

    /// Parse the input file and dump the AST
    Parse(ParseCommand),

    /// Compile to a native executable (alias: build)
    #[command(alias = "build")]
    Compile(CompileCommand),

    /// Run an instrumented build that records type tags at polymorphic sites
    Profile(ProfileCommand),

    /// Compile the input file and run it immediately
    Run(RunCommand),

    /// Check native compatibility without generating or running a program
    Check(CheckCommand),

    /// Build an experimental CPython extension from native numerical functions
    BuildExtension(ExtensionCommand),

    /// Add a [tool.pyrs] table to this project's pyproject.toml
    Init(InitCommand),

    /// Inspect, clean and prune the build cache
    Cache(CacheCommand),

    /// Remove this project's build output directory
    Clean(CleanCommand),

    /// Report the toolchain PyRs found, and whether it is usable
    Doctor,

    /// Print a shell completion script
    Completions(CompletionsCommand),

    /// Show the import graph the compiler resolved
    Tree(TreeCommand),

    /// Compile and run the project's tests natively
    Test(TestCommand),
}

#[derive(Debug, Args)]
pub struct TestCommand {
    /// Only run tests whose name contains this
    #[arg(value_name = "FILTER")]
    pub filter: Option<String>,

    /// Test file or directory to search (default: the project's roots)
    #[arg(short, long)]
    pub input: Option<path::PathBuf>,

    /// Optimization level (0-3); defaults to the manifest, then 2
    #[arg(short = 'O', long = "opt-level", value_parser = clap::value_parser!(u8).range(0..=3))]
    pub opt_level: Option<u8>,

    /// Target CPU: "generic" for a portable binary, "native" for this host's
    /// model and ISA extensions, or a specific model name
    #[arg(long, value_name = "CPU")]
    pub target_cpu: Option<String>,

    /// Recompile from scratch, reusing and publishing nothing
    #[arg(long)]
    pub no_cache: bool,

    /// List the tests that would run, without running them
    #[arg(long)]
    pub list: bool,
}

#[derive(Debug, Args)]
pub struct TreeCommand {
    /// Input file path (default: the project's [tool.pyrs] entry)
    #[arg(short, long)]
    pub input: Option<path::PathBuf>,

    /// Show the file each module was resolved from
    #[arg(long)]
    pub paths: bool,

    /// Limit the depth shown
    #[arg(long, value_name = "N")]
    pub depth: Option<usize>,
}

#[derive(Debug, Args)]
pub struct CleanCommand {
    /// Report what would be removed without removing it
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct CompletionsCommand {
    /// Shell to generate for
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

#[derive(Debug, Args)]
pub struct CacheCommand {
    #[clap(subcommand)]
    pub action: CacheAction,
}

#[derive(Debug, Subcommand)]
pub enum CacheAction {
    /// Print the cache directory
    Dir,

    /// Report entry counts and sizes for each cache layer
    Info,

    /// Remove cached entries
    Clean(CacheClean),

    /// Remove entries that are stale or over a size budget
    Prune(CachePrune),
}

#[derive(Debug, Args)]
pub struct CacheClean {
    /// Only compiled programs
    #[arg(long)]
    pub programs: bool,

    /// Only compiled runtime objects
    #[arg(long)]
    pub runtime: bool,

    /// Report what would be removed without removing it
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct CachePrune {
    /// Drop entries not reused within this long (7d, 24h, 30m)
    #[arg(long, value_name = "DURATION")]
    pub older_than: Option<String>,

    /// Drop least-recently-used entries until the cache fits (500MB, 2GiB)
    #[arg(long, value_name = "SIZE")]
    pub max_size: Option<String>,

    /// Only compiled programs
    #[arg(long)]
    pub programs: bool,

    /// Only compiled runtime objects
    #[arg(long)]
    pub runtime: bool,

    /// Report what would be removed without removing it
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct InitCommand {
    /// Project directory (default: the current directory)
    pub path: Option<path::PathBuf>,

    /// Project name (default: the directory's name)
    #[arg(long)]
    pub name: Option<String>,

    /// Entry module to record, relative to the project directory
    #[arg(long)]
    pub entry: Option<path::PathBuf>,

    /// Scaffold a single main.py instead of a src/ package layout
    #[arg(long)]
    pub script: bool,

    /// Version control to initialize (default: git, when available)
    #[arg(long, value_name = "VCS", default_value = "git")]
    pub vcs: Vcs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Vcs {
    Git,
    None,
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
    /// Input file path (default: the project's [tool.pyrs] entry)
    #[arg(short, long)]
    pub input: Option<path::PathBuf>,

    /// Output executable path (default: target/NAME in a project, else a.out)
    #[arg(short, long)]
    pub output: Option<path::PathBuf>,

    /// Optimization level (0-3); defaults to the manifest, then 2
    #[arg(short = 'O', long = "opt-level", value_parser = clap::value_parser!(u8).range(0..=3))]
    pub opt_level: Option<u8>,

    /// Target CPU: "generic" for a portable binary, "native" for this host's
    /// model and ISA extensions, or a specific model name
    #[arg(long, value_name = "CPU")]
    pub target_cpu: Option<String>,

    /// Also write the generated LLVM IR next to the output (<output>.ll)
    #[arg(long)]
    pub emit_llvm: bool,

    /// Recompile from scratch, reusing and publishing nothing
    #[arg(long)]
    pub no_cache: bool,

    /// Do not print the build summary
    #[arg(short, long)]
    pub quiet: bool,

    /// Type profile from `pyrs profile`; ignored if missing or stale
    #[arg(long, value_name = "FILE")]
    pub profile: Option<path::PathBuf>,

    /// How to print diagnostics
    #[arg(long, value_name = "FORMAT", default_value = "human")]
    pub message_format: crate::diagnostics::Format,
}

#[derive(Debug, Args)]
pub struct ProfileCommand {
    /// Input file path
    #[arg(short, long)]
    pub input: Option<path::PathBuf>,

    /// Where to write the profile (default: <script>.prof)
    #[arg(short, long)]
    pub output: Option<path::PathBuf>,

    /// Optimization level (0-3); defaults to 2
    #[arg(short = 'O', long = "opt-level", value_parser = clap::value_parser!(u8).range(0..=3))]
    pub opt_level: Option<u8>,

    /// Target CPU: "generic", "native", or a model name
    #[arg(long, value_name = "CPU")]
    pub target_cpu: Option<String>,

    /// How to print diagnostics
    #[arg(long, value_name = "FORMAT", default_value = "human")]
    pub message_format: crate::diagnostics::Format,

    /// Script and arguments when not passed as -i
    #[arg(trailing_var_arg = true)]
    pub args: Vec<OsString>,
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

    /// Compile and run natively even when the manifest asks for compat
    #[arg(long, conflicts_with = "compat")]
    pub no_compat: bool,

    /// CPython executable for --compat (default: PYRS_PYTHON or python3)
    #[arg(long)]
    pub python: Option<path::PathBuf>,

    /// Optimization level (0-3); defaults to the manifest, then 2
    #[arg(short = 'O', long = "opt-level", value_parser = clap::value_parser!(u8).range(0..=3))]
    pub opt_level: Option<u8>,

    /// Target CPU: "generic" for a portable binary, "native" for this host's
    /// model and ISA extensions, or a specific model name
    #[arg(long, value_name = "CPU")]
    pub target_cpu: Option<String>,

    /// Recompile from scratch, reusing and publishing nothing
    #[arg(long)]
    pub no_cache: bool,

    /// How to print diagnostics
    #[arg(long, value_name = "FORMAT", default_value = "human")]
    pub message_format: crate::diagnostics::Format,

    /// Script and arguments, or just arguments with -i/-c/-m; '-' reads stdin
    #[arg(trailing_var_arg = true)]
    pub args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct CheckCommand {
    /// Input file path (default: the project's [tool.pyrs] entry)
    #[arg(short, long)]
    pub input: Option<path::PathBuf>,

    /// How to print diagnostics
    #[arg(long, value_name = "FORMAT", default_value = "human")]
    pub message_format: crate::diagnostics::Format,
}

#[derive(Debug, Args)]
pub struct ExtensionCommand {
    /// Source containing numerical function definitions
    /// (default: [tool.pyrs.extension] source)
    #[arg(short, long)]
    pub input: Option<path::PathBuf>,

    /// Import name of the resulting extension (an ASCII identifier)
    /// (default: [tool.pyrs.extension] module)
    #[arg(long)]
    pub module: Option<String>,

    /// CPython executable whose headers and ABI to target
    #[arg(long)]
    pub python: Option<path::PathBuf>,

    /// Output extension path (defaults to MODULE plus Python's extension suffix)
    #[arg(short, long)]
    pub output: Option<path::PathBuf>,

    /// Optimization level (0–3)
    #[arg(short = 'O', long = "opt-level", default_value_t = 2, value_parser = clap::value_parser!(u8).range(0..=3))]
    pub opt_level: u8,

    /// Target CPU: "generic" for a portable binary, "native" for this host's
    /// model and ISA extensions, or a specific model name
    #[arg(long, value_name = "CPU")]
    pub target_cpu: Option<String>,
}
