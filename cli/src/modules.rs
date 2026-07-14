//! Multi-file module loading: resolve `import` / `from ... import`
//! transitively (including packages and relative imports), detect cycles,
//! and produce modules in topological order (dependencies first, root last)
//! for the semantic analyzer.
//!
//! Resolution mirrors `python root.py`: every absolute import is looked up
//! relative to the **root script's directory** (`sys.path[0]`). A directory
//! with `__init__.py` is a package; `import pkg.mod` loads `pkg/__init__.py`
//! then `pkg/mod.py`. Relative imports (`from . import x`) resolve against
//! the importing module's package (`__package__`) and are rewritten to
//! absolute form before semantic analysis.
//!
//! **Partial package init:** a package `__init__.py` may import its own
//! submodules (`from . import mod`). While the package is being visited,
//! those children load without treating the parent edge as a cycle (like
//! CPython’s partially-initialized package). True mutual imports between
//! unrelated modules remain compile errors.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use common::Diagnostic;
use parser::ast;

/// The entry/root module's synthetic name.
pub const ROOT_NAME: &str = "__main__";

/// A parsed module ready for analysis.
pub struct Loaded {
    /// Fully-qualified import name (`utils`, `pkg.mod`), or [`ROOT_NAME`].
    pub name: String,
    /// Path shown in diagnostics.
    pub display: String,
    pub source: String,
    /// AST with relative imports already rewritten to absolute (`level = 0`).
    pub ast: ast::Module,
}

/// A load failure already rendered against the right file.
#[derive(Debug)]
pub struct LoadError(pub String);

/// Load the whole program starting from `root`. Returns modules in
/// topological order with the root last; the index is the diagnostic
/// file id.
pub fn load_program(root: &Path) -> Result<Vec<Loaded>, LoadError> {
    let base = root
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let mut state = LoadState {
        base: base.clone(),
        modules: HashMap::new(),
        order: Vec::new(),
        visiting: HashSet::new(),
    };

    let root_parsed = parse_module(root, ROOT_NAME, None, &base)?;
    state.modules.insert(ROOT_NAME.to_string(), root_parsed);
    state.require(ROOT_NAME, common::Span::default(), ROOT_NAME)?;

    let mut out = Vec::new();
    for name in state.order {
        let p = state.modules.remove(&name).unwrap();
        out.push(Loaded {
            name,
            display: p.display,
            source: p.source,
            ast: p.ast,
        });
    }
    Ok(out)
}

struct Parsed {
    display: String,
    source: String,
    ast: ast::Module,
    is_package: bool,
    /// Absolute module names this file depends on (leaves; parents are
    /// required automatically), each with the importing span.
    deps: Vec<(String, common::Span)>,
}

struct LoadState {
    base: PathBuf,
    modules: HashMap<String, Parsed>,
    order: Vec<String>,
    visiting: HashSet<String>,
}

impl LoadState {
    /// Ensure `name` and its import graph are loaded and appear in `order`.
    fn require(&mut self, name: &str, span: common::Span, importer: &str) -> Result<(), LoadError> {
        if self.order.iter().any(|n| n == name) {
            return Ok(());
        }
        if self.visiting.contains(name) {
            // Package mid-init importing a child (or child touching parent):
            // allow if `name` is a proper package prefix of `importer`.
            if is_strict_package_prefix(name, importer) {
                return Ok(());
            }
            let (display, source) = self.importer_context(importer);
            let d = Diagnostic::new(
                common::Phase::Load,
                format!("circular import: '{importer}' and '{name}' import each other"),
                span,
            );
            return Err(LoadError(render(&d, &display, &source)));
        }

        self.ensure_loaded(name, span, importer)?;

        // Parents before children, unless the parent is already mid-init
        // (package `__init__` importing a submodule).
        if let Some(parent) = parent_name(name)
            && !self.visiting.contains(&parent)
        {
            self.require(&parent, span, importer)?;
        }

        self.visiting.insert(name.to_string());
        let deps = self.modules[name].deps.clone();
        for (dep, dep_span) in deps {
            self.require(&dep, dep_span, name)?;
        }
        self.visiting.remove(name);

        if !self.order.iter().any(|n| n == name) {
            self.order.push(name.to_string());
        }
        Ok(())
    }

    fn importer_context(&self, importer: &str) -> (String, String) {
        let p = &self.modules[importer];
        (p.display.clone(), p.source.clone())
    }

    /// Parse `name` and any missing parent packages into `modules`.
    fn ensure_loaded(
        &mut self,
        name: &str,
        span: common::Span,
        importer: &str,
    ) -> Result<(), LoadError> {
        if name == ROOT_NAME {
            return Ok(());
        }

        let parts: Vec<&str> = name.split('.').collect();
        for i in 1..=parts.len() {
            let partial = parts[..i].join(".");
            if let Some(existing) = self.modules.get(&partial) {
                if i < parts.len() && !existing.is_package {
                    let (display, source) = self.importer_context(importer);
                    let d = Diagnostic::new(
                        common::Phase::Load,
                        format!("No module named '{name}'; '{partial}' is not a package"),
                        span,
                    );
                    return Err(LoadError(render(&d, &display, &source)));
                }
                continue;
            }

            match resolve_module_path(&self.base, &partial) {
                Some((path, is_package)) => {
                    if i < parts.len() && !is_package {
                        let (display, source) = self.importer_context(importer);
                        let d = Diagnostic::new(
                            common::Phase::Load,
                            format!("No module named '{name}'; '{partial}' is not a package"),
                            span,
                        );
                        return Err(LoadError(render(&d, &display, &source)));
                    }
                    let package = package_of(&partial, is_package);
                    let parsed = parse_module(&path, &partial, package, &self.base)?;
                    self.modules.insert(partial, parsed);
                }
                None => {
                    let (display, source) = self.importer_context(importer);
                    let d = Diagnostic::new(
                        common::Phase::Load,
                        format!("No module named '{name}'"),
                        span,
                    );
                    return Err(LoadError(render(&d, &display, &source)));
                }
            }
        }
        Ok(())
    }
}

/// True if `parent` is a dotted prefix of `child` (`pkg` of `pkg.mod`).
fn is_strict_package_prefix(parent: &str, child: &str) -> bool {
    child.len() > parent.len()
        && child.as_bytes().get(parent.len()) == Some(&b'.')
        && child.starts_with(parent)
}

fn parent_name(name: &str) -> Option<String> {
    name.rsplit_once('.').map(|(p, _)| p.to_string())
}

/// `__package__` for a loaded module.
fn package_of(name: &str, is_package: bool) -> Option<String> {
    if is_package {
        Some(name.to_string())
    } else {
        parent_name(name)
    }
}

/// Locate `name` under `base`: package (`…/__init__.py`) or module (`….py`).
fn resolve_module_path(base: &Path, name: &str) -> Option<(PathBuf, bool)> {
    let rel: PathBuf = name.split('.').collect();
    let pkg_init = base.join(&rel).join("__init__.py");
    if pkg_init.is_file() {
        return Some((pkg_init, true));
    }
    let mut py = base.join(&rel);
    py.set_extension("py");
    if py.is_file() {
        return Some((py, false));
    }
    None
}

fn module_exists(base: &Path, name: &str) -> bool {
    resolve_module_path(base, name).is_some()
}

fn parse_module(
    path: &Path,
    name: &str,
    package: Option<String>,
    base: &Path,
) -> Result<Parsed, LoadError> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| LoadError(format!("failed to read {}: {e}", path.display())))?;
    let display = path.display().to_string();
    let mut module =
        parser::parse(&source).map_err(|d| LoadError(render(&d, &display, &source)))?;

    rewrite_relative_imports(&mut module, package.as_deref(), &display, &source)?;
    let deps = collect_deps(&module, name, base);
    let is_package = path.file_name().is_some_and(|f| f == "__init__.py");

    Ok(Parsed {
        display,
        source,
        ast: module,
        is_package,
        deps,
    })
}

/// Absolute module dependencies from a (relative-rewritten) AST.
/// Only **top-level** import statements are collected (same as semantic).
fn collect_deps(module: &ast::Module, self_name: &str, base: &Path) -> Vec<(String, common::Span)> {
    let mut deps = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |mod_name: String, span: common::Span| {
        if mod_name != "sys" && mod_name != self_name && seen.insert(mod_name.clone()) {
            deps.push((mod_name, span));
        }
    };

    for stmt in &module.body {
        match &stmt.kind {
            ast::StmtKind::Import {
                module: m, span, ..
            } if m != "sys" => {
                push(m.clone(), *span);
            }
            ast::StmtKind::FromImport {
                module: m,
                names,
                span,
                ..
            } => {
                if m != "sys" && !m.is_empty() && m != self_name {
                    push(m.clone(), *span);
                }
                // `from pkg import mod` may refer to a submodule
                for (import_name, _, nspan) in names {
                    let sub = if m.is_empty() {
                        import_name.clone()
                    } else {
                        format!("{m}.{import_name}")
                    };
                    if module_exists(base, &sub) {
                        push(sub, *nspan);
                    }
                }
            }
            _ => {}
        }
    }
    deps
}

/// Turn relative `from` imports into absolute (`level = 0`) in place.
/// Only top-level statements (nested imports are rejected in semantic).
fn rewrite_relative_imports(
    module: &mut ast::Module,
    package: Option<&str>,
    display: &str,
    source: &str,
) -> Result<(), LoadError> {
    for stmt in &mut module.body {
        if let ast::StmtKind::FromImport {
            module: m,
            level,
            span,
            ..
        } = &mut stmt.kind
        {
            if *level == 0 {
                continue;
            }
            let abs = resolve_relative(*level, m, package).map_err(|msg| {
                let d = Diagnostic::new(common::Phase::Load, msg, *span);
                LoadError(render(&d, display, source))
            })?;
            *m = abs;
            *level = 0;
        }
    }
    Ok(())
}

/// Resolve a relative import to an absolute module name (PEP 328).
fn resolve_relative(level: u32, module: &str, package: Option<&str>) -> Result<String, String> {
    let Some(pkg) = package else {
        return Err("attempted relative import with no known parent package".to_string());
    };
    let mut parts: Vec<&str> = pkg.split('.').filter(|p| !p.is_empty()).collect();
    if level as usize > parts.len() {
        return Err("attempted relative import beyond top-level package".to_string());
    }
    // level 1: current package; level 2: drop one component; …
    let keep = parts.len() - (level as usize - 1);
    parts.truncate(keep);
    if module.is_empty() {
        if parts.is_empty() {
            return Err("attempted relative import beyond top-level package".to_string());
        }
        Ok(parts.join("."))
    } else if parts.is_empty() {
        Ok(module.to_string())
    } else {
        Ok(format!("{}.{}", parts.join("."), module))
    }
}

/// Render a diagnostic against a specific file's source.
fn render(diag: &Diagnostic, display: &str, source: &str) -> String {
    if diag.span == common::Span::default() {
        format!("{diag}")
    } else {
        diag.render(display, source)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        is_strict_package_prefix, resolve_module_path, resolve_relative, rewrite_relative_imports,
    };
    use parser::ast;
    use std::path::PathBuf;

    #[test]
    fn relative_resolution() {
        assert_eq!(resolve_relative(1, "", Some("pkg")).unwrap(), "pkg");
        assert_eq!(resolve_relative(1, "mod", Some("pkg")).unwrap(), "pkg.mod");
        assert_eq!(resolve_relative(2, "", Some("pkg.sub")).unwrap(), "pkg");
        assert_eq!(resolve_relative(2, "a", Some("pkg.sub")).unwrap(), "pkg.a");
        assert_eq!(
            resolve_relative(2, "other", Some("pkg.sub")).unwrap(),
            "pkg.other"
        );
        assert!(resolve_relative(1, "x", None).is_err());
        assert!(resolve_relative(2, "", Some("pkg")).is_err());
        assert!(resolve_relative(3, "x", Some("pkg.sub")).is_err());
    }

    #[test]
    fn package_prefix() {
        assert!(is_strict_package_prefix("pkg", "pkg.mod"));
        assert!(is_strict_package_prefix("pkg.sub", "pkg.sub.m"));
        assert!(!is_strict_package_prefix("pkg", "pkg"));
        assert!(!is_strict_package_prefix("pkg", "pkgx"));
        assert!(!is_strict_package_prefix("a", "b"));
    }

    #[test]
    fn rewrite_relative_from_dot() {
        let mut m = parser::parse("from .mod import x\nfrom .. import y\n").unwrap();
        rewrite_relative_imports(&mut m, Some("pkg.sub"), "t.py", "from .mod import x\n").unwrap();
        assert!(matches!(
            &m.body[0].kind,
            ast::StmtKind::FromImport {
                module,
                level: 0,
                ..
            } if module == "pkg.sub.mod"
        ));
        assert!(matches!(
            &m.body[1].kind,
            ast::StmtKind::FromImport {
                module,
                level: 0,
                ..
            } if module == "pkg"
        ));
    }

    #[test]
    fn resolve_prefers_package_init() {
        let dir = std::env::temp_dir().join(format!("pyrs-modtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        std::fs::write(dir.join("pkg/__init__.py"), "").unwrap();
        std::fs::write(dir.join("pkg/mod.py"), "").unwrap();
        let (p, is_pkg) = resolve_module_path(&dir, "pkg").unwrap();
        assert!(is_pkg);
        assert!(p.ends_with("__init__.py"));
        let (p2, is_pkg2) = resolve_module_path(&dir, "pkg.mod").unwrap();
        assert!(!is_pkg2);
        assert!(p2.ends_with("mod.py"));
        assert!(resolve_module_path(&dir, "nope").is_none());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = PathBuf::from("."); // silence unused in some editions
    }
}
