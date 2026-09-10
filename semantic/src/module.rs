//! Per-module analysis context, import bindings, and module path helpers.

use std::collections::{HashMap, HashSet};

use parser::ast;

use crate::prelude::*;

/// One parsed module handed to [`analyze_program`]. The driver supplies
/// these in topological order (dependencies first) with the root last;
/// the index doubles as the diagnostic file id.
pub struct ModuleInput<'a> {
    /// Import name: `"utils"` for `utils.py`, [`ENTRY_NAME`] for the root.
    pub name: String,
    pub ast: &'a ast::Module,
}

/// How a locally-visible imported name resolves.
#[derive(Debug, Clone)]
pub(crate) enum ImportBinding {
    /// `import sys [as s]`
    Sys,
    /// `import M [as m]` — the local name refers to module `M`.
    Module(String),
    /// `from M import x [as y]` — the local name refers to `M.x`.
    Symbol { module: String, name: String },
    /// `from M import Class` — user class type/constructor.
    Class(ir::ClassId),
}

/// A fully analyzed module's exported surface, for cross-module lookup.
pub(crate) struct ModuleData {
    pub(crate) funcs: HashMap<String, FuncSig>,
    pub(crate) globals: HashMap<String, ir::Ty>,
    /// User classes defined in this module (short name → id).
    pub(crate) classes: HashMap<String, ir::ClassId>,
    /// Names bound by `from … import` into this module: local → (origin module,
    /// origin name). Attribute loads and calls use the origin IR symbol.
    /// Local assignments in this module win over re-exports of the same name.
    pub(crate) reexports: HashMap<String, (String, String)>,
}

/// Read-only per-module context threaded into every function lowering.
pub(crate) struct ModuleCtx<'a> {
    /// The module's own name (`""`-prefixed for the root, `"M."` otherwise
    /// via [`ModuleCtx::prefix`]).
    pub(crate) module: &'a str,
    pub(crate) is_root: bool,
    pub(crate) funcs: &'a HashMap<String, FuncSig>,
    pub(crate) imports: &'a HashMap<String, ImportBinding>,
    /// Fully analyzed dependency modules, keyed by name.
    pub(crate) mods: &'a HashMap<String, ModuleData>,
    /// Parent module → (child short name → fully-qualified child name) for
    /// every module in the program (built before lowering).
    pub(crate) submodules: &'a HashMap<String, HashMap<String, String>>,
    /// Partial package init: parent → child → (name → ty) for simple
    /// assignments in the parent **before** the import that loads the child.
    pub(crate) partial_prelim: &'a HashMap<String, HashMap<String, HashMap<String, ir::Ty>>>,
    /// Func names defined in parent **before** the import that loads the child
    /// (parent → child → names). Visible at child module top level mid-init.
    pub(crate) partial_funcs: &'a HashMap<String, HashMap<String, HashSet<String>>>,
    /// Full simple-assign surface of each package (entire body). Used for
    /// **deferred** parent attribute loads inside child function bodies.
    pub(crate) package_final_values: &'a HashMap<String, HashMap<String, ir::Ty>>,
    /// Own function tables (all modules). Deferred parent calls use these
    /// when the parent is not yet in `mods`.
    pub(crate) all_own_funcs: &'a HashMap<String, HashMap<String, FuncSig>>,
    /// Last top-level export kind per module (Module vs Symbol).
    pub(crate) last_exports: &'a HashMap<String, HashMap<String, LastExport>>,
    /// `from … import` value/function re-export origins per module:
    /// local name → (origin module, origin name). Used for deferred parent
    /// access while the parent package is not yet in `mods`.
    pub(crate) reexport_origins: &'a HashMap<String, HashMap<String, (String, String)>>,
    /// Re-export names bound on parent before each child load (mid-init hasattr).
    pub(crate) partial_reexports: &'a HashMap<String, HashMap<String, HashSet<String>>>,
}

impl ModuleCtx<'_> {
    /// The IR name prefix for this module's own symbols. The root keeps
    /// bare names (`x`, `foo`); other modules are namespaced (`utils.x`).
    pub(crate) fn prefix(&self) -> String {
        if self.is_root {
            String::new()
        } else {
            format!("{}.", self.module)
        }
    }

    /// Names visible on `parent` while lowering `self.module` under partial init
    /// (child **module body** only — names assigned before the child-loading import).
    pub(crate) fn partial_parent_globals(&self, parent: &str) -> Option<&HashMap<String, ir::Ty>> {
        self.partial_prelim
            .get(parent)
            .and_then(|by_child| by_child.get(self.module))
    }

    /// Parent function names defined before this child was loaded (mid-init module body).
    pub(crate) fn partial_parent_funcs(&self, parent: &str) -> Option<&HashSet<String>> {
        self.partial_funcs
            .get(parent)
            .and_then(|by_child| by_child.get(self.module))
    }

    /// Parent re-export names bound before this child was loaded.
    pub(crate) fn partial_parent_reexports(&self, parent: &str) -> Option<&HashSet<String>> {
        self.partial_reexports
            .get(parent)
            .and_then(|by_child| by_child.get(self.module))
    }
}

/// The IR/emit name of `name` defined in module `module` (always
/// namespaced — only the root is bare, handled by the caller).
pub(crate) fn qual(module: &str, name: &str) -> String {
    format!("{module}.{name}")
}

/// The builtins that cannot be shadowed by a user `def`.
pub(crate) const BUILTINS: [&str; 28] = [
    "print",
    "len",
    "range",
    "input",
    "open",
    "abs",
    "min",
    "max",
    "sum",
    "sorted",
    "set",
    "isinstance",
    "any",
    "all",
    "enumerate",
    "zip",
    "reversed",
    "next",
    "round",
    "ord",
    "chr",
    "hex",
    "bin",
    "oct",
    "divmod",
    "pow",
    "repr",
    "ascii",
];

/// A call to a module's run-once init function, `<mod>.__init__()`.
pub(crate) fn init_call(module: &str) -> ir::Stmt {
    ir::Stmt::ExprStmt(ir::Expr {
        ty: ir::Ty::None,
        kind: ir::ExprKind::Call {
            func: qual(module, "__init__"),
            args: vec![],
        },
    })
}

/// Init calls for `module` and every parent package (`pkg` then `pkg.mod`).
pub(crate) fn init_calls_for(module: &str) -> Vec<ir::Stmt> {
    let parts: Vec<&str> = module.split('.').collect();
    let mut out = Vec::with_capacity(parts.len());
    for i in 1..=parts.len() {
        out.push(init_call(&parts[..i].join(".")));
    }
    out
}

/// Top-level name bound by `import a.b.c` (without `as`): `a`.
pub(crate) fn import_bind_name(module: &str, alias: &Option<String>) -> String {
    alias
        .clone()
        .unwrap_or_else(|| module.split('.').next().unwrap_or(module).to_string())
}

/// Module object referred to by `import a.b.c` / `import a.b.c as x`.
/// Without alias, the local name is the top-level package `a`.
pub(crate) fn import_bound_module(module: &str, alias: &Option<String>) -> String {
    if alias.is_some() {
        module.to_string()
    } else {
        module.split('.').next().unwrap_or(module).to_string()
    }
}

/// Build parent → child short name → full name for all modules in the program.
pub(crate) fn build_submodule_map(
    module_names: &[String],
) -> HashMap<String, HashMap<String, String>> {
    let mut map: HashMap<String, HashMap<String, String>> = HashMap::new();
    for name in module_names {
        if let Some((parent, child)) = name.rsplit_once('.') {
            map.entry(parent.to_string())
                .or_default()
                .insert(child.to_string(), name.clone());
        }
    }
    map
}

/// True if `parent` is a dotted package prefix of `child` (`pkg` of `pkg.mod`).
pub(crate) fn is_strict_package_prefix(parent: &str, child: &str) -> bool {
    child.len() > parent.len()
        && child.as_bytes().get(parent.len()) == Some(&b'.')
        && child.starts_with(parent)
}

/// If `expr` is a chain of attributes rooted at an imported module name,
/// resolve it to the fully-qualified module name (`pkg.mod`).
///
/// Does **not** step into a name that the parent last-bound as a value or
/// function re-export (so `pkg.mod` stays a value when `__init__` re-exported
/// `mod` over the submodule).
pub(crate) fn resolve_module_path(expr: &ast::Expr, ctx: &FnCtx) -> Option<String> {
    match &expr.kind {
        ast::ExprKind::Name(n) => ctx.module_alias(n),
        ast::ExprKind::Attribute { base, attr, .. } => {
            let parent = resolve_module_path(base, ctx)?;
            if let Some(data) = ctx.mctx.mods.get(&parent) {
                // Value or function export wins over the submodule of the same name.
                if data.globals.contains_key(attr) || data.funcs.contains_key(attr) {
                    return None;
                }
            }
            ctx.mctx
                .submodules
                .get(&parent)
                .and_then(|kids| kids.get(attr))
                .cloned()
        }
        _ => None,
    }
}
