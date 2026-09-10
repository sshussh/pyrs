//! Function-lowering context (`FnCtx`).

use std::collections::{HashMap, HashSet};

use crate::prelude::*;

pub(crate) struct FnCtx<'a> {
    pub(crate) mctx: &'a ModuleCtx<'a>,
    pub(crate) globals: &'a mut HashMap<String, ir::Ty>,
    pub(crate) globals_order: &'a mut Vec<(String, ir::Ty)>,
    /// This function is a module init: its top-level bindings are globals.
    pub(crate) is_entry: bool,
    /// Unused (imports always allowed); kept for call-site compatibility.
    #[allow(dead_code)]
    pub(crate) allow_import: bool,
    /// Names this function declared with `global`.
    pub(crate) declared_globals: std::collections::HashSet<String>,
    pub(crate) fn_name: String,
    pub(crate) ret: ir::Ty,
    pub(crate) locals: HashMap<String, ir::Ty>,
    pub(crate) locals_order: Vec<(String, ir::Ty)>,
    pub(crate) loop_depth: usize,
    pub(crate) temp_counter: usize,
    /// Active comprehension variables: user name → (storage local, type).
    /// Innermost last. Comprehension variables shadow but never leak
    /// (Python 3 scoping).
    pub(crate) comp_renames: Vec<(String, String, ir::Ty)>,
    /// Nested functions defined in this function (name → info).
    pub(crate) nested_funcs: HashMap<String, NestedFnInfo>,
    /// IR functions produced for nested defs (and their nested defs).
    pub(crate) nested_ir: Vec<ir::Function>,
    /// Names declared `nonlocal` in this function.
    pub(crate) declared_nonlocals: HashSet<String>,
    /// Names that are cell-backed (mutable free vars / nonlocal).
    pub(crate) cell_locals: HashMap<String, ir::Ty>,
    /// When inside a generator function, the yield element type.
    pub(crate) yield_ty: Option<ir::Ty>,
    /// Refinements: name → narrowed type (stacked for control flow).
    pub(crate) type_refinements: HashMap<String, ir::Ty>,
    /// Cell boxing inits deferred from nested-def analysis (flushed after def).
    pub(crate) pending_cell_inits: Vec<ir::Stmt>,
    /// Nesting depth of try/except/finally.
    pub(crate) try_depth: usize,
    /// Nesting depth of `except` handler bodies, so a bare `raise` can be
    /// rejected where there is nothing to re-raise.
    pub(crate) handler_depth: usize,
    /// Function-local import bindings (CPython: import in function is local).
    pub(crate) local_imports: HashMap<String, ImportBinding>,
    /// Names that some nested function declares `nonlocal` (pre-scanned).
    pub(crate) sibling_nonlocal_names: HashSet<String>,
    /// Outer locals free-captured by any nested def/lambda in this function.
    /// Promoted to cells on first bind so CellNew runs even if the nested
    /// def sits in a branch that is not taken at runtime.
    pub(crate) cell_candidates: HashSet<String>,
    /// Inferred types for names assigned in this function body (for late
    /// free-var binding: nested def before the assign that fills the cell).
    pub(crate) late_bind_tys: HashMap<String, ir::Ty>,
    /// Full outer function body (for late-bind type scan); empty at module entry.
    #[allow(dead_code)]
    pub(crate) outer_body_assigned: HashSet<String>,
    /// Nested defs whose body has been fully lowered (vs provisional sig only).
    pub(crate) lowered_nested: HashSet<String>,
    /// Joined storage types for locals (and module globals when entry) from a
    /// pre-pass over all assignments. Consulted by `bind_name` so multi-assign
    /// uses a union/promoted type without pre-allocating locals (which would
    /// break `global` ordering and late free-cell unbound traps).
    pub(crate) storage_tys: HashMap<String, ir::Ty>,
    /// When lowering an instance method, the class that owns this method.
    /// Used by zero-arg `super().m(...)`.
    pub(crate) current_class: Option<ir::ClassId>,
    /// Name of the `self` parameter of the current instance method (if any).
    pub(crate) self_param: Option<String>,
    /// When lowering a `@classmethod`, `(cls_param_name, class_id)` so
    /// `cls(...)` constructs that class.
    pub(crate) classmethod_cls: Option<(String, ir::ClassId)>,
}

impl FnCtx<'_> {
    /// A compiler-synthesized local. The leading '.' keeps it out of the
    /// user namespace (Python identifiers cannot start with '.').
    pub(crate) fn fresh_temp(&mut self, hint: &str, ty: ir::Ty) -> String {
        self.temp_counter += 1;
        let name = format!(".{hint}{}", self.temp_counter);
        self.locals.insert(name.clone(), ty);
        self.locals_order.push((name.clone(), ty));
        name
    }

    /// Does an assignment to `name` target a module global here?
    pub(crate) fn binds_global(&self, name: &str) -> bool {
        self.is_entry || self.declared_globals.contains(name)
    }

    /// This module's functions.
    pub(crate) fn funcs(&self) -> &HashMap<String, FuncSig> {
        self.mctx.funcs
    }

    /// The IR/emit name for one of *this* module's own globals.
    pub(crate) fn own_global(&self, name: &str) -> String {
        format!("{}{}", self.mctx.prefix(), name)
    }

    /// The IR/emit name for one of *this* module's own functions.
    pub(crate) fn own_func(&self, name: &str) -> String {
        format!("{}{}", self.mctx.prefix(), name)
    }

    /// Is `sys` imported (under any alias)?
    pub(crate) fn sys_alias(&self, name: &str) -> bool {
        matches!(
            self.local_imports
                .get(name)
                .or_else(|| self.mctx.imports.get(name)),
            Some(ImportBinding::Sys)
        )
    }

    /// If `name` is an imported module alias, its real module name.
    pub(crate) fn module_alias(&self, name: &str) -> Option<String> {
        match self
            .local_imports
            .get(name)
            .or_else(|| self.mctx.imports.get(name))
        {
            Some(ImportBinding::Module(real)) => Some(real.clone()),
            _ => None,
        }
    }
}
