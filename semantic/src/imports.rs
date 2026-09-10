//! Import collection, re-exports, and `__all__` handling.

use std::collections::{HashMap, HashSet};

use common::Span;
use parser::ast;

use crate::prelude::*;

/// Build name → AST for modules in the program.
pub(crate) fn module_ast_map<'a>(
    modules: &'a [ModuleInput<'a>],
) -> HashMap<&'a str, &'a ast::Module> {
    modules.iter().map(|m| (m.name.as_str(), m.ast)).collect()
}

/// Names that `from src import *` should bind, for a single expansion step.
///
/// - Static `__all__`: those names (including private).
/// - Dynamic `__all__`: empty here; [`collect_imports`] reports the error.
/// - No `__all__`: public names from the current export surface (funcs +
///   values) plus any extra public names supplied by the caller (e.g.
///   submodule short names from last-export / submodule maps).
pub(crate) fn star_import_name_list(
    src: &str,
    src_ast: Option<&ast::Module>,
    export_funcs: &HashMap<String, HashMap<String, FuncSig>>,
    export_values: &HashMap<String, std::collections::HashSet<String>>,
    extra_public: Option<&std::collections::HashSet<String>>,
    span: Span,
) -> Vec<(String, Option<String>, Span)> {
    if let Some(ast) = src_ast {
        match static_dunder_all(ast) {
            Some(Ok(names)) => {
                return names.into_iter().map(|n| (n, None, span)).collect();
            }
            Some(Err(())) => return Vec::new(),
            None => {}
        }
    }
    let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    if let Some(f) = export_funcs.get(src) {
        for k in f.keys() {
            if !k.starts_with('_') {
                names.insert(k.clone());
            }
        }
    }
    if let Some(v) = export_values.get(src) {
        for k in v {
            if !k.starts_with('_') {
                names.insert(k.clone());
            }
        }
    }
    if let Some(extra) = extra_public {
        for k in extra {
            if !k.starts_with('_') {
                names.insert(k.clone());
            }
        }
    }
    names.into_iter().map(|n| (n, None, span)).collect()
}

/// Effective from-import name list: expand `*` when `star` is set.
#[allow(clippy::too_many_arguments)]
pub(crate) fn effective_from_names(
    src: &str,
    names: &[(String, Option<String>, Span)],
    star: bool,
    span: Span,
    src_ast: Option<&ast::Module>,
    export_funcs: &HashMap<String, HashMap<String, FuncSig>>,
    export_values: &HashMap<String, std::collections::HashSet<String>>,
    extra_public: Option<&std::collections::HashSet<String>>,
) -> Vec<(String, Option<String>, Span)> {
    if star {
        star_import_name_list(
            src,
            src_ast,
            export_funcs,
            export_values,
            extra_public,
            span,
        )
    } else {
        names.to_vec()
    }
}

/// Names bound by top-level assignment / augassign only (not imports).
/// Used so `from . import mod` is not mistaken for a scalar global that
/// shadows the submodule.
pub(crate) fn collect_assigned_names(module: &ast::Module) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    for stmt in &module.body {
        match &stmt.kind {
            ast::StmtKind::Assign { targets, .. } => {
                for t in targets {
                    collect_assign_names(t, &mut names);
                }
            }
            ast::StmtKind::AugAssign {
                target: ast::AssignTarget::Name { name, .. },
                ..
            } => {
                names.insert(name.clone());
            }
            _ => {}
        }
    }
    names
}

/// Export value names: own assignments plus `from … import` value re-exports
/// (fixpoint). Submodule imports are excluded so they stay Module bindings.
pub(crate) fn expand_export_values(
    modules: &[ModuleInput],
    assigned: &HashMap<String, std::collections::HashSet<String>>,
    export_funcs: &HashMap<String, HashMap<String, FuncSig>>,
    submodules: &HashMap<String, HashMap<String, String>>,
) -> HashMap<String, std::collections::HashSet<String>> {
    let asts = module_ast_map(modules);
    let mut export = assigned.clone();
    loop {
        let mut changed = false;
        for m in modules {
            for stmt in &m.ast.body {
                let ast::StmtKind::FromImport {
                    module: src,
                    names,
                    star,
                    span,
                    ..
                } = &stmt.kind
                else {
                    continue;
                };
                let eff = effective_from_names(
                    src,
                    names,
                    *star,
                    *span,
                    asts.get(src.as_str()).copied(),
                    export_funcs,
                    &export,
                    None,
                );
                for (name, alias, _) in &eff {
                    let local = alias.as_ref().unwrap_or(name);
                    // Submodule: not a value export.
                    if submodules
                        .get(src.as_str())
                        .is_some_and(|s| s.contains_key(name))
                    {
                        continue;
                    }
                    // Function re-exports live in export_funcs, not values.
                    if export_funcs
                        .get(src.as_str())
                        .is_some_and(|f| f.contains_key(name))
                    {
                        continue;
                    }
                    if !export
                        .get(src.as_str())
                        .is_some_and(|g| g.contains(name.as_str()))
                    {
                        continue;
                    }
                    if export
                        .entry(m.name.clone())
                        .or_default()
                        .insert(local.clone())
                    {
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    export
}

pub(crate) fn single_assign_name(targets: &[ast::AssignTarget]) -> Option<String> {
    if targets.len() != 1 {
        return None;
    }
    match &targets[0] {
        ast::AssignTarget::Name { name, .. } => Some(name.clone()),
        _ => None,
    }
}

pub(crate) fn literal_expr_ty(e: &ast::Expr) -> Option<ir::Ty> {
    match &e.kind {
        ast::ExprKind::Int(_) | ast::ExprKind::IntDigits(_) => Some(ir::Ty::Int),
        ast::ExprKind::Float(_) => Some(ir::Ty::Float),
        ast::ExprKind::Bool(_) => Some(ir::Ty::Bool),
        ast::ExprKind::Str(_) => Some(ir::Ty::Str),
        _ => None,
    }
}

pub(crate) fn record_simple_assign(stmt: &ast::Stmt, tys: &mut HashMap<String, ir::Ty>) {
    let ast::StmtKind::Assign {
        targets,
        value,
        annotation,
        ..
    } = &stmt.kind
    else {
        return;
    };
    let Some(name) = single_assign_name(targets) else {
        return;
    };
    if let Some(ann) = annotation {
        if let Ok(ty) = resolve_type_checked(*ann, stmt.span) {
            tys.insert(name, ty);
        }
        return;
    }
    if let Some(ty) = literal_expr_ty(value) {
        tys.insert(name, ty);
    }
}

/// Whether this statement causes `child` (fully-qualified) to be loaded when
/// executed in package `parent`.
pub(crate) fn stmt_loads_child(stmt: &ast::Stmt, parent: &str, child: &str) -> bool {
    match &stmt.kind {
        ast::StmtKind::Import { names } => names.iter().any(|(m, _, _)| {
            m == child || is_strict_package_prefix(child, m) || is_strict_package_prefix(m, child)
        }),
        ast::StmtKind::FromImport {
            module: src,
            names,
            star,
            ..
        } => {
            if src == child || is_strict_package_prefix(child, src) {
                return true;
            }
            // `import *` does not force-load specific submodules.
            if *star {
                return false;
            }
            // `from parent import child_tail` / `from . import mod`
            for (name, _, _) in names {
                let full = if src.is_empty() {
                    name.clone()
                } else {
                    format!("{src}.{name}")
                };
                if full == child || is_strict_package_prefix(child, &full) {
                    return true;
                }
                // short name under parent
                if src == parent {
                    let under = format!("{parent}.{name}");
                    if under == child || is_strict_package_prefix(child, &under) {
                        return true;
                    }
                }
            }
            false
        }
        _ => false,
    }
}

/// For each parent package P and child module C under P: simple assignment
/// types in P **before** the first statement that loads C (CPython partial
/// init: later names are not visible at child **module top level**).
/// Map: parent → child → (name → ty).
pub(crate) fn build_partial_prelim(
    modules: &[ModuleInput],
) -> HashMap<String, HashMap<String, HashMap<String, ir::Ty>>> {
    let by_name: HashMap<&str, &ast::Module> =
        modules.iter().map(|m| (m.name.as_str(), m.ast)).collect();
    let mut out: HashMap<String, HashMap<String, HashMap<String, ir::Ty>>> = HashMap::new();
    for m in modules {
        // Walk each ancestor package
        let parts: Vec<&str> = m.name.split('.').collect();
        for i in 1..parts.len() {
            let parent_name = parts[..i].join(".");
            let Some(parent_ast) = by_name.get(parent_name.as_str()) else {
                continue;
            };
            let mut tys = HashMap::new();
            for stmt in &parent_ast.body {
                if stmt_loads_child(stmt, &parent_name, &m.name) {
                    break;
                }
                record_simple_assign(stmt, &mut tys);
            }
            out.entry(parent_name)
                .or_default()
                .insert(m.name.clone(), tys);
        }
    }
    out
}

/// Func names defined in parent **before** the import that loads each child
/// (visible for mid-init module-level calls, like CPython).
pub(crate) fn build_partial_funcs(
    modules: &[ModuleInput],
) -> HashMap<String, HashMap<String, HashSet<String>>> {
    let by_name: HashMap<&str, &ast::Module> =
        modules.iter().map(|m| (m.name.as_str(), m.ast)).collect();
    let mut out: HashMap<String, HashMap<String, HashSet<String>>> = HashMap::new();
    for m in modules {
        let parts: Vec<&str> = m.name.split('.').collect();
        for i in 1..parts.len() {
            let parent_name = parts[..i].join(".");
            let Some(parent_ast) = by_name.get(parent_name.as_str()) else {
                continue;
            };
            let mut names = HashSet::new();
            for stmt in &parent_ast.body {
                if stmt_loads_child(stmt, &parent_name, &m.name) {
                    break;
                }
                if let ast::StmtKind::FuncDef(f) = &stmt.kind {
                    names.insert(f.name.clone());
                }
            }
            out.entry(parent_name)
                .or_default()
                .insert(m.name.clone(), names);
        }
    }
    out
}

/// All simple assignments in each module body (full package surface for deferred
/// parent attribute loads inside child **function** bodies after parent finishes).
pub(crate) fn build_package_final_values(
    modules: &[ModuleInput],
) -> HashMap<String, HashMap<String, ir::Ty>> {
    let mut out = HashMap::new();
    for m in modules {
        let mut tys = HashMap::new();
        for stmt in &m.ast.body {
            record_simple_assign(stmt, &mut tys);
        }
        out.insert(m.name.clone(), tys);
    }
    out
}

/// Names bound by `from … import` on parent **before** each child is loaded
/// (re-exports visible mid-init via hasattr).
pub(crate) fn build_partial_reexports(
    modules: &[ModuleInput],
) -> HashMap<String, HashMap<String, HashSet<String>>> {
    let by_name: HashMap<&str, &ast::Module> =
        modules.iter().map(|m| (m.name.as_str(), m.ast)).collect();
    let mut out: HashMap<String, HashMap<String, HashSet<String>>> = HashMap::new();
    for m in modules {
        let parts: Vec<&str> = m.name.split('.').collect();
        for i in 1..parts.len() {
            let parent_name = parts[..i].join(".");
            let Some(parent_ast) = by_name.get(parent_name.as_str()) else {
                continue;
            };
            let mut names = HashSet::new();
            for stmt in &parent_ast.body {
                if stmt_loads_child(stmt, &parent_name, &m.name) {
                    break;
                }
                if let ast::StmtKind::FromImport {
                    module: src,
                    names: imported,
                    star,
                    span,
                    ..
                } = &stmt.kind
                {
                    // Partial reexport scan has no full export surface; for star,
                    // use static __all__ or public names already scanned.
                    let eff = if *star {
                        match by_name.get(src.as_str()).copied() {
                            Some(src_ast) => match static_dunder_all(src_ast) {
                                Some(Ok(all)) => all
                                    .into_iter()
                                    .map(|n| (n, None, *span))
                                    .collect::<Vec<_>>(),
                                _ => Vec::new(),
                            },
                            None => Vec::new(),
                        }
                    } else {
                        imported.clone()
                    };
                    for (name, alias, _) in &eff {
                        let local = alias.as_ref().unwrap_or(name);
                        names.insert(local.clone());
                    }
                }
            }
            out.entry(parent_name)
                .or_default()
                .insert(m.name.clone(), names);
        }
    }
    out
}

/// Local → (origin module, origin name) for Symbol re-exports on each module.
/// Only names whose **last** top-level binding is a `from … import`.
pub(crate) fn build_reexport_origins(
    modules: &[ModuleInput],
    all_imports: &[HashMap<String, ImportBinding>],
    last_exports: &HashMap<String, HashMap<String, LastExport>>,
) -> HashMap<String, HashMap<String, (String, String)>> {
    let mut out: HashMap<String, HashMap<String, (String, String)>> = HashMap::new();
    for (i, m) in modules.iter().enumerate() {
        let mut map = HashMap::new();
        for (local, binding) in &all_imports[i] {
            let ImportBinding::Symbol {
                module: src,
                name: src_name,
            } = binding
            else {
                continue;
            };
            if matches!(
                last_exports.get(&m.name).and_then(|e| e.get(local)),
                Some(LastExport::Module(_))
            ) {
                continue;
            }
            if !last_binding_is_from_import(m.ast, local) {
                continue;
            }
            map.insert(local.clone(), (src.clone(), src_name.clone()));
        }
        out.insert(m.name.clone(), map);
    }
    out
}

/// Last top-level export kind for each name in each module (source order).
#[derive(Debug, Clone)]
pub(crate) enum LastExport {
    /// Submodule binding (`from . import mod` / package attribute is a module).
    Module(String),
    /// Value or function (assignment, def, or value re-export).
    Symbol,
}

/// Walk each module body in order; last binding of each name wins
/// (Module vs Symbol re-exports).
pub(crate) fn compute_last_exports(
    modules: &[ModuleInput],
    submodules: &HashMap<String, HashMap<String, String>>,
    export_funcs: &HashMap<String, HashMap<String, FuncSig>>,
    export_values: &HashMap<String, std::collections::HashSet<String>>,
) -> HashMap<String, HashMap<String, LastExport>> {
    let asts = module_ast_map(modules);
    // Process in given order (dependencies first) so sources are ready.
    let mut all: HashMap<String, HashMap<String, LastExport>> = HashMap::new();
    for m in modules {
        let mut last: HashMap<String, LastExport> = HashMap::new();
        for stmt in &m.ast.body {
            match &stmt.kind {
                ast::StmtKind::FuncDef(f) => {
                    last.insert(f.name.clone(), LastExport::Symbol);
                }
                ast::StmtKind::Assign { targets, .. } => {
                    let mut names = std::collections::HashSet::new();
                    for t in targets {
                        collect_assign_names(t, &mut names);
                    }
                    for n in names {
                        last.insert(n, LastExport::Symbol);
                    }
                }
                ast::StmtKind::AugAssign {
                    target: ast::AssignTarget::Name { name, .. },
                    ..
                } => {
                    last.insert(name.clone(), LastExport::Symbol);
                }
                ast::StmtKind::FromImport {
                    module: src,
                    names,
                    star,
                    span,
                    ..
                } => {
                    // Public submodule short names available for star expansion.
                    let sub_public: std::collections::HashSet<String> = submodules
                        .get(src.as_str())
                        .map(|s| s.keys().filter(|k| !k.starts_with('_')).cloned().collect())
                        .unwrap_or_default();
                    let src_last_public: std::collections::HashSet<String> = all
                        .get(src.as_str())
                        .map(|e| e.keys().filter(|k| !k.starts_with('_')).cloned().collect())
                        .unwrap_or_default();
                    let mut extra = sub_public;
                    extra.extend(src_last_public);
                    let eff = effective_from_names(
                        src,
                        names,
                        *star,
                        *span,
                        asts.get(src.as_str()).copied(),
                        export_funcs,
                        export_values,
                        Some(&extra),
                    );
                    for (name, alias, _) in &eff {
                        let local = alias.as_ref().unwrap_or(name);
                        let kind = resolve_from_export(
                            src,
                            name,
                            &all,
                            &last,
                            m.name.as_str(),
                            submodules,
                            export_funcs,
                            export_values,
                        );
                        if let Some(k) = kind {
                            last.insert(local.clone(), k);
                        }
                    }
                }
                _ => {}
            }
        }
        all.insert(m.name.clone(), last);
    }
    all
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_from_export(
    src: &str,
    name: &str,
    completed: &HashMap<String, HashMap<String, LastExport>>,
    self_so_far: &HashMap<String, LastExport>,
    self_name: &str,
    submodules: &HashMap<String, HashMap<String, String>>,
    export_funcs: &HashMap<String, HashMap<String, FuncSig>>,
    export_values: &HashMap<String, std::collections::HashSet<String>>,
) -> Option<LastExport> {
    let sub_full = submodules.get(src).and_then(|s| s.get(name)).cloned();
    // What does `src` export under `name`?
    let src_export = if src == self_name {
        self_so_far.get(name).cloned()
    } else {
        completed.get(src).and_then(|e| e.get(name)).cloned()
    };
    match src_export {
        Some(LastExport::Symbol) => Some(LastExport::Symbol),
        Some(LastExport::Module(full)) => Some(LastExport::Module(full)),
        None => {
            // Fall back to structural info when source has no explicit last map yet.
            if let Some(full) = sub_full {
                // Prefer value/func on source over bare submodule if present.
                if export_funcs.get(src).is_some_and(|f| f.contains_key(name))
                    || export_values.get(src).is_some_and(|v| v.contains(name))
                {
                    // Only if those come from assignment/def/reexport on src —
                    // for a pure submodule package, export_values won't have it.
                    // Submodule name alone: Module. If also a value export name
                    // from expand, Symbol wins when it's a real re-export.
                    // expand_export_values skips submodules, so values won't
                    // include pure submodule names. Funcs are own defs.
                    if export_funcs.get(src).is_some_and(|f| f.contains_key(name)) {
                        Some(LastExport::Symbol)
                    } else {
                        Some(LastExport::Module(full))
                    }
                } else {
                    Some(LastExport::Module(full))
                }
            } else if export_funcs.get(src).is_some_and(|f| f.contains_key(name))
                || export_values.get(src).is_some_and(|v| v.contains(name))
            {
                Some(LastExport::Symbol)
            } else {
                None
            }
        }
    }
}

/// Whether the last top-level binding of `local` is a `from … import`
/// (CPython: last binding wins for package exports).
pub(crate) fn last_binding_is_from_import(module: &ast::Module, local: &str) -> bool {
    let mut last_import = false;
    for stmt in &module.body {
        match &stmt.kind {
            ast::StmtKind::Assign { targets, .. } => {
                let mut names = std::collections::HashSet::new();
                for t in targets {
                    collect_assign_names(t, &mut names);
                }
                if names.contains(local) {
                    last_import = false;
                }
            }
            ast::StmtKind::AugAssign {
                target: ast::AssignTarget::Name { name, .. },
                ..
            } if name == local => {
                last_import = false;
            }
            ast::StmtKind::FuncDef(f) if f.name == local => {
                last_import = false;
            }
            ast::StmtKind::FromImport { names, star, .. } => {
                if *star {
                    // `collect_imports` only puts real star-expanded names into
                    // the imports map; callers only query those locals, so a
                    // star statement counts as an import binding of `local`.
                    last_import = true;
                } else {
                    for (name, alias, _) in names {
                        let bound = alias.as_ref().unwrap_or(name);
                        if bound == local {
                            last_import = true;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    last_import
}

/// Build each module's **export** function table: own `def`s plus
/// `from … import` re-exports (fixpoint). Used only for import validation
/// and `ModuleData`; per-module lowering still uses own `def`s only so a
/// re-exported name is not mistaken for a local function IR symbol.
pub(crate) fn expand_export_funcs(
    modules: &[ModuleInput],
    own_funcs: &HashMap<String, HashMap<String, FuncSig>>,
) -> HashMap<String, HashMap<String, FuncSig>> {
    let asts = module_ast_map(modules);
    // Empty values map for star public-name filtering during func expansion.
    let empty_values: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    let mut export = own_funcs.clone();
    loop {
        let mut changed = false;
        for m in modules {
            for stmt in &m.ast.body {
                let ast::StmtKind::FromImport {
                    module: src,
                    names,
                    star,
                    span,
                    ..
                } = &stmt.kind
                else {
                    continue;
                };
                let eff = effective_from_names(
                    src,
                    names,
                    *star,
                    *span,
                    asts.get(src.as_str()).copied(),
                    &export,
                    &empty_values,
                    None,
                );
                for (name, alias, _) in &eff {
                    let local = alias.as_ref().unwrap_or(name);
                    let Some(sig) = export
                        .get(src.as_str())
                        .and_then(|f| f.get(name.as_str()))
                        .cloned()
                    else {
                        continue;
                    };
                    let slot = export.entry(m.name.clone()).or_default();
                    if let std::collections::hash_map::Entry::Vacant(e) = slot.entry(local.clone())
                    {
                        e.insert(sig);
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    export
}

/// Origin `(module, name)` for a symbol exported by `module` under `name`,
/// following re-export aliases recorded on finished modules.
pub(crate) fn origin_of(
    mods: &HashMap<String, ModuleData>,
    module: &str,
    name: &str,
) -> (String, String) {
    let mut m = module.to_string();
    let mut n = name.to_string();
    for _ in 0..32 {
        let Some(data) = mods.get(&m) else {
            break;
        };
        let Some((om, on)) = data.reexports.get(&n) else {
            break;
        };
        m = om.clone();
        n = on.clone();
    }
    (m, n)
}

/// Attach `from … import` re-exports to a finished module. **Last top-level
/// binding wins** (CPython): an assignment/`def` after the import keeps the
/// local binding; an import after an assignment re-exports instead.
pub(crate) fn apply_reexports(
    data: &mut ModuleData,
    own_func_names: &std::collections::HashSet<String>,
    imports: &HashMap<String, ImportBinding>,
    mods: &HashMap<String, ModuleData>,
    module_ast: &ast::Module,
) {
    for (local, binding) in imports {
        let ImportBinding::Symbol {
            module: src,
            name: src_name,
        } = binding
        else {
            continue;
        };
        let import_last = last_binding_is_from_import(module_ast, local);
        // Own assignment/`def` wins only when it is the last binding.
        if !import_last && (data.globals.contains_key(local) || own_func_names.contains(local)) {
            continue;
        }
        let (om, on) = origin_of(mods, src, src_name);
        let Some(src_data) = mods.get(&om) else {
            continue;
        };

        if let Some(ty) = src_data.globals.get(&on) {
            data.funcs.remove(local);
            data.globals.insert(local.clone(), *ty);
            data.reexports
                .insert(local.clone(), (om.clone(), on.clone()));
        } else if let Some(sig) = src_data.funcs.get(&on) {
            data.globals.remove(local);
            data.funcs.insert(local.clone(), sig.clone());
            data.reexports
                .insert(local.clone(), (om.clone(), on.clone()));
        }
    }
}

pub(crate) fn collect_assign_names(
    target: &ast::AssignTarget,
    names: &mut std::collections::HashSet<String>,
) {
    match target {
        ast::AssignTarget::Name { name, .. } => {
            names.insert(name.clone());
        }
        ast::AssignTarget::Index { .. }
        | ast::AssignTarget::Slice { .. }
        | ast::AssignTarget::Attr { .. } => {}
        ast::AssignTarget::Tuple(items) => {
            for t in items {
                collect_assign_names(t, names);
            }
        }
        ast::AssignTarget::Starred { target, .. } => collect_assign_names(target, names),
    }
}

/// Build a module's import bindings (local name → target), validating that
/// imported modules and symbols exist. Uses each source module's **last
/// top-level export** (Module vs Symbol) so re-exports and same-named
/// submodules follow source order.
///
/// CPython `fromlist` short-circuits on `hasattr`: if a package already
/// bound a name as a value/function, a later `from . import same_name` does
/// not replace that binding with a self-ref or submodule. We keep the prior
/// Symbol origin (or skip inserting a self-ref for own assign/`def`).
///
/// Nested imports bind function-locally via [`FnCtx::local_imports`] at lower
/// time; only top-level imports are recorded here.
/// AST of `src` for star-import expansion. When importing from self, use the
/// current module body; otherwise look up the program map.
pub(crate) fn star_src_ast<'a>(
    src: &str,
    self_name: &str,
    self_ast: &'a ast::Module,
    asts: &HashMap<&str, &'a ast::Module>,
) -> Option<&'a ast::Module> {
    if src == self_name {
        Some(self_ast)
    } else {
        asts.get(src).copied()
    }
}

pub(crate) fn collect_imports(
    module: &ast::Module,
    self_name: &str,
    last_exports: &HashMap<String, HashMap<String, LastExport>>,
    export_funcs: &HashMap<String, HashMap<String, FuncSig>>,
    export_values: &HashMap<String, std::collections::HashSet<String>>,
    submodules: &HashMap<String, HashMap<String, String>>,
    asts: &HashMap<&str, &ast::Module>,
) -> SResult<HashMap<String, ImportBinding>> {
    let mut imports: HashMap<String, ImportBinding> = HashMap::new();
    // Names already bound on this module as Symbol exports (assign/def/reexport)
    // while walking in source order — for hasattr short-circuit on self-imports.
    // Only top-level stmts (not nested function/if bodies).
    let mut self_value_bound: HashSet<String> = HashSet::new();
    for stmt in &module.body {
        match &stmt.kind {
            ast::StmtKind::FuncDef(f) => {
                self_value_bound.insert(f.name.clone());
            }
            ast::StmtKind::Assign { targets, .. } => {
                let mut names = HashSet::new();
                for t in targets {
                    collect_assign_names(t, &mut names);
                }
                self_value_bound.extend(names);
            }
            ast::StmtKind::AugAssign {
                target: ast::AssignTarget::Name { name, .. },
                ..
            } => {
                self_value_bound.insert(name.clone());
            }
            ast::StmtKind::Import { names } => {
                for (m, alias, span) in names {
                    // `import typing` binds nothing usable: annotations name
                    // the types directly, and there is no runtime module.
                    if is_typing_module(m) {
                        continue;
                    }
                    let local = import_bind_name(m, alias);
                    let binding = if m == "sys" {
                        ImportBinding::Sys
                    } else {
                        if m == self_name {
                            return Err(err(format!("module '{m}' cannot import itself"), *span));
                        }
                        ImportBinding::Module(import_bound_module(m, alias))
                    };
                    imports.insert(local.clone(), binding);
                    self_value_bound.insert(local);
                }
            }
            ast::StmtKind::FromImport {
                module: m,
                names,
                star,
                span,
                ..
            } => {
                if m == FUTURE_MODULE {
                    check_future_import(names, *star, *span)?;
                    continue;
                }
                // Annotation-only: the names are type spellings the parser
                // already recognises, so the import binds nothing at run time.
                if is_typing_module(m) {
                    if *star {
                        return Err(err(
                            format!(
                                "'from {m} import *' is not supported; import the names you use"
                            ),
                            *span,
                        ));
                    }
                    continue;
                }
                if m == "sys" {
                    return Err(err(
                        if *star {
                            "'from sys import *' is not supported; use 'import sys' \
                             and 'sys.argv'"
                        } else {
                            "'from sys import ...' is not supported; use 'import sys' \
                             and 'sys.argv'"
                        },
                        *span,
                    ));
                }
                if *star {
                    // Validate static __all__ / expand public names.
                    if let Some(src_ast) = star_src_ast(m, self_name, module, asts)
                        && let Some(Err(())) = static_dunder_all(src_ast)
                    {
                        return Err(err(
                            format!(
                                "module '{m}' has a non-static __all__; \
                                 star import requires a list or tuple of string literals"
                            ),
                            *span,
                        ));
                    }
                }
                let sub_public: std::collections::HashSet<String> = submodules
                    .get(m.as_str())
                    .map(|s| s.keys().filter(|k| !k.starts_with('_')).cloned().collect())
                    .unwrap_or_default();
                let last_public: std::collections::HashSet<String> = last_exports
                    .get(m.as_str())
                    .map(|e| e.keys().filter(|k| !k.starts_with('_')).cloned().collect())
                    .unwrap_or_default();
                let mut extra = sub_public;
                extra.extend(last_public);
                let src_ast = star_src_ast(m, self_name, module, asts);
                let eff = effective_from_names(
                    m,
                    names,
                    *star,
                    *span,
                    src_ast,
                    export_funcs,
                    export_values,
                    Some(&extra),
                );
                if *star && eff.is_empty() {
                    // Empty __all__ or empty public surface is fine.
                }
                for (name, alias, nspan) in &eff {
                    let local = alias.clone().unwrap_or_else(|| name.clone());
                    let export = last_exports.get(m).and_then(|e| e.get(name)).cloned();
                    let sub_full = submodules.get(m).and_then(|s| s.get(name)).cloned();
                    let is_func = export_funcs.get(m).is_some_and(|f| f.contains_key(name));
                    let is_value = export_values.get(m).is_some_and(|g| g.contains(name));

                    // `from <self> import name` when name is already a value/func
                    // on self: CPython hasattr short-circuit — keep prior origin.
                    if m == self_name
                        && (self_value_bound.contains(name)
                            || matches!(imports.get(&local), Some(ImportBinding::Symbol { .. })))
                    {
                        if !imports.contains_key(&local) {
                            // Own assign/def only — no import binding needed.
                        }
                        self_value_bound.insert(local);
                        continue;
                    }

                    match export {
                        Some(LastExport::Module(full)) => {
                            imports.insert(local.clone(), ImportBinding::Module(full));
                        }
                        Some(LastExport::Symbol) => {
                            if !is_func && !is_value && sub_full.is_none() {
                                return Err(err(
                                    format!("cannot import name '{name}' from '{m}'"),
                                    *nspan,
                                ));
                            }
                            let binding = if let Some(id) = lookup_class_in_module(m, name) {
                                ImportBinding::Class(id)
                            } else {
                                ImportBinding::Symbol {
                                    module: m.clone(),
                                    name: name.clone(),
                                }
                            };
                            imports.insert(local.clone(), binding);
                            if m == self_name || is_func || is_value {
                                self_value_bound.insert(local);
                            }
                        }
                        None => {
                            if let Some(full) = sub_full {
                                imports.insert(local.clone(), ImportBinding::Module(full));
                            } else if is_func || is_value {
                                let binding = if let Some(id) = lookup_class_in_module(m, name) {
                                    ImportBinding::Class(id)
                                } else {
                                    ImportBinding::Symbol {
                                        module: m.clone(),
                                        name: name.clone(),
                                    }
                                };
                                imports.insert(local.clone(), binding);
                                self_value_bound.insert(local);
                            } else {
                                return Err(err(
                                    format!("cannot import name '{name}' from '{m}'"),
                                    *nspan,
                                ));
                            }
                        }
                    }
                }
            }
            // Module-level control flow: imports here are still module globals
            // (CPython). Do not descend into function bodies.
            ast::StmtKind::If { branches, orelse } => {
                for (_, b) in branches {
                    let nested = collect_imports_block(
                        b,
                        self_name,
                        last_exports,
                        export_funcs,
                        export_values,
                        submodules,
                        &self_value_bound,
                        asts,
                    )?;
                    imports.extend(nested);
                }
                let nested = collect_imports_block(
                    orelse,
                    self_name,
                    last_exports,
                    export_funcs,
                    export_values,
                    submodules,
                    &self_value_bound,
                    asts,
                )?;
                imports.extend(nested);
            }
            ast::StmtKind::While { body, orelse, .. } | ast::StmtKind::For { body, orelse, .. } => {
                let nested = collect_imports_block(
                    body,
                    self_name,
                    last_exports,
                    export_funcs,
                    export_values,
                    submodules,
                    &self_value_bound,
                    asts,
                )?;
                imports.extend(nested);
                let nested = collect_imports_block(
                    orelse,
                    self_name,
                    last_exports,
                    export_funcs,
                    export_values,
                    submodules,
                    &self_value_bound,
                    asts,
                )?;
                imports.extend(nested);
            }
            ast::StmtKind::Try {
                body,
                handlers,
                orelse,
                finally,
            } => {
                for block in std::iter::once(body)
                    .chain(handlers.iter().map(|h| &h.body))
                    .chain(std::iter::once(orelse))
                    .chain(std::iter::once(finally))
                {
                    let nested = collect_imports_block(
                        block,
                        self_name,
                        last_exports,
                        export_funcs,
                        export_values,
                        submodules,
                        &self_value_bound,
                        asts,
                    )?;
                    imports.extend(nested);
                }
            }
            ast::StmtKind::With { body, .. } => {
                let nested = collect_imports_block(
                    body,
                    self_name,
                    last_exports,
                    export_funcs,
                    export_values,
                    submodules,
                    &self_value_bound,
                    asts,
                )?;
                imports.extend(nested);
            }
            ast::StmtKind::Match { cases, .. } => {
                for c in cases {
                    let nested = collect_imports_block(
                        &c.body,
                        self_name,
                        last_exports,
                        export_funcs,
                        export_values,
                        submodules,
                        &self_value_bound,
                        asts,
                    )?;
                    imports.extend(nested);
                }
            }
            _ => {}
        }
    }
    Ok(imports)
}

/// Collect imports from a block without entering nested function defs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_imports_block(
    stmts: &[ast::Stmt],
    self_name: &str,
    last_exports: &HashMap<String, HashMap<String, LastExport>>,
    export_funcs: &HashMap<String, HashMap<String, FuncSig>>,
    export_values: &HashMap<String, std::collections::HashSet<String>>,
    submodules: &HashMap<String, HashMap<String, String>>,
    self_value_bound: &HashSet<String>,
    asts: &HashMap<&str, &ast::Module>,
) -> SResult<HashMap<String, ImportBinding>> {
    // Reuse main collector by building a temporary module body that skips FuncDefs.
    let filtered: Vec<ast::Stmt> = stmts
        .iter()
        .filter(|s| !matches!(s.kind, ast::StmtKind::FuncDef(_)))
        .cloned()
        .collect();
    let m = ast::Module { body: filtered };
    // self_value_bound is only used for short-circuit; pass a synthetic module
    // walk. For simplicity, call collect_imports which re-walks (including
    // nested ifs) — FuncDefs already filtered out.
    let _ = self_value_bound;
    collect_imports(
        &m,
        self_name,
        last_exports,
        export_funcs,
        export_values,
        submodules,
        asts,
    )
}
