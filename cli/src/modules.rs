//! Multi-file module loading: resolve `import`/`from ... import`
//! transitively, detect cycles, and produce modules in topological order
//! (dependencies first, root last) for the semantic analyzer.
//!
//! Resolution mirrors `python root.py`: every import is looked up relative
//! to the **root script's directory** (`sys.path[0]`), so `import utils`
//! always means `<rootdir>/utils.py` no matter which file imports it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use common::Diagnostic;
use parser::ast;

/// The entry/root module's synthetic name.
pub const ROOT_NAME: &str = "__main__";

/// A parsed module ready for analysis.
pub struct Loaded {
    /// Import name (`utils`), or [`ROOT_NAME`] for the root.
    pub name: String,
    /// Path shown in diagnostics.
    pub display: String,
    pub source: String,
    pub ast: ast::Module,
}

/// A load failure already rendered against the right file.
pub struct LoadError(pub String);

/// Load the whole program starting from `root`. Returns modules in
/// topological order with the root last; the index is the diagnostic
/// file id.
pub fn load_program(root: &Path) -> Result<Vec<Loaded>, LoadError> {
    let base = root
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let mut modules: HashMap<String, Parsed> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut visiting: HashSet<String> = HashSet::new();

    // parse the root under its synthetic name, resolving from `base`
    let root_parsed = parse_file(root)?;
    modules.insert(ROOT_NAME.to_string(), root_parsed);
    visit(ROOT_NAME, &base, &mut modules, &mut order, &mut visiting)?;

    // `order` is a post-order DFS: dependencies precede dependents, root last
    let mut out = Vec::new();
    for name in order {
        let p = modules.remove(&name).unwrap();
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
    /// non-`sys` modules this file imports, with each import's span
    imports: Vec<(String, common::Span)>,
}

fn parse_file(path: &Path) -> Result<Parsed, LoadError> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| LoadError(format!("failed to read {}: {e}", path.display())))?;
    let display = path.display().to_string();
    let module = parser::parse(&source).map_err(|d| LoadError(render(&d, &display, &source)))?;

    let mut imports = Vec::new();
    for stmt in &module.body {
        match &stmt.kind {
            ast::StmtKind::Import {
                module: m, span, ..
            }
            | ast::StmtKind::FromImport {
                module: m, span, ..
            } if m != "sys" => {
                imports.push((m.clone(), *span));
            }
            _ => {}
        }
    }
    Ok(Parsed {
        display,
        source,
        ast: module,
        imports,
    })
}

fn visit(
    name: &str,
    base: &Path,
    modules: &mut HashMap<String, Parsed>,
    order: &mut Vec<String>,
    visiting: &mut HashSet<String>,
) -> Result<(), LoadError> {
    visiting.insert(name.to_string());
    // clone the import list so we can recurse without holding a borrow
    let imports = modules[name].imports.clone();
    let importer_display = modules[name].display.clone();
    let importer_source = modules[name].source.clone();

    for (dep, span) in imports {
        if visiting.contains(&dep) {
            let d = Diagnostic::new(
                common::Phase::Semantic,
                format!("circular import: '{name}' and '{dep}' import each other"),
                span,
            );
            return Err(LoadError(render(&d, &importer_display, &importer_source)));
        }
        if !modules.contains_key(&dep) {
            let path = base.join(format!("{dep}.py"));
            if !path.is_file() {
                let d = Diagnostic::new(
                    common::Phase::Semantic,
                    format!("No module named '{dep}'"),
                    span,
                );
                return Err(LoadError(render(&d, &importer_display, &importer_source)));
            }
            let parsed = parse_file(&path)?;
            modules.insert(dep.clone(), parsed);
            visit(&dep, base, modules, order, visiting)?;
        }
    }

    visiting.remove(name);
    if !order.iter().any(|n| n == name) {
        order.push(name.to_string());
    }
    Ok(())
}

/// Render a diagnostic against a specific file's source.
fn render(diag: &Diagnostic, display: &str, source: &str) -> String {
    if diag.span == common::Span::default() {
        format!("{diag}")
    } else {
        diag.render(display, source)
    }
}
