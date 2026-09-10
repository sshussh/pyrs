//! Class and exception environments, method resolution, and class constants.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use common::Span;
use parser::ast;

use crate::prelude::*;

// Defaults for nested functions / lambdas keyed by fully-qualified IR name.
// Populated when nested defs are lowered; used by CallClosure after escape.
pub(crate) type ClosureDefaultEntry = (ir::Ty, Option<ast::Expr>);
pub(crate) type ClosureDefaultsMap = HashMap<String, Vec<ClosureDefaultEntry>>;
thread_local! {
    pub(crate) static CLOSURE_DEFAULTS: RefCell<ClosureDefaultsMap> = RefCell::new(HashMap::new());
}

/// How a class method is bound (decorators).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum MethodKind {
    #[default]
    Instance,
    Static,
    Class,
    /// Read-only `@property` — attribute load calls the zero-arg method.
    Property,
}

/// Classes registered for the program currently being analyzed.
/// Keys are always `(module, short_name)` — never last-wins bare short names.
#[derive(Default, Clone)]
pub(crate) struct ClassEnv {
    pub(crate) infos: Vec<ir::ClassInfo>,
    /// `(module_name, ClassName)` → ClassId. Root uses [`ENTRY_NAME`].
    pub(crate) by_key: HashMap<(String, String), ir::ClassId>,
    /// Declaring module for each class id.
    pub(crate) module_of: HashMap<ir::ClassId, String>,
    /// Module currently being lowered/typed (bare `Class` lookups).
    pub(crate) current_module: String,
    /// All method signatures by fully-qualified IR name (cross-module).
    pub(crate) method_sigs: HashMap<String, FuncSig>,
    /// IR method name → kind (static / class / property / instance).
    pub(crate) method_kinds: HashMap<String, MethodKind>,
    /// `(class_id, attr)` → IR func for `@property` getters.
    pub(crate) properties: HashMap<(ir::ClassId, String), String>,
}

thread_local! {
    pub(crate) static CLASS_ENV: RefCell<ClassEnv> = RefCell::new(ClassEnv::default());
}

/// User-defined exception classes. Kept apart from [`ClassEnv`] because they
/// are not instantiable objects here: they carry a runtime tag and a parent,
/// which is all `raise` and `except` need, and they never get an instance
/// layout or a vtable.
#[derive(Default)]
pub(crate) struct ExcEnv {
    /// In tag order from [`ir::USER_EXC_BASE`].
    pub(crate) classes: Vec<ir::ExcClass>,
    /// `(module_name, ClassName)` → runtime tag.
    pub(crate) by_key: HashMap<(String, String), u32>,
}

thread_local! {
    pub(crate) static EXC_ENV: RefCell<ExcEnv> = RefCell::new(ExcEnv::default());
}

thread_local! {
    /// Class-body constants: `(module, Class, NAME)` -> literal value.
    ///
    /// Only literals are accepted, and the value is substituted at every use
    /// rather than stored. That is exact for an immutable constant, needs no
    /// storage and no initialisation ordering, and is why assigning to one is
    /// rejected: there is nothing to assign to.
    pub(crate) static CLASS_CONSTS: RefCell<HashMap<(String, String, String), ir::Expr>> =
        RefCell::new(HashMap::new());
}

pub(crate) fn set_class_const(module: &str, class: &str, name: &str, value: ir::Expr) {
    CLASS_CONSTS.with(|m| {
        m.borrow_mut().insert(
            (module.to_string(), class.to_string(), name.to_string()),
            value,
        )
    });
}

/// A class constant, searching the class and then its bases.
pub(crate) fn class_const(class_id: ir::ClassId, name: &str) -> Option<ir::Expr> {
    let mut cur = Some(class_id);
    while let Some(id) = cur {
        let info = class_info(id)?;
        let module = with_class_env(|e| e.module_of.get(&id).cloned())?;
        // `info.name` may be qualified (`mod.Class`); constants are keyed by
        // the bare name the class body declared.
        let bare = info
            .name
            .rsplit('.')
            .next()
            .unwrap_or(&info.name)
            .to_string();
        if let Some(v) =
            CLASS_CONSTS.with(|m| m.borrow().get(&(module, bare, name.to_string())).cloned())
        {
            return Some(v);
        }
        cur = info.parent;
    }
    Option::None
}

pub(crate) fn clear_class_consts() {
    CLASS_CONSTS.with(|m| m.borrow_mut().clear());
}

pub(crate) fn clear_exc_env() {
    EXC_ENV.with(|e| *e.borrow_mut() = ExcEnv::default());
}

pub(crate) fn with_exc_env<R>(f: impl FnOnce(&ExcEnv) -> R) -> R {
    EXC_ENV.with(|e| f(&e.borrow()))
}

/// Resolve an exception class name to its tag: this module first, then a
/// unique match anywhere (the same order `lookup_class` uses).
pub(crate) fn lookup_exc_class(module: &str, name: &str) -> Option<u32> {
    with_exc_env(|e| {
        if let Some(tag) = e.by_key.get(&(module.to_string(), name.to_string())) {
            return Some(*tag);
        }
        let hits: Vec<u32> = e
            .by_key
            .iter()
            .filter(|((_, n), _)| n == name)
            .map(|(_, t)| *t)
            .collect();
        if hits.len() == 1 {
            Some(hits[0])
        } else {
            Option::None
        }
    })
}

/// Is this a user exception class name visible from `module`?
pub(crate) fn is_exc_class_name(module: &str, name: &str) -> bool {
    lookup_exc_class(module, name).is_some()
}

pub(crate) fn clear_class_env() {
    CLASS_ENV.with(|e| *e.borrow_mut() = ClassEnv::default());
}

pub(crate) fn with_class_env<R>(f: impl FnOnce(&ClassEnv) -> R) -> R {
    CLASS_ENV.with(|e| f(&e.borrow()))
}

pub(crate) fn with_class_env_mut<R>(f: impl FnOnce(&mut ClassEnv) -> R) -> R {
    CLASS_ENV.with(|e| f(&mut e.borrow_mut()))
}

pub(crate) fn set_class_current_module(module: &str) {
    with_class_env_mut(|e| e.current_module = module.to_string());
}

/// Look up a class by bare name in `current_module`, or by `mod.Class` qualified form.
pub(crate) fn lookup_class(name: &str) -> Option<ir::ClassId> {
    with_class_env(|e| {
        if let Some((mod_part, cls)) = name.rsplit_once('.') {
            // Prefer exact (module, class); also try full dotted module path.
            if let Some(id) = e.by_key.get(&(mod_part.to_string(), cls.to_string())) {
                return Some(*id);
            }
        }
        // Bare name in the module currently under analysis.
        if let Some(id) = e.by_key.get(&(e.current_module.clone(), name.to_string())) {
            return Some(*id);
        }
        // Root module convenience: bare names when analyzing non-root? only current.
        None
    })
}

/// Look up class defined in a specific module (import / attribute resolution).
pub(crate) fn lookup_class_in_module(module: &str, name: &str) -> Option<ir::ClassId> {
    with_class_env(|e| {
        e.by_key
            .get(&(module.to_string(), name.to_string()))
            .copied()
    })
}

pub(crate) fn class_info(id: ir::ClassId) -> Option<ir::ClassInfo> {
    with_class_env(|e| e.infos.get(id as usize).cloned())
}

pub(crate) fn class_display_name(id: ir::ClassId) -> String {
    class_info(id)
        .map(|c| c.name)
        .unwrap_or_else(|| format!("class#{id}"))
}

pub(crate) fn method_sig_lookup(ir_name: &str) -> Option<FuncSig> {
    with_class_env(|e| e.method_sigs.get(ir_name).cloned())
}

pub(crate) fn register_method_sig(ir_name: &str, sig: FuncSig) {
    with_class_env_mut(|e| {
        e.method_sigs.insert(ir_name.to_string(), sig);
    });
}

pub(crate) fn register_method_kind(ir_name: &str, kind: MethodKind) {
    with_class_env_mut(|e| {
        e.method_kinds.insert(ir_name.to_string(), kind);
    });
}

pub(crate) fn method_kind_lookup(ir_name: &str) -> MethodKind {
    with_class_env(|e| {
        e.method_kinds
            .get(ir_name)
            .copied()
            .unwrap_or(MethodKind::Instance)
    })
}

pub(crate) fn register_property(class_id: ir::ClassId, name: &str, ir_name: &str) {
    with_class_env_mut(|e| {
        e.properties
            .insert((class_id, name.to_string()), ir_name.to_string());
    });
}

pub(crate) fn resolve_property(class_id: ir::ClassId, name: &str) -> Option<String> {
    with_class_env(|e| {
        let mut cur = Some(class_id);
        while let Some(id) = cur {
            if let Some(func) = e.properties.get(&(id, name.to_string())) {
                return Some(func.clone());
            }
            let info = e.infos.get(id as usize)?;
            cur = info.parent;
        }
        None
    })
}

pub(crate) fn method_kind_from_decorators(
    decorators: &[ast::Decorator],
    span: Span,
) -> SResult<MethodKind> {
    if decorators.is_empty() {
        return Ok(MethodKind::Instance);
    }
    if decorators.len() > 1 {
        return Err(err(
            "stacked method decorators are not supported yet",
            decorators[1].span,
        ));
    }
    match decorators[0].name.as_str() {
        "staticmethod" => Ok(MethodKind::Static),
        "classmethod" => Ok(MethodKind::Class),
        "property" => Ok(MethodKind::Property),
        other => Err(err(
            format!(
                "method decorator '@{other}' is not supported yet \
                 (supported: @staticmethod, @classmethod, @property)"
            ),
            decorators[0].span.to(span),
        )),
    }
}

pub(crate) fn class_is_subclass_in(
    env: &ClassEnv,
    child: ir::ClassId,
    parent: ir::ClassId,
) -> bool {
    if child == parent {
        return true;
    }
    let mut cur = child;
    loop {
        let Some(info) = env.infos.get(cur as usize) else {
            return false;
        };
        match info.parent {
            Some(p) if p == parent => return true,
            Some(p) => cur = p,
            None => return false,
        }
    }
}

pub(crate) fn class_is_subclass(child: ir::ClassId, parent: ir::ClassId) -> bool {
    with_class_env(|e| class_is_subclass_in(e, child, parent))
}

/// Resolve a method on `class_id` (walk MRO / parent chain). Returns IR func name.
pub(crate) fn resolve_method(class_id: ir::ClassId, method: &str) -> Option<String> {
    with_class_env(|e| {
        let mut cur = Some(class_id);
        while let Some(id) = cur {
            let info = e.infos.get(id as usize)?;
            if let Some((_, func)) = info.methods.iter().find(|(n, _)| n == method) {
                return Some(func.clone());
            }
            cur = info.parent;
        }
        None
    })
}

/// All concrete class ids that are `base` or a subclass of `base`.
pub(crate) fn subclasses_of(base: ir::ClassId) -> Vec<ir::ClassId> {
    with_class_env(|e| {
        e.infos
            .iter()
            .filter(|c| class_is_subclass_in(e, c.id, base))
            .map(|c| c.id)
            .collect()
    })
}

/// Resolve the dunder used for `str(obj)` / default print of a class instance:
/// prefer `__str__`, else `__repr__` (CPython). Returns method name if present
/// on the class or any parent.
pub(crate) fn resolve_str_dunder(class_id: ir::ClassId) -> Option<&'static str> {
    if resolve_method(class_id, "__str__").is_some() {
        Some("__str__")
    } else if resolve_method(class_id, "__repr__").is_some() {
        Some("__repr__")
    } else {
        None
    }
}

/// Call `__str__` / `__repr__` on a class instance (virtual when subclasses
/// override), or fall back to [`ir::ExprKind::ObjectToStr`] (`"<Name object>"`).
/// Each concrete class resolves its own dunder (`__str__` preferred, else
/// `__repr__`) so a base-typed value with only a parent `__repr__` still
/// prints via a subclass `__str__` when that is the live type.
pub(crate) fn lower_class_to_str(
    value: ir::Expr,
    class_id: ir::ClassId,
    span: Span,
) -> SResult<ir::Expr> {
    let mut candidates: Vec<(ir::ClassId, String)> = Vec::new();
    let mut unique_funcs: HashSet<String> = HashSet::new();
    for sid in subclasses_of(class_id) {
        if let Some(m) = resolve_str_dunder(sid)
            && let Some(func) = resolve_method(sid, m)
        {
            let sig = method_sig_lookup(&func).ok_or_else(|| {
                err(
                    format!("internal error: missing signature for method '{m}'"),
                    span,
                )
            })?;
            if sig.ret != ir::Ty::Str {
                return Err(err(format!("{m} must return str"), span));
            }
            if sig.params.len() != 1 {
                return Err(err(
                    format!("{m} must take only self (no extra parameters)"),
                    span,
                ));
            }
            unique_funcs.insert(func.clone());
            candidates.push((sid, func));
        }
    }
    if candidates.is_empty() {
        return Ok(ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::ObjectToStr(Box::new(value)),
        });
    }
    // Prefer the static type's dunder as direct_func when present; else first.
    let direct = resolve_str_dunder(class_id)
        .and_then(|m| resolve_method(class_id, m))
        .or_else(|| candidates.first().map(|(_, f)| f.clone()))
        .ok_or_else(|| err("internal error: str dunder candidates empty", span))?;

    let args = vec![value];
    let virtual_dispatch = unique_funcs.len() > 1
        || candidates
            .iter()
            .any(|(cid, _)| *cid != class_id && resolve_str_dunder(*cid).is_some());
    // Also virtualize when any subclass has a dunder and static type may not
    // share the same func (e.g. only parent has __repr__, child has __str__).
    let virtual_dispatch = virtual_dispatch || candidates.len() > 1;
    if virtual_dispatch {
        Ok(ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::CallMethod {
                direct_func: direct,
                candidates,
                args,
                virtual_dispatch: true,
            },
        })
    } else {
        Ok(ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::Call { func: direct, args },
        })
    }
}

/// IR function name for a method defined on a class in `module`.
pub(crate) fn method_ir_name(
    module: &str,
    is_root: bool,
    class_name: &str,
    method: &str,
) -> String {
    if is_root || module == ENTRY_NAME {
        format!("{class_name}.{method}")
    } else {
        format!("{module}.{class_name}.{method}")
    }
}

/// Field index in layout, walking parent fields (layout is flattened).
pub(crate) fn field_index(class_id: ir::ClassId, field: &str) -> Option<(u32, ir::Ty)> {
    let info = class_info(class_id)?;
    info.fields
        .iter()
        .enumerate()
        .find(|(_, (n, _))| n == field)
        .map(|(i, (_, ty))| (i as u32, *ty))
}

pub(crate) fn register_closure_defaults(ir_name: &str, params: &[ParamSig]) {
    let defs: Vec<(ir::Ty, Option<ast::Expr>)> =
        params.iter().map(|p| (p.ty, p.default.clone())).collect();
    CLOSURE_DEFAULTS.with(|m| {
        m.borrow_mut().insert(ir_name.to_string(), defs);
    });
}

pub(crate) fn lookup_closure_defaults(ir_name: &str) -> Option<Vec<(ir::Ty, Option<ast::Expr>)>> {
    CLOSURE_DEFAULTS.with(|m| m.borrow().get(ir_name).cloned())
}

pub(crate) fn clear_closure_defaults() {
    CLOSURE_DEFAULTS.with(|m| m.borrow_mut().clear());
}
