//! Project configuration: `[tool.pyrs]` in `pyproject.toml`.
//!
//! PyRs source is valid Python, so a PyRs project is a Python project. It
//! will have a `pyproject.toml` regardless — for ruff, and for the
//! environment `--compat` and `build-extension` point at — and `[tool.<name>]`
//! is the table Python tooling already agrees on. A second config file would
//! make PyRs a foreign object in a Python repo.
//!
//! Only settings that are **not derivable from the filesystem** and **not
//! expressible in Python source** belong here. Dependencies deliberately do
//! not: PyRs cannot compile arbitrary PyPI code, so a dependency table would
//! be a promise the compiler could not keep. That is uv's job, and
//! `--compat` already delegates to a real interpreter with a real
//! environment.

use std::fs;
use std::path::{Path, PathBuf};

/// How a project's program is executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Execution {
    /// Compile to a native executable.
    #[default]
    Native,
    /// Run the whole program under CPython.
    ///
    /// Declared, never inferred. Selecting this from a metadata table — an
    /// empty `dependencies` list, say — would make an invisible switch out of
    /// something the product contract requires to be explicit, and the
    /// inference is wrong in both directions anyway: a dependency may not be
    /// imported by the entry point, and a program with no dependencies at all
    /// can still leave PyRs's supported subset.
    Compat,
}

/// A parsed `[tool.pyrs]` table.
#[derive(Debug, Clone)]
pub struct Manifest {
    /// Directory holding `pyproject.toml`.
    pub dir: PathBuf,
    /// Entry module, relative to `dir`.
    pub entry: Option<PathBuf>,
    /// Import root, relative to `dir`.
    pub root: Option<PathBuf>,
    pub opt_level: Option<u8>,
    pub execution: Execution,
    /// Interpreter for `--compat` and `build-extension`.
    pub python: Option<PathBuf>,
    pub extension: Option<Extension>,
}

/// `[tool.pyrs.extension]` — what `build-extension` otherwise retypes on
/// every invocation.
#[derive(Debug, Clone)]
pub struct Extension {
    pub module: String,
    pub source: PathBuf,
}

impl Manifest {
    /// Resolve `entry` against the project directory.
    pub fn entry_path(&self) -> Option<PathBuf> {
        self.entry.as_ref().map(|e| self.dir.join(e))
    }

    /// Resolve `root`, defaulting to the project directory.
    pub fn root_path(&self) -> PathBuf {
        match &self.root {
            Some(r) => self.dir.join(r),
            None => self.dir.clone(),
        }
    }
}

/// Nearest ancestor directory (including `start`) whose `pyproject.toml`
/// declares `[tool.pyrs]`.
///
/// A `pyproject.toml` without that table belongs to some other Python
/// project, so it does not make its directory a PyRs project.
pub fn discover(start: &Path) -> Result<Option<PathBuf>, String> {
    let mut dir = match start.is_dir() {
        true => start.to_path_buf(),
        false => match start.parent() {
            Some(p) => p.to_path_buf(),
            None => return Ok(None),
        },
    };
    loop {
        let candidate = dir.join("pyproject.toml");
        if candidate.is_file() {
            let text = fs::read_to_string(&candidate)
                .map_err(|e| format!("failed to read {}: {e}", candidate.display()))?;
            // A file that does not parse is reported rather than skipped:
            // silently treating a broken manifest as "no project here" turns
            // a typo into a confusing absence of configuration.
            let doc = toml::de::DeTable::parse(&text)
                .map_err(|e| format!("{}: invalid TOML: {e}", candidate.display()))?;
            let has_table = doc
                .get_ref()
                .get("tool")
                .and_then(|t| t.get_ref().as_table())
                .map(|t| t.contains_key("pyrs"))
                .unwrap_or(false);
            if has_table {
                return Ok(Some(candidate));
            }
            // Parses, but belongs to some other Python project: keep looking.
        }
        if !dir.pop() {
            return Ok(None);
        }
    }
}

/// Read and validate the `[tool.pyrs]` table of `path`.
///
/// Unknown keys are rejected rather than ignored: an accepted-but-ignored key
/// silently does nothing, and becomes a compatibility obligation the moment
/// someone writes it expecting an effect.
pub fn load(path: &Path) -> Result<Manifest, String> {
    let text =
        fs::read_to_string(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let doc = toml::de::DeTable::parse(&text)
        .map_err(|e| format!("{}: invalid TOML: {e}", path.display()))?;
    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let root_table = doc.get_ref();
    let table = root_table
        .get("tool")
        .and_then(|t| t.get_ref().as_table())
        .and_then(|t| t.get("pyrs"))
        .and_then(|t| t.get_ref().as_table())
        .ok_or_else(|| format!("{}: no [tool.pyrs] table", path.display()))?;

    let where_ = format!("{}: [tool.pyrs]", path.display());
    let mut manifest = Manifest {
        dir,
        entry: None,
        root: None,
        opt_level: None,
        execution: Execution::Native,
        python: None,
        extension: None,
    };

    for (key, value) in table.iter() {
        let key = key.get_ref().as_ref();
        let value = value.get_ref();
        match key {
            "entry" => manifest.entry = Some(PathBuf::from(want_str(value, &where_, key)?)),
            "root" => manifest.root = Some(PathBuf::from(want_str(value, &where_, key)?)),
            "python" => manifest.python = Some(PathBuf::from(want_str(value, &where_, key)?)),
            "opt-level" => {
                let n = value
                    .as_integer()
                    .and_then(|i| i.as_str().parse::<i64>().ok())
                    .ok_or_else(|| format!("{where_}: 'opt-level' must be an integer 0-3"))?;
                if !(0..=3).contains(&n) {
                    return Err(format!("{where_}: 'opt-level' must be 0-3, found {n}"));
                }
                manifest.opt_level = Some(n as u8);
            }
            "execution" => {
                manifest.execution = match want_str(value, &where_, key)? {
                    "native" => Execution::Native,
                    "compat" => Execution::Compat,
                    other => {
                        return Err(format!(
                            "{where_}: 'execution' must be \"native\" or \"compat\", found \
                             \"{other}\""
                        ));
                    }
                }
            }
            "extension" => {
                let t = value
                    .as_table()
                    .ok_or_else(|| format!("{where_}: 'extension' must be a table"))?;
                let sub = format!("{where_}.extension");
                let mut module = None;
                let mut source = None;
                for (k, v) in t.iter() {
                    match k.get_ref().as_ref() {
                        "module" => {
                            module = Some(want_str(v.get_ref(), &sub, "module")?.to_string())
                        }
                        "source" => {
                            source = Some(PathBuf::from(want_str(v.get_ref(), &sub, "source")?))
                        }
                        other => return Err(unknown_key(&sub, other, &["module", "source"])),
                    }
                }
                let module = module.ok_or_else(|| format!("{sub}: 'module' is required"))?;
                let source = source.ok_or_else(|| format!("{sub}: 'source' is required"))?;
                manifest.extension = Some(Extension { module, source });
            }
            other => {
                return Err(unknown_key(
                    &where_,
                    other,
                    &[
                        "entry",
                        "root",
                        "opt-level",
                        "execution",
                        "python",
                        "extension",
                    ],
                ));
            }
        }
    }
    Ok(manifest)
}

fn want_str<'a>(
    value: &'a toml::de::DeValue<'a>,
    where_: &str,
    key: &str,
) -> Result<&'a str, String> {
    value
        .as_str()
        .ok_or_else(|| format!("{where_}: '{key}' must be a string"))
}

fn unknown_key(where_: &str, found: &str, known: &[&str]) -> String {
    format!(
        "{where_}: unknown key '{found}'; expected one of {}",
        known
            .iter()
            .map(|k| format!("'{k}'"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}
