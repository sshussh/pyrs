//! Semantic analysis: name resolution, type checking, and lowering the AST
//! into the typed IR.
//!
//! Typing rules (a statically-typed subset of Python, mypy-flavored):
//! - `int`, `float`, `bool`, `str`, `list[T]` values; `bool` is assignable
//!   to `int`, and `int`/`bool` are assignable to `float` (implicit
//!   promotion casts are inserted).
//! - a local's storage type is the join of all RHS types (and annotation);
//!   bare multi-assign like `x = 1; x = "a"` yields `int | str`.
//! - `/` is true division and always produces `float`; `//` and `%` follow
//!   Python's floored semantics; `**` on ints yields int (a negative
//!   exponent traps at runtime), on floats yields float.
//! - str supports `+` (concat), `*` int (repeat), comparisons, indexing,
//!   `len()`, and `str(...)` conversions.
//! - lists are homogeneous; they support indexing (read/write), `len()`,
//!   `.append(...)`, and iteration; assignment aliases (like Python).
//! - conditions accept any value with truthiness (numerics `!= 0`,
//!   str/list `len != 0`); `and`/`or`/`not` produce `bool`.
//! - `for` iterates `range(...)`, lists, and strings; it desugars to a
//!   `while` whose `continue` target runs the increment.
//!
//! The program entry is the top-level script statements; if there are none
//! but a zero-parameter `main` is defined, `main()` is called instead.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use common::{Diagnostic, Phase, Span};
use parser::ast;

pub fn ping() -> String {
    String::from("pong")
}

// Defaults for nested functions / lambdas keyed by fully-qualified IR name.
// Populated when nested defs are lowered; used by CallClosure after escape.
type ClosureDefaultEntry = (ir::Ty, Option<ast::Expr>);
type ClosureDefaultsMap = HashMap<String, Vec<ClosureDefaultEntry>>;
thread_local! {
    static CLOSURE_DEFAULTS: RefCell<ClosureDefaultsMap> = RefCell::new(HashMap::new());
}

/// How a class method is bound (decorators).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum MethodKind {
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
struct ClassEnv {
    infos: Vec<ir::ClassInfo>,
    /// `(module_name, ClassName)` → ClassId. Root uses [`ENTRY_NAME`].
    by_key: HashMap<(String, String), ir::ClassId>,
    /// Declaring module for each class id.
    module_of: HashMap<ir::ClassId, String>,
    /// Module currently being lowered/typed (bare `Class` lookups).
    current_module: String,
    /// All method signatures by fully-qualified IR name (cross-module).
    method_sigs: HashMap<String, FuncSig>,
    /// IR method name → kind (static / class / property / instance).
    method_kinds: HashMap<String, MethodKind>,
    /// `(class_id, attr)` → IR func for `@property` getters.
    properties: HashMap<(ir::ClassId, String), String>,
}

thread_local! {
    static CLASS_ENV: RefCell<ClassEnv> = RefCell::new(ClassEnv::default());
}

fn clear_class_env() {
    CLASS_ENV.with(|e| *e.borrow_mut() = ClassEnv::default());
}

fn with_class_env<R>(f: impl FnOnce(&ClassEnv) -> R) -> R {
    CLASS_ENV.with(|e| f(&e.borrow()))
}

fn with_class_env_mut<R>(f: impl FnOnce(&mut ClassEnv) -> R) -> R {
    CLASS_ENV.with(|e| f(&mut e.borrow_mut()))
}

fn set_class_current_module(module: &str) {
    with_class_env_mut(|e| e.current_module = module.to_string());
}

/// Look up a class by bare name in `current_module`, or by `mod.Class` qualified form.
fn lookup_class(name: &str) -> Option<ir::ClassId> {
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
fn lookup_class_in_module(module: &str, name: &str) -> Option<ir::ClassId> {
    with_class_env(|e| {
        e.by_key
            .get(&(module.to_string(), name.to_string()))
            .copied()
    })
}

fn class_info(id: ir::ClassId) -> Option<ir::ClassInfo> {
    with_class_env(|e| e.infos.get(id as usize).cloned())
}

fn method_sig_lookup(ir_name: &str) -> Option<FuncSig> {
    with_class_env(|e| e.method_sigs.get(ir_name).cloned())
}

fn register_method_sig(ir_name: &str, sig: FuncSig) {
    with_class_env_mut(|e| {
        e.method_sigs.insert(ir_name.to_string(), sig);
    });
}

fn register_method_kind(ir_name: &str, kind: MethodKind) {
    with_class_env_mut(|e| {
        e.method_kinds.insert(ir_name.to_string(), kind);
    });
}

fn method_kind_lookup(ir_name: &str) -> MethodKind {
    with_class_env(|e| {
        e.method_kinds
            .get(ir_name)
            .copied()
            .unwrap_or(MethodKind::Instance)
    })
}

fn register_property(class_id: ir::ClassId, name: &str, ir_name: &str) {
    with_class_env_mut(|e| {
        e.properties
            .insert((class_id, name.to_string()), ir_name.to_string());
    });
}

fn resolve_property(class_id: ir::ClassId, name: &str) -> Option<String> {
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

fn method_kind_from_decorators(decorators: &[ast::Decorator], span: Span) -> SResult<MethodKind> {
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

fn class_is_subclass_in(env: &ClassEnv, child: ir::ClassId, parent: ir::ClassId) -> bool {
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

fn class_is_subclass(child: ir::ClassId, parent: ir::ClassId) -> bool {
    with_class_env(|e| class_is_subclass_in(e, child, parent))
}

/// Resolve a method on `class_id` (walk MRO / parent chain). Returns IR func name.
fn resolve_method(class_id: ir::ClassId, method: &str) -> Option<String> {
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
fn subclasses_of(base: ir::ClassId) -> Vec<ir::ClassId> {
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
fn resolve_str_dunder(class_id: ir::ClassId) -> Option<&'static str> {
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
fn lower_class_to_str(value: ir::Expr, class_id: ir::ClassId, span: Span) -> SResult<ir::Expr> {
    let Some(method) = resolve_str_dunder(class_id) else {
        return Ok(ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::ObjectToStr(Box::new(value)),
        });
    };
    let direct = resolve_method(class_id, method).expect("resolve_str_dunder checked");
    let sig = method_sig_lookup(&direct).ok_or_else(|| {
        err(
            format!("internal error: missing signature for method '{method}'"),
            span,
        )
    })?;
    if sig.ret != ir::Ty::Str {
        return Err(err(format!("{method} must return str"), span));
    }
    // User params after self must be empty for str/repr protocol.
    if sig.params.len() != 1 {
        return Err(err(
            format!("{method} must take only self (no extra parameters)"),
            span,
        ));
    }

    let mut candidates: Vec<(ir::ClassId, String)> = Vec::new();
    let mut unique_funcs: HashSet<String> = HashSet::new();
    for sid in subclasses_of(class_id) {
        if let Some(func) = resolve_method(sid, method) {
            unique_funcs.insert(func.clone());
            candidates.push((sid, func));
        }
    }
    // Prefer __str__ on a subclass even if static type only has __repr__;
    // when the resolved dunder name differs per class, still virtualize on
    // the chosen method name from the static type (matches closed-world
    // resolve_method MRO for each candidate's own method of that name).
    let virtual_dispatch = unique_funcs.len() > 1;
    let args = vec![value];
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
fn method_ir_name(module: &str, is_root: bool, class_name: &str, method: &str) -> String {
    if is_root || module == ENTRY_NAME {
        format!("{class_name}.{method}")
    } else {
        format!("{module}.{class_name}.{method}")
    }
}

/// Field index in layout, walking parent fields (layout is flattened).
fn field_index(class_id: ir::ClassId, field: &str) -> Option<(u32, ir::Ty)> {
    let info = class_info(class_id)?;
    info.fields
        .iter()
        .enumerate()
        .find(|(_, (n, _))| n == field)
        .map(|(i, (_, ty))| (i as u32, *ty))
}

fn register_closure_defaults(ir_name: &str, params: &[ParamSig]) {
    let defs: Vec<(ir::Ty, Option<ast::Expr>)> =
        params.iter().map(|p| (p.ty, p.default.clone())).collect();
    CLOSURE_DEFAULTS.with(|m| {
        m.borrow_mut().insert(ir_name.to_string(), defs);
    });
}

fn lookup_closure_defaults(ir_name: &str) -> Option<Vec<(ir::Ty, Option<ast::Expr>)>> {
    CLOSURE_DEFAULTS.with(|m| m.borrow().get(ir_name).cloned())
}

fn clear_closure_defaults() {
    CLOSURE_DEFAULTS.with(|m| m.borrow_mut().clear());
}

type SResult<T> = Result<T, Diagnostic>;

/// Name of the synthesized entry function holding top-level statements.
pub const ENTRY_NAME: &str = "__main__";

fn err(message: impl Into<String>, span: Span) -> Diagnostic {
    Diagnostic::new(Phase::Semantic, message, span)
}

fn resolve_type(ty: ast::TypeName) -> ir::Ty {
    match ty {
        ast::TypeName::Int => ir::Ty::Int,
        ast::TypeName::Float => ir::Ty::Float,
        ast::TypeName::Bool => ir::Ty::Bool,
        ast::TypeName::Str => ir::Ty::Str,
        ast::TypeName::File => ir::Ty::File,
        ast::TypeName::Any => ir::Ty::Any,
        ast::TypeName::List(e) => ir::list_of(resolve_type(*e)),
        ast::TypeName::Tuple(elems) => {
            let ts: Vec<ir::Ty> = elems.iter().copied().map(resolve_type).collect();
            ir::tuple_of(&ts)
        }
        // Key/element restrictions are enforced in resolve_type_checked when a
        // span is available; bare resolve is used only for already-validated paths.
        ast::TypeName::Dict { key, value } => ir::dict_of(resolve_type(*key), resolve_type(*value)),
        ast::TypeName::Set(e) => ir::set_of(resolve_type(*e)),
        ast::TypeName::None => ir::Ty::None,
        ast::TypeName::Union(ms) => {
            let ts: Vec<ir::Ty> = ms.iter().copied().map(resolve_type).collect();
            ir::union_of(&ts)
        }
        ast::TypeName::Class(name) => {
            if let Some(id) = lookup_class(name) {
                ir::Ty::Class(id)
            } else {
                // Should have been rejected by resolve_type_checked.
                ir::Ty::Class(0)
            }
        }
    }
}

/// Resolve a parameter's type: explicit annotation, else infer from a
/// constant/simple default, else `None` (caller may infer from body usage).
fn resolve_param_ty_opt(p: &ast::Param) -> SResult<Option<ir::Ty>> {
    if let Some(t) = p.ty {
        return Ok(Some(resolve_type_checked(t, p.span)?));
    }
    if let Some(d) = &p.default {
        return Ok(Some(infer_ty_from_default(d)?));
    }
    Ok(None)
}

/// Resolve a parameter's type, requiring annotation/default (no body inference).
fn resolve_param_ty(p: &ast::Param) -> SResult<ir::Ty> {
    match resolve_param_ty_opt(p)? {
        Some(t) => Ok(t),
        None => Err(err(
            format!(
                "parameter '{}' is missing a type annotation and has no default to \
                 infer from (e.g. '{}: int' or '{}: int = 0')",
                p.name, p.name, p.name
            ),
            p.span,
        )),
    }
}

/// Error when bare-param body inference fails.
fn bare_param_infer_err(p: &ast::Param) -> Diagnostic {
    err(
        format!(
            "parameter '{}' is missing a type annotation; could not infer a unique \
             type from the function body (add e.g. '{}: int')",
            p.name, p.name
        ),
        p.span,
    )
}

/// Resolve all formal params, inferring bare ones monomorphically from body usage.
fn resolve_params_with_body_infer(
    formals: &[ast::Param],
    body: &[ast::Stmt],
) -> SResult<Vec<ParamSig>> {
    let mut params = Vec::new();
    let mut bare_idxs = Vec::new();
    let mut seen = HashSet::new();
    for p in formals {
        if !seen.insert(p.name.clone()) {
            return Err(err(
                format!("duplicate parameter name '{}'", p.name),
                p.span,
            ));
        }
        let ty = match resolve_param_ty_opt(p)? {
            Some(t) => t,
            None => {
                bare_idxs.push(params.len());
                ir::Ty::Int // placeholder
            }
        };
        if ty == ir::Ty::None {
            return Err(err(
                format!("parameter '{}' cannot have type None", p.name),
                p.span,
            ));
        }
        params.push(ParamSig {
            name: p.name.clone(),
            ty,
            default: p.default.clone(),
        });
    }
    if !bare_idxs.is_empty() {
        let mut bare_names: HashSet<String> =
            bare_idxs.iter().map(|&i| params[i].name.clone()).collect();
        let mut changed = true;
        let mut rounds = 0;
        while changed && rounds < 8 {
            changed = false;
            rounds += 1;
            let param_map: HashMap<String, ir::Ty> =
                params.iter().map(|p| (p.name.clone(), p.ty)).collect();
            for &i in &bare_idxs {
                let name = params[i].name.clone();
                if let Some(ty) = try_infer_param_from_body(&name, body, &param_map, &bare_names) {
                    if params[i].ty != ty {
                        params[i].ty = ty;
                        changed = true;
                    }
                    bare_names.remove(&name);
                }
            }
        }
        for &i in &bare_idxs {
            if bare_names.contains(&params[i].name) {
                let param_map: HashMap<String, ir::Ty> =
                    params.iter().map(|p| (p.name.clone(), p.ty)).collect();
                if let Some(ty) =
                    try_infer_param_from_body(&params[i].name, body, &param_map, &HashSet::new())
                {
                    params[i].ty = ty;
                } else {
                    return Err(bare_param_infer_err(&formals[i]));
                }
            }
        }
    }
    Ok(params)
}

/// Infer a type from a simple default expression (literals and short forms).
fn infer_ty_from_default(expr: &ast::Expr) -> SResult<ir::Ty> {
    match &expr.kind {
        ast::ExprKind::Int(_) | ast::ExprKind::IntDigits(_) => Ok(ir::Ty::Int),
        ast::ExprKind::Float(_) => Ok(ir::Ty::Float),
        ast::ExprKind::Bool(_) => Ok(ir::Ty::Bool),
        ast::ExprKind::Str(_) | ast::ExprKind::JoinedStr(_) => Ok(ir::Ty::Str),
        ast::ExprKind::NoneLit => Ok(ir::Ty::None),
        ast::ExprKind::Unary {
            op: ast::UnaryOp::Neg | ast::UnaryOp::Invert,
            operand,
        } => infer_ty_from_default(operand),
        ast::ExprKind::Unary {
            op: ast::UnaryOp::Not,
            ..
        } => Ok(ir::Ty::Bool),
        ast::ExprKind::ListLit(items) if items.is_empty() => Ok(ir::list_of(ir::Ty::Any)),
        ast::ExprKind::ListLit(items) => {
            let mut elem: Option<ir::Ty> = None;
            for it in items {
                let e = match it {
                    ast::ListElem::Item(e) => e,
                    ast::ListElem::Star(_) => {
                        return Err(err(
                            "cannot infer parameter type from starred list default; \
                             annotate the parameter",
                            expr.span,
                        ));
                    }
                };
                let t = infer_ty_from_default(e)?;
                elem = Some(match elem {
                    None => t,
                    Some(prev) => join_elem_types(prev, t).ok_or_else(|| {
                        err(
                            format!(
                                "list default elements must share one type; found {prev} and {t}"
                            ),
                            e.span,
                        )
                    })?,
                });
            }
            let elem = elem.unwrap_or(ir::Ty::Int);
            let elem = elem_of(elem, expr.span).unwrap_or(elem);
            Ok(ir::list_of(elem))
        }
        ast::ExprKind::TupleLit(items) => {
            let mut ts = Vec::new();
            for it in items {
                ts.push(infer_ty_from_default(it)?);
            }
            Ok(ir::tuple_of(&ts))
        }
        ast::ExprKind::DictLit(items) if items.is_empty() => Err(err(
            "cannot infer type of default {}; annotate the parameter",
            expr.span,
        )),
        ast::ExprKind::Lambda { .. } => Err(err(
            "cannot infer parameter type from a lambda default; annotate the parameter",
            expr.span,
        )),
        _ => Err(err(
            "cannot infer parameter type from this default expression; \
             add an explicit annotation",
            expr.span,
        )),
    }
}

/// Resolve a type annotation with span, rejecting unsupported dict/set keys.
fn resolve_type_checked(ty: ast::TypeName, span: Span) -> SResult<ir::Ty> {
    match ty {
        ast::TypeName::Dict { key, value } => {
            let k = resolve_type_checked(*key, span)?;
            let v = resolve_type_checked(*value, span)?;
            check_hashable_key(k, span, "dict")?;
            if v == ir::Ty::File {
                return Err(err("dict values cannot be file", span));
            }
            if v == ir::Ty::Exception {
                return Err(err("dict values cannot be exception objects", span));
            }
            Ok(ir::dict_of(k, v))
        }
        ast::TypeName::Set(e) => {
            let elem = resolve_type_checked(*e, span)?;
            check_hashable_key(elem, span, "set")?;
            Ok(ir::set_of(elem))
        }
        ast::TypeName::List(e) => {
            let elem = resolve_type_checked(*e, span)?;
            if elem == ir::Ty::File {
                return Err(err("list elements cannot be file", span));
            }
            if elem == ir::Ty::Exception {
                return Err(err("list elements cannot be exception objects", span));
            }
            Ok(ir::list_of(elem))
        }
        ast::TypeName::Tuple(elems) => {
            let mut ts = Vec::with_capacity(elems.len());
            for e in elems {
                let t = resolve_type_checked(*e, span)?;
                if t == ir::Ty::None
                    || matches!(t, ir::Ty::Union(_))
                    || t == ir::Ty::File
                    || t == ir::Ty::Exception
                {
                    return Err(err(
                        format!("tuple elements of type {t} are not supported"),
                        span,
                    ));
                }
                ts.push(t);
            }
            Ok(ir::tuple_of(&ts))
        }
        ast::TypeName::Union(ms) => {
            let mut ts = Vec::with_capacity(ms.len());
            for m in ms {
                ts.push(resolve_type_checked(*m, span)?);
            }
            if ts.is_empty() {
                return Err(err("empty union type", span));
            }
            Ok(ir::union_of(&ts))
        }
        ast::TypeName::Class(name) => {
            let Some(id) = lookup_class(name) else {
                return Err(err(
                    format!("unknown type '{name}' (not a builtin or defined class)"),
                    span,
                ));
            };
            Ok(ir::Ty::Class(id))
        }
        other => Ok(resolve_type(other)),
    }
}

fn elem_of(ty: ir::Ty, span: Span) -> SResult<ir::Ty> {
    match ty {
        ir::Ty::File => Err(err("files cannot be stored in lists yet", span)),
        ir::Ty::Exception => Err(err("exception objects cannot be stored in lists yet", span)),
        // Pure None list elements are allowed only as part of a union annotation
        // path; a bare None elem type is rejected when building untyped lists.
        other => Ok(other),
    }
}

/// Reject exception objects as container elements (no print-slot encoding yet).
fn reject_exception_container_elem(ty: ir::Ty, span: Span, what: &str) -> SResult<()> {
    match ty {
        ir::Ty::Exception => Err(err(
            format!("exception objects cannot be stored in {what} yet"),
            span,
        )),
        ir::Ty::Union(ms) if ms.contains(&ir::Ty::Exception) => Err(err(
            format!("unions containing exception objects cannot be stored in {what} yet"),
            span,
        )),
        _ => Ok(()),
    }
}

/// Keys for dict/set: only int and str in the current language surface.
fn check_hashable_key(ty: ir::Ty, span: Span, what: &str) -> SResult<()> {
    match ty {
        ir::Ty::Int | ir::Ty::Str => Ok(()),
        other => Err(err(
            format!(
                "{what} keys/elements of type {other} are not supported yet \
                 (only int and str)"
            ),
            span,
        )),
    }
}

fn ast_exc_to_ir(e: ast::ExcType) -> ir::ExcType {
    match e {
        ast::ExcType::ValueError => ir::ExcType::ValueError,
        ast::ExcType::KeyError => ir::ExcType::KeyError,
        ast::ExcType::IndexError => ir::ExcType::IndexError,
        ast::ExcType::ZeroDivisionError => ir::ExcType::ZeroDivisionError,
        ast::ExcType::TypeError => ir::ExcType::TypeError,
        ast::ExcType::RuntimeError => ir::ExcType::RuntimeError,
        ast::ExcType::GeneratorExit => ir::ExcType::GeneratorExit,
        ast::ExcType::OverflowError => ir::ExcType::OverflowError,
        ast::ExcType::EOFError => ir::ExcType::EOFError,
        ast::ExcType::FileNotFoundError => ir::ExcType::FileNotFoundError,
        ast::ExcType::OSError => ir::ExcType::OSError,
        ast::ExcType::NameError => ir::ExcType::NameError,
        ast::ExcType::UnboundLocalError => ir::ExcType::UnboundLocalError,
        ast::ExcType::StopIteration => ir::ExcType::StopIteration,
        ast::ExcType::Exception => ir::ExcType::Exception,
        ast::ExcType::PermissionError => ir::ExcType::PermissionError,
        ast::ExcType::IsADirectoryError => ir::ExcType::IsADirectoryError,
        ast::ExcType::AssertionError => ir::ExcType::AssertionError,
    }
}

#[derive(Debug, Clone)]
struct ParamSig {
    name: String,
    ty: ir::Ty,
    /// Cloned AST default; lowered at each call site when the arg is omitted.
    default: Option<ast::Expr>,
}

#[derive(Debug, Clone)]
struct FuncSig {
    params: Vec<ParamSig>,
    /// `*args: T` — element type `T`; IR param is `list[T]`.
    vararg: Option<ParamSig>,
    /// `**kwargs: T` — value type `T`; IR param is `dict[str, T]`.
    kwarg: Option<ParamSig>,
    ret: ir::Ty,
    span: Span,
    /// True when the function body contains `yield` (returns a generator).
    is_generator: bool,
    /// Element type yielded when `is_generator`.
    yield_ty: Option<ir::Ty>,
    /// Frame slot count for generator resume (params + locals); 0 if unknown.
    gen_frame_slots: i64,
}

/// Nested function visible only inside its enclosing function.
#[derive(Debug, Clone)]
struct NestedFnInfo {
    /// Fully-qualified IR name (`outer.inner` or `mod.outer.inner`).
    ir_name: String,
    /// Signature of the nested function **without** capture parameters.
    sig: FuncSig,
    /// Outer locals/params captured as leading IR params (cell ptr or value).
    captures: Vec<(String, ir::Ty)>,
    /// Parallel to captures: true if the capture is a cell pointer.
    capture_is_cell: Vec<bool>,
    /// True when this nested function uses the closure calling convention
    /// (env pointer first) rather than plain leading value params.
    #[allow(dead_code)]
    uses_env: bool,
}

fn make_closure_expr(info: &NestedFnInfo, span: Span, ctx: &mut FnCtx) -> SResult<ir::Expr> {
    // Generator functions may escape as first-class values; calling them
    // produces a generator object (MakeGenerator) with captures in the frame.
    let mut caps = Vec::new();
    let mut is_cell = Vec::new();
    for (i, (name, ty)) in info.captures.iter().enumerate() {
        // If the outer has promoted this name to a cell after this nested fn
        // was registered, upgrade this capture to a cell (sibling nonlocal).
        let cell = info.capture_is_cell.get(i).copied().unwrap_or(false)
            || ctx.cell_locals.contains_key(name);
        if cell {
            let cell_ty = ir::cell_of(*ty);
            let cell_name = format!(".cell.{name}");
            if !ctx.locals.contains_key(&cell_name) {
                // cell should already exist if outer uses cells
                return Err(err(
                    format!("internal: missing cell for capture '{name}'"),
                    span,
                ));
            }
            caps.push(ir::Expr {
                ty: cell_ty,
                kind: ir::ExprKind::Local(cell_name),
            });
            is_cell.push(true);
        } else {
            let Some(local_ty) = ctx.locals.get(name).copied() else {
                return Err(err(format!("cannot capture '{name}': not in scope"), span));
            };
            if local_ty != *ty {
                return Err(err(format!("capture type mismatch for '{name}'"), span));
            }
            caps.push(ir::Expr {
                ty: *ty,
                kind: ir::ExprKind::Local(name.clone()),
            });
            is_cell.push(false);
        }
    }
    let params: Vec<ir::Ty> = info.sig.params.iter().map(|p| p.ty).collect();
    let capture_tys: Vec<ir::Ty> = info
        .captures
        .iter()
        .enumerate()
        .map(|(i, (_, ty))| {
            if info.capture_is_cell.get(i).copied().unwrap_or(false) {
                ir::cell_of(*ty)
            } else {
                *ty
            }
        })
        .collect();
    let ty = ir::closure_of_full(&params, info.sig.ret, &capture_tys, &info.ir_name);
    Ok(ir::Expr {
        ty,
        kind: ir::ExprKind::MakeClosure {
            func: info.ir_name.clone(),
            captures: caps,
            capture_is_cell: is_cell,
        },
    })
}

/// Apply a flow refinement to a loaded name.
///
/// - Union storage → concrete member: `FromUnion` peel.
/// - Union storage → subclass of a class member: extract the base class
///   member, then retype to the subclass (same pointer; layout prefix).
/// - Monomorphic class storage → subclass: retype the load (isinstance peel).
/// - Multi-member peels keep storage (tags unsafe to rematerialize).
fn apply_type_refinement(base: ir::Expr, storage: ir::Ty, nty: ir::Ty) -> ir::Expr {
    if nty == storage {
        return base;
    }
    // Class base → more specific subclass (isinstance).
    if let (ir::Ty::Class(src), ir::Ty::Class(dst)) = (storage, nty)
        && class_is_subclass(dst, src)
    {
        return ir::Expr {
            ty: nty,
            kind: base.kind,
        };
    }
    // Class base → union of subclasses (isinstance(x, (B, C))): keep the
    // Class ABI (ptr). Attribute lowering consults the refinement map for
    // common fields; retyping Local to Union would make codegen load
    // `{i32,i64}` from a `ptr` alloca.
    if matches!(storage, ir::Ty::Union(_)) && !matches!(nty, ir::Ty::Union(_)) {
        let members = ir::flatten_union_members(storage);
        // Exact member peel.
        if members.contains(&nty) {
            return ir::Expr {
                ty: nty,
                kind: ir::ExprKind::FromUnion {
                    value: Box::new(base),
                },
            };
        }
        // isinstance subclass peel: storage has Class(base), refine to Class(sub).
        if let ir::Ty::Class(want) = nty {
            for m in members {
                if let ir::Ty::Class(got) = m
                    && class_is_subclass(want, got)
                {
                    let extracted = ir::Expr {
                        ty: m,
                        kind: ir::ExprKind::FromUnion {
                            value: Box::new(base),
                        },
                    };
                    return ir::Expr {
                        ty: nty,
                        kind: extracted.kind,
                    };
                }
            }
        }
        // Multi-member / unknown peel: keep storage type.
        return base;
    }
    // Multi-member peels (class or scalar): keep storage. Member-index tags
    // must not be rematerialized — retyping a subset (A|B|C → B|C) renumbers
    // indices and blanks/mis-prints or segfaults. Exclusive class fields and
    // common fields consult `type_refinements` / `exclusive_class_field`, not
    // a retyped load ABI.
    let _ = nty;
    base
}

/// If `ty` is a union of classes that all share `attr` at the same index/type,
/// return `(representative_class_id, field_index, field_ty)`.
fn common_class_field(ty: ir::Ty, attr: &str) -> Option<(ir::ClassId, u32, ir::Ty)> {
    let members = ir::flatten_union_members(ty);
    if members.is_empty() || !members.iter().all(|m| matches!(m, ir::Ty::Class(_))) {
        return None;
    }
    let mut found: Option<(ir::ClassId, u32, ir::Ty)> = None;
    for m in members {
        let ir::Ty::Class(id) = m else {
            return None;
        };
        let (idx, fty) = field_index(id, attr)?;
        match found {
            None => found = Some((id, idx, fty)),
            Some((_, pi, pt)) if pi == idx && pt == fty => {}
            _ => return None,
        }
    }
    found
}

/// Field access that exists on a *subset* of a class union (or on classes with
/// differing layout indices). Returns closed-world `(class_id, field_index)`
/// candidates that have `attr` at a uniform field type, for a runtime type_id
/// switch. `None` when no refined class has the field or types disagree.
fn exclusive_class_field(ty: ir::Ty, attr: &str) -> Option<(Vec<(ir::ClassId, u32)>, ir::Ty)> {
    let members = ir::flatten_union_members(ty);
    if members.is_empty() || !members.iter().all(|m| matches!(m, ir::Ty::Class(_))) {
        return None;
    }
    let mut candidates: Vec<(ir::ClassId, u32)> = Vec::new();
    let mut field_ty: Option<ir::Ty> = None;
    // Expand each refined class to every closed-world subclass that may appear
    // at runtime after isinstance(x, (B, C)) (type_id is the most specific).
    let mut seen: HashSet<ir::ClassId> = HashSet::new();
    for m in members {
        let ir::Ty::Class(id) = m else {
            return None;
        };
        for sid in subclasses_of(id) {
            if !seen.insert(sid) {
                continue;
            }
            if let Some((idx, fty)) = field_index(sid, attr) {
                match field_ty {
                    None => field_ty = Some(fty),
                    Some(prev) if prev == fty => {}
                    Some(_) => return None, // incompatible field types
                }
                candidates.push((sid, idx));
            }
        }
    }
    if candidates.is_empty() {
        return None;
    }
    Some((candidates, field_ty.expect("candidates non-empty")))
}

/// Per-member `isinstance` peel: `(then_ty, else_ty)` — either side may be
/// absent when that arm is impossible for this storage member.
///
/// Class members are special: `isinstance(x, Sub)` when `x` is statically a
/// base class peels then-arm to `Sub` and keeps the base in the else-arm.
fn isinstance_peel_member(m: ir::Ty, pats: &[IsInstancePat]) -> (Option<ir::Ty>, Option<ir::Ty>) {
    let class_wants: Vec<ir::ClassId> = pats
        .iter()
        .filter_map(|p| match p {
            IsInstancePat::Class(id) => Some(*id),
            _ => None,
        })
        .collect();

    if let ir::Ty::Class(got) = m {
        if class_wants.is_empty() {
            // isinstance(obj, int) etc. — never true for user instances.
            let hit = pats.iter().any(|p| isinstance_pat_matches(m, *p));
            return if hit {
                (Some(m), None)
            } else {
                (None, Some(m))
            };
        }
        // got <: want → always True; keep the more-specific static type.
        if class_wants.iter().any(|&w| class_is_subclass(got, w)) {
            return (Some(m), None);
        }
        // want <: got → runtime check; then peels to want(s), else keeps base.
        let then_ids: Vec<ir::ClassId> = class_wants
            .iter()
            .copied()
            .filter(|&w| class_is_subclass(w, got))
            .collect();
        if !then_ids.is_empty() {
            let then_tys: Vec<ir::Ty> = then_ids.into_iter().map(ir::Ty::Class).collect();
            let then_ty = match then_tys.len() {
                1 => then_tys[0],
                _ => ir::union_of(&then_tys),
            };
            return (Some(then_ty), Some(m));
        }
        // Unrelated class patterns.
        return (None, Some(m));
    }

    // All exception instances share Ty::Exception — cannot peel subtypes.
    if m == ir::Ty::Exception {
        let has_exc = pats.iter().any(|p| matches!(p, IsInstancePat::Exc(_)));
        return if has_exc {
            (Some(m), Some(m))
        } else {
            (None, Some(m))
        };
    }

    let hit = pats.iter().any(|p| isinstance_pat_matches(m, *p));
    if hit {
        (Some(m), None)
    } else {
        (None, Some(m))
    }
}

/// Look up storage type of a local/cell/module name (not refinements).
fn name_storage_ty(name: &str, ctx: &FnCtx) -> Option<ir::Ty> {
    ctx.locals
        .get(name)
        .copied()
        .or_else(|| ctx.cell_locals.get(name).copied())
        .or_else(|| {
            // Module Optionals: free reads (no `global` needed) and explicit
            // `global` / entry use GlobalLoad.
            if !ctx.locals.contains_key(name) && !ctx.cell_locals.contains_key(name) {
                ctx.globals.get(name).copied()
            } else {
                None
            }
        })
}

/// Effective type of `name` under an active refinement overlay (and-chain mid
/// peels), falling back to `ctx.type_refinements` then storage.
fn name_refined_ty(name: &str, ctx: &FnCtx, active: &HashMap<String, ir::Ty>) -> Option<ir::Ty> {
    let storage = name_storage_ty(name, ctx)?;
    Some(
        active
            .get(name)
            .copied()
            .or_else(|| ctx.type_refinements.get(name).copied())
            .unwrap_or(storage),
    )
}

/// Flow-sensitive narrowing for `x is None` / `x is not None` / `not (x is None)`.
/// Also peels `A and B` / `A or B` so `x is not None and flag` narrows `x` in
/// the then-arm (and complementary or-arms). Returns (then, else) maps.
fn narrowing_from_condition(
    cond: &ast::Expr,
    ctx: &FnCtx,
) -> (HashMap<String, ir::Ty>, HashMap<String, ir::Ty>) {
    // Seed active peels with current refinements so nested and/or compose.
    narrowing_from_condition_with(cond, ctx, &ctx.type_refinements)
}

/// Like [`narrowing_from_condition`], but peels are computed relative to
/// `active` (left-arm peels of an `and` chain, etc.). Right-arm peels thus see
/// more-specific left refinements instead of wiping them with storage.
fn narrowing_from_condition_with(
    cond: &ast::Expr,
    ctx: &FnCtx,
    active: &HashMap<String, ir::Ty>,
) -> (HashMap<String, ir::Ty>, HashMap<String, ir::Ty>) {
    let mut then_m = HashMap::new();
    let mut else_m = HashMap::new();
    match &cond.kind {
        ast::ExprKind::Unary {
            op: ast::UnaryOp::Not,
            operand,
        } => {
            let (t, e) = narrowing_from_condition_with(operand, ctx, active);
            return (e, t);
        }
        // `A and B`: then sees left peels, then right peels computed *under*
        // left (so `isinstance(x, B) and x is not None` keeps B; chained
        // isinstance keeps the more specific class). Else is not a simple
        // complement (A may be true and B false).
        ast::ExprKind::Binary {
            op: ast::BinOp::And,
            left,
            right,
        } => {
            let (lt, _) = narrowing_from_condition_with(left, ctx, active);
            let mut active_right = active.clone();
            for (k, v) in &lt {
                active_right.insert(k.clone(), *v);
            }
            let (rt, _) = narrowing_from_condition_with(right, ctx, &active_right);
            // Start with left peels; right overwrites only when it mentions a
            // name (and was computed under left, so stays at least as specific).
            then_m = lt;
            for (k, v) in rt {
                then_m.insert(k, v);
            }
            return (then_m, else_m);
        }
        // `A or B`: else sees both else-refs (both failed). Compose right's
        // else peels under left's else peels for the same sequential reason.
        ast::ExprKind::Binary {
            op: ast::BinOp::Or,
            left,
            right,
        } => {
            let (_, le) = narrowing_from_condition_with(left, ctx, active);
            let mut active_else = active.clone();
            for (k, v) in &le {
                active_else.insert(k.clone(), *v);
            }
            let (_, re) = narrowing_from_condition_with(right, ctx, &active_else);
            else_m = le;
            for (k, v) in re {
                else_m.insert(k, v);
            }
            return (then_m, else_m);
        }
        ast::ExprKind::Binary {
            op: op @ (ast::BinOp::Is | ast::BinOp::IsNot),
            left,
            right,
        } => {
            let not = matches!(op, ast::BinOp::IsNot);
            let (name, name_ty) = match (&left.kind, &right.kind) {
                (ast::ExprKind::Name(n), ast::ExprKind::NoneLit) => {
                    (n.as_str(), name_storage_ty(n, ctx))
                }
                (ast::ExprKind::NoneLit, ast::ExprKind::Name(n)) => {
                    (n.as_str(), name_storage_ty(n, ctx))
                }
                _ => return (then_m, else_m),
            };
            let Some(storage_ty) = name_ty else {
                return (then_m, else_m);
            };
            // Prefer active overlay (and-chain left peels), then outer refinements.
            let ty = name_refined_ty(name, ctx, active).unwrap_or(storage_ty);
            let without_none = match ty {
                ir::Ty::Union(ms) => {
                    let rest: Vec<ir::Ty> =
                        ms.iter().copied().filter(|m| *m != ir::Ty::None).collect();
                    match rest.len() {
                        0 => ir::Ty::None,
                        1 => rest[0],
                        _ => ir::union_of(&rest),
                    }
                }
                ir::Ty::None => ir::Ty::None,
                other => other,
            };
            // Complement uses the *storage* optional-ness: even if we already
            // refined to a concrete member, `is not None` failing means None.
            let storage_optional = ir::is_optional(storage_ty);
            if not {
                // `is not None` → then: non-none, else: None (if storage optional)
                then_m.insert(name.to_string(), without_none);
                if storage_optional {
                    else_m.insert(name.to_string(), ir::Ty::None);
                }
            } else {
                // `is None` → then: None, else: non-none of storage
                then_m.insert(name.to_string(), ir::Ty::None);
                let storage_without = match storage_ty {
                    ir::Ty::Union(ms) => {
                        let rest: Vec<ir::Ty> =
                            ms.iter().copied().filter(|m| *m != ir::Ty::None).collect();
                        match rest.len() {
                            0 => ir::Ty::None,
                            1 => rest[0],
                            _ => ir::union_of(&rest),
                        }
                    }
                    other => other,
                };
                else_m.insert(name.to_string(), storage_without);
            }
        }
        // `isinstance(x, T)` / `isinstance(x, (T1, T2))` peels unions and class
        // bases (subclass → then-arm; complementary miss → else-arm).
        ast::ExprKind::Call {
            func,
            args,
            keywords,
            kwargs,
            ..
        } if func == "isinstance" && keywords.is_empty() && kwargs.is_none() => {
            if let Ok(plain) = require_plain_args(args, "isinstance", cond.span)
                && plain.len() == 2
                && let ast::ExprKind::Name(n) = &plain[0].kind
                && let Some(storage_ty) = name_storage_ty(n, ctx)
                && let Ok(pats) = parse_isinstance_type_arg(plain[1])
            {
                // Prefer active overlay (and-chain left peels), then outer.
                let ty = name_refined_ty(n, ctx, active).unwrap_or(storage_ty);
                let members = ir::flatten_union_members(ty);
                let mut hit: Vec<ir::Ty> = Vec::new();
                let mut miss: Vec<ir::Ty> = Vec::new();
                for m in members {
                    let (t, e) = isinstance_peel_member(m, &pats);
                    if let Some(t) = t {
                        hit.push(t);
                    }
                    if let Some(e) = e {
                        miss.push(e);
                    }
                }
                if !hit.is_empty() {
                    let then_ty = match hit.len() {
                        1 => hit[0],
                        _ => ir::union_of(&hit),
                    };
                    then_m.insert(n.clone(), then_ty);
                }
                if !miss.is_empty() {
                    let else_ty = match miss.len() {
                        1 => miss[0],
                        _ => ir::union_of(&miss),
                    };
                    else_m.insert(n.clone(), else_ty);
                }
            }
        }
        _ => {}
    }
    (then_m, else_m)
}

/// Lower `lambda params: body` to a nested function + MakeClosure.
fn lower_lambda(
    params: &[ast::Param],
    body: &ast::Expr,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    ctx.temp_counter += 1;
    let name = format!(".lambda{}", ctx.temp_counter);
    for p in params {
        resolve_param_ty(p)?;
    }
    // Leave return unannotated; lower_function infers from the return stmt body.
    let body_stmt = ast::Stmt {
        kind: ast::StmtKind::Return(Some(body.clone())),
        span: body.span,
    };
    let fd = ast::FuncDef {
        name: name.clone(),
        params: params.to_vec(),
        vararg: None,
        kwarg: None,
        ret: None,
        body: vec![body_stmt],
        span,
        decorators: Vec::new(),
    };
    lower_nested_func_def(&fd, ctx)?;
    // Patch ret type from the lowered IR return (actual body type).
    if let Some(info) = ctx.nested_funcs.get(&name).cloned() {
        let ir_name = info.ir_name.clone();
        for f in &mut ctx.nested_ir {
            if f.name == ir_name {
                if let Some(rt) = first_return_ty(&f.body) {
                    f.ret = rt;
                    if let Some(info) = ctx.nested_funcs.get_mut(&name) {
                        info.sig.ret = rt;
                    }
                }
                break;
            }
        }
    }
    // Flush cell boxing inits (same as FuncDef stmt path). Without this,
    // free-var cells stay null and loads trap with UnboundLocalError.
    let mut inits = std::mem::take(&mut ctx.pending_cell_inits);
    freeze_nested_defaults(&name, span, ctx, &mut inits)?;
    let info = ctx
        .nested_funcs
        .get(&name)
        .cloned()
        .ok_or_else(|| err("internal: lambda not registered after freeze", span))?;
    let clos = make_closure_expr(&info, span, ctx)?;
    if inits.is_empty() {
        Ok(clos)
    } else {
        Ok(ir::Expr {
            ty: clos.ty,
            kind: ir::ExprKind::Block {
                stmts: inits,
                result: Box::new(clos),
            },
        })
    }
}

/// Evaluate non-literal nested/lambda defaults once at definition time and
/// rewrite them to load frozen temps (CPython freezes `__defaults__`).
/// Pure literals are left as-is so escaped `CallClosure` can re-materialize
/// them outside the outer frame. Free-var defaults that escape still need
/// literals (temps are outer locals — documented limit).
fn freeze_nested_defaults(
    user_name: &str,
    span: Span,
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    let Some(info) = ctx.nested_funcs.get(user_name).cloned() else {
        return Ok(());
    };
    let mut new_params = info.sig.params.clone();
    let mut changed = false;
    for p in &mut new_params {
        let Some(d) = p.default.clone() else {
            continue;
        };
        // Already a frozen compiler temp — leave it.
        if let ast::ExprKind::Name(n) = &d.kind
            && n.starts_with('.')
        {
            continue;
        }
        // Literals need no freeze: re-lowering at any call site is identical.
        if default_is_literal(&d) {
            continue;
        }
        let v = lower_expr(&d, ctx)?;
        let v = coerce(v, p.ty, d.span, "default argument")?;
        let t = ctx.fresh_temp(&format!("dflt.{}", p.name), p.ty);
        out.push(ir::Stmt::Assign {
            name: t.clone(),
            value: v,
        });
        p.default = Some(ast::Expr {
            kind: ast::ExprKind::Name(t),
            span: d.span,
        });
        changed = true;
    }
    if !changed {
        return Ok(());
    }
    let Some(info) = ctx.nested_funcs.get_mut(user_name) else {
        return Err(err(
            "internal: nested fn missing during default freeze",
            span,
        ));
    };
    info.sig.params = new_params;
    register_closure_defaults(&info.ir_name, &info.sig.params);
    Ok(())
}

fn default_is_literal(e: &ast::Expr) -> bool {
    match &e.kind {
        ast::ExprKind::Int(_)
        | ast::ExprKind::IntDigits(_)
        | ast::ExprKind::Float(_)
        | ast::ExprKind::Bool(_)
        | ast::ExprKind::Str(_)
        | ast::ExprKind::NoneLit => true,
        ast::ExprKind::Unary {
            op: ast::UnaryOp::Neg | ast::UnaryOp::Not,
            operand,
        } => default_is_literal(operand),
        _ => false,
    }
}

/// Lower a nested/closure default at a call site. Frozen free-var temps
/// (`.dflt.*`) are only valid in the defining outer frame; give a clear
/// diagnostic when they escape (multi-level / returned closures).
fn lower_closure_default(
    d: &ast::Expr,
    ty: ir::Ty,
    call_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if let ast::ExprKind::Name(n) = &d.kind
        && n.starts_with(".dflt.")
        && !ctx.locals.contains_key(n)
    {
        return Err(err(
            "default argument that captures free variables cannot be used after \
             the defining function returns; use a constant default, or call the \
             nested function while still inside that scope",
            d.span,
        ));
    }
    let v = match lower_expr(d, ctx) {
        Ok(v) => v,
        Err(e) if e.message.contains("is not defined") && !default_is_literal(d) => {
            return Err(err(
                "default argument that captures free variables cannot be used after \
                 the defining function returns; use a constant default, or call the \
                 nested function while still inside that scope",
                d.span,
            ));
        }
        Err(e) => return Err(e),
    };
    let _ = call_span;
    coerce(v, ty, d.span, "default argument")
}

fn first_return_ty(stmts: &[ir::Stmt]) -> Option<ir::Ty> {
    for s in stmts {
        match s {
            ir::Stmt::Return(Some(e)) => return Some(e.ty),
            ir::Stmt::If { branches, orelse } => {
                for (_, b) in branches {
                    if let Some(t) = first_return_ty(b) {
                        return Some(t);
                    }
                }
                if let Some(t) = first_return_ty(orelse) {
                    return Some(t);
                }
            }
            _ => {}
        }
    }
    None
}

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
enum ImportBinding {
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
struct ModuleData {
    funcs: HashMap<String, FuncSig>,
    globals: HashMap<String, ir::Ty>,
    /// User classes defined in this module (short name → id).
    classes: HashMap<String, ir::ClassId>,
    /// Names bound by `from … import` into this module: local → (origin module,
    /// origin name). Attribute loads and calls use the origin IR symbol.
    /// Local assignments in this module win over re-exports of the same name.
    reexports: HashMap<String, (String, String)>,
}

/// Read-only per-module context threaded into every function lowering.
struct ModuleCtx<'a> {
    /// The module's own name (`""`-prefixed for the root, `"M."` otherwise
    /// via [`ModuleCtx::prefix`]).
    module: &'a str,
    is_root: bool,
    funcs: &'a HashMap<String, FuncSig>,
    imports: &'a HashMap<String, ImportBinding>,
    /// Fully analyzed dependency modules, keyed by name.
    mods: &'a HashMap<String, ModuleData>,
    /// Parent module → (child short name → fully-qualified child name) for
    /// every module in the program (built before lowering).
    submodules: &'a HashMap<String, HashMap<String, String>>,
    /// Partial package init: parent → child → (name → ty) for simple
    /// assignments in the parent **before** the import that loads the child.
    partial_prelim: &'a HashMap<String, HashMap<String, HashMap<String, ir::Ty>>>,
    /// Func names defined in parent **before** the import that loads the child
    /// (parent → child → names). Visible at child module top level mid-init.
    partial_funcs: &'a HashMap<String, HashMap<String, HashSet<String>>>,
    /// Full simple-assign surface of each package (entire body). Used for
    /// **deferred** parent attribute loads inside child function bodies.
    package_final_values: &'a HashMap<String, HashMap<String, ir::Ty>>,
    /// Own function tables (all modules). Deferred parent calls use these
    /// when the parent is not yet in `mods`.
    all_own_funcs: &'a HashMap<String, HashMap<String, FuncSig>>,
    /// Last top-level export kind per module (Module vs Symbol).
    last_exports: &'a HashMap<String, HashMap<String, LastExport>>,
    /// `from … import` value/function re-export origins per module:
    /// local name → (origin module, origin name). Used for deferred parent
    /// access while the parent package is not yet in `mods`.
    reexport_origins: &'a HashMap<String, HashMap<String, (String, String)>>,
    /// Re-export names bound on parent before each child load (mid-init hasattr).
    partial_reexports: &'a HashMap<String, HashMap<String, HashSet<String>>>,
}

impl ModuleCtx<'_> {
    /// The IR name prefix for this module's own symbols. The root keeps
    /// bare names (`x`, `foo`); other modules are namespaced (`utils.x`).
    fn prefix(&self) -> String {
        if self.is_root {
            String::new()
        } else {
            format!("{}.", self.module)
        }
    }

    /// Names visible on `parent` while lowering `self.module` under partial init
    /// (child **module body** only — names assigned before the child-loading import).
    fn partial_parent_globals(&self, parent: &str) -> Option<&HashMap<String, ir::Ty>> {
        self.partial_prelim
            .get(parent)
            .and_then(|by_child| by_child.get(self.module))
    }

    /// Parent function names defined before this child was loaded (mid-init module body).
    fn partial_parent_funcs(&self, parent: &str) -> Option<&HashSet<String>> {
        self.partial_funcs
            .get(parent)
            .and_then(|by_child| by_child.get(self.module))
    }

    /// Parent re-export names bound before this child was loaded.
    fn partial_parent_reexports(&self, parent: &str) -> Option<&HashSet<String>> {
        self.partial_reexports
            .get(parent)
            .and_then(|by_child| by_child.get(self.module))
    }
}

/// The IR/emit name of `name` defined in module `module` (always
/// namespaced — only the root is bare, handled by the caller).
fn qual(module: &str, name: &str) -> String {
    format!("{module}.{name}")
}

/// The builtins that cannot be shadowed by a user `def`.
const BUILTINS: [&str; 17] = [
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
];

/// A call to a module's run-once init function, `<mod>.__init__()`.
fn init_call(module: &str) -> ir::Stmt {
    ir::Stmt::ExprStmt(ir::Expr {
        ty: ir::Ty::None,
        kind: ir::ExprKind::Call {
            func: qual(module, "__init__"),
            args: vec![],
        },
    })
}

/// Init calls for `module` and every parent package (`pkg` then `pkg.mod`).
fn init_calls_for(module: &str) -> Vec<ir::Stmt> {
    let parts: Vec<&str> = module.split('.').collect();
    let mut out = Vec::with_capacity(parts.len());
    for i in 1..=parts.len() {
        out.push(init_call(&parts[..i].join(".")));
    }
    out
}

/// Top-level name bound by `import a.b.c` (without `as`): `a`.
fn import_bind_name(module: &str, alias: &Option<String>) -> String {
    alias
        .clone()
        .unwrap_or_else(|| module.split('.').next().unwrap_or(module).to_string())
}

/// Module object referred to by `import a.b.c` / `import a.b.c as x`.
/// Without alias, the local name is the top-level package `a`.
fn import_bound_module(module: &str, alias: &Option<String>) -> String {
    if alias.is_some() {
        module.to_string()
    } else {
        module.split('.').next().unwrap_or(module).to_string()
    }
}

/// Build parent → child short name → full name for all modules in the program.
fn build_submodule_map(module_names: &[String]) -> HashMap<String, HashMap<String, String>> {
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
fn is_strict_package_prefix(parent: &str, child: &str) -> bool {
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
fn resolve_module_path(expr: &ast::Expr, ctx: &FnCtx) -> Option<String> {
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

/// Analyze a single module (no file imports beyond `sys`). Used by tests
/// and by the driver for single-file programs.
pub fn analyze(module: &ast::Module) -> SResult<ir::Module> {
    analyze_program(&[ModuleInput {
        name: ENTRY_NAME.to_string(),
        ast: module,
    }])
}

/// One method extracted from a class body.
struct ClassMethodAst<'a> {
    def: &'a ast::FuncDef,
}

/// A class definition found at module top level.
struct ClassAst<'a> {
    module: String,
    is_root: bool,
    name: String,
    bases: Vec<(String, Span)>,
    methods: Vec<ClassMethodAst<'a>>,
    /// Class-body annotated attrs: name → type annotation.
    class_attrs: Vec<(String, ast::TypeName, Span)>,
    span: Span,
}

fn collect_class_asts<'a>(modules: &'a [ModuleInput<'a>]) -> SResult<Vec<ClassAst<'a>>> {
    let root_idx = modules.len() - 1;
    let mut out = Vec::new();
    for (i, m) in modules.iter().enumerate() {
        let is_root = i == root_idx;
        for stmt in &m.ast.body {
            if let ast::StmtKind::ClassDef(c) = &stmt.kind {
                if c.bases.len() > 1 {
                    return Err(err(
                        "multiple inheritance is not supported yet (single base only)",
                        c.bases.get(1).map(|(_, s)| *s).unwrap_or(c.span),
                    )
                    .with_file(i));
                }
                let mut methods = Vec::new();
                let class_attrs = Vec::new();
                let mut seen_methods = HashSet::new();
                for b in &c.body {
                    match &b.kind {
                        ast::StmtKind::FuncDef(f) => {
                            if !seen_methods.insert(f.name.clone()) {
                                return Err(err(
                                    format!(
                                        "method '{}' is defined more than once in class '{}'",
                                        f.name, c.name
                                    ),
                                    f.span,
                                )
                                .with_file(i));
                            }
                            if f.name == "__new__" {
                                return Err(
                                    err("__new__ is not supported yet", f.span).with_file(i)
                                );
                            }
                            methods.push(ClassMethodAst { def: f });
                        }
                        ast::StmtKind::Pass => {}
                        ast::StmtKind::Assign { .. } => {
                            // Class-body attributes with defaults would leave zeroed
                            // storage (untagged int 0 → SEGV). Reject until defaults
                            // are applied at NewObject. Fields belong in __init__.
                            return Err(err(
                                "class body attributes are not supported yet \
                                 (assign fields in __init__ with self.x = …)",
                                b.span,
                            )
                            .with_file(i));
                        }
                        ast::StmtKind::ExprStmt(e) if matches!(e.kind, ast::ExprKind::Str(_)) => {}
                        _ => {
                            return Err(err(
                                "this statement is not supported in a class body yet",
                                b.span,
                            )
                            .with_file(i));
                        }
                    }
                }
                out.push(ClassAst {
                    module: m.name.clone(),
                    is_root,
                    name: c.name.clone(),
                    bases: c.bases.clone(),
                    methods,
                    class_attrs,
                    span: c.span,
                });
            }
        }
    }
    Ok(out)
}

/// Infer field types from `self.attr = expr` assignments. Declaration order
/// is preserved (Vec, not HashMap). Multiple passes refine `self.x = self.y + 1`.
fn collect_self_fields(
    body: &[ast::Stmt],
    self_name: &str,
    param_tys: &HashMap<String, ir::Ty>,
    known_rets: &HashMap<String, ir::Ty>,
    fields: &mut Vec<(String, ir::Ty)>,
) {
    fn field_ty(fields: &[(String, ir::Ty)], name: &str) -> Option<ir::Ty> {
        fields.iter().find(|(n, _)| n == name).map(|(_, t)| *t)
    }
    fn set_field(fields: &mut Vec<(String, ir::Ty)>, name: &str, ty: ir::Ty) {
        if let Some(slot) = fields.iter_mut().find(|(n, _)| n == name) {
            // Keep first type; join if both numeric.
            if slot.1 != ty {
                let j = join_types(slot.1, ty);
                if j != slot.1 && matches!(j, ir::Ty::Int | ir::Ty::Float | ir::Ty::Bool) {
                    slot.1 = j;
                }
            }
        } else {
            fields.push((name.to_string(), ty));
        }
    }
    fn type_field_rhs(
        value: &ast::Expr,
        self_name: &str,
        param_tys: &HashMap<String, ir::Ty>,
        known_rets: &HashMap<String, ir::Ty>,
        fields: &[(String, ir::Ty)],
    ) -> Option<ir::Ty> {
        if let Some(t) = try_type_ast_expr(value, param_tys, known_rets) {
            return Some(t);
        }
        match &value.kind {
            // List / tuple / dict / set literals (list already partially in try_type).
            ast::ExprKind::ListLit(items) if !items.is_empty() => {
                let mut elem: Option<ir::Ty> = None;
                for it in items {
                    let e = match it {
                        ast::ListElem::Item(e) => e,
                        ast::ListElem::Star(_) => return None,
                    };
                    let t = type_field_rhs(e, self_name, param_tys, known_rets, fields)?;
                    elem = Some(match elem {
                        None => t,
                        Some(prev) => join_types(prev, t),
                    });
                }
                Some(ir::list_of(elem?))
            }
            ast::ExprKind::TupleLit(items) => {
                let mut ts = Vec::new();
                for it in items {
                    ts.push(type_field_rhs(
                        it, self_name, param_tys, known_rets, fields,
                    )?);
                }
                Some(ir::tuple_of(&ts))
            }
            ast::ExprKind::DictLit(items) if !items.is_empty() => {
                let mut key_ty: Option<ir::Ty> = None;
                let mut val_ty: Option<ir::Ty> = None;
                for (k, v) in items {
                    let kt = type_field_rhs(k, self_name, param_tys, known_rets, fields)?;
                    let vt = type_field_rhs(v, self_name, param_tys, known_rets, fields)?;
                    key_ty = Some(match key_ty {
                        None => kt,
                        Some(prev) => join_types(prev, kt),
                    });
                    val_ty = Some(match val_ty {
                        None => vt,
                        Some(prev) => join_types(prev, vt),
                    });
                }
                Some(ir::dict_of(key_ty?, val_ty?))
            }
            ast::ExprKind::SetLit(items) if !items.is_empty() => {
                let mut elem: Option<ir::Ty> = None;
                for it in items {
                    let t = type_field_rhs(it, self_name, param_tys, known_rets, fields)?;
                    elem = Some(match elem {
                        None => t,
                        Some(prev) => join_types(prev, t),
                    });
                }
                Some(ir::set_of(elem?))
            }
            // self.attr already known as a field.
            ast::ExprKind::Attribute { base, attr, .. } => {
                if let ast::ExprKind::Name(n) = &base.kind
                    && n == self_name
                {
                    return field_ty(fields, attr);
                }
                None
            }
            // Binary using known fields / params.
            ast::ExprKind::Binary { op, left, right } => {
                use ast::BinOp::*;
                let l = type_field_rhs(left, self_name, param_tys, known_rets, fields)?;
                let r = type_field_rhs(right, self_name, param_tys, known_rets, fields)?;
                match op {
                    Eq | NotEq | Lt | LtEq | Gt | GtEq | Is | IsNot | In | NotIn => {
                        Some(ir::Ty::Bool)
                    }
                    And | Or => Some(join_types(l, r)),
                    Div => Some(ir::Ty::Float),
                    Add | Sub | Mul | FloorDiv | Mod | BitAnd | BitOr | BitXor | LShift
                    | RShift | Pow => {
                        let l_num = matches!(l, ir::Ty::Bool | ir::Ty::Int | ir::Ty::Float);
                        let r_num = matches!(r, ir::Ty::Bool | ir::Ty::Int | ir::Ty::Float);
                        if l_num && r_num {
                            Some(join_types(l, r))
                        } else if *op == Add && l == ir::Ty::Str && r == ir::Ty::Str {
                            Some(ir::Ty::Str)
                        } else if l == r {
                            Some(l)
                        } else {
                            None
                        }
                    }
                }
            }
            ast::ExprKind::Call { func, .. } => match known_rets.get(func).copied() {
                Some(ir::Ty::None) => None,
                Some(t) => Some(t),
                None => None,
            },
            // self.method() when method return type is known.
            ast::ExprKind::MethodCall { base, method, .. } => {
                if let ast::ExprKind::Name(n) = &base.kind
                    && n == self_name
                {
                    return match known_rets.get(method).copied() {
                        Some(ir::Ty::None) => None,
                        Some(t) => Some(t),
                        None => None,
                    };
                }
                None
            }
            _ => None,
        }
    }
    fn walk(
        body: &[ast::Stmt],
        self_name: &str,
        param_tys: &HashMap<String, ir::Ty>,
        known_rets: &HashMap<String, ir::Ty>,
        fields: &mut Vec<(String, ir::Ty)>,
    ) {
        for st in body {
            match &st.kind {
                ast::StmtKind::Assign { targets, value, .. } => {
                    for t in targets {
                        if let ast::AssignTarget::Attr { base, attr, .. } = t
                            && let ast::ExprKind::Name(n) = &base.kind
                            && n == self_name
                            && let Some(ty) =
                                type_field_rhs(value, self_name, param_tys, known_rets, fields)
                        {
                            set_field(fields, attr, ty);
                        }
                    }
                }
                ast::StmtKind::If { branches, orelse } => {
                    for (_, b) in branches {
                        walk(b, self_name, param_tys, known_rets, fields);
                    }
                    walk(orelse, self_name, param_tys, known_rets, fields);
                }
                ast::StmtKind::While { body, orelse, .. }
                | ast::StmtKind::For { body, orelse, .. } => {
                    walk(body, self_name, param_tys, known_rets, fields);
                    walk(orelse, self_name, param_tys, known_rets, fields);
                }
                ast::StmtKind::Try {
                    body,
                    handlers,
                    orelse,
                    finally,
                } => {
                    walk(body, self_name, param_tys, known_rets, fields);
                    for h in handlers {
                        walk(&h.body, self_name, param_tys, known_rets, fields);
                    }
                    walk(orelse, self_name, param_tys, known_rets, fields);
                    walk(finally, self_name, param_tys, known_rets, fields);
                }
                ast::StmtKind::With { body, .. } => {
                    walk(body, self_name, param_tys, known_rets, fields);
                }
                ast::StmtKind::Match { cases, .. } => {
                    for c in cases {
                        walk(&c.body, self_name, param_tys, known_rets, fields);
                    }
                }
                _ => {}
            }
        }
    }
    // Fixed-point: self.x = self.y + 1 needs y first.
    for _ in 0..8 {
        let before = fields.len();
        walk(body, self_name, param_tys, known_rets, fields);
        if fields.len() == before {
            // Also re-walk once more for type refinements on existing fields.
            let snapshot = fields.clone();
            walk(body, self_name, param_tys, known_rets, fields);
            if *fields == snapshot {
                break;
            }
        }
    }
}

/// Pass A: assign ClassIds only (no base resolution yet).
fn register_class_ids(classes: &[ClassAst<'_>]) -> SResult<()> {
    clear_class_env();
    with_class_env_mut(|env| {
        for c in classes {
            let id = env.infos.len() as ir::ClassId;
            let display = if c.is_root || c.module == ENTRY_NAME {
                c.name.clone()
            } else {
                format!("{}.{}", c.module, c.name)
            };
            env.infos.push(ir::ClassInfo {
                id,
                name: display,
                parent: None,
                fields: vec![],
                methods: vec![],
            });
            env.module_of.insert(id, c.module.clone());
            env.by_key.insert((c.module.clone(), c.name.clone()), id);
        }
    });
    let mut seen: HashMap<(String, String), Span> = HashMap::new();
    for c in classes {
        let key = (c.module.clone(), c.name.clone());
        if seen.insert(key, c.span).is_some() {
            return Err(err(
                format!("class '{}' is defined more than once", c.name),
                c.span,
            ));
        }
    }
    Ok(())
}

/// Scan module top-level `from m import Name` and inject class aliases into
/// ClassEnv so bare names resolve for bases and annotations before full
/// import lowering. Relative imports are skipped here (resolved later).
fn inject_class_import_aliases(modules: &[ModuleInput<'_>]) {
    for m in modules {
        for stmt in &m.ast.body {
            let ast::StmtKind::FromImport {
                module: src,
                names,
                star,
                level,
                ..
            } = &stmt.kind
            else {
                continue;
            };
            if *star || *level != 0 || src.is_empty() || src == "sys" {
                continue;
            }
            for (name, alias, _) in names {
                let Some(id) = lookup_class_in_module(src, name) else {
                    continue;
                };
                let local = alias.clone().unwrap_or_else(|| name.clone());
                with_class_env_mut(|e| {
                    e.by_key.entry((m.name.clone(), local)).or_insert(id);
                });
            }
        }
    }
}

/// Pass B: resolve single bases (same module, import aliases, unique global).
fn resolve_class_bases(classes: &[ClassAst<'_>]) -> SResult<()> {
    for c in classes {
        set_class_current_module(&c.module);
        let Some((base_name, base_span)) = c.bases.first() else {
            continue;
        };
        // 1) same module  2) bare alias in current module (imports)  3) unique global
        let parent_id = lookup_class_in_module(&c.module, base_name)
            .or_else(|| lookup_class(base_name))
            .or_else(|| {
                // Unique class of this short name across the program.
                with_class_env(|e| {
                    let hits: Vec<_> = e
                        .by_key
                        .iter()
                        .filter(|((_, n), _)| n == base_name)
                        .map(|(_, id)| *id)
                        .collect();
                    if hits.len() == 1 { Some(hits[0]) } else { None }
                })
            })
            .ok_or_else(|| {
                err(
                    format!(
                        "unknown base class '{base_name}' \
                         (import it first, e.g. 'from mod import {base_name}')"
                    ),
                    *base_span,
                )
            })?;
        let child_id = lookup_class_in_module(&c.module, &c.name).unwrap();
        if parent_id == child_id {
            return Err(err(
                format!("class '{}' cannot inherit from itself", c.name),
                c.span,
            ));
        }
        if class_is_subclass(parent_id, child_id) {
            return Err(err(
                format!("inheritance cycle involving class '{}'", c.name),
                c.span,
            ));
        }
        with_class_env_mut(|env| {
            if let Some(info) = env.infos.get_mut(child_id as usize) {
                info.parent = Some(parent_id);
            }
        });
    }
    Ok(())
}

/// Pre-infer a method's return type from annotation or simple body returns
/// (used for field discovery before method_func_sig runs).
fn pre_infer_method_ret(f: &ast::FuncDef) -> Option<ir::Ty> {
    if f.name == "__init__" {
        return Some(ir::Ty::None);
    }
    if let Some(t) = f.ret {
        return resolve_type_checked(t, f.span).ok();
    }
    // Lightweight param map for typing `return x` when x is a param.
    let mut params = HashMap::new();
    for (i, p) in f.params.iter().enumerate() {
        if i == 0 {
            continue; // self
        }
        if let Ok(Some(ty)) = resolve_param_ty_opt(p) {
            params.insert(p.name.clone(), ty);
        }
    }
    try_infer_ret_from_ast_body(&f.body, &params, &HashMap::new()).filter(|t| *t != ir::Ty::None)
}

/// Scan top-level free `def` returns for field-RHS `self.x = make()` typing.
fn pre_infer_free_func_rets(modules: &[ModuleInput<'_>]) -> HashMap<String, ir::Ty> {
    let mut out = HashMap::new();
    for m in modules {
        set_class_current_module(&m.name);
        for stmt in &m.ast.body {
            let ast::StmtKind::FuncDef(f) = &stmt.kind else {
                continue;
            };
            let ty = if let Some(t) = f.ret {
                resolve_type_checked(t, f.span).ok()
            } else {
                let mut params = HashMap::new();
                for p in &f.params {
                    if let Ok(Some(ty)) = resolve_param_ty_opt(p) {
                        params.insert(p.name.clone(), ty);
                    }
                }
                try_infer_ret_from_ast_body(&f.body, &params, &HashMap::new())
                    .filter(|t| *t != ir::Ty::None)
            };
            if let Some(ty) = ty {
                out.entry(f.name.clone()).or_insert(ty);
                if m.name != ENTRY_NAME {
                    out.entry(format!("{}.{}", m.name, f.name)).or_insert(ty);
                }
            }
        }
    }
    out
}

/// Pass C: fields + methods (topo: parents first). Call after bases resolved.
fn finalize_class_layouts(
    classes: &[ClassAst<'_>],
    free_func_rets: &HashMap<String, ir::Ty>,
) -> SResult<()> {
    let mut order: Vec<usize> = (0..classes.len()).collect();
    order.sort_by_key(|&i| {
        let id = lookup_class_in_module(&classes[i].module, &classes[i].name).unwrap();
        let mut depth = 0u32;
        let mut cur = class_info(id).and_then(|c| c.parent);
        while let Some(p) = cur {
            depth += 1;
            cur = class_info(p).and_then(|c| c.parent);
            if depth > 64 {
                break;
            }
        }
        depth
    });

    for &idx in &order {
        let c = &classes[idx];
        set_class_current_module(&c.module);
        let id = lookup_class_in_module(&c.module, &c.name).unwrap();
        let parent = class_info(id).and_then(|i| i.parent);

        // Class-body annotated attributes (defaults rejected in collect_class_asts).
        let mut own_fields: Vec<(String, ir::Ty)> = Vec::new();
        let mut own_field_set: HashSet<String> = HashSet::new();
        for (name, ann, span) in &c.class_attrs {
            let ty = resolve_type_checked(*ann, *span)?;
            if own_field_set.insert(name.clone()) {
                own_fields.push((name.clone(), ty));
            }
        }

        // Fields from __init__ self-assignments (declaration order).
        if let Some(init) = c.methods.iter().find(|m| m.def.name == "__init__") {
            let self_name = init
                .def
                .params
                .first()
                .map(|p| p.name.as_str())
                .unwrap_or("self");
            let mut param_tys: HashMap<String, ir::Ty> = HashMap::new();
            param_tys.insert(self_name.to_string(), ir::Ty::Class(id));
            for p in init.def.params.iter().skip(1) {
                if let Ok(Some(ty)) = resolve_param_ty_opt(p) {
                    param_tys.insert(p.name.clone(), ty);
                }
            }
            // Known returns for self.m() / make() field RHS.
            // 1) This class's AST methods first (methods not yet in ClassInfo).
            // 2) Parent methods via ClassInfo / AST.
            // 3) Free module functions (pre-scanned into free_func_rets).
            let mut known_rets: HashMap<String, ir::Ty> = free_func_rets.clone();
            for m in &c.methods {
                if m.def.name == "__init__" {
                    continue;
                }
                if let Some(ty) = pre_infer_method_ret(m.def) {
                    known_rets.entry(m.def.name.clone()).or_insert(ty);
                }
            }
            let mut walk_cls = parent;
            while let Some(cid) = walk_cls {
                if let Some(info) = class_info(cid) {
                    for (mname, ir_name) in &info.methods {
                        if let Some(sig) = method_sig_lookup(ir_name) {
                            known_rets.entry(mname.clone()).or_insert(sig.ret);
                        } else if let Some(cm) = classes
                            .iter()
                            .find(|x| lookup_class_in_module(&x.module, &x.name) == Some(cid))
                            .and_then(|x| x.methods.iter().find(|mm| mm.def.name == *mname))
                            && let Some(ty) = pre_infer_method_ret(cm.def)
                        {
                            known_rets.entry(mname.clone()).or_insert(ty);
                        }
                    }
                    walk_cls = info.parent;
                } else {
                    break;
                }
            }
            // Seed parent + annotated fields for typing RHS (self.y + 1).
            let mut typing_fields: Vec<(String, ir::Ty)> = Vec::new();
            if let Some(pid) = parent
                && let Some(pinfo) = class_info(pid)
            {
                typing_fields.extend(pinfo.fields.iter().cloned());
            }
            typing_fields.extend(own_fields.iter().cloned());
            collect_self_fields(
                &init.def.body,
                self_name,
                &param_tys,
                &known_rets,
                &mut typing_fields,
            );
            let parent_names: HashSet<String> = parent
                .and_then(class_info)
                .map(|p| p.fields.iter().map(|(n, _)| n.clone()).collect())
                .unwrap_or_default();
            for (n, t) in typing_fields {
                if parent_names.contains(&n) {
                    continue;
                }
                if own_field_set.insert(n.clone()) {
                    own_fields.push((n, t));
                }
            }
        }

        // Layout: parent fields then own. Subclass cannot change parent field types.
        let mut fields = Vec::new();
        let mut field_names: HashSet<String> = HashSet::new();
        if let Some(pid) = parent
            && let Some(pinfo) = class_info(pid)
        {
            for (n, t) in pinfo.fields {
                field_names.insert(n.clone());
                fields.push((n, t));
            }
        }
        for (n, t) in own_fields {
            if field_names.contains(&n) {
                let parent_ty = fields.iter().find(|(fnm, _)| fnm == &n).map(|(_, ty)| *ty);
                if parent_ty != Some(t) {
                    return Err(err(
                        format!(
                            "class '{}' cannot change type of inherited field '{n}' \
                             (parent has {}, subclass assigns {t})",
                            c.name,
                            parent_ty
                                .map(|t| t.to_string())
                                .unwrap_or_else(|| "?".into())
                        ),
                        c.span,
                    ));
                }
                // Same type: keep parent slot.
                continue;
            }
            field_names.insert(n.clone());
            fields.push((n, t));
        }

        let mut methods = Vec::new();
        for m in &c.methods {
            let ir_name = method_ir_name(&c.module, c.is_root, &c.name, &m.def.name);
            methods.push((m.def.name.clone(), ir_name));
        }

        with_class_env_mut(|env| {
            if let Some(info) = env.infos.get_mut(id as usize) {
                info.fields = fields;
                info.methods = methods;
            }
        });
    }
    Ok(())
}

/// Build FuncSig for a class method (self typed as the class instance).
fn method_func_sig(
    class_id: ir::ClassId,
    class_short_name: &str,
    f: &ast::FuncDef,
    kind: MethodKind,
) -> SResult<FuncSig> {
    let mut formals = f.params.clone();
    let mut params;
    match kind {
        MethodKind::Static => {
            // No implicit self — all params are user params.
            params = resolve_params_with_body_infer(&formals, &f.body)?;
        }
        MethodKind::Class => {
            if formals.is_empty() {
                return Err(err(
                    format!("classmethod '{}' must have a 'cls' parameter", f.name),
                    f.span,
                ));
            }
            // First param is the class marker (typed as the class for construct).
            formals[0].ty = Some(ast::TypeName::Class(Box::leak(
                class_short_name.to_string().into_boxed_str(),
            )));
            params = resolve_params_with_body_infer(&formals, &f.body)?;
            params[0].ty = ir::Ty::Class(class_id);
        }
        MethodKind::Instance | MethodKind::Property => {
            if formals.is_empty() {
                return Err(err(
                    format!("instance method '{}' must have a 'self' parameter", f.name),
                    f.span,
                ));
            }
            // First param is always the instance (annotation optional / overridden).
            formals[0].ty = Some(ast::TypeName::Class(Box::leak(
                class_short_name.to_string().into_boxed_str(),
            )));
            params = resolve_params_with_body_infer(&formals, &f.body)?;
            params[0].ty = ir::Ty::Class(class_id);
            if kind == MethodKind::Property && params.len() != 1 {
                return Err(err(
                    format!("@property '{}' must take only self", f.name),
                    f.span,
                ));
            }
        }
    }

    let vararg = if let Some(p) = &f.vararg {
        let ty = resolve_param_ty(p)?;
        Some(ParamSig {
            name: p.name.clone(),
            ty,
            default: None,
        })
    } else {
        None
    };
    let kwarg = if let Some(p) = &f.kwarg {
        let ty = resolve_param_ty(p)?;
        Some(ParamSig {
            name: p.name.clone(),
            ty,
            default: None,
        })
    } else {
        None
    };
    let mut ret = match f.ret {
        Some(t) => resolve_type_checked(t, f.span)?,
        Option::None => ir::Ty::None,
    };
    // __init__ always returns None (CPython TypeError on non-None).
    if f.name == "__init__" {
        if ret != ir::Ty::None {
            return Err(err("__init__ must return None", f.span));
        }
        ret = ir::Ty::None;
        // Reject bare `return <non-None>` in body (annotation path already None).
        if init_body_returns_non_none(&f.body) {
            return Err(err(
                "__init__ should return None, not an explicit value \
                 (returning a non-None value is not supported)",
                f.span,
            ));
        }
    } else if f.name == "__str__" || f.name == "__repr__" {
        // Protocol methods must return str (reject at definition time).
        if f.ret.is_none() {
            // Infer from body when possible; still require Str.
            let param_map: HashMap<String, ir::Ty> =
                params.iter().map(|p| (p.name.clone(), p.ty)).collect();
            if let Some(ty) = try_infer_ret_from_ast_body(&f.body, &param_map, &HashMap::new()) {
                ret = ty;
            }
        }
        if ret != ir::Ty::Str {
            return Err(err(format!("{} must return str", f.name), f.span));
        }
    } else if f.ret.is_none() {
        // Pre-infer unannotated returns from body (same as free functions).
        let param_map: HashMap<String, ir::Ty> =
            params.iter().map(|p| (p.name.clone(), p.ty)).collect();
        if let Some(ty) = try_infer_ret_from_ast_body(&f.body, &param_map, &HashMap::new())
            && ty != ir::Ty::None
        {
            ret = ty;
        }
    }
    let is_generator = stmts_have_yield(&f.body);
    if is_generator {
        return Err(err("generator methods are not supported yet", f.span));
    }
    Ok(FuncSig {
        params,
        vararg,
        kwarg,
        ret,
        span: f.span,
        is_generator: false,
        yield_ty: None,
        gen_frame_slots: 0,
    })
}

/// True if any `return expr` in `body` is clearly non-None (literals / names).
fn init_body_returns_non_none(body: &[ast::Stmt]) -> bool {
    fn walk(stmts: &[ast::Stmt]) -> bool {
        for st in stmts {
            match &st.kind {
                ast::StmtKind::Return(Some(e)) => match &e.kind {
                    ast::ExprKind::NoneLit => {}
                    // Any other explicit return value is non-None for our purposes.
                    _ => return true,
                },
                ast::StmtKind::If { branches, orelse } => {
                    for (_, b) in branches {
                        if walk(b) {
                            return true;
                        }
                    }
                    if walk(orelse) {
                        return true;
                    }
                }
                ast::StmtKind::While { body, orelse, .. }
                | ast::StmtKind::For { body, orelse, .. } => {
                    if walk(body) || walk(orelse) {
                        return true;
                    }
                }
                ast::StmtKind::Try {
                    body,
                    handlers,
                    orelse,
                    finally,
                } => {
                    if walk(body)
                        || handlers.iter().any(|h| walk(&h.body))
                        || walk(orelse)
                        || walk(finally)
                    {
                        return true;
                    }
                }
                ast::StmtKind::With { body, .. } => {
                    if walk(body) {
                        return true;
                    }
                }
                ast::StmtKind::Match { cases, .. } if cases.iter().any(|c| walk(&c.body)) => {
                    return true;
                }
                _ => {}
            }
        }
        false
    }
    walk(body)
}

/// Reject overrides that change arity (user params) or return type vs parent.
fn check_override_compatibility(classes: &[ClassAst<'_>]) -> SResult<()> {
    for c in classes {
        set_class_current_module(&c.module);
        let id = lookup_class_in_module(&c.module, &c.name).unwrap();
        let Some(parent) = class_info(id).and_then(|i| i.parent) else {
            continue;
        };
        for m in &c.methods {
            if m.def.name == "__init__" {
                continue; // __init__ override is free (different construction args)
            }
            let child_ir = method_ir_name(&c.module, c.is_root, &c.name, &m.def.name);
            let Some(child_sig) = method_sig_lookup(&child_ir) else {
                continue;
            };
            // Find parent method IR name.
            let Some(parent_ir) = resolve_method(parent, &m.def.name) else {
                continue;
            };
            // If resolve_method found the child's own method, skip (no parent def).
            if parent_ir == child_ir {
                continue;
            }
            let Some(parent_sig) = method_sig_lookup(&parent_ir) else {
                continue;
            };
            // Compare user params (skip self) and return type.
            let c_user = &child_sig.params[1.min(child_sig.params.len())..];
            let p_user = &parent_sig.params[1.min(parent_sig.params.len())..];
            if c_user.len() != p_user.len() {
                return Err(err(
                    format!(
                        "method '{}.{}' overrides parent with incompatible arity \
                         (expected {} parameter(s) after self, found {})",
                        c.name,
                        m.def.name,
                        p_user.len(),
                        c_user.len()
                    ),
                    m.def.span,
                ));
            }
            for (a, b) in c_user.iter().zip(p_user.iter()) {
                if a.ty != b.ty {
                    return Err(err(
                        format!(
                            "method '{}.{}' overrides parent with incompatible parameter \
                             type (expected {}, found {})",
                            c.name, m.def.name, b.ty, a.ty
                        ),
                        m.def.span,
                    ));
                }
            }
            if child_sig.ret != parent_sig.ret {
                return Err(err(
                    format!(
                        "method '{}.{}' overrides parent with incompatible return type \
                         (expected {}, found {})",
                        c.name, m.def.name, parent_sig.ret, child_sig.ret
                    ),
                    m.def.span,
                ));
            }
            if child_sig.vararg.is_some() != parent_sig.vararg.is_some()
                || child_sig.kwarg.is_some() != parent_sig.kwarg.is_some()
            {
                return Err(err(
                    format!(
                        "method '{}.{}' overrides parent with incompatible *args/**kwargs",
                        c.name, m.def.name
                    ),
                    m.def.span,
                ));
            }
        }
    }
    Ok(())
}

/// Collect a module's function signatures (with builtin-shadowing and
/// duplicate checks).
fn collect_sigs(module: &ast::Module) -> SResult<(HashMap<String, FuncSig>, Vec<&ast::FuncDef>)> {
    let mut funcs: HashMap<String, FuncSig> = HashMap::new();
    let mut order: Vec<&ast::FuncDef> = Vec::new();
    for stmt in &module.body {
        if let ast::StmtKind::FuncDef(f) = &stmt.kind {
            if BUILTINS.contains(&f.name.as_str()) {
                return Err(err(
                    format!("cannot redefine the builtin '{}'", f.name),
                    f.span,
                ));
            }
            if funcs.contains_key(&f.name) {
                return Err(err(
                    format!("function '{}' is defined more than once", f.name),
                    f.span,
                ));
            }
            let params = resolve_params_with_body_infer(&f.params, &f.body)?;
            let mut seen_names: HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
            let vararg = if let Some(p) = &f.vararg {
                let ty = resolve_param_ty(p)?;
                if ty == ir::Ty::None {
                    return Err(err(
                        format!("*{} cannot have element type None", p.name),
                        p.span,
                    ));
                }
                if !seen_names.insert(p.name.clone()) {
                    return Err(err(
                        format!("duplicate parameter name '{}'", p.name),
                        p.span,
                    ));
                }
                Some(ParamSig {
                    name: p.name.clone(),
                    ty,
                    default: None,
                })
            } else {
                None
            };
            let kwarg = if let Some(p) = &f.kwarg {
                let ty = resolve_param_ty(p)?;
                if ty == ir::Ty::None {
                    return Err(err(
                        format!("**{} cannot have value type None", p.name),
                        p.span,
                    ));
                }
                if !seen_names.insert(p.name.clone()) {
                    return Err(err(
                        format!("duplicate parameter name '{}'", p.name),
                        p.span,
                    ));
                }
                Some(ParamSig {
                    name: p.name.clone(),
                    ty,
                    default: None,
                })
            } else {
                None
            };
            let mut ret = match f.ret {
                Some(t) => resolve_type_checked(t, f.span)?,
                Option::None => ir::Ty::None,
            };
            let is_generator = stmts_have_yield(&f.body);
            let yield_ty = if is_generator {
                // Prefer annotated return type as the yield element when present;
                // otherwise default to int (refined poorly; yield sites coerce).
                let y = if ret != ir::Ty::None {
                    ret
                } else {
                    ir::Ty::Int
                };
                ret = ir::generator_of(y);
                Some(y)
            } else {
                None
            };
            funcs.insert(
                f.name.clone(),
                FuncSig {
                    params,
                    vararg,
                    kwarg,
                    ret,
                    span: f.span,
                    is_generator,
                    yield_ty,
                    gen_frame_slots: 0,
                },
            );
            order.push(f);
        }
    }
    // Pre-infer unannotated returns from simple return exprs so forward
    // references (`def f: return g(..)` before `def g`) see a real ret type.
    pre_infer_module_returns(&order, &mut funcs);
    Ok((funcs, order))
}

/// Fixed-point pre-inference of unannotated top-level return types.
/// Only concrete non-None returns from lightweight AST typing are applied;
/// generators and explicitly annotated rets are left alone.
fn pre_infer_module_returns(order: &[&ast::FuncDef], funcs: &mut HashMap<String, FuncSig>) {
    // Parameter types for each function (for typing `return x` when x is a param).
    let param_maps: HashMap<String, HashMap<String, ir::Ty>> = order
        .iter()
        .filter_map(|f| {
            let sig = funcs.get(&f.name)?;
            let mut m = HashMap::new();
            for p in &sig.params {
                m.insert(p.name.clone(), p.ty);
            }
            if let Some(p) = &sig.vararg {
                m.insert(p.name.clone(), ir::list_of(p.ty));
            }
            if let Some(p) = &sig.kwarg {
                m.insert(p.name.clone(), ir::dict_of(ir::Ty::Str, p.ty));
            }
            Some((f.name.clone(), m))
        })
        .collect();

    let mut changed = true;
    while changed {
        changed = false;
        // Snapshot of currently known rets (including prior pre-infer).
        let known_rets: HashMap<String, ir::Ty> =
            funcs.iter().map(|(n, s)| (n.clone(), s.ret)).collect();
        for f in order {
            if f.ret.is_some() {
                continue; // explicit annotation
            }
            let Some(sig) = funcs.get(&f.name) else {
                continue;
            };
            if sig.is_generator {
                continue;
            }
            // Only refine still-void signatures.
            if sig.ret != ir::Ty::None {
                continue;
            }
            let params = param_maps.get(&f.name).cloned().unwrap_or_default();
            if let Some(ty) = try_infer_ret_from_ast_body(&f.body, &params, &known_rets)
                && ty != ir::Ty::None
                && let Some(sig) = funcs.get_mut(&f.name)
            {
                sig.ret = ty;
                changed = true;
            }
        }
    }
}

/// Scan returns in `body` and join their lightweight types. Returns `None` if
/// no non-None return is found or types cannot be joined consistently.
fn try_infer_ret_from_ast_body(
    body: &[ast::Stmt],
    params: &HashMap<String, ir::Ty>,
    known_rets: &HashMap<String, ir::Ty>,
) -> Option<ir::Ty> {
    let mut rets = Vec::new();
    collect_ast_return_tys(body, params, known_rets, &mut rets);
    let mut acc: Option<ir::Ty> = None;
    for t in rets {
        if t == ir::Ty::None {
            continue;
        }
        acc = Some(match acc {
            None => t,
            Some(prev) => {
                if prev == t {
                    prev
                } else {
                    // Mixed returns: promote numerics, else leave uninferred.
                    let prev_num = matches!(prev, ir::Ty::Bool | ir::Ty::Int | ir::Ty::Float);
                    let t_num = matches!(t, ir::Ty::Bool | ir::Ty::Int | ir::Ty::Float);
                    if prev_num && t_num {
                        join_types(prev, t)
                    } else {
                        return None;
                    }
                }
            }
        });
    }
    acc
}

fn collect_ast_return_tys(
    stmts: &[ast::Stmt],
    params: &HashMap<String, ir::Ty>,
    known_rets: &HashMap<String, ir::Ty>,
    out: &mut Vec<ir::Ty>,
) {
    for st in stmts {
        match &st.kind {
            ast::StmtKind::Return(Some(e)) => {
                if let Some(t) = try_type_ast_expr(e, params, known_rets) {
                    out.push(t);
                }
            }
            ast::StmtKind::Return(None) => out.push(ir::Ty::None),
            ast::StmtKind::If { branches, orelse } => {
                for (_, b) in branches {
                    collect_ast_return_tys(b, params, known_rets, out);
                }
                collect_ast_return_tys(orelse, params, known_rets, out);
            }
            ast::StmtKind::While { body, orelse, .. } | ast::StmtKind::For { body, orelse, .. } => {
                collect_ast_return_tys(body, params, known_rets, out);
                collect_ast_return_tys(orelse, params, known_rets, out);
            }
            ast::StmtKind::Try {
                body,
                handlers,
                orelse,
                finally,
            } => {
                collect_ast_return_tys(body, params, known_rets, out);
                for h in handlers {
                    collect_ast_return_tys(&h.body, params, known_rets, out);
                }
                collect_ast_return_tys(orelse, params, known_rets, out);
                collect_ast_return_tys(finally, params, known_rets, out);
            }
            ast::StmtKind::With { body, .. } => {
                collect_ast_return_tys(body, params, known_rets, out);
            }
            ast::StmtKind::Match { cases, .. } => {
                for c in cases {
                    collect_ast_return_tys(&c.body, params, known_rets, out);
                }
            }
            // Nested defs have their own rets; skip their bodies here.
            _ => {}
        }
    }
}

/// Lightweight expression typing for pre-infer only (literals, params, calls
/// of known functions, simple arithmetic/bool ops). Returns `None` if unknown.
fn try_type_ast_expr(
    e: &ast::Expr,
    params: &HashMap<String, ir::Ty>,
    known_rets: &HashMap<String, ir::Ty>,
) -> Option<ir::Ty> {
    match &e.kind {
        ast::ExprKind::Int(_) | ast::ExprKind::IntDigits(_) => Some(ir::Ty::Int),
        ast::ExprKind::Float(_) => Some(ir::Ty::Float),
        ast::ExprKind::Bool(_) => Some(ir::Ty::Bool),
        ast::ExprKind::Str(_) | ast::ExprKind::JoinedStr(_) => Some(ir::Ty::Str),
        ast::ExprKind::NoneLit => Some(ir::Ty::None),
        ast::ExprKind::Name(n) => params.get(n).copied(),
        ast::ExprKind::Unary {
            op: ast::UnaryOp::Not,
            ..
        } => Some(ir::Ty::Bool),
        ast::ExprKind::Unary {
            op: ast::UnaryOp::Neg | ast::UnaryOp::Invert,
            operand,
        } => try_type_ast_expr(operand, params, known_rets),
        ast::ExprKind::Binary { op, left, right } => {
            use ast::BinOp::*;
            match op {
                Eq | NotEq | Lt | LtEq | Gt | GtEq | Is | IsNot | In | NotIn => Some(ir::Ty::Bool),
                // `and`/`or` yield an operand (join), not always bool.
                And | Or => {
                    let l = try_type_ast_expr(left, params, known_rets)?;
                    let r = try_type_ast_expr(right, params, known_rets)?;
                    Some(join_types(l, r))
                }
                Add | Sub | Mul | FloorDiv | Mod | BitAnd | BitOr | BitXor | LShift | RShift => {
                    let l = try_type_ast_expr(left, params, known_rets)?;
                    let r = try_type_ast_expr(right, params, known_rets)?;
                    let l_num = matches!(l, ir::Ty::Bool | ir::Ty::Int | ir::Ty::Float);
                    let r_num = matches!(r, ir::Ty::Bool | ir::Ty::Int | ir::Ty::Float);
                    if l_num && r_num {
                        Some(join_types(l, r))
                    } else if *op == Add && l == ir::Ty::Str && r == ir::Ty::Str {
                        Some(ir::Ty::Str)
                    } else if l == r {
                        Some(l)
                    } else {
                        None
                    }
                }
                Div => {
                    let _l = try_type_ast_expr(left, params, known_rets)?;
                    let _r = try_type_ast_expr(right, params, known_rets)?;
                    Some(ir::Ty::Float)
                }
                Pow => {
                    let l = try_type_ast_expr(left, params, known_rets)?;
                    let r = try_type_ast_expr(right, params, known_rets)?;
                    if matches!(l, ir::Ty::Float) || matches!(r, ir::Ty::Float) {
                        Some(ir::Ty::Float)
                    } else if matches!(l, ir::Ty::Int | ir::Ty::Bool)
                        && matches!(r, ir::Ty::Int | ir::Ty::Bool)
                    {
                        Some(ir::Ty::Int)
                    } else {
                        None
                    }
                }
            }
        }
        ast::ExprKind::Compare { .. } => Some(ir::Ty::Bool),
        ast::ExprKind::Call { func, .. } => {
            // Free/method known rets, else class constructor.
            match known_rets.get(func).copied() {
                Some(ir::Ty::None) => None,
                Some(t) => Some(t),
                None => lookup_class(func).map(ir::Ty::Class),
            }
        }
        ast::ExprKind::ListLit(items) if !items.is_empty() => {
            let mut elem: Option<ir::Ty> = None;
            for it in items {
                let e = match it {
                    ast::ListElem::Item(e) => e,
                    ast::ListElem::Star(_) => return None,
                };
                let t = try_type_ast_expr(e, params, known_rets)?;
                elem = Some(match elem {
                    None => t,
                    Some(prev) => join_types(prev, t),
                });
            }
            Some(ir::list_of(elem?))
        }
        ast::ExprKind::TupleLit(items) => {
            let mut ts = Vec::new();
            for it in items {
                ts.push(try_type_ast_expr(it, params, known_rets)?);
            }
            Some(ir::tuple_of(&ts))
        }
        ast::ExprKind::DictLit(items) if !items.is_empty() => {
            let mut key_ty: Option<ir::Ty> = None;
            let mut val_ty: Option<ir::Ty> = None;
            for (k, v) in items {
                let kt = try_type_ast_expr(k, params, known_rets)?;
                let vt = try_type_ast_expr(v, params, known_rets)?;
                key_ty = Some(match key_ty {
                    None => kt,
                    Some(prev) => join_types(prev, kt),
                });
                val_ty = Some(match val_ty {
                    None => vt,
                    Some(prev) => join_types(prev, vt),
                });
            }
            Some(ir::dict_of(key_ty?, val_ty?))
        }
        ast::ExprKind::SetLit(items) if !items.is_empty() => {
            let mut elem: Option<ir::Ty> = None;
            for it in items {
                let t = try_type_ast_expr(it, params, known_rets)?;
                elem = Some(match elem {
                    None => t,
                    Some(prev) => join_types(prev, t),
                });
            }
            Some(ir::set_of(elem?))
        }
        ast::ExprKind::Cast { ty, .. } => resolve_type_checked(*ty, e.span).ok(),
        _ => None,
    }
}

/// Infer a monomorphic type for bare parameter `name` from body usage.
/// `bare` are still-unresolved bare params (their placeholders are ignored as
/// evidence). Returns `None` if unconstrained or conflicting.
fn try_infer_param_from_body(
    name: &str,
    body: &[ast::Stmt],
    params: &HashMap<String, ir::Ty>,
    bare: &HashSet<String>,
) -> Option<ir::Ty> {
    let mut constraints: Vec<ir::Ty> = Vec::new();
    collect_param_constraints(name, body, params, bare, &mut constraints);
    if constraints.is_empty() {
        return None;
    }
    let mut acc = constraints[0];
    for &t in &constraints[1..] {
        if t == acc {
            continue;
        }
        let a_num = matches!(acc, ir::Ty::Bool | ir::Ty::Int | ir::Ty::Float);
        let t_num = matches!(t, ir::Ty::Bool | ir::Ty::Int | ir::Ty::Float);
        if a_num && t_num {
            acc = join_types(acc, t);
        } else if matches!((acc, t), (ir::Ty::List(_), ir::Ty::List(_))) && acc == t {
            // same list type
        } else if let (ir::Ty::Class(a), ir::Ty::Class(b)) = (acc, t) {
            // Prefer the common base when one is a subclass of the other.
            if class_is_subclass(a, b) {
                acc = t;
            } else if class_is_subclass(b, a) {
                // keep acc (base or equal)
            } else {
                return None; // unrelated classes
            }
        } else {
            return None; // conflict
        }
    }
    // Reject pure None / unconstrained union as a param type.
    if acc == ir::Ty::None || matches!(acc, ir::Ty::Union(_)) {
        return None;
    }
    Some(acc)
}

fn collect_param_constraints(
    name: &str,
    stmts: &[ast::Stmt],
    params: &HashMap<String, ir::Ty>,
    bare: &HashSet<String>,
    out: &mut Vec<ir::Ty>,
) {
    for st in stmts {
        match &st.kind {
            ast::StmtKind::Return(Some(e))
            | ast::StmtKind::ExprStmt(e)
            | ast::StmtKind::Raise { message: e, .. } => {
                collect_param_constraints_expr(name, e, params, bare, out);
            }
            ast::StmtKind::Assign { value, .. } => {
                collect_param_constraints_expr(name, value, params, bare, out);
            }
            ast::StmtKind::AugAssign { target, value, .. } => {
                if let ast::AssignTarget::Name { name: n, .. } = target
                    && n == name
                {
                    // `x += 1` etc. implies numeric/str depending on RHS.
                    if let Some(t) = try_type_ast_expr(value, params, &HashMap::new()) {
                        if !bare.contains(name) || !matches!(t, ir::Ty::Int) {
                            out.push(t);
                        } else {
                            out.push(t); // x += 1 → int
                        }
                    } else {
                        out.push(ir::Ty::Int);
                    }
                }
                collect_param_constraints_expr(name, value, params, bare, out);
            }
            ast::StmtKind::If { branches, orelse } => {
                for (c, b) in branches {
                    collect_param_constraints_expr(name, c, params, bare, out);
                    collect_param_constraints(name, b, params, bare, out);
                }
                collect_param_constraints(name, orelse, params, bare, out);
            }
            ast::StmtKind::While { cond, body, orelse } => {
                collect_param_constraints_expr(name, cond, params, bare, out);
                collect_param_constraints(name, body, params, bare, out);
                collect_param_constraints(name, orelse, params, bare, out);
            }
            ast::StmtKind::For {
                iter, body, orelse, ..
            } => {
                collect_param_constraints_expr(name, iter, params, bare, out);
                collect_param_constraints(name, body, params, bare, out);
                collect_param_constraints(name, orelse, params, bare, out);
            }
            ast::StmtKind::Try {
                body,
                handlers,
                orelse,
                finally,
            } => {
                collect_param_constraints(name, body, params, bare, out);
                for h in handlers {
                    collect_param_constraints(name, &h.body, params, bare, out);
                }
                collect_param_constraints(name, orelse, params, bare, out);
                collect_param_constraints(name, finally, params, bare, out);
            }
            ast::StmtKind::With { item, body, .. } => {
                collect_param_constraints_expr(name, item, params, bare, out);
                collect_param_constraints(name, body, params, bare, out);
            }
            ast::StmtKind::Match { subject, cases } => {
                collect_param_constraints_expr(name, subject, params, bare, out);
                for c in cases {
                    if let Some(g) = &c.guard {
                        collect_param_constraints_expr(name, g, params, bare, out);
                    }
                    collect_param_constraints(name, &c.body, params, bare, out);
                }
            }
            // Nested defs: only scan defaults, not bodies (own scope).
            _ => {}
        }
    }
}

fn collect_param_constraints_expr(
    name: &str,
    e: &ast::Expr,
    params: &HashMap<String, ir::Ty>,
    bare: &HashSet<String>,
    out: &mut Vec<ir::Ty>,
) {
    match &e.kind {
        ast::ExprKind::Binary { op, left, right } => {
            use ast::BinOp::*;
            let left_is = matches!(&left.kind, ast::ExprKind::Name(n) if n == name);
            let right_is = matches!(&right.kind, ast::ExprKind::Name(n) if n == name);
            if left_is || right_is {
                let other = if left_is {
                    right.as_ref()
                } else {
                    left.as_ref()
                };
                match op {
                    Add | Sub | Mul | FloorDiv | Mod | Div | Pow | BitAnd | BitOr | BitXor
                    | LShift | RShift => {
                        if let Some(ot) = try_type_ast_expr(other, params, &HashMap::new()) {
                            if bare.contains(name)
                                && matches!(&other.kind, ast::ExprKind::Name(n) if bare.contains(n))
                            {
                                // both bare — weak numeric hint
                                out.push(ir::Ty::Int);
                            } else if ot == ir::Ty::Str && *op == Add {
                                out.push(ir::Ty::Str);
                            } else if matches!(ot, ir::Ty::Bool | ir::Ty::Int | ir::Ty::Float) {
                                out.push(if matches!(ot, ir::Ty::Float) {
                                    ir::Ty::Float
                                } else {
                                    ir::Ty::Int
                                });
                            } else if matches!(ot, ir::Ty::List(_)) && matches!(op, Add | Mul) {
                                out.push(ot);
                            } else if ot == ir::Ty::Str && *op == Mul {
                                out.push(ir::Ty::Str);
                            } else {
                                out.push(ot);
                            }
                        } else if matches!(
                            op,
                            Sub | Mul
                                | FloorDiv
                                | Mod
                                | Div
                                | Pow
                                | BitAnd
                                | BitOr
                                | BitXor
                                | LShift
                                | RShift
                        ) {
                            out.push(ir::Ty::Int);
                        } else {
                            // bare `x + y` unknown — prefer int
                            out.push(ir::Ty::Int);
                        }
                    }
                    Lt | LtEq | Gt | GtEq | Eq | NotEq => {
                        if let Some(ot) = try_type_ast_expr(other, params, &HashMap::new())
                            && (!bare.contains(name)
                                || !matches!(&other.kind, ast::ExprKind::Name(n) if bare.contains(n)))
                        {
                            out.push(ot);
                        }
                    }
                    In | NotIn => {
                        if left_is {
                            // name in haystack → element type of haystack
                            if let Some(ht) = try_type_ast_expr(right, params, &HashMap::new()) {
                                match ht {
                                    ir::Ty::List(e) => out.push(*e),
                                    ir::Ty::Set(e) => out.push(*e),
                                    ir::Ty::Dict { key, .. } => out.push(*key),
                                    ir::Ty::Str => out.push(ir::Ty::Str),
                                    _ => {}
                                }
                            }
                        } else if right_is {
                            // needle in name → name is container; weak list[int]
                            if let Some(nt) = try_type_ast_expr(left, params, &HashMap::new()) {
                                out.push(ir::list_of(nt));
                            }
                        }
                    }
                    _ => {}
                }
            }
            collect_param_constraints_expr(name, left, params, bare, out);
            collect_param_constraints_expr(name, right, params, bare, out);
        }
        ast::ExprKind::Unary {
            op: ast::UnaryOp::Neg | ast::UnaryOp::Invert,
            operand,
        } => {
            if matches!(&operand.kind, ast::ExprKind::Name(n) if n == name) {
                out.push(ir::Ty::Int);
            }
            collect_param_constraints_expr(name, operand, params, bare, out);
        }
        ast::ExprKind::Unary { operand, .. } => {
            collect_param_constraints_expr(name, operand, params, bare, out);
        }
        ast::ExprKind::Call { func, args, .. } => {
            if (func == "len" || func == "abs" || func == "sum" || func == "sorted")
                && let Some(ast::PosArg::Pos(ae)) = args.first()
                && matches!(&ae.kind, ast::ExprKind::Name(n) if n == name)
            {
                match func.as_str() {
                    "len" => {
                        // Ambiguous container — do not constrain alone.
                    }
                    "abs" | "sum" => out.push(ir::Ty::Int),
                    "sorted" => out.push(ir::list_of(ir::Ty::Int)),
                    _ => {}
                }
            }
            // `isinstance(x, T)` / `isinstance(x, (T1, T2))` constrains bare `x`.
            // Multi-type tuples become one union constraint (monomorphic infer
            // rejects unions → annotate). Container patterns (list/…) are skipped
            // as too ambiguous without an element type.
            if func == "isinstance"
                && let Some(ast::PosArg::Pos(val)) = args.first()
                && matches!(&val.kind, ast::ExprKind::Name(n) if n == name)
                && let Some(ast::PosArg::Pos(ty_arg)) = args.get(1)
                && let Ok(pats) = parse_isinstance_type_arg(ty_arg)
            {
                let mut tys: Vec<ir::Ty> = Vec::new();
                for p in pats {
                    if let Some(t) = isinstance_pat_to_ty(p) {
                        tys.push(t);
                    }
                }
                match tys.len() {
                    0 => {}
                    1 => out.push(tys[0]),
                    _ => out.push(ir::union_of(&tys)),
                }
            }
            for a in args {
                let ae = match a {
                    ast::PosArg::Pos(e) | ast::PosArg::Star(e) => e,
                };
                collect_param_constraints_expr(name, ae, params, bare, out);
            }
        }
        ast::ExprKind::Index { base, index } => {
            if matches!(&base.kind, ast::ExprKind::Name(n) if n == name) {
                // name[i] — list or str; prefer list[int] if index is int
                out.push(ir::list_of(ir::Ty::Int));
            }
            if matches!(&index.kind, ast::ExprKind::Name(n) if n == name) {
                out.push(ir::Ty::Int);
            }
            collect_param_constraints_expr(name, base, params, bare, out);
            collect_param_constraints_expr(name, index, params, bare, out);
        }
        ast::ExprKind::MethodCall {
            base, method, args, ..
        } => {
            if matches!(&base.kind, ast::ExprKind::Name(n) if n == name) {
                match method.as_str() {
                    "append" | "pop" | "insert" | "remove" | "clear" | "sort" | "index" => {
                        if let Some(ast::PosArg::Pos(a0)) = args.first() {
                            if let Some(t) = try_type_ast_expr(a0, params, &HashMap::new()) {
                                out.push(ir::list_of(t));
                            } else {
                                out.push(ir::list_of(ir::Ty::Int));
                            }
                        } else {
                            out.push(ir::list_of(ir::Ty::Int));
                        }
                    }
                    "add" | "discard" => {
                        if let Some(ast::PosArg::Pos(a0)) = args.first()
                            && let Some(t) = try_type_ast_expr(a0, params, &HashMap::new())
                        {
                            out.push(ir::set_of(t));
                        }
                    }
                    "upper" | "lower" | "strip" | "split" | "startswith" | "endswith" | "find"
                    | "replace" | "join" => {
                        out.push(ir::Ty::Str);
                    }
                    "keys" | "values" | "items" | "get" | "update" => {}
                    _ => {}
                }
            }
            collect_param_constraints_expr(name, base, params, bare, out);
            for a in args {
                let ae = match a {
                    ast::PosArg::Pos(e) | ast::PosArg::Star(e) => e,
                };
                collect_param_constraints_expr(name, ae, params, bare, out);
            }
        }
        ast::ExprKind::ListLit(items) => {
            for it in items {
                let e = match it {
                    ast::ListElem::Item(e) | ast::ListElem::Star(e) => e,
                };
                collect_param_constraints_expr(name, e, params, bare, out);
            }
        }
        ast::ExprKind::TupleLit(items) | ast::ExprKind::SetLit(items) => {
            for it in items {
                collect_param_constraints_expr(name, it, params, bare, out);
            }
        }
        ast::ExprKind::DictLit(items) => {
            for (k, v) in items {
                collect_param_constraints_expr(name, k, params, bare, out);
                collect_param_constraints_expr(name, v, params, bare, out);
            }
        }
        ast::ExprKind::Cast { arg, .. } => {
            collect_param_constraints_expr(name, arg, params, bare, out);
        }
        _ => {}
    }
}

/// Collect joined storage types for locals assigned in `body`.
fn collect_joined_local_types(
    body: &[ast::Stmt],
    params: &HashMap<String, ir::Ty>,
    globals: &HashMap<String, ir::Ty>,
) -> HashMap<String, ir::Ty> {
    let mut assigns: HashMap<String, ir::Ty> = HashMap::new();
    let mut annotated: HashSet<String> = HashSet::new();
    let mut env = params.clone();
    // Seed with globals for typing RHS that reference them.
    for (k, v) in globals {
        env.entry(k.clone()).or_insert(*v);
    }
    collect_assign_types_in(body, &mut env, &mut assigns, &mut annotated);
    // Empty `xs = []` followed by `xs.append(v)` (or insert) fixes list[T].
    fill_empty_list_types_from_appends(body, &env, &mut assigns, &annotated);
    assigns
}

/// Names assigned an empty list literal (`xs = []`) somewhere in `body`.
fn collect_empty_list_assign_names(stmts: &[ast::Stmt], out: &mut HashSet<String>) {
    for st in stmts {
        match &st.kind {
            ast::StmtKind::Assign {
                targets,
                annotation: None,
                value,
            } => {
                if matches!(&value.kind, ast::ExprKind::ListLit(items) if items.is_empty()) {
                    for t in targets {
                        if let ast::AssignTarget::Name { name, .. } = t {
                            out.insert(name.clone());
                        }
                    }
                }
            }
            ast::StmtKind::If { branches, orelse } => {
                for (_, b) in branches {
                    collect_empty_list_assign_names(b, out);
                }
                collect_empty_list_assign_names(orelse, out);
            }
            ast::StmtKind::While { body, orelse, .. } | ast::StmtKind::For { body, orelse, .. } => {
                collect_empty_list_assign_names(body, out);
                collect_empty_list_assign_names(orelse, out);
            }
            ast::StmtKind::Try {
                body,
                handlers,
                orelse,
                finally,
            } => {
                collect_empty_list_assign_names(body, out);
                for h in handlers {
                    collect_empty_list_assign_names(&h.body, out);
                }
                collect_empty_list_assign_names(orelse, out);
                collect_empty_list_assign_names(finally, out);
            }
            ast::StmtKind::With { body, .. } => collect_empty_list_assign_names(body, out),
            ast::StmtKind::Match { cases, .. } => {
                for c in cases {
                    collect_empty_list_assign_names(&c.body, out);
                }
            }
            _ => {}
        }
    }
}

/// Collect element-type hints from `name.append(v)` / `name.insert(i, v)`.
fn collect_list_append_elem_hints(
    stmts: &[ast::Stmt],
    env: &HashMap<String, ir::Ty>,
    out: &mut HashMap<String, ir::Ty>,
) {
    collect_list_append_elem_hints_scoped(stmts, env, out, &HashSet::new());
}

/// `shadowed`: names assigned in an enclosing nested-def scope (block free-var
/// appends from filling outer empty lists when the nested function rebinds the name).
fn collect_list_append_elem_hints_scoped(
    stmts: &[ast::Stmt],
    env: &HashMap<String, ir::Ty>,
    out: &mut HashMap<String, ir::Ty>,
    shadowed: &HashSet<String>,
) {
    for st in stmts {
        match &st.kind {
            ast::StmtKind::ExprStmt(e)
            | ast::StmtKind::Return(Some(e))
            | ast::StmtKind::Raise { message: e, .. } => {
                collect_list_append_elem_hints_expr(e, env, out, shadowed);
            }
            ast::StmtKind::Assign { value, .. } => {
                collect_list_append_elem_hints_expr(value, env, out, shadowed);
            }
            ast::StmtKind::AugAssign { value, .. } => {
                collect_list_append_elem_hints_expr(value, env, out, shadowed);
            }
            ast::StmtKind::If { branches, orelse } => {
                for (c, b) in branches {
                    collect_list_append_elem_hints_expr(c, env, out, shadowed);
                    collect_list_append_elem_hints_scoped(b, env, out, shadowed);
                }
                collect_list_append_elem_hints_scoped(orelse, env, out, shadowed);
            }
            ast::StmtKind::While { cond, body, orelse } => {
                collect_list_append_elem_hints_expr(cond, env, out, shadowed);
                collect_list_append_elem_hints_scoped(body, env, out, shadowed);
                collect_list_append_elem_hints_scoped(orelse, env, out, shadowed);
            }
            ast::StmtKind::For {
                iter, body, orelse, ..
            } => {
                collect_list_append_elem_hints_expr(iter, env, out, shadowed);
                collect_list_append_elem_hints_scoped(body, env, out, shadowed);
                collect_list_append_elem_hints_scoped(orelse, env, out, shadowed);
            }
            ast::StmtKind::Try {
                body,
                handlers,
                orelse,
                finally,
            } => {
                collect_list_append_elem_hints_scoped(body, env, out, shadowed);
                for h in handlers {
                    collect_list_append_elem_hints_scoped(&h.body, env, out, shadowed);
                }
                collect_list_append_elem_hints_scoped(orelse, env, out, shadowed);
                collect_list_append_elem_hints_scoped(finally, env, out, shadowed);
            }
            ast::StmtKind::With { item, body, .. } => {
                collect_list_append_elem_hints_expr(item, env, out, shadowed);
                collect_list_append_elem_hints_scoped(body, env, out, shadowed);
            }
            ast::StmtKind::Match { subject, cases } => {
                collect_list_append_elem_hints_expr(subject, env, out, shadowed);
                for c in cases {
                    if let Some(g) = &c.guard {
                        collect_list_append_elem_hints_expr(g, env, out, shadowed);
                    }
                    collect_list_append_elem_hints_scoped(&c.body, env, out, shadowed);
                }
            }
            // Nested def: free-var `xs.append` fills outer `xs = []`; local rebinds
            // of `xs` are shadowed and do not affect the outer empty list.
            ast::StmtKind::FuncDef(f) => {
                let nested_assigned = assigned_names_in_stmts(&f.body);
                let mut child_shadow = shadowed.clone();
                child_shadow.extend(nested_assigned);
                // Params of the nested def type append args like `xs.append(x)`.
                let mut child_env = env.clone();
                for p in &f.params {
                    if let Ok(ty) = resolve_param_ty(p) {
                        child_env.insert(p.name.clone(), ty);
                    }
                }
                if let Some(va) = &f.vararg
                    && let Ok(ty) = resolve_param_ty(va)
                {
                    child_env.insert(va.name.clone(), ir::list_of(ty));
                }
                collect_list_append_elem_hints_scoped(&f.body, &child_env, out, &child_shadow);
            }
            _ => {}
        }
    }
}

fn collect_list_append_elem_hints_expr(
    e: &ast::Expr,
    env: &HashMap<String, ir::Ty>,
    out: &mut HashMap<String, ir::Ty>,
    shadowed: &HashSet<String>,
) {
    match &e.kind {
        ast::ExprKind::MethodCall {
            base, method, args, ..
        } => {
            if let ast::ExprKind::Name(n) = &base.kind
                && !shadowed.contains(n)
            {
                let elem_hint = match method.as_str() {
                    "append" => {
                        if let Some(ast::PosArg::Pos(a0)) = args.first() {
                            try_type_ast_expr(a0, env, &HashMap::new())
                        } else {
                            None
                        }
                    }
                    "insert" => {
                        if let Some(ast::PosArg::Pos(a1)) = args.get(1) {
                            try_type_ast_expr(a1, env, &HashMap::new())
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some(elem) = elem_hint {
                    match out.get(n).copied() {
                        None => {
                            out.insert(n.clone(), elem);
                        }
                        Some(prev) if prev != elem => {
                            let j = join_types(prev, elem);
                            out.insert(n.clone(), j);
                        }
                        Some(_) => {}
                    }
                }
            }
            collect_list_append_elem_hints_expr(base, env, out, shadowed);
            for a in args {
                let ae = match a {
                    ast::PosArg::Pos(e) | ast::PosArg::Star(e) => e,
                };
                collect_list_append_elem_hints_expr(ae, env, out, shadowed);
            }
        }
        ast::ExprKind::Binary { left, right, .. } => {
            collect_list_append_elem_hints_expr(left, env, out, shadowed);
            collect_list_append_elem_hints_expr(right, env, out, shadowed);
        }
        ast::ExprKind::Compare { first, rest } => {
            collect_list_append_elem_hints_expr(first, env, out, shadowed);
            for (_, r) in rest {
                collect_list_append_elem_hints_expr(r, env, out, shadowed);
            }
        }
        ast::ExprKind::Unary { operand, .. } | ast::ExprKind::Cast { arg: operand, .. } => {
            collect_list_append_elem_hints_expr(operand, env, out, shadowed);
        }
        ast::ExprKind::Call { args, .. } => {
            for a in args {
                let ae = match a {
                    ast::PosArg::Pos(e) | ast::PosArg::Star(e) => e,
                };
                collect_list_append_elem_hints_expr(ae, env, out, shadowed);
            }
        }
        ast::ExprKind::Index { base, index } => {
            collect_list_append_elem_hints_expr(base, env, out, shadowed);
            collect_list_append_elem_hints_expr(index, env, out, shadowed);
        }
        ast::ExprKind::ListLit(items) => {
            for it in items {
                let e = match it {
                    ast::ListElem::Item(e) | ast::ListElem::Star(e) => e,
                };
                collect_list_append_elem_hints_expr(e, env, out, shadowed);
            }
        }
        ast::ExprKind::TupleLit(items) | ast::ExprKind::SetLit(items) => {
            for it in items {
                collect_list_append_elem_hints_expr(it, env, out, shadowed);
            }
        }
        ast::ExprKind::DictLit(items) => {
            for (k, v) in items {
                collect_list_append_elem_hints_expr(k, env, out, shadowed);
                collect_list_append_elem_hints_expr(v, env, out, shadowed);
            }
        }
        _ => {}
    }
}

/// When `xs = []` has no annotation, fix `list[T]` from later `xs.append`/`insert`.
/// Remaining empty lists with no elem hint default to `list[Any]`.
fn fill_empty_list_types_from_appends(
    body: &[ast::Stmt],
    env: &HashMap<String, ir::Ty>,
    assigns: &mut HashMap<String, ir::Ty>,
    annotated: &HashSet<String>,
) {
    let mut empty: HashSet<String> = HashSet::new();
    collect_empty_list_assign_names(body, &mut empty);
    if empty.is_empty() {
        return;
    }
    let mut hints: HashMap<String, ir::Ty> = HashMap::new();
    collect_list_append_elem_hints(body, env, &mut hints);
    for name in empty {
        if annotated.contains(&name) {
            continue;
        }
        if let Some(elem) = hints.get(&name).copied() {
            // Reject pure None as sole elem type (same as bare param).
            if elem == ir::Ty::None {
                // Fall through to list[Any] default below.
            } else {
                let list_ty = ir::list_of(elem);
                match assigns.get(&name).copied() {
                    None => {
                        assigns.insert(name.clone(), list_ty);
                    }
                    Some(prev) if prev == list_ty => {}
                    // Specialize provisional list[Any] (empty-list default / seed)
                    // to a concrete list[T] from append/insert hints.
                    Some(ir::Ty::List(e)) if *e == ir::Ty::Any => {
                        assigns.insert(name.clone(), list_ty);
                    }
                    Some(ir::Ty::List(_)) => {
                        // Already a more specific list from another assign — keep.
                    }
                    Some(prev) => {
                        assigns.insert(name.clone(), join_types(prev, list_ty));
                    }
                }
                continue;
            }
        }
        // No usable append/insert hint: default empty list to list[Any].
        match assigns.get(&name).copied() {
            None => {
                assigns.insert(name, ir::list_of(ir::Ty::Any));
            }
            Some(ir::Ty::List(_)) => {
                // Already specialized or joined as a list.
            }
            Some(prev) => {
                assigns.insert(name, join_types(prev, ir::list_of(ir::Ty::Any)));
            }
        }
    }
}

fn collect_assign_types_in(
    stmts: &[ast::Stmt],
    env: &mut HashMap<String, ir::Ty>,
    out: &mut HashMap<String, ir::Ty>,
    annotated: &mut HashSet<String>,
) {
    for st in stmts {
        match &st.kind {
            ast::StmtKind::Assign {
                targets,
                annotation,
                value,
            } => {
                let rhs_ty = annotation
                    .as_ref()
                    .and_then(|t| resolve_type_checked(*t, st.span).ok())
                    .or_else(|| try_type_ast_expr(value, env, &HashMap::new()))
                    // Class construction / other seeds not covered by try_type alone.
                    .or_else(|| seed_ty_from_expr(value));
                let Some(rhs_ty) = rhs_ty else {
                    continue;
                };
                for t in targets {
                    if let ast::AssignTarget::Name { name, .. } = t {
                        // Explicit annotation fixes storage permanently (no join widen).
                        if let Some(ann) = annotation
                            && let Ok(ann_ty) = resolve_type_checked(*ann, st.span)
                        {
                            out.insert(name.clone(), ann_ty);
                            env.insert(name.clone(), ann_ty);
                            annotated.insert(name.clone());
                            continue;
                        }
                        if annotated.contains(name) {
                            // Keep annotated storage; bind_name will coerce/error.
                            continue;
                        }
                        let storage = match out.get(name).copied() {
                            Some(prev) => join_types(prev, rhs_ty),
                            None => rhs_ty,
                        };
                        out.insert(name.clone(), storage);
                        env.insert(name.clone(), storage);
                    }
                }
            }
            ast::StmtKind::If { branches, orelse } => {
                for (_, b) in branches {
                    collect_assign_types_in(b, env, out, annotated);
                }
                collect_assign_types_in(orelse, env, out, annotated);
            }
            ast::StmtKind::While { body, orelse, .. } | ast::StmtKind::For { body, orelse, .. } => {
                // for-loop target is int for range; handled when lowering.
                collect_assign_types_in(body, env, out, annotated);
                collect_assign_types_in(orelse, env, out, annotated);
            }
            ast::StmtKind::Try {
                body,
                handlers,
                orelse,
                finally,
            } => {
                collect_assign_types_in(body, env, out, annotated);
                for h in handlers {
                    // except as e → exception object
                    if let Some((n, _)) = &h.bind {
                        out.entry(n.clone()).or_insert(ir::Ty::Exception);
                        env.insert(n.clone(), ir::Ty::Exception);
                    }
                    collect_assign_types_in(&h.body, env, out, annotated);
                }
                collect_assign_types_in(orelse, env, out, annotated);
                collect_assign_types_in(finally, env, out, annotated);
            }
            ast::StmtKind::With { body, .. } => {
                collect_assign_types_in(body, env, out, annotated);
            }
            ast::StmtKind::Match { cases, .. } => {
                for c in cases {
                    collect_assign_types_in(&c.body, env, out, annotated);
                }
            }
            _ => {}
        }
    }
}

/// Static `__all__ = ["a", "b"]` / `("a", "b")` from a module body.
/// `Some(Ok(names))` — static list/tuple of string literals (last assignment wins).
/// `Some(Err(()))` — `__all__` assigned to something non-static.
/// `None` — no `__all__` assignment.
fn static_dunder_all(module: &ast::Module) -> Option<Result<Vec<String>, ()>> {
    let mut found: Option<Result<Vec<String>, ()>> = None;
    for stmt in &module.body {
        let ast::StmtKind::Assign { targets, value, .. } = &stmt.kind else {
            continue;
        };
        let is_all = targets
            .iter()
            .any(|t| matches!(t, ast::AssignTarget::Name { name, .. } if name == "__all__"));
        if !is_all {
            continue;
        }
        found = Some(string_lit_sequence(value));
    }
    found
}

fn string_lit_sequence(e: &ast::Expr) -> Result<Vec<String>, ()> {
    match &e.kind {
        ast::ExprKind::ListLit(items) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                match it {
                    ast::ListElem::Item(expr) => match &expr.kind {
                        ast::ExprKind::Str(s) => out.push(s.clone()),
                        _ => return Err(()),
                    },
                    ast::ListElem::Star(_) => return Err(()),
                }
            }
            Ok(out)
        }
        ast::ExprKind::TupleLit(items) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                match &it.kind {
                    ast::ExprKind::Str(s) => out.push(s.clone()),
                    _ => return Err(()),
                }
            }
            Ok(out)
        }
        _ => Err(()),
    }
}

/// Build name → AST for modules in the program.
fn module_ast_map<'a>(modules: &'a [ModuleInput<'a>]) -> HashMap<&'a str, &'a ast::Module> {
    modules.iter().map(|m| (m.name.as_str(), m.ast)).collect()
}

/// Names that `from src import *` should bind, for a single expansion step.
///
/// - Static `__all__`: those names (including private).
/// - Dynamic `__all__`: empty here; [`collect_imports`] reports the error.
/// - No `__all__`: public names from the current export surface (funcs +
///   values) plus any extra public names supplied by the caller (e.g.
///   submodule short names from last-export / submodule maps).
fn star_import_name_list(
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
fn effective_from_names(
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
fn collect_assigned_names(module: &ast::Module) -> std::collections::HashSet<String> {
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
fn expand_export_values(
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

fn single_assign_name(targets: &[ast::AssignTarget]) -> Option<String> {
    if targets.len() != 1 {
        return None;
    }
    match &targets[0] {
        ast::AssignTarget::Name { name, .. } => Some(name.clone()),
        _ => None,
    }
}

fn literal_expr_ty(e: &ast::Expr) -> Option<ir::Ty> {
    match &e.kind {
        ast::ExprKind::Int(_) | ast::ExprKind::IntDigits(_) => Some(ir::Ty::Int),
        ast::ExprKind::Float(_) => Some(ir::Ty::Float),
        ast::ExprKind::Bool(_) => Some(ir::Ty::Bool),
        ast::ExprKind::Str(_) => Some(ir::Ty::Str),
        _ => None,
    }
}

fn record_simple_assign(stmt: &ast::Stmt, tys: &mut HashMap<String, ir::Ty>) {
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
fn stmt_loads_child(stmt: &ast::Stmt, parent: &str, child: &str) -> bool {
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
fn build_partial_prelim(
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
fn build_partial_funcs(
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
fn build_package_final_values(modules: &[ModuleInput]) -> HashMap<String, HashMap<String, ir::Ty>> {
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
fn build_partial_reexports(
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
fn build_reexport_origins(
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
enum LastExport {
    /// Submodule binding (`from . import mod` / package attribute is a module).
    Module(String),
    /// Value or function (assignment, def, or value re-export).
    Symbol,
}

/// Walk each module body in order; last binding of each name wins
/// (Module vs Symbol re-exports).
fn compute_last_exports(
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
fn resolve_from_export(
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
fn last_binding_is_from_import(module: &ast::Module, local: &str) -> bool {
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
fn expand_export_funcs(
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
fn origin_of(mods: &HashMap<String, ModuleData>, module: &str, name: &str) -> (String, String) {
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
fn apply_reexports(
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

fn collect_assign_names(target: &ast::AssignTarget, names: &mut std::collections::HashSet<String>) {
    match target {
        ast::AssignTarget::Name { name, .. } => {
            names.insert(name.clone());
        }
        ast::AssignTarget::Index { .. } | ast::AssignTarget::Attr { .. } => {}
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
fn star_src_ast<'a>(
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

fn collect_imports(
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
fn collect_imports_block(
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

/// Analyze a whole program: several modules with cross-file imports.
/// `modules` is in topological order (dependencies first, root last);
/// diagnostics are tagged with the module index as their file id.
pub fn analyze_program(modules: &[ModuleInput]) -> SResult<ir::Module> {
    assert!(!modules.is_empty(), "a program needs at least one module");
    clear_closure_defaults();
    clear_class_env();
    let root_idx = modules.len() - 1;

    // pass 0: class ids → import aliases for bases/annotations → bases → layouts
    let class_asts = collect_class_asts(modules)?;
    register_class_ids(&class_asts)?;
    inject_class_import_aliases(modules);
    resolve_class_bases(&class_asts)?;
    let free_func_rets = pre_infer_free_func_rets(modules);
    finalize_class_layouts(&class_asts, &free_func_rets)?;

    // pass 1: every module's own signatures and assignment-name surface
    let mut own_funcs: HashMap<String, HashMap<String, FuncSig>> = HashMap::new();
    let mut all_orders: Vec<Vec<&ast::FuncDef>> = Vec::new();
    // Method defs to lower per module: (ir_name, class_id, FuncDef)
    let mut method_orders: HashMap<String, Vec<(String, ir::ClassId, &ast::FuncDef)>> =
        HashMap::new();
    let mut assigned_names: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    // Classes defined per module (for exports / from-import).
    let mut module_classes: HashMap<String, HashMap<String, ir::ClassId>> = HashMap::new();
    let module_names: Vec<String> = modules.iter().map(|m| m.name.clone()).collect();
    let submodules = build_submodule_map(&module_names);
    for (i, m) in modules.iter().enumerate() {
        // Bare class names + import aliases available for annotations in collect_sigs.
        set_class_current_module(&m.name);
        let (mut funcs, order) = collect_sigs(m.ast).map_err(|d| d.with_file(i))?;
        // Class methods: register under IR names (`Point.__init__`) so
        // `lower_function` picks up the typed self parameter via known_sig.
        let mut methods = Vec::new();
        let mut classes_here: HashMap<String, ir::ClassId> = HashMap::new();
        for c in class_asts.iter().filter(|c| c.module == m.name) {
            let class_id = lookup_class_in_module(&c.module, &c.name).expect("class registered");
            classes_here.insert(c.name.clone(), class_id);
            for cm in &c.methods {
                let ir_name = method_ir_name(&c.module, c.is_root, &c.name, &cm.def.name);
                let kind = method_kind_from_decorators(&cm.def.decorators, cm.def.span)
                    .map_err(|d| d.with_file(i))?;
                let sig =
                    method_func_sig(class_id, &c.name, cm.def, kind).map_err(|d| d.with_file(i))?;
                register_method_sig(&ir_name, sig.clone());
                register_method_kind(&ir_name, kind);
                if kind == MethodKind::Property {
                    register_property(class_id, &cm.def.name, &ir_name);
                }
                methods.push((ir_name.clone(), class_id, cm.def));
                funcs.insert(ir_name, sig);
            }
        }
        module_classes.insert(m.name.clone(), classes_here);
        method_orders.insert(m.name.clone(), methods);
        own_funcs.insert(m.name.clone(), funcs);
        all_orders.push(order);
        let mut names = collect_assigned_names(m.ast);
        // Classes are exportable names (from m import C / m.C).
        for c in class_asts.iter().filter(|c| c.module == m.name) {
            names.insert(c.name.clone());
        }
        assigned_names.insert(m.name.clone(), names);
    }
    // Override ABI: reject incompatible ret/arity after all method sigs exist.
    check_override_compatibility(&class_asts)?;
    // pass 1b: export surface includes package re-exports; last-binding map
    let export_funcs = expand_export_funcs(modules, &own_funcs);
    let export_values = expand_export_values(modules, &assigned_names, &export_funcs, &submodules);
    let last_exports = compute_last_exports(modules, &submodules, &export_funcs, &export_values);
    let partial_prelim = build_partial_prelim(modules);
    let partial_funcs = build_partial_funcs(modules);
    let partial_reexports = build_partial_reexports(modules);
    let package_final_values = build_package_final_values(modules);

    // pass 2: import bindings (validated against the export surface)
    let asts = module_ast_map(modules);
    let mut all_imports: Vec<HashMap<String, ImportBinding>> = Vec::new();
    for (i, m) in modules.iter().enumerate() {
        let imports = collect_imports(
            m.ast,
            &m.name,
            &last_exports,
            &export_funcs,
            &export_values,
            &submodules,
            &asts,
        )
        .map_err(|d| d.with_file(i))?;
        all_imports.push(imports);
    }

    // Re-export origins for deferred parent attribute/call resolution.
    let reexport_origins = build_reexport_origins(modules, &all_imports, &last_exports);

    // pass 3: lower each module in dependency order, accumulating results
    let mut mods: HashMap<String, ModuleData> = HashMap::new();
    let mut out_funcs: Vec<ir::Function> = Vec::new();
    let mut out_globals: Vec<(String, ir::Ty)> = Vec::new();

    for (i, m) in modules.iter().enumerate() {
        let is_root = i == root_idx;
        let funcs = &own_funcs[&m.name];
        let mctx = ModuleCtx {
            module: &m.name,
            is_root,
            funcs,
            imports: &all_imports[i],
            mods: &mods,
            submodules: &submodules,
            partial_prelim: &partial_prelim,
            partial_funcs: &partial_funcs,
            package_final_values: &package_final_values,
            all_own_funcs: &own_funcs,
            last_exports: &last_exports,
            reexport_origins: &reexport_origins,
            partial_reexports: &partial_reexports,
        };

        let mut globals: HashMap<String, ir::Ty> = HashMap::new();
        let mut globals_order: Vec<(String, ir::Ty)> = Vec::new();
        let script: Vec<ast::Stmt> = m
            .ast
            .body
            .iter()
            .filter(|s| {
                !matches!(
                    s.kind,
                    ast::StmtKind::FuncDef(_) | ast::StmtKind::ClassDef(_)
                )
            })
            .cloned()
            .collect();

        // Pre-seed globals from simple top-level assigns so functions can
        // reference them before init runs (order: functions then init).
        seed_globals_from_script(&script, &mut globals, &mut globals_order, is_root, &m.name);

        // Lower top-level functions first so inferred return types (e.g.
        // returning a nested closure) are available when lowering the
        // module init / entry script that calls them.
        // Lower each top-level function, patching own_funcs immediately so
        // later functions see inferred return types / generator frame sizes.
        #[allow(clippy::drop_non_drop)]
        {
            drop(mctx);
        }
        // Lower class methods first (bodies may be called from free functions).
        set_class_current_module(&m.name);
        // Imported class aliases: bare name in this module resolves for annotations
        // and construction (`from shapes import Point` → `Point(...)`).
        for (local, binding) in &all_imports[i] {
            let id = match binding {
                ImportBinding::Class(id) => Some(*id),
                ImportBinding::Symbol { module, name } => lookup_class_in_module(module, name),
                _ => None,
            };
            if let Some(id) = id {
                with_class_env_mut(|e| {
                    e.by_key
                        .entry((m.name.clone(), local.clone()))
                        .or_insert(id);
                });
            }
        }
        if let Some(methods) = method_orders.get(&m.name) {
            for (ir_name, class_id, def) in methods {
                let mut method_def = (*def).clone();
                method_def.name = ir_name.clone();
                let funcs_view = &own_funcs[&m.name];
                let mctx_fn = ModuleCtx {
                    module: &m.name,
                    is_root,
                    funcs: funcs_view,
                    imports: &all_imports[i],
                    mods: &mods,
                    submodules: &submodules,
                    partial_prelim: &partial_prelim,
                    partial_funcs: &partial_funcs,
                    package_final_values: &package_final_values,
                    all_own_funcs: &own_funcs,
                    last_exports: &last_exports,
                    reexport_origins: &reexport_origins,
                    partial_reexports: &partial_reexports,
                };
                let (f, nested) = lower_function_with_class(
                    &method_def,
                    &mctx_fn,
                    &mut globals,
                    &mut globals_order,
                    false,
                    Some(*class_id),
                )
                .map_err(|d| d.with_file(i))?;
                // Patch inferred return type (same as free functions).
                if !f.is_generator {
                    if let Some(sig) = own_funcs.get_mut(&m.name).and_then(|m| m.get_mut(ir_name)) {
                        sig.ret = f.ret;
                    }
                    register_method_sig(ir_name, {
                        let mut s = method_sig_lookup(ir_name).unwrap_or_else(|| FuncSig {
                            params: f
                                .params
                                .iter()
                                .map(|(n, t)| ParamSig {
                                    name: n.clone(),
                                    ty: *t,
                                    default: None,
                                })
                                .collect(),
                            vararg: None,
                            kwarg: None,
                            ret: f.ret,
                            span: Span::default(),
                            is_generator: false,
                            yield_ty: None,
                            gen_frame_slots: 0,
                        });
                        s.ret = f.ret;
                        s
                    });
                }
                out_funcs.push(f);
                out_funcs.extend(nested);
            }
        }
        for fd in &all_orders[i] {
            let patch = {
                let funcs_view = &own_funcs[&m.name];
                let mctx_fn = ModuleCtx {
                    module: &m.name,
                    is_root,
                    funcs: funcs_view,
                    imports: &all_imports[i],
                    mods: &mods,
                    submodules: &submodules,
                    partial_prelim: &partial_prelim,
                    partial_funcs: &partial_funcs,
                    package_final_values: &package_final_values,
                    all_own_funcs: &own_funcs,
                    last_exports: &last_exports,
                    reexport_origins: &reexport_origins,
                    partial_reexports: &partial_reexports,
                };
                let (f, nested) =
                    lower_function(fd, &mctx_fn, &mut globals, &mut globals_order, false)
                        .map_err(|d| d.with_file(i))?;
                let name = fd.name.clone();
                let patch = if f.is_generator {
                    Err((f.params.len() + f.locals.len()) as i64)
                } else {
                    Ok(f.ret)
                };
                out_funcs.push(f);
                out_funcs.extend(nested);
                (name, patch)
            };
            let (name, patch) = patch;
            if let Some(sig) = own_funcs.get_mut(&m.name).and_then(|m| m.get_mut(&name)) {
                match patch {
                    Ok(ret) => sig.ret = ret,
                    Err(slots) => sig.gen_frame_slots = slots,
                }
            }
        }

        // the module's top-level statements become its init function; for
        // the root that IS the entry, otherwise `<mod>.__init__` guarded to
        // run once
        let init_name = if is_root {
            ENTRY_NAME.to_string()
        } else {
            qual(&m.name, "__init__")
        };

        let funcs = &own_funcs[&m.name];
        let mctx = ModuleCtx {
            module: &m.name,
            is_root,
            funcs,
            imports: &all_imports[i],
            mods: &mods,
            submodules: &submodules,
            partial_prelim: &partial_prelim,
            partial_funcs: &partial_funcs,
            package_final_values: &package_final_values,
            all_own_funcs: &own_funcs,
            last_exports: &last_exports,
            reexport_origins: &reexport_origins,
            partial_reexports: &partial_reexports,
        };

        let init = if is_root && script.is_empty() {
            // PyRs convenience: a root that is only definitions calls main()
            if let Some(sig) = funcs.get("main") {
                if !sig.params.is_empty() || sig.vararg.is_some() || sig.kwarg.is_some() {
                    return Err(err(
                        "main() is used as the entry point and cannot take parameters",
                        sig.span,
                    )
                    .with_file(i));
                }
                Some(ir::Function {
                    name: ENTRY_NAME.to_string(),
                    params: vec![],
                    ret: ir::Ty::None,
                    locals: vec![],
                    body: vec![ir::Stmt::ExprStmt(ir::Expr {
                        ty: sig.ret,
                        kind: ir::ExprKind::Call {
                            func: "main".to_string(),
                            args: vec![],
                        },
                    })],
                    is_generator: false,
                    yield_ty: None,
                })
            } else {
                None
            }
        } else {
            let init_def = ast::FuncDef {
                name: init_name.clone(),
                params: vec![],
                vararg: None,
                kwarg: None,
                ret: None,
                body: script,
                span: Span::default(),
                decorators: Vec::new(),
            };
            let (mut f, nested) =
                lower_function(&init_def, &mctx, &mut globals, &mut globals_order, true)
                    .map_err(|d| d.with_file(i))?;
            out_funcs.extend(nested);
            if !is_root {
                add_init_guard(&mut f, &m.name, &mut globals_order);
            }
            Some(f)
        };

        match init {
            Some(f) => out_funcs.push(f),
            None if is_root => {
                return Err(err(
                    "program has no entry point: add top-level statements or define main()",
                    Span::default(),
                )
                .with_file(i));
            }
            // a non-root module with only definitions has no init to call
            None => {}
        }

        out_globals.extend(globals_order.iter().cloned());
        let own_func_names: std::collections::HashSet<String> = funcs.keys().cloned().collect();
        let mut data = ModuleData {
            funcs: funcs.clone(),
            globals,
            classes: module_classes.get(&m.name).cloned().unwrap_or_default(),
            reexports: HashMap::new(),
        };
        // Dependencies are already in `mods` (topo order), so re-exports can
        // resolve origin types/sigs. Parent packages that import children are
        // lowered after those children.
        apply_reexports(&mut data, &own_func_names, &all_imports[i], &mods, m.ast);
        mods.insert(m.name.clone(), data);
    }

    let classes = with_class_env(|e| e.infos.clone());
    Ok(ir::Module {
        funcs: out_funcs,
        globals: out_globals,
        classes,
        entry: ENTRY_NAME.to_string(),
    })
}

/// Seed module globals from top-level assignments (literals / names), joining
/// multiple assignment types so bare multi-assign yields a union storage type.
/// Explicit annotations fix storage and are never widened by later bare assigns.
fn seed_globals_from_script(
    script: &[ast::Stmt],
    globals: &mut HashMap<String, ir::Ty>,
    globals_order: &mut Vec<(String, ir::Ty)>,
    is_root: bool,
    module: &str,
) {
    let own = |name: &str| -> String {
        if is_root {
            name.to_string()
        } else {
            format!("{module}.{name}")
        }
    };
    let mut annotated: HashSet<String> = HashSet::new();
    for st in script {
        if let ast::StmtKind::Assign {
            targets,
            annotation,
            value,
        } = &st.kind
        {
            // Annotation wins as the storage type (do not widen past it).
            if let Some(ann) = annotation {
                if let Ok(ty) = resolve_type_checked(*ann, st.span) {
                    for t in targets {
                        if let ast::AssignTarget::Name { name, .. } = t {
                            use std::collections::hash_map::Entry;
                            if let Entry::Vacant(e) = globals.entry(name.clone()) {
                                e.insert(ty);
                                globals_order.push((own(name), ty));
                            }
                            annotated.insert(name.clone());
                        }
                    }
                }
                continue;
            }
            let Some(ty) = seed_ty_from_expr(value) else {
                continue;
            };
            for t in targets {
                if let ast::AssignTarget::Name { name, .. } = t {
                    if annotated.contains(name) {
                        continue; // keep annotated storage; bind_name will error on bad RHS
                    }
                    use std::collections::hash_map::Entry;
                    match globals.entry(name.clone()) {
                        Entry::Vacant(e) => {
                            e.insert(ty);
                            globals_order.push((own(name), ty));
                        }
                        Entry::Occupied(mut e) => {
                            let joined = join_types(*e.get(), ty);
                            if joined != *e.get() {
                                e.insert(joined);
                                let q = own(name);
                                if let Some((_, t)) =
                                    globals_order.iter_mut().find(|(n, _)| n == &q)
                                {
                                    *t = joined;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn seed_ty_from_expr(e: &ast::Expr) -> Option<ir::Ty> {
    match &e.kind {
        ast::ExprKind::Int(_) | ast::ExprKind::IntDigits(_) => Some(ir::Ty::Int),
        ast::ExprKind::Float(_) => Some(ir::Ty::Float),
        ast::ExprKind::Bool(_) => Some(ir::Ty::Bool),
        ast::ExprKind::Str(_) | ast::ExprKind::JoinedStr(_) => Some(ir::Ty::Str),
        ast::ExprKind::NoneLit => Some(ir::Ty::None),
        // Module-level empty lists: pre-seed as list[Any] so nested free reads
        // (before entry init runs) resolve the name.
        ast::ExprKind::ListLit(items) if items.is_empty() => Some(ir::list_of(ir::Ty::Any)),
        ast::ExprKind::Unary {
            op: ast::UnaryOp::Neg | ast::UnaryOp::Invert,
            operand,
        } => seed_ty_from_expr(operand),
        // Class construction: `Point(1, 2)` → Class type for multi-assign join.
        ast::ExprKind::Call { func, .. } => lookup_class(func).map(ir::Ty::Class),
        ast::ExprKind::MethodCall { base, method, .. } => {
            // `mod.Class(...)` construction as a method call form.
            if let Some(mod_name) = match &base.kind {
                ast::ExprKind::Name(n) => Some(n.as_str()),
                _ => None,
            } {
                // Only if Name is an imported module — best-effort: any class in any module.
                lookup_class_in_module(mod_name, method)
                    .or_else(|| {
                        // Also try bare method name as class in current module.
                        lookup_class(method)
                    })
                    .map(ir::Ty::Class)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Prepend a run-once guard to a module init: `if <mod>.__done__: return;
/// <mod>.__done__ = True; ...`.
fn add_init_guard(f: &mut ir::Function, module: &str, globals_order: &mut Vec<(String, ir::Ty)>) {
    let done = qual(module, "__done__");
    globals_order.push((done.clone(), ir::Ty::Bool));
    let guard = ir::Stmt::If {
        branches: vec![(
            ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::GlobalLoad(done.clone()),
            },
            vec![ir::Stmt::Return(None)],
        )],
        orelse: vec![],
    };
    let set = ir::Stmt::GlobalAssign {
        name: done,
        value: ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ConstBool(true),
        },
    };
    let mut body = vec![guard, set];
    body.append(&mut f.body);
    f.body = body;
}

struct FnCtx<'a> {
    mctx: &'a ModuleCtx<'a>,
    globals: &'a mut HashMap<String, ir::Ty>,
    globals_order: &'a mut Vec<(String, ir::Ty)>,
    /// This function is a module init: its top-level bindings are globals.
    is_entry: bool,
    /// Unused (imports always allowed); kept for call-site compatibility.
    #[allow(dead_code)]
    allow_import: bool,
    /// Names this function declared with `global`.
    declared_globals: std::collections::HashSet<String>,
    fn_name: String,
    ret: ir::Ty,
    locals: HashMap<String, ir::Ty>,
    locals_order: Vec<(String, ir::Ty)>,
    loop_depth: usize,
    temp_counter: usize,
    /// Active comprehension variables: user name → (storage local, type).
    /// Innermost last. Comprehension variables shadow but never leak
    /// (Python 3 scoping).
    comp_renames: Vec<(String, String, ir::Ty)>,
    /// Nested functions defined in this function (name → info).
    nested_funcs: HashMap<String, NestedFnInfo>,
    /// IR functions produced for nested defs (and their nested defs).
    nested_ir: Vec<ir::Function>,
    /// Names declared `nonlocal` in this function.
    declared_nonlocals: HashSet<String>,
    /// Names that are cell-backed (mutable free vars / nonlocal).
    cell_locals: HashMap<String, ir::Ty>,
    /// When inside a generator function, the yield element type.
    yield_ty: Option<ir::Ty>,
    /// Refinements: name → narrowed type (stacked for control flow).
    type_refinements: HashMap<String, ir::Ty>,
    /// Cell boxing inits deferred from nested-def analysis (flushed after def).
    pending_cell_inits: Vec<ir::Stmt>,
    /// Nesting depth of try/except/finally.
    try_depth: usize,
    /// Function-local import bindings (CPython: import in function is local).
    local_imports: HashMap<String, ImportBinding>,
    /// Names that some nested function declares `nonlocal` (pre-scanned).
    sibling_nonlocal_names: HashSet<String>,
    /// Outer locals free-captured by any nested def/lambda in this function.
    /// Promoted to cells on first bind so CellNew runs even if the nested
    /// def sits in a branch that is not taken at runtime.
    cell_candidates: HashSet<String>,
    /// Inferred types for names assigned in this function body (for late
    /// free-var binding: nested def before the assign that fills the cell).
    late_bind_tys: HashMap<String, ir::Ty>,
    /// Full outer function body (for late-bind type scan); empty at module entry.
    #[allow(dead_code)]
    outer_body_assigned: HashSet<String>,
    /// Nested defs whose body has been fully lowered (vs provisional sig only).
    lowered_nested: HashSet<String>,
    /// Joined storage types for locals (and module globals when entry) from a
    /// pre-pass over all assignments. Consulted by `bind_name` so multi-assign
    /// uses a union/promoted type without pre-allocating locals (which would
    /// break `global` ordering and late free-cell unbound traps).
    storage_tys: HashMap<String, ir::Ty>,
    /// When lowering an instance method, the class that owns this method.
    /// Used by zero-arg `super().m(...)`.
    current_class: Option<ir::ClassId>,
    /// Name of the `self` parameter of the current instance method (if any).
    self_param: Option<String>,
    /// When lowering a `@classmethod`, `(cls_param_name, class_id)` so
    /// `cls(...)` constructs that class.
    classmethod_cls: Option<(String, ir::ClassId)>,
}

impl FnCtx<'_> {
    /// A compiler-synthesized local. The leading '.' keeps it out of the
    /// user namespace (Python identifiers cannot start with '.').
    fn fresh_temp(&mut self, hint: &str, ty: ir::Ty) -> String {
        self.temp_counter += 1;
        let name = format!(".{hint}{}", self.temp_counter);
        self.locals.insert(name.clone(), ty);
        self.locals_order.push((name.clone(), ty));
        name
    }

    /// Does an assignment to `name` target a module global here?
    fn binds_global(&self, name: &str) -> bool {
        self.is_entry || self.declared_globals.contains(name)
    }

    /// This module's functions.
    fn funcs(&self) -> &HashMap<String, FuncSig> {
        self.mctx.funcs
    }

    /// The IR/emit name for one of *this* module's own globals.
    fn own_global(&self, name: &str) -> String {
        format!("{}{}", self.mctx.prefix(), name)
    }

    /// The IR/emit name for one of *this* module's own functions.
    fn own_func(&self, name: &str) -> String {
        format!("{}{}", self.mctx.prefix(), name)
    }

    /// Is `sys` imported (under any alias)?
    fn sys_alias(&self, name: &str) -> bool {
        matches!(
            self.local_imports
                .get(name)
                .or_else(|| self.mctx.imports.get(name)),
            Some(ImportBinding::Sys)
        )
    }

    /// If `name` is an imported module alias, its real module name.
    fn module_alias(&self, name: &str) -> Option<String> {
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

fn lower_function(
    f: &ast::FuncDef,
    mctx: &ModuleCtx,
    globals: &mut HashMap<String, ir::Ty>,
    globals_order: &mut Vec<(String, ir::Ty)>,
    is_entry: bool,
) -> SResult<(ir::Function, Vec<ir::Function>)> {
    lower_function_with_class(f, mctx, globals, globals_order, is_entry, None)
}

/// Like [`lower_function`], but records the owning class for zero-arg `super()`.
fn lower_function_with_class(
    f: &ast::FuncDef,
    mctx: &ModuleCtx,
    globals: &mut HashMap<String, ir::Ty>,
    globals_order: &mut Vec<(String, ir::Ty)>,
    is_entry: bool,
    current_class: Option<ir::ClassId>,
) -> SResult<(ir::Function, Vec<ir::Function>)> {
    lower_function_inner(
        f,
        mctx,
        globals,
        globals_order,
        is_entry,
        None,
        HashMap::new(),
        current_class,
    )
}

/// `capture_params`: leading params for nested functions (free vars), already typed.
/// `seed_nested`: sibling (and self) nested functions visible for calls.
/// `current_class`: owning class when lowering an instance method (for `super()`).
#[allow(clippy::too_many_arguments)]
fn lower_function_inner(
    f: &ast::FuncDef,
    mctx: &ModuleCtx,
    globals: &mut HashMap<String, ir::Ty>,
    globals_order: &mut Vec<(String, ir::Ty)>,
    is_entry: bool,
    capture_params: Option<Vec<(String, ir::Ty)>>,
    seed_nested: HashMap<String, NestedFnInfo>,
    current_class: Option<ir::ClassId>,
) -> SResult<(ir::Function, Vec<ir::Function>)> {
    let mut params = Vec::new();
    // Detect @classmethod via registered kind when this is a method IR name.
    let is_classmethod =
        current_class.is_some() && method_kind_lookup(&f.name) == MethodKind::Class;
    let self_param = if current_class.is_some() && !is_classmethod {
        // Instance / property methods use self for super().
        f.params.first().map(|p| p.name.clone())
    } else {
        None
    };
    let classmethod_cls = if is_classmethod {
        current_class.map(|cid| {
            (
                f.params
                    .first()
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| "cls".into()),
                cid,
            )
        })
    } else {
        None
    };
    let mut ctx = FnCtx {
        mctx,
        globals,
        globals_order,
        is_entry,
        // Imports are allowed at module top level and inside functions (CPython).
        allow_import: true,
        declared_globals: std::collections::HashSet::new(),
        fn_name: f.name.clone(),
        ret: match f.ret {
            Some(t) => resolve_type_checked(t, f.span)?,
            Option::None => ir::Ty::None,
        },
        locals: HashMap::new(),
        locals_order: Vec::new(),
        loop_depth: 0,
        temp_counter: 0,
        comp_renames: Vec::new(),
        nested_funcs: seed_nested,
        nested_ir: Vec::new(),
        declared_nonlocals: HashSet::new(),
        cell_locals: HashMap::new(),
        yield_ty: None,
        type_refinements: HashMap::new(),
        pending_cell_inits: Vec::new(),
        try_depth: 0,
        local_imports: HashMap::new(),
        sibling_nonlocal_names: HashSet::new(),
        cell_candidates: HashSet::new(),
        late_bind_tys: HashMap::new(),
        outer_body_assigned: HashSet::new(),
        lowered_nested: HashSet::new(),
        storage_tys: HashMap::new(),
        current_class,
        self_param,
        classmethod_cls,
    };

    // Nonlocals declared in nested defs — free captures of these use cells.
    ctx.sibling_nonlocal_names = collect_nested_nonlocals(&f.body);
    ctx.cell_candidates = collect_cell_candidate_names(&f.body);
    ctx.outer_body_assigned = assigned_names_in_stmts(&f.body);
    // Types for late free captures (nested def before assignment).
    ctx.late_bind_tys = infer_late_bind_types(&f.body, &f.params, ctx.globals);
    // Names declared `global` in this function — do not treat as local storage.
    let global_decl_names: HashSet<String> = f
        .body
        .iter()
        .filter_map(|st| match &st.kind {
            ast::StmtKind::Global(names) => Some(names.iter().map(|(n, _)| n.clone())),
            _ => None,
        })
        .flatten()
        .collect();

    // Detect generator functions (contain yield / yield from).
    let is_gen = stmts_have_yield(&f.body);
    let mut gen_yield_ty: Option<ir::Ty> = None;
    if is_gen {
        // Yield type: use annotation of returns if present as element, else
        // scan for first yield value type after params are in scope — deferred
        // to after body lower would be ideal; use Int as default and refine.
        // Prefer declared ret as yield element when annotated non-None, else Int.
        let yty = match ctx.ret {
            ir::Ty::None => ir::Ty::Int, // bare/default; refined if body uses other
            other if !matches!(other, ir::Ty::Generator { .. }) => other,
            _ => ir::Ty::Int,
        };
        ctx.yield_ty = Some(yty);
        gen_yield_ty = Some(yty);
        // The *callable* appears to return Generator[Y]; resume IR uses i32.
        ctx.ret = ir::generator_of(yty);
        // Generators may contain try/except/finally; codegen re-arms setjmp
        // after yield resume. Nested generator functions are separate.
    }

    // Pre-register sibling nested function signatures so mutual / forward
    // references resolve (two-pass: sigs first, full lower on encounter).
    pre_register_nested_sigs(&f.body, &mut ctx)?;

    if let Some(caps) = &capture_params {
        for (name, ty) in caps {
            if ctx.locals.insert(name.clone(), *ty).is_some() {
                return Err(err(format!("duplicate parameter '{name}'"), f.span));
            }
            params.push((name.clone(), *ty));
            // Cell captures arrive as `.cell.<user>` params.
            if let Some(user) = name.strip_prefix(".cell.")
                && let ir::Ty::Cell(inner) = ty
            {
                ctx.cell_locals.insert(user.to_string(), **inner);
                ctx.declared_nonlocals.insert(user.to_string());
            }
        }
    }

    // Prefer collected/nested signature types (includes bare-param inference).
    let known_sig = mctx.funcs.get(&f.name).cloned().or({
        // Nested: look up provisional nested_funcs entry by source name.
        // When seed_nested was passed, those are already in ctx.nested_funcs
        // only after assignment — use resolve_params_with_body_infer.
        None
    });
    if let Some(sig) = known_sig {
        for p in &sig.params {
            if ctx.locals.insert(p.name.clone(), p.ty).is_some() {
                return Err(err(format!("duplicate parameter '{}'", p.name), f.span));
            }
            params.push((p.name.clone(), p.ty));
        }
        if let Some(p) = &sig.vararg {
            let ty = ir::list_of(p.ty);
            if ctx.locals.insert(p.name.clone(), ty).is_some() {
                return Err(err(format!("duplicate parameter '{}'", p.name), f.span));
            }
            params.push((p.name.clone(), ty));
        }
        if let Some(p) = &sig.kwarg {
            let ty = ir::dict_of(ir::Ty::Str, p.ty);
            if ctx.locals.insert(p.name.clone(), ty).is_some() {
                return Err(err(format!("duplicate parameter '{}'", p.name), f.span));
            }
            params.push((p.name.clone(), ty));
        }
    } else {
        let formals = resolve_params_with_body_infer(&f.params, &f.body)?;
        for p in &formals {
            if ctx.locals.insert(p.name.clone(), p.ty).is_some() {
                return Err(err(format!("duplicate parameter '{}'", p.name), f.span));
            }
            params.push((p.name.clone(), p.ty));
        }
        if let Some(p) = &f.vararg {
            let elem = resolve_param_ty(p)?;
            let ty = ir::list_of(elem);
            if ctx.locals.insert(p.name.clone(), ty).is_some() {
                return Err(err(format!("duplicate parameter '{}'", p.name), p.span));
            }
            params.push((p.name.clone(), ty));
        }
        if let Some(p) = &f.kwarg {
            let val = resolve_param_ty(p)?;
            let ty = ir::dict_of(ir::Ty::Str, val);
            if ctx.locals.insert(p.name.clone(), ty).is_some() {
                return Err(err(format!("duplicate parameter '{}'", p.name), p.span));
            }
            params.push((p.name.clone(), ty));
        }
    }

    // Joined storage types for multi-assign (do not pre-allocate locals).
    {
        let param_map: HashMap<String, ir::Ty> = params.iter().cloned().collect();
        let joined = collect_joined_local_types(&f.body, &param_map, ctx.globals);
        for (name, ty) in joined {
            if ctx.locals.contains_key(&name) || ctx.cell_locals.contains_key(&name) {
                continue; // params already typed
            }
            if global_decl_names.contains(&name) {
                // Function `global` name: widen module global if needed; never local.
                if let Some(existing) = ctx.globals.get(&name).copied() {
                    let j = join_types(existing, ty);
                    if j != existing {
                        let qname = ctx.own_global(&name);
                        ctx.globals.insert(name.clone(), j);
                        if let Some((_, t)) =
                            ctx.globals_order.iter_mut().find(|(n, _)| n == &qname)
                        {
                            *t = j;
                        }
                    }
                }
                continue;
            }
            if ctx.is_entry {
                // Module-level: join into globals map (seed may have first assign only).
                let qname = ctx.own_global(&name);
                if let Some(existing) = ctx.globals.get(&name).copied() {
                    let j = join_types(existing, ty);
                    if j != existing {
                        ctx.globals.insert(name.clone(), j);
                        if let Some((_, t)) =
                            ctx.globals_order.iter_mut().find(|(n, _)| n == &qname)
                        {
                            *t = j;
                        }
                    }
                } else {
                    ctx.globals.insert(name.clone(), ty);
                    ctx.globals_order.push((qname, ty));
                }
            } else {
                ctx.storage_tys.insert(name, ty);
            }
        }
    }

    // Box free-captured params / *args / **kwargs at entry (before any branch)
    // so later loads through the cell work even when the nested def is not executed.
    let mut entry_cell_inits: Vec<ir::Stmt> = Vec::new();
    let mut entry_names: Vec<String> = f.params.iter().map(|p| p.name.clone()).collect();
    if let Some(p) = &f.vararg {
        entry_names.push(p.name.clone());
    }
    if let Some(p) = &f.kwarg {
        entry_names.push(p.name.clone());
    }
    let candidates: Vec<(String, ir::Ty)> = entry_names
        .into_iter()
        .filter_map(|name| {
            if ctx.cell_candidates.contains(&name) {
                ctx.locals.get(&name).map(|ty| (name, *ty))
            } else {
                None
            }
        })
        .collect();
    for (name, ty) in candidates {
        if let Some(init) = ensure_cell(&mut ctx, &name, ty, f.span)? {
            entry_cell_inits.push(init);
        }
    }

    // Complete free-var capture lists for all nested defs before any body is
    // lowered so forward/mutual calls get correct leading cell args.
    // Runs after params/cells exist so free outer params are visible.
    complete_nested_captures(&f.body, &mut ctx)?;
    // Cells promoted during capture analysis (especially late free unbound
    // cells) must be allocated at function entry — before any assign that
    // CellStores into them. Leaving them on `pending_cell_inits` until the
    // nested `def` stmt runs stores into a null cell pointer.
    entry_cell_inits.append(&mut ctx.pending_cell_inits);

    // `math` module: replace stub bodies with MathCall intrinsics.
    let body = if mctx.module == "math"
        && let Some(op) = math_intrinsic(&f.name)
    {
        if params.len() != 1 {
            return Err(err(
                format!(
                    "math.{} must take exactly one parameter (found {})",
                    f.name,
                    params.len()
                ),
                f.span,
            ));
        }
        let (pname, pty) = &params[0];
        let arg = ir::Expr {
            ty: *pty,
            kind: ir::ExprKind::Local(pname.clone()),
        };
        // Accept int/float/bool param; coerce to float for libm.
        let arg = match arg.ty {
            ir::Ty::Float => arg,
            ir::Ty::Int => ir::Expr {
                ty: ir::Ty::Float,
                kind: ir::ExprKind::IntToFloat(Box::new(arg)),
            },
            ir::Ty::Bool => {
                let as_int = ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::BoolToInt(Box::new(arg)),
                };
                ir::Expr {
                    ty: ir::Ty::Float,
                    kind: ir::ExprKind::IntToFloat(Box::new(as_int)),
                }
            }
            other => {
                return Err(err(
                    format!("math.{} expects a numeric parameter, found {other}", f.name),
                    f.span,
                ));
            }
        };
        let ret_ty = match op {
            ir::MathOp::Floor | ir::MathOp::Ceil => ir::Ty::Int,
            _ => ir::Ty::Float,
        };
        if ctx.ret != ret_ty {
            return Err(err(
                format!(
                    "math.{} must be declared to return {ret_ty} (found {})",
                    f.name, ctx.ret
                ),
                f.span,
            ));
        }
        vec![ir::Stmt::Return(Some(ir::Expr {
            ty: ret_ty,
            kind: ir::ExprKind::MathCall {
                op,
                arg: Box::new(arg),
            },
        }))]
    } else if mctx.module == "os" && f.name == "getcwd" {
        if !params.is_empty() {
            return Err(err("os.getcwd must take no parameters".to_string(), f.span));
        }
        if ctx.ret != ir::Ty::Str {
            return Err(err(
                format!("os.getcwd must return str (found {})", ctx.ret),
                f.span,
            ));
        }
        vec![ir::Stmt::Return(Some(ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::OsGetcwd,
        }))]
    } else if mctx.module == "json"
        && let Some(kind) = json_loads_kind(&f.name)
    {
        if params.len() != 1 || params[0].1 != ir::Ty::Str {
            return Err(err(
                format!("json.{} must take a single str parameter", f.name),
                f.span,
            ));
        }
        let expected_ret = json_loads_ret(kind);
        if ctx.ret != expected_ret {
            return Err(err(
                format!(
                    "json.{} must be declared to return {expected_ret} (found {})",
                    f.name, ctx.ret
                ),
                f.span,
            ));
        }
        let arg = ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::Local(params[0].0.clone()),
        };
        vec![ir::Stmt::Return(Some(ir::Expr {
            ty: expected_ret,
            kind: ir::ExprKind::JsonLoads {
                kind,
                arg: Box::new(arg),
            },
        }))]
    } else if mctx.module == "json" && f.name == "dumps" {
        // Polymorphic: body never used; calls are special-cased. Keep a
        // trivial body so the function still exists for signature lookup.
        if params.len() != 1 {
            return Err(err(
                "json.dumps must take exactly one parameter".to_string(),
                f.span,
            ));
        }
        if ctx.ret != ir::Ty::Str {
            return Err(err(
                format!("json.dumps must return str (found {})", ctx.ret),
                f.span,
            ));
        }
        // Return dumps of the parameter (typed as str in the stub).
        let arg = ir::Expr {
            ty: params[0].1,
            kind: ir::ExprKind::Local(params[0].0.clone()),
        };
        vec![ir::Stmt::Return(Some(ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::JsonDumps(Box::new(arg)),
        }))]
    } else {
        lower_block(&f.body, &mut ctx)?
    };
    let body = {
        let mut b = entry_cell_inits;
        b.extend(body);
        b
    };

    // every path through a value-returning function must return
    // (generators end by falling off the end → StopIteration; no check)
    if !is_gen && ctx.ret != ir::Ty::None && !block_returns(&body) {
        return Err(err(
            format!(
                "function '{}' is declared to return {} but can reach the end \
                 of its body without a return statement",
                f.name, ctx.ret
            ),
            f.span,
        ));
    }

    // functions keep their given name for the init (already qualified); a
    // regular function is namespaced by its module
    let ir_name = if f.name == ENTRY_NAME || f.name.contains('.') {
        f.name.clone()
    } else {
        ctx.own_func(&f.name)
    };

    let nested = ctx.nested_ir;
    // Generator resume functions return i32 status (0=yielded, 1=done).
    let (fn_ret, is_generator, yield_ty) = if is_gen {
        (ir::Ty::Int, true, gen_yield_ty)
    } else {
        (ctx.ret, false, None)
    };
    Ok((
        ir::Function {
            name: ir_name,
            params,
            ret: fn_ret,
            locals: ctx.locals_order,
            body,
            is_generator,
            yield_ty,
        },
        nested,
    ))
}

fn stmts_have_yield(stmts: &[ast::Stmt]) -> bool {
    stmts.iter().any(stmt_has_yield)
}

fn stmt_has_yield(st: &ast::Stmt) -> bool {
    match &st.kind {
        ast::StmtKind::ExprStmt(e) => expr_has_yield(e),
        ast::StmtKind::Return(Some(e)) => expr_has_yield(e),
        ast::StmtKind::Assign { value, .. } => expr_has_yield(value),
        ast::StmtKind::If { branches, orelse } => {
            branches
                .iter()
                .any(|(c, b)| expr_has_yield(c) || stmts_have_yield(b))
                || stmts_have_yield(orelse)
        }
        ast::StmtKind::While { cond, body, orelse } => {
            expr_has_yield(cond) || stmts_have_yield(body) || stmts_have_yield(orelse)
        }
        ast::StmtKind::For {
            iter, body, orelse, ..
        } => expr_has_yield(iter) || stmts_have_yield(body) || stmts_have_yield(orelse),
        ast::StmtKind::Try {
            body,
            handlers,
            orelse,
            finally,
        } => {
            stmts_have_yield(body)
                || handlers.iter().any(|h| stmts_have_yield(&h.body))
                || stmts_have_yield(orelse)
                || stmts_have_yield(finally)
        }
        ast::StmtKind::With { body, .. } => stmts_have_yield(body),
        ast::StmtKind::Match { cases, .. } => cases.iter().any(|c| stmts_have_yield(&c.body)),
        ast::StmtKind::FuncDef(_) => false, // nested gens are separate
        _ => false,
    }
}

fn expr_has_yield(e: &ast::Expr) -> bool {
    match &e.kind {
        ast::ExprKind::Yield(_) | ast::ExprKind::YieldFrom(_) => true,
        ast::ExprKind::Binary { left, right, .. } => expr_has_yield(left) || expr_has_yield(right),
        ast::ExprKind::Unary { operand, .. } => expr_has_yield(operand),
        ast::ExprKind::Call {
            args,
            keywords,
            kwargs,
            ..
        } => {
            args.iter().any(|a| match a {
                ast::PosArg::Pos(e) | ast::PosArg::Star(e) => expr_has_yield(e),
            }) || keywords.iter().any(|k| expr_has_yield(&k.value))
                || kwargs.as_ref().is_some_and(|k| expr_has_yield(k))
        }
        ast::ExprKind::ListLit(items) => items.iter().any(|i| match i {
            ast::ListElem::Item(e) | ast::ListElem::Star(e) => expr_has_yield(e),
        }),
        ast::ExprKind::TupleLit(items) | ast::ExprKind::SetLit(items) => {
            items.iter().any(expr_has_yield)
        }
        _ => false,
    }
}

fn json_loads_kind(name: &str) -> Option<ir::JsonLoadsKind> {
    Some(match name {
        "loads_int" => ir::JsonLoadsKind::Int,
        "loads_float" => ir::JsonLoadsKind::Float,
        "loads_bool" => ir::JsonLoadsKind::Bool,
        "loads_str" => ir::JsonLoadsKind::Str,
        "loads_list_int" => ir::JsonLoadsKind::ListInt,
        "loads_list_float" => ir::JsonLoadsKind::ListFloat,
        "loads_list_str" => ir::JsonLoadsKind::ListStr,
        "loads_list_bool" => ir::JsonLoadsKind::ListBool,
        "loads_dict_str_int" => ir::JsonLoadsKind::DictStrInt,
        "loads_dict_str_float" => ir::JsonLoadsKind::DictStrFloat,
        "loads_dict_str_str" => ir::JsonLoadsKind::DictStrStr,
        "loads_dict_str_bool" => ir::JsonLoadsKind::DictStrBool,
        _ => return None,
    })
}

fn json_loads_ret(kind: ir::JsonLoadsKind) -> ir::Ty {
    match kind {
        ir::JsonLoadsKind::Int => ir::Ty::Int,
        ir::JsonLoadsKind::Float => ir::Ty::Float,
        ir::JsonLoadsKind::Bool => ir::Ty::Bool,
        ir::JsonLoadsKind::Str => ir::Ty::Str,
        ir::JsonLoadsKind::ListInt => ir::list_of(ir::Ty::Int),
        ir::JsonLoadsKind::ListFloat => ir::list_of(ir::Ty::Float),
        ir::JsonLoadsKind::ListStr => ir::list_of(ir::Ty::Str),
        ir::JsonLoadsKind::ListBool => ir::list_of(ir::Ty::Bool),
        ir::JsonLoadsKind::DictStrInt => ir::dict_of(ir::Ty::Str, ir::Ty::Int),
        ir::JsonLoadsKind::DictStrFloat => ir::dict_of(ir::Ty::Str, ir::Ty::Float),
        ir::JsonLoadsKind::DictStrStr => ir::dict_of(ir::Ty::Str, ir::Ty::Str),
        ir::JsonLoadsKind::DictStrBool => ir::dict_of(ir::Ty::Str, ir::Ty::Bool),
    }
}

fn lower_block(stmts: &[ast::Stmt], ctx: &mut FnCtx) -> SResult<Vec<ir::Stmt>> {
    let mut out = Vec::new();
    for stmt in stmts {
        lower_stmt(stmt, ctx, &mut out)?;
    }
    Ok(out)
}

/// Pre-scan nested `def`s at one nesting level and register provisional
/// signatures so forward / mutual sibling calls type-check.
fn pre_register_nested_sigs(stmts: &[ast::Stmt], ctx: &mut FnCtx) -> SResult<()> {
    for st in stmts {
        match &st.kind {
            ast::StmtKind::FuncDef(f) => {
                pre_register_one_nested_sig(f, ctx)?;
            }
            // Nested defs inside control flow are still function-local siblings.
            ast::StmtKind::If { branches, orelse } => {
                for (_, b) in branches {
                    pre_register_nested_sigs(b, ctx)?;
                }
                pre_register_nested_sigs(orelse, ctx)?;
            }
            ast::StmtKind::While { body, orelse, .. } | ast::StmtKind::For { body, orelse, .. } => {
                pre_register_nested_sigs(body, ctx)?;
                pre_register_nested_sigs(orelse, ctx)?;
            }
            ast::StmtKind::Try {
                body,
                handlers,
                orelse,
                finally,
            } => {
                pre_register_nested_sigs(body, ctx)?;
                for h in handlers {
                    pre_register_nested_sigs(&h.body, ctx)?;
                }
                pre_register_nested_sigs(orelse, ctx)?;
                pre_register_nested_sigs(finally, ctx)?;
            }
            ast::StmtKind::With { body, .. } => pre_register_nested_sigs(body, ctx)?,
            ast::StmtKind::Match { cases, .. } => {
                for c in cases {
                    pre_register_nested_sigs(&c.body, ctx)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn pre_register_one_nested_sig(f: &ast::FuncDef, ctx: &mut FnCtx) -> SResult<()> {
    if ctx.nested_funcs.contains_key(&f.name) {
        return Ok(()); // already provisional or fully lowered
    }
    if ctx.locals.contains_key(&f.name) {
        return Ok(());
    }
    let params = resolve_params_with_body_infer(&f.params, &f.body)?;
    let mut seen: HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
    let vararg = if let Some(p) = &f.vararg {
        let ty = resolve_param_ty(p)?;
        seen.insert(p.name.clone());
        Some(ParamSig {
            name: p.name.clone(),
            ty,
            default: None,
        })
    } else {
        None
    };
    let kwarg = if let Some(p) = &f.kwarg {
        let ty = resolve_param_ty(p)?;
        seen.insert(p.name.clone());
        Some(ParamSig {
            name: p.name.clone(),
            ty,
            default: None,
        })
    } else {
        None
    };
    let mut ret = match f.ret {
        Some(t) => resolve_type_checked(t, f.span)?,
        Option::None => ir::Ty::None,
    };
    let is_generator = stmts_have_yield(&f.body);
    let yield_ty = if is_generator {
        let y = if ret != ir::Ty::None {
            ret
        } else {
            ir::Ty::Int
        };
        ret = ir::generator_of(y);
        Some(y)
    } else {
        None
    };
    let ir_name = if ctx.mctx.is_root {
        format!("{}.{}", ctx.fn_name, f.name)
    } else {
        format!("{}.{}.{}", ctx.mctx.module, ctx.fn_name, f.name)
    };
    let sig = FuncSig {
        params,
        vararg,
        kwarg,
        ret,
        span: f.span,
        is_generator,
        yield_ty,
        gen_frame_slots: 0,
    };
    ctx.nested_funcs.insert(
        f.name.clone(),
        NestedFnInfo {
            ir_name,
            sig,
            captures: Vec::new(),
            capture_is_cell: Vec::new(),
            uses_env: false,
        },
    );
    Ok(())
}

/// Fixed-point free-var capture analysis for all nested defs at this scope so
/// forward / mutual sibling calls see complete capture lists (not provisional []).
fn complete_nested_captures(stmts: &[ast::Stmt], ctx: &mut FnCtx) -> SResult<()> {
    let mut nested_defs: Vec<ast::FuncDef> = Vec::new();
    collect_nested_func_defs(stmts, &mut nested_defs);
    if nested_defs.is_empty() {
        return Ok(());
    }
    // Iterate until capture sets stabilize (sibling cell threading).
    // Do **not** allocate cells here — that still happens when each nested
    // def is lowered (pending inits flush at the def site so assign order
    // stays correct). This pass only fills NestedFnInfo.captures so forward
    // call sites see complete leading-cell ABI.
    for _ in 0..(nested_defs.len() + 2) {
        let mut changed = false;
        for f in &nested_defs {
            let (captures, capture_is_cell) = analyze_nested_captures(f, ctx)?;
            let Some(info) = ctx.nested_funcs.get_mut(&f.name) else {
                continue;
            };
            if info.captures != captures || info.capture_is_cell != capture_is_cell {
                info.captures = captures;
                info.capture_is_cell = capture_is_cell;
                info.uses_env = info.capture_is_cell.iter().any(|b| *b);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    Ok(())
}

fn collect_nested_func_defs(stmts: &[ast::Stmt], out: &mut Vec<ast::FuncDef>) {
    for st in stmts {
        match &st.kind {
            ast::StmtKind::FuncDef(f) => out.push(f.clone()),
            ast::StmtKind::If { branches, orelse } => {
                for (_, b) in branches {
                    collect_nested_func_defs(b, out);
                }
                collect_nested_func_defs(orelse, out);
            }
            ast::StmtKind::While { body, orelse, .. } | ast::StmtKind::For { body, orelse, .. } => {
                collect_nested_func_defs(body, out);
                collect_nested_func_defs(orelse, out);
            }
            ast::StmtKind::Try {
                body,
                handlers,
                orelse,
                finally,
            } => {
                collect_nested_func_defs(body, out);
                for h in handlers {
                    collect_nested_func_defs(&h.body, out);
                }
                collect_nested_func_defs(orelse, out);
                collect_nested_func_defs(finally, out);
            }
            ast::StmtKind::With { body, .. } => collect_nested_func_defs(body, out),
            ast::StmtKind::Match { cases, .. } => {
                for c in cases {
                    collect_nested_func_defs(&c.body, out);
                }
            }
            _ => {}
        }
    }
}

type NestedCaptures = (Vec<(String, ir::Ty)>, Vec<bool>);

/// Capture list for a nested def (own free vars + sibling-threaded cells).
fn analyze_nested_captures(f: &ast::FuncDef, ctx: &FnCtx) -> SResult<NestedCaptures> {
    let mut seen = HashSet::new();
    for p in &f.params {
        seen.insert(p.name.clone());
    }
    if let Some(p) = &f.vararg {
        seen.insert(p.name.clone());
    }
    if let Some(p) = &f.kwarg {
        seen.insert(p.name.clone());
    }
    let used = free_names_used_in_stmts(&f.body);
    let assigned = assigned_names_in_stmts(&f.body);
    let nonlocals = collect_nonlocals_in_stmts(&f.body);

    let mut captures: Vec<(String, ir::Ty)> = Vec::new();
    let mut capture_set = HashSet::new();
    let mut capture_is_cell: Vec<bool> = Vec::new();
    let mut candidate_names: Vec<String> = Vec::new();
    for (n, _) in &ctx.locals_order {
        if used.contains(n) || nonlocals.contains(n) {
            candidate_names.push(n.clone());
        }
    }
    for n in used.iter().chain(nonlocals.iter()) {
        if !candidate_names.iter().any(|x| x == n) {
            candidate_names.push(n.clone());
        }
    }
    for name in &candidate_names {
        if seen.contains(name) {
            continue;
        }
        let is_nl = nonlocals.contains(name);
        if assigned.contains(name) && !is_nl {
            continue;
        }
        if let Some(ty) = ctx.locals.get(name).copied() {
            if capture_set.insert(name.clone()) {
                captures.push((name.clone(), ty));
                capture_is_cell.push(true);
            }
        } else if let Some(ty) = ctx.cell_locals.get(name).copied() {
            if capture_set.insert(name.clone()) {
                captures.push((name.clone(), ty));
                capture_is_cell.push(true);
            }
        } else if let Some(ty) = ctx.late_bind_tys.get(name).copied()
            && capture_set.insert(name.clone())
        {
            captures.push((name.clone(), ty));
            capture_is_cell.push(true);
        }
    }
    // Sibling nested calls: thread cell captures through callers.
    let mut called = HashSet::new();
    collect_called_func_names_in_stmts(&f.body, &mut called);
    for cname in &called {
        let Some(cinfo) = ctx.nested_funcs.get(cname) else {
            continue;
        };
        for (i, (n, ty)) in cinfo.captures.iter().enumerate() {
            if !cinfo.capture_is_cell.get(i).copied().unwrap_or(false) {
                continue;
            }
            if capture_set.contains(n) {
                continue;
            }
            let cell_ty = ctx.cell_locals.get(n).copied().unwrap_or(*ty);
            if capture_set.insert(n.clone()) {
                captures.push((n.clone(), cell_ty));
                capture_is_cell.push(true);
            }
        }
    }
    Ok((captures, capture_is_cell))
}

fn free_names_used_in_stmts(stmts: &[ast::Stmt]) -> HashSet<String> {
    let mut s = HashSet::new();
    for st in stmts {
        collect_used_names_in_stmt(st, &mut s);
    }
    s
}

/// Infer types of simple assignments for late free-var cell allocation.
fn infer_late_bind_types(
    stmts: &[ast::Stmt],
    params: &[ast::Param],
    globals: &HashMap<String, ir::Ty>,
) -> HashMap<String, ir::Ty> {
    let mut out = HashMap::new();
    let mut param_tys: HashMap<String, ir::Ty> = HashMap::new();
    for p in params {
        if let Ok(ty) = resolve_param_ty(p) {
            param_tys.insert(p.name.clone(), ty);
        }
    }
    infer_late_bind_types_in(stmts, &param_tys, globals, &mut out);
    out
}

fn infer_late_bind_types_in(
    stmts: &[ast::Stmt],
    params: &HashMap<String, ir::Ty>,
    globals: &HashMap<String, ir::Ty>,
    out: &mut HashMap<String, ir::Ty>,
) {
    for st in stmts {
        match &st.kind {
            ast::StmtKind::Assign {
                targets,
                value,
                annotation,
                ..
            } => {
                let ty = if let Some(ann) = annotation {
                    resolve_type_checked(*ann, st.span).ok()
                } else {
                    guess_expr_ty(value, params, globals, out)
                };
                if let Some(ty) = ty {
                    for t in targets {
                        if let ast::AssignTarget::Name { name, .. } = t {
                            out.entry(name.clone()).or_insert(ty);
                        }
                    }
                }
            }
            ast::StmtKind::If { branches, orelse } => {
                for (_, b) in branches {
                    infer_late_bind_types_in(b, params, globals, out);
                }
                infer_late_bind_types_in(orelse, params, globals, out);
            }
            ast::StmtKind::While { body, orelse, .. } | ast::StmtKind::For { body, orelse, .. } => {
                infer_late_bind_types_in(body, params, globals, out);
                infer_late_bind_types_in(orelse, params, globals, out);
            }
            ast::StmtKind::Try {
                body,
                handlers,
                orelse,
                finally,
            } => {
                infer_late_bind_types_in(body, params, globals, out);
                for h in handlers {
                    infer_late_bind_types_in(&h.body, params, globals, out);
                }
                infer_late_bind_types_in(orelse, params, globals, out);
                infer_late_bind_types_in(finally, params, globals, out);
            }
            ast::StmtKind::With { body, .. } => {
                infer_late_bind_types_in(body, params, globals, out);
            }
            ast::StmtKind::Match { cases, .. } => {
                for c in cases {
                    infer_late_bind_types_in(&c.body, params, globals, out);
                }
            }
            // Nested defs don't assign into outer for this scan.
            _ => {}
        }
    }
}

fn guess_expr_ty(
    e: &ast::Expr,
    params: &HashMap<String, ir::Ty>,
    globals: &HashMap<String, ir::Ty>,
    known: &HashMap<String, ir::Ty>,
) -> Option<ir::Ty> {
    match &e.kind {
        ast::ExprKind::Int(_) | ast::ExprKind::IntDigits(_) => Some(ir::Ty::Int),
        ast::ExprKind::Float(_) => Some(ir::Ty::Float),
        ast::ExprKind::Bool(_) => Some(ir::Ty::Bool),
        ast::ExprKind::Str(_) => Some(ir::Ty::Str),
        ast::ExprKind::NoneLit => Some(ir::Ty::None),
        ast::ExprKind::Name(n) => params
            .get(n)
            .copied()
            .or_else(|| known.get(n).copied())
            .or_else(|| globals.get(n).copied()),
        ast::ExprKind::ListLit(items) if !items.is_empty() => {
            // Homogeneous list of guessed element types.
            let mut ety = None;
            for it in items {
                let ast::ListElem::Item(e) = it else {
                    return None;
                };
                let t = guess_expr_ty(e, params, globals, known)?;
                ety = Some(match ety {
                    None => t,
                    Some(prev) => join_elem_types(prev, t)?,
                });
            }
            ety.map(ir::list_of)
        }
        _ => None,
    }
}

/// Ensure a late-bound free var has an unbound cell (NameError on load until assign).
fn ensure_cell_unbound(
    ctx: &mut FnCtx,
    name: &str,
    ty: ir::Ty,
    span: Span,
) -> SResult<Option<ir::Stmt>> {
    if ctx.cell_locals.contains_key(name) {
        return Ok(None);
    }
    // Reject types we cannot store once assigned (same subset as before).
    match ty {
        ir::Ty::Int
        | ir::Ty::Bool
        | ir::Ty::Float
        | ir::Ty::Str
        | ir::Ty::None
        | ir::Ty::Union(_) => {}
        other => {
            return Err(err(
                format!(
                    "late free-variable '{name}' of type {other} cannot be \
                     cell-allocated before assignment in this subset"
                ),
                span,
            ));
        }
    }
    let cell_name = format!(".cell.{name}");
    if !ctx.locals.contains_key(&cell_name) {
        ctx.locals.insert(cell_name.clone(), ir::cell_of(ty));
        ctx.locals_order.push((cell_name.clone(), ir::cell_of(ty)));
        // Unbound until first CellStore (CPython NameError on free load).
        let init = ir::Stmt::Assign {
            name: cell_name,
            value: ir::Expr {
                ty: ir::cell_of(ty),
                kind: ir::ExprKind::CellNewUnbound,
            },
        };
        ctx.cell_locals.insert(name.to_string(), ty);
        return Ok(Some(init));
    }
    ctx.cell_locals.insert(name.to_string(), ty);
    Ok(None)
}

/// Lower a nested `def` inside a function: capture free vars as leading
/// parameters (cells for outer locals); register the name for local calls.
fn lower_nested_func_def(f: &ast::FuncDef, ctx: &mut FnCtx) -> SResult<()> {
    if ctx.locals.contains_key(&f.name) || ctx.lowered_nested.contains(&f.name) {
        return Err(err(
            format!(
                "function '{}' is defined more than once in this scope",
                f.name
            ),
            f.span,
        ));
    }
    if BUILTINS.contains(&f.name.as_str()) {
        return Err(err(
            format!("cannot redefine the builtin '{}'", f.name),
            f.span,
        ));
    }

    // Build nested signature (params / *args / **kwargs) with bare-param infer.
    let params = resolve_params_with_body_infer(&f.params, &f.body)?;
    let mut seen: HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
    let vararg = if let Some(p) = &f.vararg {
        let ty = resolve_param_ty(p)?;
        if !seen.insert(p.name.clone()) {
            return Err(err(
                format!("duplicate parameter name '{}'", p.name),
                p.span,
            ));
        }
        Some(ParamSig {
            name: p.name.clone(),
            ty,
            default: None,
        })
    } else {
        None
    };
    let kwarg = if let Some(p) = &f.kwarg {
        let ty = resolve_param_ty(p)?;
        if !seen.insert(p.name.clone()) {
            return Err(err(
                format!("duplicate parameter name '{}'", p.name),
                p.span,
            ));
        }
        Some(ParamSig {
            name: p.name.clone(),
            ty,
            default: None,
        })
    } else {
        None
    };
    let mut ret = match f.ret {
        Some(t) => resolve_type_checked(t, f.span)?,
        Option::None => ir::Ty::None,
    };
    let is_generator = stmts_have_yield(&f.body);
    let yield_ty = if is_generator {
        let y = if ret != ir::Ty::None {
            ret
        } else {
            ir::Ty::Int
        };
        ret = ir::generator_of(y);
        Some(y)
    } else {
        None
    };
    let sig = FuncSig {
        params,
        vararg,
        kwarg,
        ret,
        span: f.span,
        is_generator,
        yield_ty,
        gen_frame_slots: 0,
    };

    // Free vars: names loaded in nested body that resolve to outer locals.
    let assigned = assigned_names_in_stmts(&f.body);
    let mut used = HashSet::new();
    collect_used_names_in_stmts(&f.body, &mut used);
    for p in &f.params {
        if let Some(d) = &p.default {
            collect_used_names_in_expr(d, &mut used);
        }
    }
    // Nonlocal names in the nested body
    let nonlocals = collect_nonlocals_in_stmts(&f.body);

    let mut captures: Vec<(String, ir::Ty)> = Vec::new();
    let mut capture_set = HashSet::new();
    let mut capture_is_cell: Vec<bool> = Vec::new();
    let mut candidate_names: Vec<String> = Vec::new();
    for (n, _) in &ctx.locals_order {
        if used.contains(n) || nonlocals.contains(n) {
            candidate_names.push(n.clone());
        }
    }
    for n in used.iter().chain(nonlocals.iter()) {
        if !candidate_names.iter().any(|x| x == n) {
            candidate_names.push(n.clone());
        }
    }
    for name in &candidate_names {
        if seen.contains(name) {
            continue;
        }
        let is_nl = nonlocals.contains(name);
        // Assigned without nonlocal → new local in nested (not a capture).
        if assigned.contains(name) && !is_nl {
            continue;
        }
        if let Some(ty) = ctx.locals.get(name).copied() {
            if capture_set.insert(name.clone()) {
                // Free outer locals always use cells so escaping closures see
                // later outer assignments (CPython cell semantics). Nested
                // *assignment* still requires an explicit `nonlocal`.
                // (Previously only nonlocal / sibling-nonlocal used cells;
                // by-value capture froze the value at MakeClosure time.)
                if let Some(init) = ensure_cell(ctx, name, ty, f.span)? {
                    ctx.pending_cell_inits.push(init);
                }
                captures.push((name.clone(), ty));
                capture_is_cell.push(true);
            }
        } else if ctx.cell_locals.contains_key(name) {
            let ty = ctx.cell_locals[name];
            if capture_set.insert(name.clone()) {
                captures.push((name.clone(), ty));
                capture_is_cell.push(true);
            }
        } else if let Some(ty) = ctx.late_bind_tys.get(name).copied() {
            // Free name assigned later in the same outer block (CPython cell):
            //   def f(): return n
            //   n = 5
            if capture_set.insert(name.clone()) {
                if let Some(init) = ensure_cell_unbound(ctx, name, ty, f.span)? {
                    ctx.pending_cell_inits.push(init);
                }
                captures.push((name.clone(), ty));
                capture_is_cell.push(true);
            }
        } else if is_nl {
            return Err(err(
                format!("no binding for nonlocal '{name}' found"),
                f.span,
            ));
        } else if ctx.nested_funcs.contains_key(name) {
            // Sibling nested function: call by name; capturing the function
            // object is handled via MakeClosure when used as a value.
            // Not a free data capture.
        }
    }

    // Sibling nested calls: if this body calls a nested def that needs cell
    // captures, thread those cells through this function too (CPython: all
    // nested funcs share the same outer cells via the closure environment).
    // Capture lists were completed in `complete_nested_captures` so forward /
    // mutual sibling calls already see full free-var sets.
    {
        let mut called = HashSet::new();
        collect_called_func_names_in_stmts(&f.body, &mut called);
        // Snapshot (name, ty) pairs so we can mutate ctx after.
        let mut needed: Vec<(String, ir::Ty)> = Vec::new();
        for cname in &called {
            let Some(cinfo) = ctx.nested_funcs.get(cname) else {
                continue;
            };
            for (i, (n, ty)) in cinfo.captures.iter().enumerate() {
                if !cinfo.capture_is_cell.get(i).copied().unwrap_or(false) {
                    continue;
                }
                if capture_set.contains(n) {
                    continue;
                }
                let cell_ty = ctx.cell_locals.get(n).copied().unwrap_or(*ty);
                needed.push((n.clone(), cell_ty));
            }
        }
        for (n, cell_ty) in needed {
            if !capture_set.insert(n.clone()) {
                continue;
            }
            // Late free cells (assigned after this nested def in the outer)
            // are not yet in `locals` — use unbound allocation, not ensure_cell.
            if ctx.locals.contains_key(&n) {
                if let Some(init) = ensure_cell(ctx, &n, cell_ty, f.span)? {
                    ctx.pending_cell_inits.push(init);
                }
            } else if ctx.cell_locals.contains_key(&n) {
                // already cell-backed
            } else if let Some(ty) = ctx.late_bind_tys.get(&n).copied() {
                if let Some(init) = ensure_cell_unbound(ctx, &n, ty, f.span)? {
                    ctx.pending_cell_inits.push(init);
                }
            } else {
                return Err(err(format!("no binding for nonlocal '{n}' found"), f.span));
            }
            captures.push((n, cell_ty));
            capture_is_cell.push(true);
        }
    }

    let ir_name = format!("{}.{}", ctx.fn_name, f.name);
    // qualify with module for non-root
    let ir_name = if ctx.mctx.is_root {
        ir_name
    } else {
        format!("{}.{}", ctx.mctx.module, ir_name)
    };

    // Build a FuncDef with a fully-qualified name for IR.
    let nested_def = ast::FuncDef {
        name: ir_name.clone(),
        params: f.params.clone(),
        vararg: f.vararg.clone(),
        kwarg: f.kwarg.clone(),
        ret: f.ret,
        body: f.body.clone(),
        span: f.span,
        decorators: f.decorators.clone(),
    };

    let uses_env = capture_is_cell.iter().any(|b| *b);
    let info = NestedFnInfo {
        ir_name: ir_name.clone(),
        sig: sig.clone(),
        captures: captures.clone(),
        capture_is_cell: capture_is_cell.clone(),
        uses_env,
    };
    // Seed: already-defined siblings + self (for recursion).
    let mut seed = ctx.nested_funcs.clone();
    seed.insert(f.name.clone(), info.clone());

    // Pass cell-typed captures as leading params when needed.
    let cap_params: Vec<(String, ir::Ty)> = captures
        .iter()
        .zip(capture_is_cell.iter())
        .map(|((n, ty), is_cell)| {
            if *is_cell {
                (format!(".cell.{n}"), ir::cell_of(*ty))
            } else {
                (n.clone(), *ty)
            }
        })
        .collect();
    let (func, more) = lower_function_inner(
        &nested_def,
        ctx.mctx,
        ctx.globals,
        ctx.globals_order,
        false,
        Some(cap_params),
        seed,
        // Nested defs are not methods for zero-arg super() purposes.
        None,
    )?;
    // Patch NestedFnInfo.ret from the lowered body (optional return inference
    // / cell-union returns) so callers see a valued ret, not void.
    let mut info = info;
    if !func.is_generator {
        info.sig.ret = func.ret;
    }
    // Register cell locals + nonlocals inside nested so loads/stores use cells.
    // (lower_function_inner already put cell params in locals as `.cell.n`.)
    ctx.nested_ir.push(func);
    ctx.nested_ir.extend(more);

    register_closure_defaults(&info.ir_name, &info.sig.params);
    ctx.nested_funcs.insert(f.name.clone(), info);
    ctx.lowered_nested.insert(f.name.clone());
    Ok(())
}

fn collect_nonlocals_in_stmts(stmts: &[ast::Stmt]) -> HashSet<String> {
    let mut s = HashSet::new();
    for st in stmts {
        collect_nonlocals_in_stmt(st, &mut s);
    }
    s
}

/// Nonlocal names declared in nested function bodies (not this function's own).
fn collect_nested_nonlocals(stmts: &[ast::Stmt]) -> HashSet<String> {
    let mut s = HashSet::new();
    for st in stmts {
        match &st.kind {
            ast::StmtKind::FuncDef(f) => {
                s.extend(collect_nonlocals_in_stmts(&f.body));
            }
            ast::StmtKind::If { branches, orelse } => {
                for (_, b) in branches {
                    s.extend(collect_nested_nonlocals(b));
                }
                s.extend(collect_nested_nonlocals(orelse));
            }
            ast::StmtKind::While { body, orelse, .. } | ast::StmtKind::For { body, orelse, .. } => {
                s.extend(collect_nested_nonlocals(body));
                s.extend(collect_nested_nonlocals(orelse));
            }
            ast::StmtKind::Try {
                body,
                handlers,
                orelse,
                finally,
            } => {
                s.extend(collect_nested_nonlocals(body));
                for h in handlers {
                    s.extend(collect_nested_nonlocals(&h.body));
                }
                s.extend(collect_nested_nonlocals(orelse));
                s.extend(collect_nested_nonlocals(finally));
            }
            ast::StmtKind::With { body, .. } => s.extend(collect_nested_nonlocals(body)),
            ast::StmtKind::Match { cases, .. } => {
                for c in cases {
                    s.extend(collect_nested_nonlocals(&c.body));
                }
            }
            _ => {}
        }
    }
    s
}

fn collect_nonlocals_in_stmt(st: &ast::Stmt, out: &mut HashSet<String>) {
    match &st.kind {
        ast::StmtKind::Nonlocal(names) => {
            for (n, _) in names {
                out.insert(n.clone());
            }
        }
        ast::StmtKind::If { branches, orelse } => {
            for (_, b) in branches {
                for s in b {
                    collect_nonlocals_in_stmt(s, out);
                }
            }
            for s in orelse {
                collect_nonlocals_in_stmt(s, out);
            }
        }
        ast::StmtKind::While { body, orelse, .. } | ast::StmtKind::For { body, orelse, .. } => {
            for s in body {
                collect_nonlocals_in_stmt(s, out);
            }
            for s in orelse {
                collect_nonlocals_in_stmt(s, out);
            }
        }
        ast::StmtKind::Try {
            body,
            handlers,
            orelse,
            finally,
        } => {
            for s in body {
                collect_nonlocals_in_stmt(s, out);
            }
            for h in handlers {
                for s in &h.body {
                    collect_nonlocals_in_stmt(s, out);
                }
            }
            for s in orelse {
                collect_nonlocals_in_stmt(s, out);
            }
            for s in finally {
                collect_nonlocals_in_stmt(s, out);
            }
        }
        ast::StmtKind::With { body, .. } => {
            for s in body {
                collect_nonlocals_in_stmt(s, out);
            }
        }
        ast::StmtKind::Match { cases, .. } => {
            for c in cases {
                for s in &c.body {
                    collect_nonlocals_in_stmt(s, out);
                }
            }
        }
        ast::StmtKind::FuncDef(_) => {
            // Nested function's nonlocal is its own concern.
        }
        _ => {}
    }
}

/// Merge refinements after an if with multiple fallthrough arms.
/// Drops peels for names assigned in any arm; keeps a peel only when every
/// fallthrough arm agrees on the same concrete refinement (or all lack one).
fn merge_fallthrough_refinements(
    dest: &mut HashMap<String, ir::Ty>,
    exits: &[HashMap<String, ir::Ty>],
    assigned: &HashSet<String>,
) {
    for name in assigned {
        dest.remove(name);
    }
    if exits.is_empty() {
        return;
    }
    let mut keys: HashSet<String> = dest.keys().cloned().collect();
    for e in exits {
        keys.extend(e.keys().cloned());
    }
    for name in keys {
        if assigned.contains(&name) {
            continue;
        }
        let first = exits[0].get(&name).copied();
        if exits.iter().all(|e| e.get(&name).copied() == first) {
            if let Some(t) = first {
                dest.insert(name, t);
            } else {
                dest.remove(&name);
            }
        } else {
            dest.remove(&name);
        }
    }
}

/// Names free-captured by nested `def`/`lambda` in `stmts` (over-approx ok).
fn collect_cell_candidate_names(stmts: &[ast::Stmt]) -> HashSet<String> {
    let mut out = HashSet::new();
    collect_cell_candidates_in_stmts(stmts, &mut out);
    out
}

fn collect_cell_candidates_in_stmts(stmts: &[ast::Stmt], out: &mut HashSet<String>) {
    for st in stmts {
        match &st.kind {
            ast::StmtKind::FuncDef(f) => {
                add_nested_free_names(f, out);
            }
            ast::StmtKind::If { branches, orelse } => {
                for (c, b) in branches {
                    collect_cell_candidates_in_expr(c, out);
                    collect_cell_candidates_in_stmts(b, out);
                }
                collect_cell_candidates_in_stmts(orelse, out);
            }
            ast::StmtKind::While { cond, body, orelse } => {
                collect_cell_candidates_in_expr(cond, out);
                collect_cell_candidates_in_stmts(body, out);
                collect_cell_candidates_in_stmts(orelse, out);
            }
            ast::StmtKind::For {
                iter, body, orelse, ..
            } => {
                collect_cell_candidates_in_expr(iter, out);
                collect_cell_candidates_in_stmts(body, out);
                collect_cell_candidates_in_stmts(orelse, out);
            }
            ast::StmtKind::Try {
                body,
                handlers,
                orelse,
                finally,
            } => {
                collect_cell_candidates_in_stmts(body, out);
                for h in handlers {
                    collect_cell_candidates_in_stmts(&h.body, out);
                }
                collect_cell_candidates_in_stmts(orelse, out);
                collect_cell_candidates_in_stmts(finally, out);
            }
            ast::StmtKind::With { item, body, .. } => {
                collect_cell_candidates_in_expr(item, out);
                collect_cell_candidates_in_stmts(body, out);
            }
            ast::StmtKind::Match { subject, cases } => {
                collect_cell_candidates_in_expr(subject, out);
                for c in cases {
                    if let Some(g) = &c.guard {
                        collect_cell_candidates_in_expr(g, out);
                    }
                    collect_cell_candidates_in_stmts(&c.body, out);
                }
            }
            ast::StmtKind::Assign { value, .. } => collect_cell_candidates_in_expr(value, out),
            ast::StmtKind::AugAssign { value, .. } => collect_cell_candidates_in_expr(value, out),
            ast::StmtKind::Return(Some(e)) | ast::StmtKind::ExprStmt(e) => {
                collect_cell_candidates_in_expr(e, out);
            }
            ast::StmtKind::Delete { target } => {
                collect_cell_candidates_in_target(target, out);
            }
            _ => {}
        }
    }
}

fn collect_cell_candidates_in_target(t: &ast::AssignTarget, out: &mut HashSet<String>) {
    match t {
        ast::AssignTarget::Index { base, index } => {
            collect_cell_candidates_in_expr(base, out);
            collect_cell_candidates_in_expr(index, out);
        }
        ast::AssignTarget::Attr { base, .. } => {
            collect_cell_candidates_in_expr(base, out);
        }
        ast::AssignTarget::Tuple(ts) => {
            for t in ts {
                collect_cell_candidates_in_target(t, out);
            }
        }
        ast::AssignTarget::Starred { target, .. } => collect_cell_candidates_in_target(target, out),
        ast::AssignTarget::Name { .. } => {}
    }
}

fn collect_cell_candidates_in_expr(e: &ast::Expr, out: &mut HashSet<String>) {
    walk_expr_for_lambdas(e, out);
}

fn walk_expr_for_lambdas(e: &ast::Expr, out: &mut HashSet<String>) {
    match &e.kind {
        ast::ExprKind::Lambda { params, body } => {
            let mut used = HashSet::new();
            collect_used_names_in_expr(body, &mut used);
            for p in params {
                if let Some(d) = &p.default {
                    collect_used_names_in_expr(d, &mut used);
                }
                used.remove(&p.name);
            }
            out.extend(used);
            walk_expr_for_lambdas(body, out);
        }
        ast::ExprKind::Binary { left, right, .. } => {
            walk_expr_for_lambdas(left, out);
            walk_expr_for_lambdas(right, out);
        }
        ast::ExprKind::Unary { operand, .. } => walk_expr_for_lambdas(operand, out),
        ast::ExprKind::Call {
            args,
            keywords,
            kwargs,
            ..
        } => {
            for a in args {
                match a {
                    ast::PosArg::Pos(e) | ast::PosArg::Star(e) => walk_expr_for_lambdas(e, out),
                }
            }
            for kw in keywords {
                walk_expr_for_lambdas(&kw.value, out);
            }
            if let Some(k) = kwargs {
                walk_expr_for_lambdas(k, out);
            }
        }
        ast::ExprKind::MethodCall {
            base,
            args,
            keywords,
            kwargs,
            ..
        } => {
            walk_expr_for_lambdas(base, out);
            for a in args {
                match a {
                    ast::PosArg::Pos(e) | ast::PosArg::Star(e) => walk_expr_for_lambdas(e, out),
                }
            }
            for kw in keywords {
                walk_expr_for_lambdas(&kw.value, out);
            }
            if let Some(k) = kwargs {
                walk_expr_for_lambdas(k, out);
            }
        }
        ast::ExprKind::ListLit(items) => {
            for it in items {
                match it {
                    ast::ListElem::Item(e) | ast::ListElem::Star(e) => {
                        walk_expr_for_lambdas(e, out)
                    }
                }
            }
        }
        ast::ExprKind::TupleLit(items) | ast::ExprKind::SetLit(items) => {
            for e in items {
                walk_expr_for_lambdas(e, out);
            }
        }
        ast::ExprKind::DictLit(items) => {
            for (k, v) in items {
                walk_expr_for_lambdas(k, out);
                walk_expr_for_lambdas(v, out);
            }
        }
        ast::ExprKind::Attribute { base, .. } => walk_expr_for_lambdas(base, out),
        ast::ExprKind::Index { base, index } => {
            walk_expr_for_lambdas(base, out);
            walk_expr_for_lambdas(index, out);
        }
        ast::ExprKind::Slice { base, lo, hi, step } => {
            walk_expr_for_lambdas(base, out);
            if let Some(e) = lo {
                walk_expr_for_lambdas(e, out);
            }
            if let Some(e) = hi {
                walk_expr_for_lambdas(e, out);
            }
            if let Some(e) = step {
                walk_expr_for_lambdas(e, out);
            }
        }
        ast::ExprKind::Compare { first, rest } => {
            walk_expr_for_lambdas(first, out);
            for (_, r) in rest {
                walk_expr_for_lambdas(r, out);
            }
        }
        ast::ExprKind::ListComp { elem, generators } => {
            walk_expr_for_lambdas(elem, out);
            for g in generators {
                walk_expr_for_lambdas(&g.iter, out);
                for c in &g.ifs {
                    walk_expr_for_lambdas(c, out);
                }
            }
        }
        ast::ExprKind::JoinedStr(parts) => {
            for p in parts {
                if let ast::FStringPart::Expr {
                    expr, format_spec, ..
                } = p
                {
                    walk_expr_for_lambdas(expr, out);
                    if let Some(spec) = format_spec {
                        walk_expr_for_lambdas(spec, out);
                    }
                }
            }
        }
        ast::ExprKind::Cast { arg, .. } => walk_expr_for_lambdas(arg, out),
        ast::ExprKind::Starred(v) | ast::ExprKind::Yield(Some(v)) | ast::ExprKind::YieldFrom(v) => {
            walk_expr_for_lambdas(v, out)
        }
        _ => {}
    }
}

fn add_nested_free_names(f: &ast::FuncDef, out: &mut HashSet<String>) {
    let mut used = HashSet::new();
    collect_used_names_in_stmts(&f.body, &mut used);
    for p in &f.params {
        if let Some(d) = &p.default {
            collect_used_names_in_expr(d, &mut used);
        }
        used.remove(&p.name);
    }
    if let Some(p) = &f.vararg {
        used.remove(&p.name);
    }
    if let Some(p) = &f.kwarg {
        used.remove(&p.name);
    }
    let nonlocals = collect_nonlocals_in_stmts(&f.body);
    let assigned = assigned_names_in_stmts(&f.body);
    for n in used {
        // Assigned without nonlocal is a new local of the nested, not free.
        if assigned.contains(&n) && !nonlocals.contains(&n) {
            continue;
        }
        out.insert(n);
    }
    // Nested defs inside this nested function also free-capture outer names.
    collect_cell_candidates_in_stmts(&f.body, out);
}

/// Ensure `name` is stored in a heap cell in the current function.
/// Returns an optional init statement to box the current value.
fn ensure_cell(ctx: &mut FnCtx, name: &str, ty: ir::Ty, span: Span) -> SResult<Option<ir::Stmt>> {
    if ctx.cell_locals.contains_key(name) {
        return Ok(None);
    }
    if !ctx.locals.contains_key(name) {
        return Err(err(format!("no binding for nonlocal '{name}' found"), span));
    }
    let cell_name = format!(".cell.{name}");
    let mut init = None;
    if !ctx.locals.contains_key(&cell_name) {
        ctx.locals.insert(cell_name.clone(), ir::cell_of(ty));
        ctx.locals_order.push((cell_name.clone(), ir::cell_of(ty)));
        // Box current value
        init = Some(ir::Stmt::Assign {
            name: cell_name,
            value: ir::Expr {
                ty: ir::cell_of(ty),
                kind: ir::ExprKind::CellNew(Box::new(ir::Expr {
                    ty,
                    kind: ir::ExprKind::Local(name.to_string()),
                })),
            },
        });
    }
    ctx.cell_locals.insert(name.to_string(), ty);
    Ok(init)
}

fn assigned_names_in_stmts(stmts: &[ast::Stmt]) -> HashSet<String> {
    let mut s = HashSet::new();
    for st in stmts {
        assigned_names_in_stmt(st, &mut s);
    }
    s
}

fn assigned_names_in_stmt(st: &ast::Stmt, out: &mut HashSet<String>) {
    match &st.kind {
        ast::StmtKind::Assign { targets, .. } => {
            for t in targets {
                assigned_names_in_target(t, out);
            }
        }
        ast::StmtKind::AugAssign { target, .. } => assigned_names_in_target(target, out),
        ast::StmtKind::For {
            target,
            body,
            orelse,
            ..
        } => {
            assigned_names_in_target(target, out);
            for s in body {
                assigned_names_in_stmt(s, out);
            }
            for s in orelse {
                assigned_names_in_stmt(s, out);
            }
        }
        ast::StmtKind::If { branches, orelse } => {
            for (_, body) in branches {
                for s in body {
                    assigned_names_in_stmt(s, out);
                }
            }
            for s in orelse {
                assigned_names_in_stmt(s, out);
            }
        }
        ast::StmtKind::While { body, orelse, .. } => {
            for s in body {
                assigned_names_in_stmt(s, out);
            }
            for s in orelse {
                assigned_names_in_stmt(s, out);
            }
        }
        ast::StmtKind::Try {
            body,
            handlers,
            orelse,
            finally,
        } => {
            for s in body {
                assigned_names_in_stmt(s, out);
            }
            for h in handlers {
                if let Some((name, _)) = &h.bind {
                    out.insert(name.clone());
                }
                for s in &h.body {
                    assigned_names_in_stmt(s, out);
                }
            }
            for s in orelse {
                assigned_names_in_stmt(s, out);
            }
            for s in finally {
                assigned_names_in_stmt(s, out);
            }
        }
        ast::StmtKind::FuncDef(f) => {
            // nested nested: its name is local binding in the enclosing nested fn
            out.insert(f.name.clone());
        }
        ast::StmtKind::With { target, body, .. } => {
            if let Some((name, _)) = target {
                out.insert(name.clone());
            }
            for s in body {
                assigned_names_in_stmt(s, out);
            }
        }
        ast::StmtKind::Match { cases, .. } => {
            for c in cases {
                // Capture patterns bind names; treat as assigned for freevar analysis.
                for name in pattern_capture_names(&c.pattern) {
                    out.insert(name);
                }
                for s in &c.body {
                    assigned_names_in_stmt(s, out);
                }
            }
        }
        _ => {}
    }
}

fn assigned_names_in_target(t: &ast::AssignTarget, out: &mut HashSet<String>) {
    match t {
        ast::AssignTarget::Name { name, .. } => {
            out.insert(name.clone());
        }
        ast::AssignTarget::Index { .. } | ast::AssignTarget::Attr { .. } => {}
        ast::AssignTarget::Tuple(ts) => {
            for t in ts {
                assigned_names_in_target(t, out);
            }
        }
        ast::AssignTarget::Starred { target, .. } => assigned_names_in_target(target, out),
    }
}

fn collect_used_names_in_stmts(stmts: &[ast::Stmt], out: &mut HashSet<String>) {
    for st in stmts {
        collect_used_names_in_stmt(st, out);
    }
}

/// Bare-name call targets in a statement list (for sibling nested capture threading).
fn collect_called_func_names_in_stmts(stmts: &[ast::Stmt], out: &mut HashSet<String>) {
    for st in stmts {
        collect_called_func_names_in_stmt(st, out);
    }
}

fn collect_called_func_names_in_stmt(st: &ast::Stmt, out: &mut HashSet<String>) {
    match &st.kind {
        ast::StmtKind::Assign { value, .. }
        | ast::StmtKind::AugAssign { value, .. }
        | ast::StmtKind::ExprStmt(value)
        | ast::StmtKind::Return(Some(value))
        | ast::StmtKind::Raise { message: value, .. } => {
            collect_called_func_names_in_expr(value, out);
        }
        ast::StmtKind::If { branches, orelse } => {
            for (c, body) in branches {
                collect_called_func_names_in_expr(c, out);
                collect_called_func_names_in_stmts(body, out);
            }
            collect_called_func_names_in_stmts(orelse, out);
        }
        ast::StmtKind::While { cond, body, orelse } => {
            collect_called_func_names_in_expr(cond, out);
            collect_called_func_names_in_stmts(body, out);
            collect_called_func_names_in_stmts(orelse, out);
        }
        ast::StmtKind::For {
            iter, body, orelse, ..
        } => {
            collect_called_func_names_in_expr(iter, out);
            collect_called_func_names_in_stmts(body, out);
            collect_called_func_names_in_stmts(orelse, out);
        }
        ast::StmtKind::With { item, body, .. } => {
            collect_called_func_names_in_expr(item, out);
            collect_called_func_names_in_stmts(body, out);
        }
        ast::StmtKind::Match { subject, cases } => {
            collect_called_func_names_in_expr(subject, out);
            for c in cases {
                if let Some(g) = &c.guard {
                    collect_called_func_names_in_expr(g, out);
                }
                collect_called_func_names_in_stmts(&c.body, out);
            }
        }
        ast::StmtKind::Try {
            body,
            handlers,
            orelse,
            finally,
        } => {
            collect_called_func_names_in_stmts(body, out);
            for h in handlers {
                collect_called_func_names_in_stmts(&h.body, out);
            }
            collect_called_func_names_in_stmts(orelse, out);
            collect_called_func_names_in_stmts(finally, out);
        }
        ast::StmtKind::Delete { target } => {
            // index deletes may call nothing meaningful for bare names
            let _ = target;
        }
        ast::StmtKind::FuncDef(inner) => {
            // Nested nested: its own analysis; calls inside don't pull into outer sibling.
            let _ = inner;
        }
        _ => {}
    }
}

fn collect_called_func_names_in_expr(e: &ast::Expr, out: &mut HashSet<String>) {
    match &e.kind {
        ast::ExprKind::Call {
            func,
            args,
            keywords,
            kwargs,
            ..
        } => {
            out.insert(func.clone());
            for a in args {
                match a {
                    ast::PosArg::Pos(e) | ast::PosArg::Star(e) => {
                        collect_called_func_names_in_expr(e, out);
                    }
                }
            }
            for kw in keywords {
                collect_called_func_names_in_expr(&kw.value, out);
            }
            if let Some(k) = kwargs {
                collect_called_func_names_in_expr(k, out);
            }
        }
        ast::ExprKind::MethodCall {
            base,
            args,
            keywords,
            kwargs,
            ..
        } => {
            collect_called_func_names_in_expr(base, out);
            for a in args {
                match a {
                    ast::PosArg::Pos(e) | ast::PosArg::Star(e) => {
                        collect_called_func_names_in_expr(e, out);
                    }
                }
            }
            for kw in keywords {
                collect_called_func_names_in_expr(&kw.value, out);
            }
            if let Some(k) = kwargs {
                collect_called_func_names_in_expr(k, out);
            }
        }
        ast::ExprKind::Binary { left, right, .. } => {
            collect_called_func_names_in_expr(left, out);
            collect_called_func_names_in_expr(right, out);
        }
        ast::ExprKind::Unary { operand, .. } | ast::ExprKind::Starred(operand) => {
            collect_called_func_names_in_expr(operand, out);
        }
        ast::ExprKind::Compare { first, rest } => {
            collect_called_func_names_in_expr(first, out);
            for (_, e) in rest {
                collect_called_func_names_in_expr(e, out);
            }
        }
        ast::ExprKind::Index { base, index } => {
            collect_called_func_names_in_expr(base, out);
            collect_called_func_names_in_expr(index, out);
        }
        ast::ExprKind::Slice {
            base, lo, hi, step, ..
        } => {
            collect_called_func_names_in_expr(base, out);
            if let Some(e) = lo {
                collect_called_func_names_in_expr(e, out);
            }
            if let Some(e) = hi {
                collect_called_func_names_in_expr(e, out);
            }
            if let Some(e) = step {
                collect_called_func_names_in_expr(e, out);
            }
        }
        ast::ExprKind::Attribute { base, .. } => collect_called_func_names_in_expr(base, out),
        ast::ExprKind::ListLit(items) => {
            for it in items {
                match it {
                    ast::ListElem::Item(e) | ast::ListElem::Star(e) => {
                        collect_called_func_names_in_expr(e, out);
                    }
                }
            }
        }
        ast::ExprKind::TupleLit(items) | ast::ExprKind::SetLit(items) => {
            for e in items {
                collect_called_func_names_in_expr(e, out);
            }
        }
        ast::ExprKind::DictLit(pairs) => {
            for (k, v) in pairs {
                collect_called_func_names_in_expr(k, out);
                collect_called_func_names_in_expr(v, out);
            }
        }
        ast::ExprKind::ListComp { elem, generators } => {
            collect_called_func_names_in_expr(elem, out);
            for g in generators {
                collect_called_func_names_in_expr(&g.iter, out);
                for c in &g.ifs {
                    collect_called_func_names_in_expr(c, out);
                }
            }
        }
        ast::ExprKind::Cast { arg, .. } => collect_called_func_names_in_expr(arg, out),
        ast::ExprKind::JoinedStr(parts) => {
            for p in parts {
                if let ast::FStringPart::Expr {
                    expr, format_spec, ..
                } = p
                {
                    collect_called_func_names_in_expr(expr, out);
                    if let Some(spec) = format_spec {
                        collect_called_func_names_in_expr(spec, out);
                    }
                }
            }
        }
        ast::ExprKind::Lambda { body, .. } => collect_called_func_names_in_expr(body, out),
        ast::ExprKind::Yield(Some(e)) | ast::ExprKind::YieldFrom(e) => {
            collect_called_func_names_in_expr(e, out);
        }
        _ => {}
    }
}

fn collect_used_names_in_stmt(st: &ast::Stmt, out: &mut HashSet<String>) {
    match &st.kind {
        ast::StmtKind::Assign { targets, value, .. } => {
            collect_used_names_in_expr(value, out);
            for t in targets {
                collect_used_names_in_target_read(t, out);
            }
        }
        ast::StmtKind::AugAssign { target, value, .. } => {
            collect_used_names_in_expr(value, out);
            collect_used_names_in_target_read(target, out);
            // augassign also reads the target name
            if let ast::AssignTarget::Name { name, .. } = target {
                out.insert(name.clone());
            }
        }
        ast::StmtKind::ExprStmt(e) | ast::StmtKind::Return(Some(e)) => {
            collect_used_names_in_expr(e, out);
        }
        ast::StmtKind::If { branches, orelse } => {
            for (c, body) in branches {
                collect_used_names_in_expr(c, out);
                collect_used_names_in_stmts(body, out);
            }
            collect_used_names_in_stmts(orelse, out);
        }
        ast::StmtKind::While { cond, body, orelse } => {
            collect_used_names_in_expr(cond, out);
            collect_used_names_in_stmts(body, out);
            collect_used_names_in_stmts(orelse, out);
        }
        ast::StmtKind::For {
            target,
            iter,
            body,
            orelse,
        } => {
            collect_used_names_in_target_read(target, out);
            collect_used_names_in_expr(iter, out);
            collect_used_names_in_stmts(body, out);
            collect_used_names_in_stmts(orelse, out);
        }
        ast::StmtKind::Raise { message, .. } => {
            collect_used_names_in_expr(message, out);
        }
        ast::StmtKind::Delete { target } => collect_used_names_in_target_read(target, out),
        ast::StmtKind::With { item, body, .. } => {
            collect_used_names_in_expr(item, out);
            collect_used_names_in_stmts(body, out);
        }
        ast::StmtKind::Match { subject, cases } => {
            collect_used_names_in_expr(subject, out);
            for c in cases {
                if let Some(g) = &c.guard {
                    collect_used_names_in_expr(g, out);
                }
                collect_used_names_in_stmts(&c.body, out);
            }
        }
        ast::StmtKind::Try {
            body,
            handlers,
            orelse,
            finally,
        } => {
            collect_used_names_in_stmts(body, out);
            for h in handlers {
                collect_used_names_in_stmts(&h.body, out);
            }
            collect_used_names_in_stmts(orelse, out);
            collect_used_names_in_stmts(finally, out);
        }
        ast::StmtKind::FuncDef(f) => {
            for p in &f.params {
                if let Some(d) = &p.default {
                    collect_used_names_in_expr(d, out);
                }
            }
            collect_used_names_in_stmts(&f.body, out);
        }
        _ => {}
    }
}

fn collect_used_names_in_target_read(t: &ast::AssignTarget, out: &mut HashSet<String>) {
    match t {
        ast::AssignTarget::Name { .. } => {}
        ast::AssignTarget::Index { base, index } => {
            collect_used_names_in_expr(base, out);
            collect_used_names_in_expr(index, out);
        }
        ast::AssignTarget::Attr { base, .. } => {
            collect_used_names_in_expr(base, out);
        }
        ast::AssignTarget::Tuple(ts) => {
            for t in ts {
                collect_used_names_in_target_read(t, out);
            }
        }
        ast::AssignTarget::Starred { target, .. } => collect_used_names_in_target_read(target, out),
    }
}

fn collect_used_names_in_expr(e: &ast::Expr, out: &mut HashSet<String>) {
    match &e.kind {
        ast::ExprKind::Name(n) => {
            out.insert(n.clone());
        }
        ast::ExprKind::Call {
            args,
            keywords,
            kwargs,
            ..
        } => {
            for a in args {
                match a {
                    ast::PosArg::Pos(x) | ast::PosArg::Star(x) => {
                        collect_used_names_in_expr(x, out);
                    }
                }
            }
            for kw in keywords {
                collect_used_names_in_expr(&kw.value, out);
            }
            if let Some(k) = kwargs {
                collect_used_names_in_expr(k, out);
            }
        }
        ast::ExprKind::MethodCall {
            base,
            args,
            keywords,
            kwargs,
            ..
        } => {
            collect_used_names_in_expr(base, out);
            for a in args {
                match a {
                    ast::PosArg::Pos(x) | ast::PosArg::Star(x) => {
                        collect_used_names_in_expr(x, out);
                    }
                }
            }
            for kw in keywords {
                collect_used_names_in_expr(&kw.value, out);
            }
            if let Some(k) = kwargs {
                collect_used_names_in_expr(k, out);
            }
        }
        ast::ExprKind::Attribute { base, .. } => collect_used_names_in_expr(base, out),
        ast::ExprKind::Index { base, index } => {
            collect_used_names_in_expr(base, out);
            collect_used_names_in_expr(index, out);
        }
        ast::ExprKind::Slice {
            base, lo, hi, step, ..
        } => {
            collect_used_names_in_expr(base, out);
            if let Some(x) = lo {
                collect_used_names_in_expr(x, out);
            }
            if let Some(x) = hi {
                collect_used_names_in_expr(x, out);
            }
            if let Some(x) = step {
                collect_used_names_in_expr(x, out);
            }
        }
        ast::ExprKind::Binary { left, right, .. } => {
            collect_used_names_in_expr(left, out);
            collect_used_names_in_expr(right, out);
        }
        ast::ExprKind::Compare { first, rest } => {
            collect_used_names_in_expr(first, out);
            for (_, e) in rest {
                collect_used_names_in_expr(e, out);
            }
        }
        ast::ExprKind::Unary { operand, .. } => collect_used_names_in_expr(operand, out),
        ast::ExprKind::ListLit(items) => {
            for i in items {
                match i {
                    ast::ListElem::Item(e) | ast::ListElem::Star(e) => {
                        collect_used_names_in_expr(e, out)
                    }
                }
            }
        }
        ast::ExprKind::TupleLit(items) => {
            for i in items {
                collect_used_names_in_expr(i, out);
            }
        }
        ast::ExprKind::DictLit(pairs) => {
            for (k, v) in pairs {
                collect_used_names_in_expr(k, out);
                collect_used_names_in_expr(v, out);
            }
        }
        ast::ExprKind::SetLit(items) => {
            for i in items {
                collect_used_names_in_expr(i, out);
            }
        }
        ast::ExprKind::Cast { arg, .. } => collect_used_names_in_expr(arg, out),
        ast::ExprKind::ListComp { elem, generators } => {
            collect_used_names_in_expr(elem, out);
            for g in generators {
                collect_used_names_in_target_read(&g.target, out);
                collect_used_names_in_expr(&g.iter, out);
                for c in &g.ifs {
                    collect_used_names_in_expr(c, out);
                }
            }
        }
        ast::ExprKind::JoinedStr(parts) => {
            for p in parts {
                if let ast::FStringPart::Expr {
                    expr, format_spec, ..
                } = p
                {
                    collect_used_names_in_expr(expr, out);
                    if let Some(spec) = format_spec {
                        collect_used_names_in_expr(spec, out);
                    }
                }
            }
        }
        ast::ExprKind::Lambda { params, body } => {
            for p in params {
                if let Some(d) = &p.default {
                    collect_used_names_in_expr(d, out);
                }
            }
            collect_used_names_in_expr(body, out);
        }
        ast::ExprKind::Yield(Some(v)) | ast::ExprKind::YieldFrom(v) | ast::ExprKind::Starred(v) => {
            collect_used_names_in_expr(v, out)
        }
        ast::ExprKind::Yield(None) => {}
        _ => {}
    }
}

/// Nested suite (if/for/try/…): imports remain allowed (function-level imports
/// match CPython; they bind locals when inside a function).
fn lower_nested_block(stmts: &[ast::Stmt], ctx: &mut FnCtx) -> SResult<Vec<ir::Stmt>> {
    lower_block(stmts, ctx)
}

fn lower_stmt(stmt: &ast::Stmt, ctx: &mut FnCtx, out: &mut Vec<ir::Stmt>) -> SResult<()> {
    match &stmt.kind {
        ast::StmtKind::FuncDef(f) => {
            if ctx.is_entry {
                return Err(err(
                    format!(
                        "nested function definitions are only supported inside \
                         functions, not at module top level ('{}')",
                        f.name
                    ),
                    f.span,
                ));
            }
            lower_nested_func_def(f, ctx)?;
            // Flush cell boxing for nonlocal captures
            out.append(&mut ctx.pending_cell_inits);
            // Freeze default argument expressions at def time (CPython).
            freeze_nested_defaults(&f.name, f.span, ctx, out)?;
            Ok(())
        }
        ast::StmtKind::ClassDef(c) => {
            // Top-level classes are collected and lowered in analyze_program.
            // Nested class statements should not reach here (parser rejects
            // class-in-class; function-nested classes are rejected here).
            if ctx.is_entry {
                return Ok(());
            }
            Err(err(
                format!("nested classes are not supported yet ('{}')", c.name),
                c.span,
            ))
        }
        ast::StmtKind::Pass => Ok(()),
        ast::StmtKind::Import { names } => {
            for (module, alias, _span) in names {
                // Module-level and function-level imports both run init once.
                if module != "sys" {
                    out.extend(init_calls_for(module));
                }
                // CPython: import inside a function creates a local binding.
                if !ctx.is_entry {
                    let local = import_bind_name(module, alias);
                    let binding = if module == "sys" {
                        ImportBinding::Sys
                    } else {
                        ImportBinding::Module(import_bound_module(module, alias))
                    };
                    ctx.local_imports.insert(local, binding);
                }
            }
            Ok(())
        }
        ast::StmtKind::FromImport {
            module,
            names,
            star,
            span,
            ..
        } => {
            if *star && !ctx.is_entry {
                return Err(err("import * only allowed at module level", *span));
            }
            // package / module body, then any submodules pulled in by name
            if !module.is_empty() && module != "sys" {
                out.extend(init_calls_for(module));
            }
            // Star: bindings come from `collect_imports` + re-exports; still
            // ensure any Module bindings from this source get their init run.
            let eff: Vec<(String, Option<String>, Span)> = if *star {
                let mut from_imports: Vec<(String, Option<String>, Span)> = Vec::new();
                for (local, binding) in ctx.mctx.imports.iter() {
                    match binding {
                        ImportBinding::Symbol {
                            module: src,
                            name: src_name,
                        } if src == module && local == src_name => {
                            from_imports.push((src_name.clone(), None, *span));
                        }
                        ImportBinding::Module(full)
                            if full.rsplit_once('.').is_some_and(|(p, c)| {
                                p == module.as_str() && c == local.as_str()
                            }) =>
                        {
                            from_imports.push((local.clone(), None, *span));
                        }
                        _ => {}
                    }
                }
                from_imports
            } else {
                names.clone()
            };
            for (name, alias, nspan) in &eff {
                // Function-local from-import binding (CPython local scope).
                if !ctx.is_entry {
                    let local = alias.clone().unwrap_or_else(|| name.clone());
                    // Prefer submodule module binding when applicable.
                    let sub_full = ctx
                        .mctx
                        .submodules
                        .get(module.as_str())
                        .and_then(|kids| kids.get(name))
                        .cloned();
                    let binding = if let Some(full) = sub_full {
                        ImportBinding::Module(full)
                    } else {
                        ImportBinding::Symbol {
                            module: module.clone(),
                            name: name.clone(),
                        }
                    };
                    ctx.local_imports.insert(local, binding);
                }
                // CPython fromlist: only load/run a submodule when the source
                // package does not already have that name as a value/function
                // (hasattr short-circuit). LastExport::Symbol → skip submodule init.
                let last = ctx
                    .mctx
                    .last_exports
                    .get(module.as_str())
                    .and_then(|e| e.get(name));
                let is_submodule_init = match last {
                    Some(LastExport::Module(full)) => {
                        out.extend(init_calls_for(full));
                        true
                    }
                    Some(LastExport::Symbol) => {
                        // value/function binding wins — do not run submodule body
                        false
                    }
                    None => {
                        if let Some(full) = ctx
                            .mctx
                            .submodules
                            .get(module)
                            .and_then(|kids| kids.get(name))
                        {
                            out.extend(init_calls_for(full));
                            true
                        } else {
                            false
                        }
                    }
                };
                if is_submodule_init {
                    continue;
                }
                // Partial package init: at child **module top level**, names
                // must already be on the parent before this child was loaded
                // (simple assigns or defs — CPython ImportError mid-init).
                // Deferred use inside function bodies is handled at load/call sites.
                if !module.is_empty()
                    && !ctx.mctx.mods.contains_key(module.as_str())
                    && is_strict_package_prefix(module, ctx.mctx.module)
                {
                    let visible_val = ctx
                        .mctx
                        .partial_parent_globals(module)
                        .is_some_and(|g| g.contains_key(name));
                    let visible_fn = ctx
                        .mctx
                        .partial_parent_funcs(module)
                        .is_some_and(|s| s.contains(name));
                    let visible_reexport = ctx
                        .mctx
                        .partial_parent_reexports(module)
                        .is_some_and(|s| s.contains(name));
                    if !visible_val && !visible_fn && !visible_reexport {
                        return Err(err(
                            format!(
                                "cannot import name '{name}' from partially initialized \
                                 package '{module}' (most likely due to a circular import)"
                            ),
                            *nspan,
                        ));
                    }
                }
            }
            Ok(())
        }
        ast::StmtKind::With { item, target, body } => {
            lower_with(item, target.as_ref(), body, ctx, out)
        }
        ast::StmtKind::Global(names) => {
            // a no-op at module level, like Python
            if ctx.is_entry {
                return Ok(());
            }
            for (name, span) in names {
                if ctx.locals.contains_key(name) {
                    return Err(err(
                        format!(
                            "'{name}' is already a parameter or local here; \
                             the 'global' declaration must come before any use"
                        ),
                        *span,
                    ));
                }
                if !ctx.globals.contains_key(name) {
                    return Err(err(
                        format!(
                            "no global '{name}' is assigned at the top level \
                             of the program"
                        ),
                        *span,
                    ));
                }
                ctx.declared_globals.insert(name.clone());
            }
            Ok(())
        }
        ast::StmtKind::Break => {
            if ctx.loop_depth == 0 {
                return Err(err("'break' outside of a loop", stmt.span));
            }
            out.push(ir::Stmt::Break);
            Ok(())
        }
        ast::StmtKind::Continue => {
            if ctx.loop_depth == 0 {
                return Err(err("'continue' outside of a loop", stmt.span));
            }
            out.push(ir::Stmt::Continue);
            Ok(())
        }
        ast::StmtKind::Return(value) => {
            // Generator stop: bare `return` ends iteration; `return <expr>`
            // stores StopIteration.value for `yield from` consumers (and
            // evaluates for side effects).
            if let Some(yty) = ctx.yield_ty {
                if let Some(e) = value {
                    let v = lower_expr(e, ctx)?;
                    // Coerce to this generator's yield type so the payload
                    // encoding matches what yield-from loaders expect.
                    let v = coerce(v, yty, e.span, "generator return value")?;
                    out.push(ir::Stmt::Return(Some(v)));
                } else {
                    out.push(ir::Stmt::Return(None));
                }
                return Ok(());
            }
            match (value, ctx.ret) {
                (None, ir::Ty::None) => out.push(ir::Stmt::Return(None)),
                (None, expected) => {
                    return Err(err(
                        format!(
                            "function '{}' must return a value of type {}",
                            ctx.fn_name, expected
                        ),
                        stmt.span,
                    ));
                }
                (Some(e), ir::Ty::None) => {
                    // `return None` is fine in a None function
                    if matches!(e.kind, ast::ExprKind::NoneLit) {
                        out.push(ir::Stmt::Return(None));
                        return Ok(());
                    }
                    // Optional return annotation: infer ret from first returned value.
                    let v = lower_expr(e, ctx)?;
                    ctx.ret = v.ty;
                    out.push(ir::Stmt::Return(Some(v)));
                }
                (Some(e), expected) => {
                    // `return []` needs the declared type for inference
                    let value = if let (ast::ExprKind::ListLit(items), ir::Ty::List(elem)) =
                        (&e.kind, expected)
                    {
                        lower_list_lit(items, Some(*elem), e.span, ctx)?
                    } else {
                        let v = lower_expr(e, ctx)?;
                        coerce(v, expected, e.span, "return value")?
                    };
                    out.push(ir::Stmt::Return(Some(value)));
                }
            }
            Ok(())
        }
        ast::StmtKind::Assign {
            targets,
            annotation,
            value,
        } => {
            if targets.is_empty() {
                return Err(err("assignment has no targets", stmt.span));
            }
            if targets.len() == 1 {
                return lower_assign(&targets[0], *annotation, value, ctx, out);
            }
            if annotation.is_some() {
                return Err(err(
                    "type annotations are not allowed in multi-target assignment",
                    stmt.span,
                ));
            }
            // evaluate RHS once, then assign right-to-left (Python order)
            let value_ir = lower_expr(value, ctx)?;
            let tmp = ctx.fresh_temp("multi", value_ir.ty);
            out.push(ir::Stmt::Assign {
                name: tmp.clone(),
                value: value_ir.clone(),
            });
            let load = ir::Expr {
                ty: value_ir.ty,
                kind: ir::ExprKind::Local(tmp),
            };
            for target in targets.iter().rev() {
                lower_assign_ir(target, None, load.clone(), value.span, ctx, out)?;
            }
            Ok(())
        }
        ast::StmtKind::Delete { target } => lower_delete(target, stmt.span, ctx, out),
        ast::StmtKind::Raise { exc, message } => {
            let msg = lower_expr(message, ctx)?;
            let msg = coerce(msg, ir::Ty::Str, message.span, "raise message")?;
            out.push(ir::Stmt::Raise {
                exc: ast_exc_to_ir(*exc),
                message: msg,
            });
            Ok(())
        }
        ast::StmtKind::Assert { test, msg } => {
            // Desugar: if not test: raise AssertionError(str(msg) or "")
            let cond = lower_condition(test, ctx)?;
            let not_cond = ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Unary {
                    op: ir::UnOp::Not,
                    operand: Box::new(cond),
                },
            };
            let message = if let Some(m) = msg {
                let m_ir = lower_expr(m, ctx)?;
                if m_ir.ty == ir::Ty::Str {
                    m_ir
                } else {
                    lower_cast(ast::TypeName::Str, m_ir, m.span)?
                }
            } else {
                ir::Expr {
                    ty: ir::Ty::Str,
                    kind: ir::ExprKind::ConstStr(String::new()),
                }
            };
            out.push(ir::Stmt::If {
                branches: vec![(
                    not_cond,
                    vec![ir::Stmt::Raise {
                        exc: ir::ExcType::AssertionError,
                        message,
                    }],
                )],
                orelse: vec![],
            });
            Ok(())
        }
        ast::StmtKind::Try {
            body,
            handlers,
            orelse,
            finally,
        } => {
            ctx.try_depth += 1;
            let body_ir = lower_nested_block(body, ctx)?;
            let mut handlers_ir = Vec::new();
            for h in handlers {
                let name = if let Some((n, span)) = &h.bind {
                    // always a function local (even in the entry function), so
                    // codegen can store to %v.<name>
                    if let Some(existing) = ctx.locals.get(n) {
                        if *existing != ir::Ty::Exception {
                            ctx.try_depth -= 1;
                            return Err(err(
                                format!(
                                    "type mismatch in assignment to '{n}': expected \
                                     {existing}, found exception"
                                ),
                                *span,
                            ));
                        }
                    } else {
                        ctx.locals.insert(n.clone(), ir::Ty::Exception);
                        ctx.locals_order.push((n.clone(), ir::Ty::Exception));
                    }
                    Some(n.clone())
                } else {
                    None
                };
                let body_h = lower_nested_block(&h.body, ctx)?;
                let filter = h
                    .exc
                    .as_ref()
                    .map(|ts| ts.iter().copied().map(ast_exc_to_ir).collect::<Vec<_>>());
                handlers_ir.push((filter, name, body_h));
            }
            let orelse_ir = lower_nested_block(orelse, ctx)?;
            let finally_ir = lower_nested_block(finally, ctx)?;
            ctx.try_depth -= 1;
            out.push(ir::Stmt::Try {
                body: body_ir,
                handlers: handlers_ir,
                orelse: orelse_ir,
                finally: finally_ir,
            });
            Ok(())
        }
        ast::StmtKind::AugAssign { target, op, value } => {
            lower_aug_assign(target, *op, value, stmt.span, ctx, out)
        }
        ast::StmtKind::ExprStmt(e) => {
            // print is a statement-level builtin
            if let ast::ExprKind::Call {
                func,
                args,
                keywords,
                kwargs,
                ..
            } = &e.kind
                && func == "print"
                && !ctx.funcs().contains_key("print")
            {
                if !keywords.is_empty() {
                    return Err(err(
                        "print() keyword arguments are not supported yet",
                        keywords[0].name_span,
                    ));
                }
                if kwargs.is_some() {
                    return Err(err("print() does not take **kwargs", e.span));
                }
                let plain = require_plain_args(args, "print", e.span)?;
                let mut lowered_args = Vec::new();
                for arg in plain.iter() {
                    let a = lower_expr(arg, ctx)?;
                    if a.ty == ir::Ty::File {
                        return Err(err("file objects cannot be printed yet", arg.span));
                    }
                    // Honor class `__str__` / `__repr__` for print (CPython).
                    let a = if let ir::Ty::Class(id) = a.ty {
                        if resolve_str_dunder(id).is_some() {
                            lower_class_to_str(a, id, arg.span)?
                        } else {
                            a
                        }
                    } else {
                        a
                    };
                    // None, unions, tuples/dicts/sets/lists/scalars are printable
                    lowered_args.push(a);
                }
                out.push(ir::Stmt::Print(lowered_args));
                return Ok(());
            }
            // xs.append(v) is a statement in the IR
            if let ast::ExprKind::MethodCall {
                base,
                method,
                method_span,
                args,
                keywords,
                kwargs,
            } = &e.kind
            {
                // `module.func(args)` / `pkg.mod.func(args)` as a statement
                if let Some(real) = resolve_module_path(base, ctx) {
                    let call = lower_module_call(
                        &real,
                        method,
                        *method_span,
                        args,
                        keywords,
                        kwargs.as_deref(),
                        ctx,
                    )?;
                    out.push(ir::Stmt::ExprStmt(call));
                    return Ok(());
                }
                if !keywords.is_empty() {
                    return Err(err(
                        "keyword arguments are not supported for this method call",
                        keywords[0].name_span,
                    ));
                }
                if kwargs.is_some() {
                    return Err(err(
                        "** unpacking is not supported for this method call",
                        *method_span,
                    ));
                }
                let plain = require_plain_args(args, method, *method_span)?;
                let plain_owned: Vec<ast::Expr> = plain.iter().map(|e| (*e).clone()).collect();
                let stmt = lower_method_stmt(base, method, *method_span, &plain_owned, ctx)?;
                out.push(stmt);
                return Ok(());
            }
            let lowered = lower_expr(e, ctx)?;
            out.push(ir::Stmt::ExprStmt(lowered));
            Ok(())
        }
        ast::StmtKind::If { branches, orelse } => {
            // `active` = refinements known when control reaches the next branch.
            // Elif/else only run when prior conditions failed, so each arm's
            // complementary (`else_ref`) always applies to subsequent arms —
            // even when the then body falls through (does not return).
            // Post-if fallthrough only uses those complements when every then
            // arm returns (otherwise control may exit a then with then_ref).
            let outer_ref = ctx.type_refinements.clone();
            let mut active = outer_ref.clone();
            let mut lowered_branches = Vec::new();
            let mut all_thens_return = true;
            // Exit refinements of arms that can fall through (for merge).
            let mut fallthrough_exits: Vec<HashMap<String, ir::Ty>> = Vec::new();
            let mut fallthrough_assigned: HashSet<String> = HashSet::new();
            for (cond, body) in branches {
                // Evaluate condition under `active` refinements.
                ctx.type_refinements = active.clone();
                let c = lower_condition(cond, ctx)?;
                let (then_ref, else_ref) = narrowing_from_condition(cond, ctx);
                // Then body: active ∪ then_ref
                for (k, v) in &then_ref {
                    ctx.type_refinements.insert(k.clone(), *v);
                }
                let b = lower_nested_block(body, ctx)?;
                if !block_returns(&b) {
                    all_thens_return = false;
                    fallthrough_exits.push(ctx.type_refinements.clone());
                    fallthrough_assigned.extend(assigned_names_in_stmts(body));
                }
                lowered_branches.push((c, b));
                // Subsequent elif/else always see the complement (cond was false).
                for (k, v) in else_ref {
                    active.insert(k, v);
                }
            }
            // Else branch under `active` (complements of all prior conditions).
            ctx.type_refinements = active.clone();
            let lowered_orelse = lower_nested_block(orelse, ctx)?;
            let orelse_returns = block_returns(&lowered_orelse);
            if orelse.is_empty() {
                // All conditions false is a fallthrough path when some then
                // also falls through, or when no then exists.
                if !all_thens_return || branches.is_empty() {
                    fallthrough_exits.push(active.clone());
                }
            } else if !orelse_returns {
                fallthrough_exits.push(ctx.type_refinements.clone());
                fallthrough_assigned.extend(assigned_names_in_stmts(orelse));
            }
            // Restore outer, then apply fallthrough refinements.
            ctx.type_refinements = outer_ref;
            out.push(ir::Stmt::If {
                branches: lowered_branches,
                orelse: lowered_orelse,
            });
            // Fallthrough after if: only when every then returned can we keep
            // the accumulated complements (else is the only surviving path, or
            // empty else with all thens returning — still may fall through when
            // all conditions are false).
            if all_thens_return && (orelse.is_empty() || !orelse_returns) {
                // Surviving path is "all conditions false" (and maybe empty else).
                for (k, v) in active {
                    ctx.type_refinements.insert(k, v);
                }
            } else {
                // Merge fallthrough arms: drop peels that disagree or were rebound.
                merge_fallthrough_refinements(
                    &mut ctx.type_refinements,
                    &fallthrough_exits,
                    &fallthrough_assigned,
                );
            }
            Ok(())
        }
        ast::StmtKind::While { cond, body, orelse } => {
            let c = lower_condition(cond, ctx)?;
            let (then_ref, else_ref) = narrowing_from_condition(cond, ctx);
            let saved = ctx.type_refinements.clone();
            for (k, v) in &then_ref {
                ctx.type_refinements.insert(k.clone(), *v);
            }
            ctx.loop_depth += 1;
            let b = lower_nested_block(body, ctx)?;
            ctx.loop_depth -= 1;
            let body_assigned = assigned_names_in_stmts(body);
            let has_break = loop_breaks(&b);
            // Do not restore pre-loop peels after the body may have rebound.
            ctx.type_refinements = saved;
            for name in &body_assigned {
                ctx.type_refinements.remove(name);
            }
            // Without break, exit means the condition is false → else_ref.
            // With break, exit can be break (then peels) or false (else peels)
            // — drop both so post-loop code cannot keep a stale concrete peel.
            if !has_break {
                for (k, v) in else_ref {
                    ctx.type_refinements.insert(k, v);
                }
            } else {
                for k in then_ref.keys().chain(else_ref.keys()) {
                    ctx.type_refinements.remove(k);
                }
            }
            push_loop_with_else(c, b, vec![], orelse, ctx, out)?;
            // Else arm may rebind further; clear assigns from else on fallthrough.
            if !orelse.is_empty() {
                // push_loop_with_else lowered else under post-loop refs; if else
                // can fall through, drop peels for names it assigned.
                for name in assigned_names_in_stmts(orelse) {
                    ctx.type_refinements.remove(&name);
                }
            }
            Ok(())
        }
        ast::StmtKind::For {
            target,
            iter,
            body,
            orelse,
        } => lower_for(target, iter, body, orelse, ctx, out),
        ast::StmtKind::Nonlocal(names) => {
            if ctx.is_entry {
                return Err(err(
                    "nonlocal declaration not allowed at module level",
                    stmt.span,
                ));
            }
            for (name, span) in names {
                if ctx.declared_globals.contains(name) {
                    return Err(err(format!("name '{name}' is nonlocal and global"), *span));
                }
                if ctx.locals.contains_key(name) {
                    return Err(err(
                        format!("name '{name}' is assigned to before nonlocal declaration"),
                        *span,
                    ));
                }
                // Must exist in an outer function's locals — recorded on NestedFnInfo
                // via free-var analysis; here we mark it as a nonlocal binding.
                ctx.declared_nonlocals.insert(name.clone());
            }
            Ok(())
        }
        ast::StmtKind::Match { subject, cases } => lower_match(subject, cases, stmt.span, ctx, out),
    }
}

/// Desugar `match subject:` into a chain of if/elif with pattern tests.
fn lower_match(
    subject: &ast::Expr,
    cases: &[ast::MatchCase],
    span: Span,
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    let subj_ir = lower_expr(subject, ctx)?;
    let subj_tmp = ctx.fresh_temp("match", subj_ir.ty);
    out.push(ir::Stmt::Assign {
        name: subj_tmp.clone(),
        value: subj_ir.clone(),
    });
    let subj = ir::Expr {
        ty: subj_ir.ty,
        kind: ir::ExprKind::Local(subj_tmp),
    };

    if cases.is_empty() {
        return Err(err("match statement must have at least one case", span));
    }

    // If the last case is irrefutable (`_` / bare capture / `[*rest]` / `{**rest}`,
    // no guard), use it as the final `else` so return-analysis sees a complete
    // if/else tree.
    let last = cases.last().unwrap();
    validate_pattern_no_duplicate_binds(&last.pattern, subject.span)?;
    let last_irrefutable = last.guard.is_none() && pattern_is_irrefutable(&last.pattern);

    let (prefix, final_else) = if last_irrefutable {
        let mut body = Vec::new();
        let _ = lower_pattern_match(&last.pattern, &subj, subject.span, ctx, &mut body)?;
        // Irrefutable last case may still have a guard-less body that benefits
        // from no extra refine; just lower the body.
        body.extend(lower_nested_block(&last.body, ctx)?);
        (&cases[..cases.len() - 1], body)
    } else {
        (cases, Vec::new())
    };

    // CPython: capture binds run *before* the guard. Desugar each case as:
    //   if pattern_matches:
    //       binds
    //       if guard: case_body    # guard omitted → just case_body
    //       else: <next case>
    //   else: <next case>
    // Guard conditions refine the case body (e.g. `case y if y is not None`).
    let mut acc = final_else;
    for case in prefix.iter().rev() {
        validate_pattern_no_duplicate_binds(&case.pattern, subject.span)?;
        let mut binds = Vec::new();
        let pat_cond = lower_pattern_match(&case.pattern, &subj, subject.span, ctx, &mut binds)?;
        let then_body = if let Some(guard) = &case.guard {
            // Apply guard narrowing into the case body (pattern binds already
            // registered in locals via lower_pattern_match).
            let (then_ref, _) = narrowing_from_condition(guard, ctx);
            let saved = ctx.type_refinements.clone();
            for (k, v) in &then_ref {
                ctx.type_refinements.insert(k.clone(), *v);
            }
            let case_body = lower_nested_block(&case.body, ctx)?;
            ctx.type_refinements = saved;
            let g = lower_condition(guard, ctx)?;
            let mut mid = binds;
            mid.push(ir::Stmt::If {
                branches: vec![(g, case_body)],
                orelse: acc.clone(), // guard fail → next case
            });
            mid
        } else {
            let case_body = lower_nested_block(&case.body, ctx)?;
            let mut b = binds;
            b.extend(case_body);
            b
        };
        acc = vec![ir::Stmt::If {
            branches: vec![(pat_cond, then_body)],
            orelse: acc, // pattern miss → next case
        }];
    }
    out.extend(acc);
    Ok(())
}

fn pattern_is_irrefutable(p: &ast::Pattern) -> bool {
    match p {
        ast::Pattern::Wildcard | ast::Pattern::Capture(_) => true,
        ast::Pattern::As { pattern, .. } => pattern_is_irrefutable(pattern),
        // `[*rest]` / `(*rest,)` always matches a sequence subject (any length).
        ast::Pattern::Sequence {
            items,
            star: Some(si),
        } => {
            items.len() == 1
                && *si == 0
                && matches!(&items[0], ast::Pattern::Capture(_) | ast::Pattern::Wildcard)
        }
        // `{**rest}` always matches a mapping subject.
        ast::Pattern::Mapping {
            items,
            rest: Some(_),
        } if items.is_empty() => true,
        // Empty `{}` always matches a mapping subject (CPython irrefutable).
        ast::Pattern::Mapping { items, rest: None } if items.is_empty() => true,
        _ => false,
    }
}

/// CPython SyntaxError: multiple assignments to the same name in one pattern,
/// or duplicate keys in a mapping pattern.
fn validate_pattern_no_duplicate_binds(p: &ast::Pattern, span: Span) -> SResult<()> {
    let mut names = HashSet::new();
    check_dup_binds(p, &mut names, span)?;
    Ok(())
}

fn check_dup_binds(p: &ast::Pattern, seen: &mut HashSet<String>, span: Span) -> SResult<()> {
    match p {
        ast::Pattern::Capture(n) => {
            if !seen.insert(n.clone()) {
                return Err(err(
                    format!("multiple assignments to name '{n}' in pattern"),
                    span,
                ));
            }
            Ok(())
        }
        ast::Pattern::As { pattern, name } => {
            check_dup_binds(pattern, seen, span)?;
            if !seen.insert(name.clone()) {
                return Err(err(
                    format!("multiple assignments to name '{name}' in pattern"),
                    span,
                ));
            }
            Ok(())
        }
        ast::Pattern::Or(alts) => {
            // Each alternative is checked independently (shared names required
            // across alts by validate_or_pattern_binds).
            for alt in alts {
                let mut local = HashSet::new();
                check_dup_binds(alt, &mut local, span)?;
            }
            Ok(())
        }
        ast::Pattern::Sequence { items, .. } => {
            for it in items {
                check_dup_binds(it, seen, span)?;
            }
            Ok(())
        }
        ast::Pattern::Mapping { items, rest } => {
            let mut keys = HashSet::new();
            for (k, v) in items {
                if !keys.insert(k.clone()) {
                    return Err(err(
                        format!("mapping pattern checks duplicate key ('{k}')"),
                        span,
                    ));
                }
                check_dup_binds(v, seen, span)?;
            }
            if let Some(r) = rest
                && !seen.insert(r.clone())
            {
                return Err(err(
                    format!("multiple assignments to name '{r}' in pattern"),
                    span,
                ));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Collect capture names in a pattern (for or-pattern consistency checks).
fn pattern_capture_names(p: &ast::Pattern) -> Vec<String> {
    match p {
        ast::Pattern::Capture(n) => vec![n.clone()],
        ast::Pattern::As { pattern, name } => {
            let mut names = pattern_capture_names(pattern);
            names.push(name.clone());
            names.sort();
            names.dedup();
            names
        }
        ast::Pattern::Or(alts) => {
            let mut names = Vec::new();
            for a in alts {
                names.extend(pattern_capture_names(a));
            }
            names.sort();
            names.dedup();
            names
        }
        ast::Pattern::Sequence { items, .. } => {
            let mut names = Vec::new();
            for it in items {
                names.extend(pattern_capture_names(it));
            }
            names
        }
        ast::Pattern::Mapping { items, rest } => {
            let mut names = Vec::new();
            for (_, v) in items {
                names.extend(pattern_capture_names(v));
            }
            if let Some(r) = rest {
                names.push(r.clone());
            }
            names
        }
        _ => Vec::new(),
    }
}

/// CPython SyntaxError: alternative patterns bind different names.
fn validate_or_pattern_binds(alts: &[ast::Pattern], span: Span) -> SResult<()> {
    if alts.is_empty() {
        return Ok(());
    }
    let first = pattern_capture_names(&alts[0]);
    for alt in &alts[1..] {
        let names = pattern_capture_names(alt);
        if names != first {
            return Err(err("alternative patterns bind different names", span));
        }
    }
    Ok(())
}

/// Generate IR that tests `subject` against `pattern`, appending binds to `binds`.
/// Returns a Bool condition expression.
fn lower_pattern_match(
    pattern: &ast::Pattern,
    subject: &ir::Expr,
    span: Span,
    ctx: &mut FnCtx,
    binds: &mut Vec<ir::Stmt>,
) -> SResult<ir::Expr> {
    match pattern {
        ast::Pattern::Wildcard => Ok(ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ConstBool(true),
        }),
        ast::Pattern::Capture(name) => {
            // bind name = subject
            let stmt = bind_name(name, span, None, subject.clone(), span, ctx)?;
            binds.push(stmt);
            Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::ConstBool(true),
            })
        }
        ast::Pattern::Int(v) => {
            let left = subject.clone();
            let left = coerce(left, ir::Ty::Int, span, "match subject")?;
            Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Eq,
                    left: Box::new(left),
                    right: Box::new(int_const(*v)),
                },
            })
        }
        ast::Pattern::IntDigits(s) => {
            let left = subject.clone();
            let left = coerce(left, ir::Ty::Int, span, "match subject")?;
            Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Eq,
                    left: Box::new(left),
                    right: Box::new(ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::ConstIntDigits(s.clone()),
                    }),
                },
            })
        }
        ast::Pattern::Str(s) => {
            if subject.ty != ir::Ty::Str {
                return Err(err(
                    format!("match subject type {} cannot match str pattern", subject.ty),
                    span,
                ));
            }
            Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Eq,
                    left: Box::new(subject.clone()),
                    right: Box::new(ir::Expr {
                        ty: ir::Ty::Str,
                        kind: ir::ExprKind::ConstStr(s.clone()),
                    }),
                },
            })
        }
        ast::Pattern::Bool(b) => {
            let left = to_bool_default(subject.clone(), span)?;
            Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Eq,
                    left: Box::new(left),
                    right: Box::new(ir::Expr {
                        ty: ir::Ty::Bool,
                        kind: ir::ExprKind::ConstBool(*b),
                    }),
                },
            })
        }
        ast::Pattern::None => Ok(ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::IsNone {
                value: Box::new(subject.clone()),
                not: false,
            },
        }),
        ast::Pattern::Or(alts) => {
            // CPython: all alternatives must bind the same names (or none).
            validate_or_pattern_binds(alts, span)?;
            // Bind only the matching alternative: desugar to a Block that
            // tries each alt with nested ifs and sets a success flag.
            // Side-effect binds run inside the condition evaluation, so the
            // outer match arm's `binds` list stays empty for the Or node.
            let ok_tmp = ctx.fresh_temp("orpat", ir::Ty::Bool);
            let mut stmts = vec![ir::Stmt::Assign {
                name: ok_tmp.clone(),
                value: ir::Expr {
                    ty: ir::Ty::Bool,
                    kind: ir::ExprKind::ConstBool(false),
                },
            }];
            let mut chain_else: Vec<ir::Stmt> = Vec::new();
            for alt in alts.iter().rev() {
                let mut alt_binds = Vec::new();
                let cond = lower_pattern_match(alt, subject, span, ctx, &mut alt_binds)?;
                let mut then_body = alt_binds;
                then_body.push(ir::Stmt::Assign {
                    name: ok_tmp.clone(),
                    value: ir::Expr {
                        ty: ir::Ty::Bool,
                        kind: ir::ExprKind::ConstBool(true),
                    },
                });
                chain_else = vec![ir::Stmt::If {
                    branches: vec![(cond, then_body)],
                    orelse: chain_else,
                }];
            }
            stmts.extend(chain_else);
            // binds intentionally unused for Or — captures already applied.
            let _ = binds;
            Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Block {
                    stmts,
                    result: Box::new(ir::Expr {
                        ty: ir::Ty::Bool,
                        kind: ir::ExprKind::Local(ok_tmp),
                    }),
                },
            })
        }
        ast::Pattern::As { pattern, name } => {
            // Match sub-pattern, then bind the whole subject to `name`.
            let c = lower_pattern_match(pattern, subject, span, ctx, binds)?;
            let stmt = bind_name(name, span, None, subject.clone(), span, ctx)?;
            binds.push(stmt);
            Ok(c)
        }
        ast::Pattern::Sequence { items, star } => {
            lower_sequence_pattern(items, *star, subject, span, ctx, binds)
        }
        ast::Pattern::Mapping { items, rest } => {
            lower_mapping_pattern(items, rest.as_deref(), subject, span, ctx, binds)
        }
    }
}

fn lower_sequence_pattern(
    items: &[ast::Pattern],
    star: Option<usize>,
    subject: &ir::Expr,
    span: Span,
    ctx: &mut FnCtx,
    binds: &mut Vec<ir::Stmt>,
) -> SResult<ir::Expr> {
    match subject.ty {
        ir::Ty::List(elem) => {
            if let Some(si) = star {
                // `[a, *rest, b]`: len >= fixed, rest is a slice list.
                let before = si;
                let after = items.len() - si - 1;
                let min_len = (before + after) as i64;
                let len_e = ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Len(Box::new(subject.clone())),
                };
                let len_ok = ir::Expr {
                    ty: ir::Ty::Bool,
                    kind: ir::ExprKind::Binary {
                        op: ir::BinOp::Ge,
                        left: Box::new(len_e.clone()),
                        right: Box::new(int_const(min_len)),
                    },
                };
                let mut cond = len_ok;
                // Prefix items
                for (i, pat) in items.iter().enumerate().take(before) {
                    let elem_e = ir::Expr {
                        ty: *elem,
                        kind: ir::ExprKind::Index {
                            base: Box::new(subject.clone()),
                            index: Box::new(int_const(i as i64)),
                        },
                    };
                    let c = lower_pattern_match(pat, &elem_e, span, ctx, binds)?;
                    cond = bool_and(cond, c);
                }
                // Starred rest: subject[before : len-after]
                let rest_lo = int_const(before as i64);
                let rest_hi = ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Binary {
                        op: ir::BinOp::Sub,
                        left: Box::new(len_e),
                        right: Box::new(int_const(after as i64)),
                    },
                };
                let rest_list = ir::Expr {
                    ty: ir::list_of(*elem),
                    kind: ir::ExprKind::Slice {
                        base: Box::new(subject.clone()),
                        lo: Box::new(rest_lo),
                        hi: Box::new(rest_hi),
                        step: Box::new(int_const(1)),
                    },
                };
                let star_pat = &items[si];
                let c = lower_pattern_match(star_pat, &rest_list, span, ctx, binds)?;
                cond = bool_and(cond, c);
                // Suffix items (from end)
                for j in 0..after {
                    let pat = &items[si + 1 + j];
                    let idx = ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::Binary {
                            op: ir::BinOp::Sub,
                            left: Box::new(ir::Expr {
                                ty: ir::Ty::Int,
                                kind: ir::ExprKind::Len(Box::new(subject.clone())),
                            }),
                            right: Box::new(int_const((after - j) as i64)),
                        },
                    };
                    let elem_e = ir::Expr {
                        ty: *elem,
                        kind: ir::ExprKind::Index {
                            base: Box::new(subject.clone()),
                            index: Box::new(idx),
                        },
                    };
                    let c = lower_pattern_match(pat, &elem_e, span, ctx, binds)?;
                    cond = bool_and(cond, c);
                }
                Ok(cond)
            } else {
                let n = items.len() as i64;
                let len_ok = ir::Expr {
                    ty: ir::Ty::Bool,
                    kind: ir::ExprKind::Binary {
                        op: ir::BinOp::Eq,
                        left: Box::new(ir::Expr {
                            ty: ir::Ty::Int,
                            kind: ir::ExprKind::Len(Box::new(subject.clone())),
                        }),
                        right: Box::new(int_const(n)),
                    },
                };
                let mut cond = len_ok;
                for (i, pat) in items.iter().enumerate() {
                    let elem_e = ir::Expr {
                        ty: *elem,
                        kind: ir::ExprKind::Index {
                            base: Box::new(subject.clone()),
                            index: Box::new(int_const(i as i64)),
                        },
                    };
                    let c = lower_pattern_match(pat, &elem_e, span, ctx, binds)?;
                    cond = bool_and(cond, c);
                }
                Ok(cond)
            }
        }
        ir::Ty::Tuple(elems) => {
            if let Some(si) = star {
                // Variable-length star on fixed tuples: only when rest can be a list
                // of a uniform type — reject heterogeneous rest for now.
                let before = si;
                let after = items.len() - si - 1;
                if elems.len() < before + after {
                    return Ok(ir::Expr {
                        ty: ir::Ty::Bool,
                        kind: ir::ExprKind::ConstBool(false),
                    });
                }
                let rest_elems = &elems[before..elems.len() - after];
                // Rest must share one type (or be empty) for list materialization.
                let rest_ty = if rest_elems.is_empty() {
                    ir::Ty::Int // unused
                } else {
                    let mut t = rest_elems[0];
                    for e in &rest_elems[1..] {
                        t = join_elem_types(t, *e).ok_or_else(|| {
                            err(
                                "starred sequence pattern on tuple requires homogeneous rest elements",
                                span,
                            )
                        })?;
                    }
                    t
                };
                let mut cond = ir::Expr {
                    ty: ir::Ty::Bool,
                    kind: ir::ExprKind::ConstBool(true),
                };
                for (i, pat) in items.iter().enumerate().take(before) {
                    let elem_e = ir::Expr {
                        ty: elems[i],
                        kind: ir::ExprKind::Index {
                            base: Box::new(subject.clone()),
                            index: Box::new(int_const(i as i64)),
                        },
                    };
                    let c = lower_pattern_match(pat, &elem_e, span, ctx, binds)?;
                    cond = bool_and(cond, c);
                }
                // Materialize rest as list
                let mut rest_items = Vec::new();
                for (k, _) in rest_elems.iter().enumerate() {
                    let idx = (before + k) as i64;
                    rest_items.push(ir::Expr {
                        ty: rest_ty,
                        kind: ir::ExprKind::Index {
                            base: Box::new(subject.clone()),
                            index: Box::new(int_const(idx)),
                        },
                    });
                }
                let rest_list = ir::Expr {
                    ty: ir::list_of(rest_ty),
                    kind: ir::ExprKind::ListLit(rest_items),
                };
                let c = lower_pattern_match(&items[si], &rest_list, span, ctx, binds)?;
                cond = bool_and(cond, c);
                for j in 0..after {
                    let pat = &items[si + 1 + j];
                    let idx = (elems.len() - after + j) as i64;
                    let elem_e = ir::Expr {
                        ty: elems[idx as usize],
                        kind: ir::ExprKind::Index {
                            base: Box::new(subject.clone()),
                            index: Box::new(int_const(idx)),
                        },
                    };
                    let c = lower_pattern_match(pat, &elem_e, span, ctx, binds)?;
                    cond = bool_and(cond, c);
                }
                Ok(cond)
            } else {
                if elems.len() != items.len() {
                    return Ok(ir::Expr {
                        ty: ir::Ty::Bool,
                        kind: ir::ExprKind::ConstBool(false),
                    });
                }
                let mut cond = ir::Expr {
                    ty: ir::Ty::Bool,
                    kind: ir::ExprKind::ConstBool(true),
                };
                for (i, pat) in items.iter().enumerate() {
                    let elem_e = ir::Expr {
                        ty: elems[i],
                        kind: ir::ExprKind::Index {
                            base: Box::new(subject.clone()),
                            index: Box::new(int_const(i as i64)),
                        },
                    };
                    let c = lower_pattern_match(pat, &elem_e, span, ctx, binds)?;
                    cond = bool_and(cond, c);
                }
                Ok(cond)
            }
        }
        other => Err(err(
            format!("sequence pattern requires list or tuple subject, found {other}"),
            span,
        )),
    }
}

fn lower_mapping_pattern(
    pairs: &[(String, ast::Pattern)],
    rest: Option<&str>,
    subject: &ir::Expr,
    span: Span,
    ctx: &mut FnCtx,
    binds: &mut Vec<ir::Stmt>,
) -> SResult<ir::Expr> {
    let ir::Ty::Dict { key, value } = subject.ty else {
        return Err(err(
            format!(
                "mapping pattern requires dict subject, found {}",
                subject.ty
            ),
            span,
        ));
    };
    if *key != ir::Ty::Str {
        return Err(err(
            "mapping patterns require dict[str, ...] in this subset",
            span,
        ));
    }
    let mut cond = ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::ConstBool(true),
    };
    for (k, pat) in pairs {
        let key_e = ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::ConstStr(k.clone()),
        };
        let has = ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::Contains {
                needle: Box::new(key_e.clone()),
                haystack: Box::new(subject.clone()),
            },
        };
        let val_e = ir::Expr {
            ty: *value,
            kind: ir::ExprKind::Index {
                base: Box::new(subject.clone()),
                index: Box::new(key_e),
            },
        };
        let c = lower_pattern_match(pat, &val_e, span, ctx, binds)?;
        cond = bool_and(cond, bool_and(has, c));
    }
    if let Some(rest_name) = rest {
        // rest = {k: v for k,v in subject.items() if k not in matched_keys}
        // Build by copying all keys then deleting matched ones.
        let rest_ty = ir::dict_of(*key, *value);
        let rest_tmp = ctx.fresh_temp("mrest", rest_ty);
        // Start with empty dict, copy unmatched keys.
        // Use: keys = d.keys(); for each key, if not matched, rest[k] = d[k]
        // Simpler IR: DictCopy-like via iterating keys.
        // Build list of matched key strings for exclusion.
        let keys_list = ir::Expr {
            ty: ir::list_of(ir::Ty::Str),
            kind: ir::ExprKind::DictKeys(Box::new(subject.clone())),
        };
        let keys_t = ctx.fresh_temp("mrest.keys", ir::list_of(ir::Ty::Str));
        binds.push(ir::Stmt::Assign {
            name: keys_t.clone(),
            value: keys_list,
        });
        binds.push(ir::Stmt::Assign {
            name: rest_tmp.clone(),
            value: ir::Expr {
                ty: rest_ty,
                kind: ir::ExprKind::DictNew,
            },
        });
        let i_t = ctx.fresh_temp("mrest.i", ir::Ty::Int);
        binds.push(ir::Stmt::Assign {
            name: i_t.clone(),
            value: int_const(0),
        });
        let k_t = ctx.fresh_temp("mrest.k", ir::Ty::Str);
        // matched keys exclusion chain
        let key_local = ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::Local(k_t.clone()),
        };
        let mut not_matched = ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ConstBool(true),
        };
        for (mk, _) in pairs {
            let is_m = ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Eq,
                    left: Box::new(key_local.clone()),
                    right: Box::new(ir::Expr {
                        ty: ir::Ty::Str,
                        kind: ir::ExprKind::ConstStr(mk.clone()),
                    }),
                },
            };
            let not_m = ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Unary {
                    op: ir::UnOp::Not,
                    operand: Box::new(is_m),
                },
            };
            not_matched = bool_and(not_matched, not_m);
        }
        let val_at = ir::Expr {
            ty: *value,
            kind: ir::ExprKind::Index {
                base: Box::new(subject.clone()),
                index: Box::new(key_local.clone()),
            },
        };
        let body = vec![
            ir::Stmt::Assign {
                name: k_t.clone(),
                value: ir::Expr {
                    ty: ir::Ty::Str,
                    kind: ir::ExprKind::Index {
                        base: Box::new(ir::Expr {
                            ty: ir::list_of(ir::Ty::Str),
                            kind: ir::ExprKind::Local(keys_t.clone()),
                        }),
                        index: Box::new(ir::Expr {
                            ty: ir::Ty::Int,
                            kind: ir::ExprKind::Local(i_t.clone()),
                        }),
                    },
                },
            },
            ir::Stmt::If {
                branches: vec![(
                    not_matched,
                    vec![ir::Stmt::IndexAssign {
                        base: ir::Expr {
                            ty: rest_ty,
                            kind: ir::ExprKind::Local(rest_tmp.clone()),
                        },
                        index: ir::Expr {
                            ty: ir::Ty::Str,
                            kind: ir::ExprKind::Local(k_t.clone()),
                        },
                        value: val_at,
                    }],
                )],
                orelse: vec![],
            },
        ];
        let step = vec![ir::Stmt::Assign {
            name: i_t.clone(),
            value: ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Add,
                    left: Box::new(ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::Local(i_t.clone()),
                    }),
                    right: Box::new(int_const(1)),
                },
            },
        }];
        let loop_cond = ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::Binary {
                op: ir::BinOp::Lt,
                left: Box::new(ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Local(i_t),
                }),
                right: Box::new(ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Len(Box::new(ir::Expr {
                        ty: ir::list_of(ir::Ty::Str),
                        kind: ir::ExprKind::Local(keys_t),
                    })),
                }),
            },
        };
        binds.push(ir::Stmt::While {
            cond: loop_cond,
            body,
            step,
        });
        let rest_e = ir::Expr {
            ty: rest_ty,
            kind: ir::ExprKind::Local(rest_tmp),
        };
        let stmt = bind_name(rest_name, span, None, rest_e, span, ctx)?;
        binds.push(stmt);
    }
    Ok(cond)
}

/// `with open(...) as f:` — files only. Desugars to bind +
/// `try: body finally: f.close()`, so catchable raise/die still close
/// the handle (same as CPython's context-manager finally).
fn lower_with(
    item: &ast::Expr,
    target: Option<&(String, Span)>,
    body: &[ast::Stmt],
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    let item_ir = lower_expr(item, ctx)?;
    if item_ir.ty != ir::Ty::File {
        return Err(err(
            format!(
                "'{}' object does not support the context manager protocol",
                item_ir.ty
            ),
            item.span,
        ));
    }

    // bind the handle: `as name` uses the user's variable, otherwise a temp
    let (load_handle, bind_stmt) = match target {
        Some((name, name_span)) => {
            let bind = bind_name(name, *name_span, None, item_ir, item.span, ctx)?;
            let load = if ctx.binds_global(name) {
                ir::ExprKind::GlobalLoad(ctx.own_global(name))
            } else {
                ir::ExprKind::Local(name.clone())
            };
            (
                ir::Expr {
                    ty: ir::Ty::File,
                    kind: load,
                },
                bind,
            )
        }
        Option::None => {
            let t = ctx.fresh_temp("with", ir::Ty::File);
            let load = ir::Expr {
                ty: ir::Ty::File,
                kind: ir::ExprKind::Local(t.clone()),
            };
            (
                load,
                ir::Stmt::Assign {
                    name: t,
                    value: item_ir,
                },
            )
        }
    };
    out.push(bind_stmt);

    let close_stmt = || {
        ir::Stmt::ExprStmt(ir::Expr {
            ty: ir::Ty::None,
            kind: ir::ExprKind::FileCall {
                func: ir::FileFn::Close,
                args: vec![load_handle.clone()],
            },
        })
    };

    // Lower as try/finally so catchable raise/die still closes the file.
    let body_ir = lower_nested_block(body, ctx)?;
    out.push(ir::Stmt::Try {
        body: body_ir,
        handlers: vec![],
        orelse: vec![],
        finally: vec![close_stmt()],
    });
    Ok(())
}

/// Method calls in statement position (`xs.append(v)`, `xs.pop()`,
/// `s.upper()` with the result discarded).
fn lower_method_stmt(
    base: &ast::Expr,
    method: &str,
    method_span: Span,
    args: &[ast::Expr],
    ctx: &mut FnCtx,
) -> SResult<ir::Stmt> {
    // `super().m(...)` in statement position (e.g. super().__init__(...)).
    if is_zero_arg_super(base) {
        let call = lower_super_method_call(method, method_span, args, ctx)?;
        return Ok(ir::Stmt::ExprStmt(call));
    }
    let base_ir = lower_expr(base, ctx)?;
    if let ir::Ty::Class(id) = base_ir.ty {
        let call = lower_instance_method_call(base_ir, id, method, method_span, args, ctx)?;
        return Ok(ir::Stmt::ExprStmt(call));
    }
    match base_ir.ty {
        ir::Ty::List(elem) => match method {
            "append" => {
                if args.len() != 1 {
                    return Err(err(
                        format!("append() takes exactly one argument ({} given)", args.len()),
                        method_span,
                    ));
                }
                let value = lower_expr(&args[0], ctx)?;
                let value = coerce(value, *elem, args[0].span, "append() argument")?;
                Ok(ir::Stmt::ListAppend {
                    list: base_ir,
                    value,
                })
            }
            "insert" => {
                if args.len() != 2 {
                    return Err(err(
                        format!("insert() takes exactly 2 arguments ({} given)", args.len()),
                        method_span,
                    ));
                }
                let index = lower_expr(&args[0], ctx)?;
                let index = coerce(index, ir::Ty::Int, args[0].span, "insert() index")?;
                let value = lower_expr(&args[1], ctx)?;
                let value = coerce(value, *elem, args[1].span, "insert() argument")?;
                Ok(ir::Stmt::ListInsert {
                    list: base_ir,
                    index,
                    value,
                })
            }
            "remove" => {
                if args.len() != 1 {
                    return Err(err(
                        format!("remove() takes exactly one argument ({} given)", args.len()),
                        method_span,
                    ));
                }
                let value = lower_expr(&args[0], ctx)?;
                let value = coerce(value, *elem, args[0].span, "remove() argument")?;
                Ok(ir::Stmt::ListRemove {
                    list: base_ir,
                    value,
                })
            }
            "clear" => {
                if !args.is_empty() {
                    return Err(err(
                        format!("clear() takes no arguments ({} given)", args.len()),
                        method_span,
                    ));
                }
                Ok(ir::Stmt::ListClear { list: base_ir })
            }
            "sort" => {
                if !args.is_empty() {
                    return Err(err(
                        format!(
                            "sort() takes no arguments ({} given); key=/reverse= \
                             are not supported yet",
                            args.len()
                        ),
                        method_span,
                    ));
                }
                ensure_sortable_list_elem(*elem, method_span)?;
                Ok(ir::Stmt::ListSort { list: base_ir })
            }
            "extend" => {
                if args.len() != 1 {
                    return Err(err(
                        format!("extend() takes exactly one argument ({} given)", args.len()),
                        method_span,
                    ));
                }
                let other = lower_expr(&args[0], ctx)?;
                match other.ty {
                    ir::Ty::List(other_elem) => {
                        // Same element type; provisional empty `list[Any]` is allowed
                        // (CPython extends fine from []).
                        if *other_elem != *elem && *other_elem != ir::Ty::Any {
                            return Err(err(
                                format!(
                                    "list.extend() element type mismatch: expected \
                                     list[{elem}], found list[{other_elem}]"
                                ),
                                args[0].span,
                            ));
                        }
                        Ok(ir::Stmt::ListExtend {
                            list: base_ir,
                            other,
                        })
                    }
                    other_ty => Err(err(
                        format!("list.extend() currently requires a list argument, got {other_ty}"),
                        args[0].span,
                    )),
                }
            }
            "copy" => {
                if !args.is_empty() {
                    return Err(err(
                        format!("copy() takes no arguments ({} given)", args.len()),
                        method_span,
                    ));
                }
                Ok(ir::Stmt::ExprStmt(ir::Expr {
                    ty: ir::list_of(*elem),
                    kind: ir::ExprKind::ListCopy(Box::new(base_ir)),
                }))
            }
            // pop / index as statements discard the result
            "pop" => {
                let pop = lower_list_pop(base_ir, *elem, args, method_span, ctx)?;
                Ok(ir::Stmt::ExprStmt(pop))
            }
            "index" => {
                let idx = lower_list_index_of(base_ir, *elem, args, method_span, ctx)?;
                Ok(ir::Stmt::ExprStmt(idx))
            }
            _ => Err(err(
                format!(
                    "list method '{method}' is not supported yet (supported: \
                     append, pop, insert, remove, index, clear, sort)"
                ),
                method_span,
            )),
        },
        ir::Ty::Str => {
            let call = lower_str_method(base_ir, method, method_span, args, ctx)?;
            Ok(ir::Stmt::ExprStmt(call))
        }
        ir::Ty::File => {
            let call = lower_file_method(base_ir, method, method_span, args, ctx)?;
            Ok(ir::Stmt::ExprStmt(call))
        }
        ir::Ty::Dict { key, value } => {
            lower_dict_method_stmt(base_ir, *key, *value, method, method_span, args, ctx)
        }
        ir::Ty::Set(elem) => lower_set_method_stmt(base_ir, *elem, method, method_span, args, ctx),
        ir::Ty::Generator { yield_ty } => {
            lower_generator_method_stmt(base_ir, *yield_ty, method, method_span, args, ctx)
        }
        other => Err(err(
            format!("'{other}' has no method '{method}'"),
            method_span,
        )),
    }
}

fn lower_generator_method_stmt(
    base_ir: ir::Expr,
    yield_ty: ir::Ty,
    method: &str,
    method_span: Span,
    args: &[ast::Expr],
    ctx: &mut FnCtx,
) -> SResult<ir::Stmt> {
    match method {
        "close" => {
            if !args.is_empty() {
                return Err(err(
                    format!("close() takes no arguments ({} given)", args.len()),
                    method_span,
                ));
            }
            Ok(ir::Stmt::GenClose { generator: base_ir })
        }
        "send" => {
            if args.len() != 1 {
                return Err(err(
                    format!("send() takes exactly one argument ({} given)", args.len()),
                    method_span,
                ));
            }
            let send = lower_gen_send_arg(&args[0], yield_ty, ctx)?;
            let next = ir::Expr {
                ty: ir::optional_of(yield_ty),
                kind: ir::ExprKind::GeneratorNext {
                    generator: Box::new(base_ir),
                    send: Box::new(send),
                },
            };
            Ok(ir::Stmt::ExprStmt(next))
        }
        "throw" => {
            let (exc, message) = lower_gen_throw_args(args, method_span, ctx)?;
            let thr = ir::Expr {
                ty: ir::optional_of(yield_ty),
                kind: ir::ExprKind::GeneratorThrow {
                    generator: Box::new(base_ir),
                    exc,
                    message: Box::new(message),
                },
            };
            Ok(ir::Stmt::ExprStmt(thr))
        }
        _ => Err(err(
            format!(
                "generator method '{method}' is not supported yet (supported: close, send, throw)"
            ),
            method_span,
        )),
    }
}

/// `send` arg: `None` or a value coerced to the generator's yield type.
fn lower_gen_send_arg(arg: &ast::Expr, yield_ty: ir::Ty, ctx: &mut FnCtx) -> SResult<ir::Expr> {
    let v = lower_expr(arg, ctx)?;
    if matches!(v.kind, ir::ExprKind::ConstNone) || v.ty == ir::Ty::None {
        return Ok(const_none());
    }
    coerce(v, yield_ty, arg.span, "generator.send value")
}

/// Parse `throw(ExcType)`, `throw(ExcType("msg"))`, or `throw(ExcType, "msg")`.
fn lower_gen_throw_args(
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<(ir::ExcType, ir::Expr)> {
    if args.is_empty() || args.len() > 2 {
        return Err(err(
            format!(
                "throw() takes 1 or 2 arguments ({} given); use throw(ExcType) or \
                 throw(ExcType(\"msg\"))",
                args.len()
            ),
            method_span,
        ));
    }
    if args.len() == 2 {
        let exc = parse_exc_type_name(&args[0])?;
        let msg = lower_expr(&args[1], ctx)?;
        let msg = coerce(msg, ir::Ty::Str, args[1].span, "throw message")?;
        return Ok((exc, msg));
    }
    // Single argument: ExcType or ExcType("msg")
    match &args[0].kind {
        ast::ExprKind::Name(n) => {
            let exc = name_to_exc_type(n, args[0].span)?;
            Ok((exc, empty_str_const()))
        }
        ast::ExprKind::Call {
            func,
            func_span,
            args: call_args,
            keywords,
            kwargs,
        } => {
            if !keywords.is_empty() || kwargs.is_some() {
                return Err(err(
                    "throw() exception constructor does not accept keyword arguments",
                    args[0].span,
                ));
            }
            let exc = name_to_exc_type(func, *func_span)?;
            let plain = require_plain_args(call_args, "throw exception", args[0].span)?;
            if plain.len() > 1 {
                return Err(err(
                    format!(
                        "exception constructor takes at most 1 argument ({} given)",
                        plain.len()
                    ),
                    args[0].span,
                ));
            }
            if plain.is_empty() {
                Ok((exc, empty_str_const()))
            } else {
                let msg = lower_expr(plain[0], ctx)?;
                let msg = coerce(msg, ir::Ty::Str, plain[0].span, "throw message")?;
                Ok((exc, msg))
            }
        }
        _ => Err(err(
            format!(
                "throw() expects ExcType or ExcType(\"msg\") (supported: {})",
                ir::ExcType::all_names()
            ),
            args[0].span,
        )),
    }
}

fn parse_exc_type_name(e: &ast::Expr) -> SResult<ir::ExcType> {
    match &e.kind {
        ast::ExprKind::Name(n) => name_to_exc_type(n, e.span),
        _ => Err(err(
            "throw() first argument must be an exception type name",
            e.span,
        )),
    }
}

fn name_to_exc_type(name: &str, span: Span) -> SResult<ir::ExcType> {
    match name {
        "ValueError" => Ok(ir::ExcType::ValueError),
        "KeyError" => Ok(ir::ExcType::KeyError),
        "IndexError" => Ok(ir::ExcType::IndexError),
        "ZeroDivisionError" => Ok(ir::ExcType::ZeroDivisionError),
        "TypeError" => Ok(ir::ExcType::TypeError),
        "RuntimeError" => Ok(ir::ExcType::RuntimeError),
        "GeneratorExit" => Ok(ir::ExcType::GeneratorExit),
        "OverflowError" => Ok(ir::ExcType::OverflowError),
        "EOFError" => Ok(ir::ExcType::EOFError),
        "FileNotFoundError" => Ok(ir::ExcType::FileNotFoundError),
        "OSError" => Ok(ir::ExcType::OSError),
        "NameError" => Ok(ir::ExcType::NameError),
        "UnboundLocalError" => Ok(ir::ExcType::UnboundLocalError),
        "StopIteration" => Ok(ir::ExcType::StopIteration),
        "Exception" => Ok(ir::ExcType::Exception),
        "PermissionError" => Ok(ir::ExcType::PermissionError),
        "IsADirectoryError" => Ok(ir::ExcType::IsADirectoryError),
        "AssertionError" => Ok(ir::ExcType::AssertionError),
        _ => Err(err(
            format!(
                "unsupported exception type '{name}' (supported: {})",
                ir::ExcType::all_names()
            ),
            span,
        )),
    }
}

fn empty_str_const() -> ir::Expr {
    ir::Expr {
        ty: ir::Ty::Str,
        kind: ir::ExprKind::ConstStr(String::new()),
    }
}

fn lower_dict_method_stmt(
    base_ir: ir::Expr,
    key_ty: ir::Ty,
    val_ty: ir::Ty,
    method: &str,
    method_span: Span,
    args: &[ast::Expr],
    ctx: &mut FnCtx,
) -> SResult<ir::Stmt> {
    match method {
        "clear" => {
            if !args.is_empty() {
                return Err(err(
                    format!("clear() takes no arguments ({} given)", args.len()),
                    method_span,
                ));
            }
            Ok(ir::Stmt::DictClear { dict: base_ir })
        }
        "update" => {
            if args.len() != 1 {
                return Err(err(
                    format!("update() takes exactly one argument ({} given)", args.len()),
                    method_span,
                ));
            }
            let other = lower_expr(&args[0], ctx)?;
            let expect = ir::dict_of(key_ty, val_ty);
            if other.ty != expect {
                return Err(err(
                    format!(
                        "dict.update() expects {expect}, found {} (same key/value types required)",
                        other.ty
                    ),
                    args[0].span,
                ));
            }
            Ok(ir::Stmt::DictUpdate {
                dict: base_ir,
                other,
            })
        }
        "get" | "pop" | "keys" | "values" | "items" => {
            let call = lower_dict_method(base_ir, key_ty, val_ty, method, method_span, args, ctx)?;
            Ok(ir::Stmt::ExprStmt(call))
        }
        _ => Err(err(
            format!(
                "dict method '{method}' is not supported yet (supported: get, pop, \
                 keys, values, items, clear, update)"
            ),
            method_span,
        )),
    }
}

fn lower_set_method_stmt(
    base_ir: ir::Expr,
    elem_ty: ir::Ty,
    method: &str,
    method_span: Span,
    args: &[ast::Expr],
    ctx: &mut FnCtx,
) -> SResult<ir::Stmt> {
    match method {
        "add" => {
            if args.len() != 1 {
                return Err(err(
                    format!("add() takes exactly one argument ({} given)", args.len()),
                    method_span,
                ));
            }
            let v = lower_expr(&args[0], ctx)?;
            let v = coerce(v, elem_ty, args[0].span, "set.add() argument")?;
            Ok(ir::Stmt::SetAdd {
                set: base_ir,
                value: v,
            })
        }
        "remove" => {
            if args.len() != 1 {
                return Err(err(
                    format!("remove() takes exactly one argument ({} given)", args.len()),
                    method_span,
                ));
            }
            let v = lower_expr(&args[0], ctx)?;
            let v = coerce(v, elem_ty, args[0].span, "set.remove() argument")?;
            Ok(ir::Stmt::SetRemove {
                set: base_ir,
                value: v,
            })
        }
        "discard" => {
            if args.len() != 1 {
                return Err(err(
                    format!(
                        "discard() takes exactly one argument ({} given)",
                        args.len()
                    ),
                    method_span,
                ));
            }
            let v = lower_expr(&args[0], ctx)?;
            let v = coerce(v, elem_ty, args[0].span, "set.discard() argument")?;
            Ok(ir::Stmt::SetDiscard {
                set: base_ir,
                value: v,
            })
        }
        "clear" => {
            if !args.is_empty() {
                return Err(err(
                    format!("clear() takes no arguments ({} given)", args.len()),
                    method_span,
                ));
            }
            Ok(ir::Stmt::SetClear { set: base_ir })
        }
        "union" => {
            if args.len() != 1 {
                return Err(err(
                    format!("union() takes exactly one argument ({} given)", args.len()),
                    method_span,
                ));
            }
            let other = lower_expr(&args[0], ctx)?;
            let u = lower_set_union(base_ir, other, method_span)?;
            Ok(ir::Stmt::ExprStmt(u))
        }
        "intersection" | "difference" | "symmetric_difference" => {
            if args.len() != 1 {
                return Err(err(
                    format!(
                        "{method}() takes exactly one argument ({} given)",
                        args.len()
                    ),
                    method_span,
                ));
            }
            let other = lower_expr(&args[0], ctx)?;
            let u = match method {
                "intersection" => {
                    lower_set_binary_op(base_ir, other, method_span, method, |l, r| {
                        ir::ExprKind::SetIntersect { left: l, right: r }
                    })?
                }
                "difference" => {
                    lower_set_binary_op(base_ir, other, method_span, method, |l, r| {
                        ir::ExprKind::SetDiff { left: l, right: r }
                    })?
                }
                _ => lower_set_binary_op(base_ir, other, method_span, method, |l, r| {
                    ir::ExprKind::SetSymDiff { left: l, right: r }
                })?,
            };
            Ok(ir::Stmt::ExprStmt(u))
        }
        "update" => {
            // In-place union (same as |=).
            if args.len() != 1 {
                return Err(err(
                    format!("update() takes exactly one argument ({} given)", args.len()),
                    method_span,
                ));
            }
            let other = lower_expr(&args[0], ctx)?;
            let expect = ir::set_of(elem_ty);
            if other.ty != expect {
                return Err(err(
                    format!(
                        "set.update() expects {expect}, found {} (same element type required)",
                        other.ty
                    ),
                    args[0].span,
                ));
            }
            Ok(ir::Stmt::SetUpdate {
                set: base_ir,
                other,
            })
        }
        _ => Err(err(
            format!(
                "set method '{method}' is not supported yet (supported: add, remove, \
                 discard, clear, union, intersection, difference, symmetric_difference, update)"
            ),
            method_span,
        )),
    }
}

fn lower_set_union(l: ir::Expr, r: ir::Expr, span: Span) -> SResult<ir::Expr> {
    lower_set_binary_op(l, r, span, "union", |left, right| ir::ExprKind::SetUnion {
        left,
        right,
    })
}

fn lower_set_binary_op(
    l: ir::Expr,
    r: ir::Expr,
    span: Span,
    name: &str,
    kind: impl FnOnce(Box<ir::Expr>, Box<ir::Expr>) -> ir::ExprKind,
) -> SResult<ir::Expr> {
    match (l.ty, r.ty) {
        (ir::Ty::Set(a), ir::Ty::Set(b)) if a == b => Ok(ir::Expr {
            ty: ir::set_of(*a),
            kind: kind(Box::new(l), Box::new(r)),
        }),
        (ir::Ty::Set(a), ir::Ty::Set(b)) => Err(err(
            format!("set {name} requires the same element type (set[{a}] vs set[{b}])"),
            span,
        )),
        _ => Err(err(
            format!("set {name} requires two sets, found {} and {}", l.ty, r.ty),
            span,
        )),
    }
}

/// `list(iterable)` — shallow copy for lists; chars for str; keys for dict;
/// elements for set; fixed-arity homogeneous tuple → list.
fn lower_list_ctor(arg: ir::Expr, span: Span) -> SResult<ir::Expr> {
    match arg.ty {
        ir::Ty::List(elem) => Ok(ir::Expr {
            ty: ir::list_of(*elem),
            kind: ir::ExprKind::ListCopy(Box::new(arg)),
        }),
        ir::Ty::Str => Ok(ir::Expr {
            ty: ir::list_of(ir::Ty::Str),
            kind: ir::ExprKind::ListFromStr(Box::new(arg)),
        }),
        ir::Ty::Set(elem) => Ok(ir::Expr {
            ty: ir::list_of(*elem),
            kind: ir::ExprKind::SetToList(Box::new(arg)),
        }),
        ir::Ty::Dict { key, .. } => Ok(ir::Expr {
            ty: ir::list_of(*key),
            kind: ir::ExprKind::DictKeys(Box::new(arg)),
        }),
        ir::Ty::Tuple(elems) => {
            if elems.is_empty() {
                return Ok(ir::Expr {
                    ty: ir::list_of(ir::Ty::Any),
                    kind: ir::ExprKind::ListLit(vec![]),
                });
            }
            let first = elems[0];
            if !elems.iter().all(|e| *e == first) {
                return Err(err(
                    "tuple() to list requires a homogeneous tuple (or use a list)",
                    span,
                ));
            }
            // Fixed-arity: index each element into a new list.
            let n = elems.len();
            let mut items = Vec::new();
            for i in 0..n {
                items.push(ir::Expr {
                    ty: first,
                    kind: ir::ExprKind::Index {
                        base: Box::new(arg.clone()),
                        index: Box::new(int_const(i as i64)),
                    },
                });
            }
            Ok(ir::Expr {
                ty: ir::list_of(first),
                kind: ir::ExprKind::ListLit(items),
            })
        }
        other => Err(err(format!("list() cannot convert {other} yet"), span)),
    }
}

fn lower_set_ctor(arg: ir::Expr, span: Span) -> SResult<ir::Expr> {
    match arg.ty {
        ir::Ty::List(elem) => {
            if !matches!(*elem, ir::Ty::Int | ir::Ty::Str) {
                return Err(err(
                    format!(
                        "set() from list only supports list[int] or list[str], found list[{elem}]"
                    ),
                    span,
                ));
            }
            Ok(ir::Expr {
                ty: ir::set_of(*elem),
                kind: ir::ExprKind::SetFromList {
                    list: Box::new(arg),
                    elem: Box::new(*elem),
                },
            })
        }
        ir::Ty::Str => Ok(ir::Expr {
            ty: ir::set_of(ir::Ty::Str),
            kind: ir::ExprKind::SetFromStr(Box::new(arg)),
        }),
        ir::Ty::Set(elem) => {
            // set(s) shallow copy via union with empty is heavy; rebuild from list.
            let as_list = ir::Expr {
                ty: ir::list_of(*elem),
                kind: ir::ExprKind::SetToList(Box::new(arg)),
            };
            Ok(ir::Expr {
                ty: ir::set_of(*elem),
                kind: ir::ExprKind::SetFromList {
                    list: Box::new(as_list),
                    elem: Box::new(*elem),
                },
            })
        }
        other => Err(err(format!("set() cannot convert {other} yet"), span)),
    }
}

fn lower_dict_ctor(arg: ir::Expr, span: Span) -> SResult<ir::Expr> {
    match arg.ty {
        ir::Ty::Dict { key, value } => Ok(ir::Expr {
            ty: ir::dict_of(*key, *value),
            kind: ir::ExprKind::DictCopy(Box::new(arg)),
        }),
        ir::Ty::List(elem) => match *elem {
            ir::Ty::Tuple(ts) if ts.len() == 2 => {
                let k = ts[0];
                let v = ts[1];
                if !matches!(k, ir::Ty::Int | ir::Ty::Str) {
                    return Err(err(
                        format!("dict() keys must be int or str, found {k}"),
                        span,
                    ));
                }
                Ok(ir::Expr {
                    ty: ir::dict_of(k, v),
                    kind: ir::ExprKind::DictFromPairs {
                        pairs: Box::new(arg),
                        key: Box::new(k),
                        value: Box::new(v),
                    },
                })
            }
            other => Err(err(
                format!("dict() from list expects list of 2-tuples, found list[{other}]"),
                span,
            )),
        },
        other => Err(err(format!("dict() cannot convert {other} yet"), span)),
    }
}

fn lower_tuple_ctor(arg: ir::Expr, span: Span) -> SResult<ir::Expr> {
    match arg.ty {
        ir::Ty::Tuple(_) => {
            // Identity/copy: re-index into a new TupleLit of same arity.
            // For now accept identity (same value); CPython tuple(t) is t if already tuple.
            Ok(arg)
        }
        ir::Ty::List(_) => Err(err(
            "tuple() from dynamic list is not supported yet; use a tuple literal",
            span,
        )),
        ir::Ty::Str => Err(err("tuple() from str is not supported yet", span)),
        other => Err(err(format!("tuple() cannot convert {other} yet"), span)),
    }
}

fn lower_dict_method(
    base_ir: ir::Expr,
    key_ty: ir::Ty,
    val_ty: ir::Ty,
    method: &str,
    method_span: Span,
    args: &[ast::Expr],
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    match method {
        "get" => {
            // Bare get(key) → Optional[V] (None on miss). get(key, default) keeps V.
            if args.is_empty() || args.len() > 2 {
                return Err(err(
                    format!("get() takes 1 or 2 arguments ({} given)", args.len()),
                    method_span,
                ));
            }
            let key = lower_expr(&args[0], ctx)?;
            let key = coerce(key, key_ty, args[0].span, "dict.get() key")?;
            if args.len() == 1 {
                let result_ty = ir::optional_of(val_ty);
                let default = coerce(const_none(), result_ty, method_span, "dict.get() default")?;
                return Ok(ir::Expr {
                    ty: result_ty,
                    kind: ir::ExprKind::DictGet {
                        dict: Box::new(base_ir),
                        key: Box::new(key),
                        default: Box::new(default),
                    },
                });
            }
            let d = lower_expr(&args[1], ctx)?;
            let default = coerce(d, val_ty, args[1].span, "dict.get() default")?;
            Ok(ir::Expr {
                ty: val_ty,
                kind: ir::ExprKind::DictGet {
                    dict: Box::new(base_ir),
                    key: Box::new(key),
                    default: Box::new(default),
                },
            })
        }
        "pop" => {
            if args.is_empty() || args.len() > 2 {
                return Err(err(
                    format!("pop() takes 1 or 2 arguments ({} given)", args.len()),
                    method_span,
                ));
            }
            let key = lower_expr(&args[0], ctx)?;
            let key = coerce(key, key_ty, args[0].span, "dict.pop() key")?;
            let default = if args.len() == 2 {
                let d = lower_expr(&args[1], ctx)?;
                Some(Box::new(coerce(
                    d,
                    val_ty,
                    args[1].span,
                    "dict.pop() default",
                )?))
            } else {
                None
            };
            Ok(ir::Expr {
                ty: val_ty,
                kind: ir::ExprKind::DictPop {
                    dict: Box::new(base_ir),
                    key: Box::new(key),
                    default,
                },
            })
        }
        "keys" => {
            if !args.is_empty() {
                return Err(err(
                    format!("keys() takes no arguments ({} given)", args.len()),
                    method_span,
                ));
            }
            Ok(ir::Expr {
                ty: ir::list_of(key_ty),
                kind: ir::ExprKind::DictKeys(Box::new(base_ir)),
            })
        }
        "values" => {
            if !args.is_empty() {
                return Err(err(
                    format!("values() takes no arguments ({} given)", args.len()),
                    method_span,
                ));
            }
            Ok(ir::Expr {
                ty: ir::list_of(val_ty),
                kind: ir::ExprKind::DictValues(Box::new(base_ir)),
            })
        }
        "items" => {
            if !args.is_empty() {
                return Err(err(
                    format!("items() takes no arguments ({} given)", args.len()),
                    method_span,
                ));
            }
            Ok(ir::Expr {
                ty: ir::list_of(ir::tuple_of(&[key_ty, val_ty])),
                kind: ir::ExprKind::DictItems(Box::new(base_ir)),
            })
        }
        "clear" => Err(err(
            "dict.clear() returns None and cannot be used in an expression",
            method_span,
        )),
        "copy" => {
            if !args.is_empty() {
                return Err(err(
                    format!("copy() takes no arguments ({} given)", args.len()),
                    method_span,
                ));
            }
            Ok(ir::Expr {
                ty: ir::dict_of(key_ty, val_ty),
                kind: ir::ExprKind::DictCopy(Box::new(base_ir)),
            })
        }
        _ => Err(err(
            format!(
                "dict method '{method}' is not supported yet (supported: get, pop, \
                 keys, values, items, clear, copy)"
            ),
            method_span,
        )),
    }
}

/// The supported file methods.
fn lower_file_method(
    base_ir: ir::Expr,
    method: &str,
    method_span: Span,
    args: &[ast::Expr],
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    use ir::FileFn::*;

    let (func, ret, takes_str_arg) = match method {
        "read" => (Read, ir::Ty::Str, false),
        "readline" => (ReadLine, ir::Ty::Str, false),
        "readlines" => (ReadLines, ir::list_of(ir::Ty::Str), false),
        "write" => (Write, ir::Ty::Int, true),
        "close" => (Close, ir::Ty::None, false),
        _ => {
            return Err(err(
                format!(
                    "file method '{method}' is not supported yet (supported: \
                     read, readline, readlines, write, close)"
                ),
                method_span,
            ));
        }
    };

    let expected_args = usize::from(takes_str_arg);
    if args.len() != expected_args {
        return Err(err(
            format!(
                "{method}() takes exactly {expected_args} argument(s) ({} given)",
                args.len()
            ),
            method_span,
        ));
    }

    let mut call_args = vec![base_ir];
    if takes_str_arg {
        let a = lower_expr(&args[0], ctx)?;
        if a.ty != ir::Ty::Str {
            return Err(err(
                format!("{method}() expects a str argument, found {}", a.ty),
                args[0].span,
            ));
        }
        call_args.push(a);
    }
    Ok(ir::Expr {
        ty: ret,
        kind: ir::ExprKind::FileCall {
            func,
            args: call_args,
        },
    })
}

/// The supported `str` methods (ASCII case/whitespace rules).
fn lower_str_method(
    base_ir: ir::Expr,
    method: &str,
    method_span: Span,
    args: &[ast::Expr],
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    use ir::StrFn::*;

    // (runtime function, result type, extra str args expected)
    let (func, ret, str_args): (ir::StrFn, ir::Ty, usize) = match method {
        "upper" => (Upper, ir::Ty::Str, 0),
        "lower" => (Lower, ir::Ty::Str, 0),
        "strip" => (Strip, ir::Ty::Str, 0),
        "lstrip" => (Lstrip, ir::Ty::Str, 0),
        "rstrip" => (Rstrip, ir::Ty::Str, 0),
        "startswith" => (StartsWith, ir::Ty::Bool, 1),
        "endswith" => (EndsWith, ir::Ty::Bool, 1),
        "find" => (Find, ir::Ty::Int, 1),
        "rfind" => (RFind, ir::Ty::Int, 1),
        "rindex" => (RIndex, ir::Ty::Int, 1),
        "count" => (Count, ir::Ty::Int, 1),
        "replace" => (Replace, ir::Ty::Str, 2),
        "split" => {
            return lower_str_split(base_ir, args, method_span, ctx);
        }
        "join" => {
            if args.len() != 1 {
                return Err(err(
                    format!("join() takes exactly one argument ({} given)", args.len()),
                    method_span,
                ));
            }
            let parts = lower_expr(&args[0], ctx)?;
            if parts.ty != ir::list_of(ir::Ty::Str) {
                return Err(err(
                    format!("join() expects a list[str], found {}", parts.ty),
                    args[0].span,
                ));
            }
            return Ok(ir::Expr {
                ty: ir::Ty::Str,
                kind: ir::ExprKind::StrCall {
                    func: Join,
                    args: vec![base_ir, parts],
                },
            });
        }
        "isdigit" => (IsDigit, ir::Ty::Bool, 0),
        "isalpha" => (IsAlpha, ir::Ty::Bool, 0),
        "isspace" => (IsSpace, ir::Ty::Bool, 0),
        "isupper" => (IsUpper, ir::Ty::Bool, 0),
        "islower" => (IsLower, ir::Ty::Bool, 0),
        _ => {
            return Err(err(
                format!(
                    "str method '{method}' is not supported yet (supported: \
                     upper, lower, strip, lstrip, rstrip, startswith, \
                     endswith, find, rfind, rindex, count, replace, split, \
                     join, isdigit, isalpha, isspace, isupper, islower)"
                ),
                method_span,
            ));
        }
    };

    if args.len() != str_args {
        return Err(err(
            format!(
                "{method}() takes exactly {str_args} argument(s) ({} given)",
                args.len()
            ),
            method_span,
        ));
    }
    let mut call_args = vec![base_ir];
    for arg in args {
        let a = lower_expr(arg, ctx)?;
        if a.ty != ir::Ty::Str {
            return Err(err(
                format!("{method}() expects str arguments, found {}", a.ty),
                arg.span,
            ));
        }
        call_args.push(a);
    }
    Ok(ir::Expr {
        ty: ret,
        kind: ir::ExprKind::StrCall {
            func,
            args: call_args,
        },
    })
}

fn lower_str_split(
    base_ir: ir::Expr,
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let (func, call_args) = match args {
        [] => (ir::StrFn::SplitWs, vec![base_ir]),
        [sep] => {
            let s = lower_expr(sep, ctx)?;
            if s.ty != ir::Ty::Str {
                return Err(err(
                    format!("split() separator must be a str, found {}", s.ty),
                    sep.span,
                ));
            }
            if matches!(&s.kind, ir::ExprKind::ConstStr(c) if c.is_empty()) {
                return Err(err("empty separator", sep.span));
            }
            (ir::StrFn::Split, vec![base_ir, s])
        }
        _ => {
            return Err(err(
                format!("split() takes at most one argument ({} given)", args.len()),
                method_span,
            ));
        }
    };
    Ok(ir::Expr {
        ty: ir::list_of(ir::Ty::Str),
        kind: ir::ExprKind::StrCall {
            func,
            args: call_args,
        },
    })
}

fn lower_list_pop(
    list: ir::Expr,
    elem: ir::Ty,
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let index = match args {
        [] => int_const(-1),
        [arg] => {
            let i = lower_expr(arg, ctx)?;
            coerce(i, ir::Ty::Int, arg.span, "pop() index")?
        }
        _ => {
            return Err(err(
                format!("pop() takes at most one argument ({} given)", args.len()),
                method_span,
            ));
        }
    };
    Ok(ir::Expr {
        ty: elem,
        kind: ir::ExprKind::ListPop {
            list: Box::new(list),
            index: Box::new(index),
        },
    })
}

fn ensure_sortable_list_elem(elem: ir::Ty, span: Span) -> SResult<()> {
    match elem {
        ir::Ty::Int | ir::Ty::Float | ir::Ty::Bool | ir::Ty::Str => Ok(()),
        other => Err(err(
            format!(
                "sort is only supported for list[int], list[float], list[bool], \
                 and list[str], found list[{other}]"
            ),
            span,
        )),
    }
}

fn lower_list_index_of(
    list: ir::Expr,
    elem: ir::Ty,
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.len() != 1 {
        return Err(err(
            format!("index() takes exactly one argument ({} given)", args.len()),
            method_span,
        ));
    }
    let value = lower_expr(&args[0], ctx)?;
    let value = coerce(value, elem, args[0].span, "index() argument")?;
    Ok(ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::ListIndexOf {
            list: Box::new(list),
            value: Box::new(value),
        },
    })
}

fn lower_assign(
    target: &ast::AssignTarget,
    annotation: Option<ast::TypeName>,
    value: &ast::Expr,
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    let ann_ty = match annotation {
        Some(t) => Some(resolve_type_checked(t, value.span)?),
        Option::None => Option::None,
    };
    // Storage type from multi-assign / empty-list-from-append pre-pass. Only
    // used as expected type for *empty* container literals (so `xs = []` then
    // `xs.append(1)` types as list[int]). Non-empty literals must keep strict
    // element joining — a pre-pass `join_types` union must not loosen them.
    let storage_hint = if ann_ty.is_none() {
        match target {
            ast::AssignTarget::Name { name, .. } => {
                ctx.storage_tys.get(name).copied().or_else(|| {
                    if ctx.binds_global(name) {
                        ctx.globals.get(name).copied()
                    } else {
                        None
                    }
                })
            }
            _ => None,
        }
    } else {
        None
    };
    let expected = ann_ty.or(storage_hint);
    // Propagate expected types into empty / annotated container literals
    let lowered = match (&value.kind, expected) {
        (ast::ExprKind::ListLit(items), Some(ir::Ty::List(elem)))
            if ann_ty.is_some() || items.is_empty() =>
        {
            lower_list_lit(items, Some(*elem), value.span, ctx)?
        }
        (ast::ExprKind::DictLit(items), Some(ir::Ty::Dict { key, value: val }))
            if ann_ty.is_some() || items.is_empty() =>
        {
            lower_dict_lit(items, Some((*key, *val)), value.span, ctx)?
        }
        (ast::ExprKind::SetLit(items), Some(ir::Ty::Set(elem)))
            if ann_ty.is_some() || items.is_empty() =>
        {
            lower_set_lit(items, Some(*elem), value.span, ctx)?
        }
        (ast::ExprKind::TupleLit(items), Some(ir::Ty::Tuple(elems))) if ann_ty.is_some() => {
            lower_tuple_lit(items, Some(elems), value.span, ctx)?
        }
        (
            ast::ExprKind::Call {
                func,
                args,
                keywords,
                kwargs,
                ..
            },
            Some(ir::Ty::Set(elem)),
        ) if func == "set"
            && args.is_empty()
            && keywords.is_empty()
            && kwargs.is_none()
            && !ctx.funcs().contains_key("set") =>
        {
            check_hashable_key(*elem, value.span, "set")?;
            ir::Expr {
                ty: ir::set_of(*elem),
                kind: ir::ExprKind::SetNew,
            }
        }
        _ => lower_expr(value, ctx)?,
    };
    lower_assign_ir(target, ann_ty, lowered, value.span, ctx, out)
}

/// Assign an already-lowered IR value to a target (used by multi-assign).
fn lower_assign_ir(
    target: &ast::AssignTarget,
    ann_ty: Option<ir::Ty>,
    value_ir: ir::Expr,
    value_span: Span,
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    match target {
        ast::AssignTarget::Name { name, span } => {
            let stmt = bind_name(name, *span, ann_ty, value_ir, value_span, ctx)?;
            out.push(stmt);
            // First outer bind of a free-captured name → box into a cell so
            // CellNew is not stuck inside an untaken nested-def branch.
            if ctx.cell_candidates.contains(name)
                && !ctx.cell_locals.contains_key(name)
                && let Some(ty) = ctx.locals.get(name).copied()
                && let Some(init) = ensure_cell(ctx, name, ty, *span)?
            {
                out.push(init);
            }
            Ok(())
        }
        ast::AssignTarget::Index { base, index } => {
            if ann_ty.is_some() {
                return Err(err(
                    "type annotations are only allowed on plain variable names",
                    value_span,
                ));
            }
            let (base_ir, elem, index_ir) = lower_index_target(base, index, ctx)?;
            let value_ir = coerce(value_ir, elem, value_span, "item assignment")?;
            out.push(ir::Stmt::IndexAssign {
                base: base_ir,
                index: index_ir,
                value: value_ir,
            });
            Ok(())
        }
        ast::AssignTarget::Tuple(targets) => {
            if ann_ty.is_some() {
                return Err(err(
                    "type annotations are only allowed on plain variable names",
                    value_span,
                ));
            }
            lower_unpack(targets, value_ir, value_span, ctx, out)
        }
        ast::AssignTarget::Starred { span, .. } => Err(err(
            "starred assignment target must be inside a tuple unpack (e.g. 'a, *rest = xs')",
            *span,
        )),
        ast::AssignTarget::Attr {
            base,
            attr,
            attr_span,
        } => {
            if ann_ty.is_some() {
                return Err(err(
                    "type annotations are only allowed on plain variable names",
                    value_span,
                ));
            }
            let base_ir = lower_expr(base, ctx)?;
            let class_id = match base_ir.ty {
                ir::Ty::Class(id) => id,
                other => {
                    return Err(err(
                        format!("cannot set attribute '{attr}' on '{other}'"),
                        *attr_span,
                    ));
                }
            };
            let (field_index, field_ty) = field_index(class_id, attr).ok_or_else(|| {
                err(
                    format!(
                        "'{}' object has no attribute '{attr}'",
                        class_info(class_id)
                            .map(|c| c.name)
                            .unwrap_or_else(|| format!("class#{class_id}"))
                    ),
                    *attr_span,
                )
            })?;
            let value_ir = coerce(value_ir, field_ty, value_span, "attribute assignment")?;
            out.push(ir::Stmt::SetField {
                object: base_ir,
                class_id,
                field_index,
                value: value_ir,
            });
            Ok(())
        }
    }
}

fn lower_delete(
    target: &ast::AssignTarget,
    span: Span,
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    match target {
        ast::AssignTarget::Index { base, index } => {
            let base_ir = lower_expr(base, ctx)?;
            match base_ir.ty {
                ir::Ty::Dict { key, .. } => {
                    let key_ir = lower_expr(index, ctx)?;
                    let key_ir = coerce(key_ir, *key, index.span, "dict key")?;
                    out.push(ir::Stmt::IndexDelete {
                        base: base_ir,
                        index: key_ir,
                    });
                    Ok(())
                }
                other => Err(err(
                    format!("'del' on '{other}' is not supported yet (only dict keys)"),
                    span,
                )),
            }
        }
        _ => Err(err(
            "'del' only supports dict item deletion (del d[key]) for now",
            span,
        )),
    }
}

/// Unpack `value` into `targets` (tuple/list RHS). Supports a single `*rest`.
fn lower_unpack(
    targets: &[ast::AssignTarget],
    value_ir: ir::Expr,
    value_span: Span,
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    let star_pos = targets
        .iter()
        .position(|t| matches!(t, ast::AssignTarget::Starred { .. }));
    let n = targets.len() as i64;
    let tmp = ctx.fresh_temp("unpack", value_ir.ty);
    out.push(ir::Stmt::Assign {
        name: tmp.clone(),
        value: value_ir.clone(),
    });
    let seq = ir::Expr {
        ty: value_ir.ty,
        kind: ir::ExprKind::Local(tmp),
    };

    match value_ir.ty {
        ir::Ty::Tuple(elems) => {
            if let Some(si) = star_pos {
                // fixed before/after; rest is a list of mixed types — only
                // allow when remaining tuple elems share a type (or empty).
                let before = si;
                let after = targets.len() - si - 1;
                let got = elems.len();
                if got < before + after {
                    return Err(err(
                        format!(
                            "not enough values to unpack (expected at least {}, got {})",
                            before + after,
                            got
                        ),
                        value_span,
                    ));
                }
                for (i, t) in targets.iter().enumerate().take(before) {
                    let elem = ir::Expr {
                        ty: elems[i],
                        kind: ir::ExprKind::Index {
                            base: Box::new(seq.clone()),
                            index: Box::new(int_const(i as i64)),
                        },
                    };
                    lower_assign_ir(t, None, elem, value_span, ctx, out)?;
                }
                let rest_start = before;
                let rest_end = got - after;
                let rest_elems = &elems[rest_start..rest_end];
                let rest_ty = if rest_elems.is_empty() {
                    // empty rest → list[int] placeholder is wrong; use first
                    // surrounding element type or int
                    elems.first().copied().unwrap_or(ir::Ty::Int)
                } else {
                    let mut ty = rest_elems[0];
                    for e in &rest_elems[1..] {
                        ty = join_elem_types(ty, *e).ok_or_else(|| {
                            err(
                                format!(
                                    "starred unpack rest elements must share one type; \
                                     found {ty} and {e}"
                                ),
                                value_span,
                            )
                        })?;
                    }
                    ty
                };
                let rest_ty = elem_of(rest_ty, value_span).unwrap_or(rest_ty);
                let mut rest_items = Vec::new();
                for (i, elem_ty) in elems.iter().enumerate().take(rest_end).skip(rest_start) {
                    rest_items.push(ir::Expr {
                        ty: *elem_ty,
                        kind: ir::ExprKind::Index {
                            base: Box::new(seq.clone()),
                            index: Box::new(int_const(i as i64)),
                        },
                    });
                }
                // coerce rest items to rest_ty
                let mut coerced = Vec::new();
                for it in rest_items {
                    coerced.push(coerce(it, rest_ty, value_span, "starred unpack")?);
                }
                let rest_list = ir::Expr {
                    ty: ir::list_of(rest_ty),
                    kind: ir::ExprKind::ListLit(coerced),
                };
                let ast::AssignTarget::Starred { target, .. } = &targets[si] else {
                    unreachable!()
                };
                lower_assign_ir(target, None, rest_list, value_span, ctx, out)?;
                for (j, t) in targets.iter().enumerate().skip(si + 1) {
                    let idx = got - after + (j - si - 1);
                    let elem = ir::Expr {
                        ty: elems[idx],
                        kind: ir::ExprKind::Index {
                            base: Box::new(seq.clone()),
                            index: Box::new(int_const(idx as i64)),
                        },
                    };
                    lower_assign_ir(t, None, elem, value_span, ctx, out)?;
                }
                return Ok(());
            }
            let got = elems.len() as i64;
            if got < n {
                return Err(err(
                    format!("not enough values to unpack (expected {n}, got {got})"),
                    value_span,
                ));
            }
            if got > n {
                return Err(err(
                    format!("too many values to unpack (expected {n}, got {got})"),
                    value_span,
                ));
            }
            for (i, t) in targets.iter().enumerate() {
                let elem = ir::Expr {
                    ty: elems[i],
                    kind: ir::ExprKind::Index {
                        base: Box::new(seq.clone()),
                        index: Box::new(int_const(i as i64)),
                    },
                };
                lower_assign_ir(t, None, elem, value_span, ctx, out)?;
            }
            Ok(())
        }
        ir::Ty::List(elem_ty) => {
            if let Some(si) = star_pos {
                let before = si as i64;
                let after = (targets.len() - si - 1) as i64;
                // Runtime: check len >= before+after; rest = xs[before:len-after]
                let len_e = ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Len(Box::new(seq.clone())),
                };
                let min_n = before + after;
                out.push(ir::Stmt::UnpackCheckMin {
                    len: len_e.clone(),
                    minimum: min_n,
                });
                for i in 0..before {
                    let elem = ir::Expr {
                        ty: *elem_ty,
                        kind: ir::ExprKind::Index {
                            base: Box::new(seq.clone()),
                            index: Box::new(int_const(i)),
                        },
                    };
                    lower_assign_ir(&targets[i as usize], None, elem, value_span, ctx, out)?;
                }
                // rest = seq[before : len - after]
                let lo = int_const(before);
                let hi = if after == 0 {
                    // use a large hi — slice clamps; i64::MAX style via len
                    len_e.clone()
                } else {
                    ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::Binary {
                            op: ir::BinOp::Sub,
                            left: Box::new(len_e.clone()),
                            right: Box::new(int_const(after)),
                        },
                    }
                };
                let rest = ir::Expr {
                    ty: ir::list_of(*elem_ty),
                    kind: ir::ExprKind::Slice {
                        base: Box::new(seq.clone()),
                        lo: Box::new(lo),
                        hi: Box::new(hi),
                        step: Box::new(int_const(1)),
                    },
                };
                let ast::AssignTarget::Starred { target, .. } = &targets[si] else {
                    unreachable!()
                };
                lower_assign_ir(target, None, rest, value_span, ctx, out)?;
                for j in 0..after {
                    let idx = ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::Binary {
                            op: ir::BinOp::Sub,
                            left: Box::new(len_e.clone()),
                            right: Box::new(int_const(after - j)),
                        },
                    };
                    let elem = ir::Expr {
                        ty: *elem_ty,
                        kind: ir::ExprKind::Index {
                            base: Box::new(seq.clone()),
                            index: Box::new(idx),
                        },
                    };
                    lower_assign_ir(
                        &targets[si + 1 + j as usize],
                        None,
                        elem,
                        value_span,
                        ctx,
                        out,
                    )?;
                }
                return Ok(());
            }
            out.push(ir::Stmt::UnpackCheck {
                len: ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Len(Box::new(seq.clone())),
                },
                expected: n,
            });
            for (i, t) in targets.iter().enumerate() {
                let elem = ir::Expr {
                    ty: *elem_ty,
                    kind: ir::ExprKind::Index {
                        base: Box::new(seq.clone()),
                        index: Box::new(int_const(i as i64)),
                    },
                };
                lower_assign_ir(t, None, elem, value_span, ctx, out)?;
            }
            Ok(())
        }
        other => Err(err(
            format!("cannot unpack non-iterable {other} object"),
            value_span,
        )),
    }
}

/// Check and lower the target of `base[index] = ...` (list or dict).
fn lower_index_target(
    base: &ast::Expr,
    index: &ast::Expr,
    ctx: &mut FnCtx,
) -> SResult<(ir::Expr, ir::Ty, ir::Expr)> {
    let base_ir = lower_expr(base, ctx)?;
    match base_ir.ty {
        ir::Ty::List(e) => {
            let index_ir = lower_expr(index, ctx)?;
            let index_ir = coerce(index_ir, ir::Ty::Int, index.span, "list index")?;
            Ok((base_ir, *e, index_ir))
        }
        ir::Ty::Dict { key, value } => {
            let key_ir = lower_expr(index, ctx)?;
            let key_ir = coerce(key_ir, *key, index.span, "dict key")?;
            Ok((base_ir, *value, key_ir))
        }
        ir::Ty::Str => Err(err(
            "'str' object does not support item assignment (strings are \
             immutable)",
            base.span,
        )),
        ir::Ty::Tuple(_) => Err(err(
            "'tuple' object does not support item assignment",
            base.span,
        )),
        other => Err(err(
            format!("'{other}' object does not support item assignment"),
            base.span,
        )),
    }
}

/// Bind `name = value_ir`, inferring or checking the variable's type.
/// At the top level (or after a `global` declaration) the binding targets
/// a module global; otherwise it creates/updates a function local.
fn bind_name(
    name: &str,
    name_span: Span,
    annotation: Option<ir::Ty>,
    value_ir: ir::Expr,
    value_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Stmt> {
    if ctx.funcs().contains_key(name) {
        return Err(err(
            format!("'{name}' is a function and cannot be assigned to"),
            name_span,
        ));
    }

    let is_global = ctx.binds_global(name);
    // Nonlocal/cell bindings keep their type in `cell_locals`, not `locals`
    // (locals holds `.cell.<name>`). Prefer that so stores coerce into the
    // union/optional element type (ToUnion) instead of the bare RHS type.
    // `storage_tys` holds joined multi-assign types from the body pre-pass.
    let existing = if is_global {
        ctx.globals.get(name).copied()
    } else if let Some(&t) = ctx.cell_locals.get(name) {
        Some(t)
    } else {
        ctx.locals
            .get(name)
            .copied()
            .or_else(|| ctx.storage_tys.get(name).copied())
    };

    let target_ty = match (annotation, existing) {
        (Some(ann_ty), existing) => {
            // Pure `None` annotation is allowed (variable holds only None); unions too.
            if let Some(existing) = existing
                && existing != ann_ty
            {
                return Err(err(
                    format!(
                        "variable '{name}' already has type {existing}; \
                         it cannot be re-declared as {ann_ty}"
                    ),
                    name_span,
                ));
            }
            ann_ty
        }
        (None, Some(existing)) => existing,
        // First assignment fixes the type, including pure None or a union.
        (None, None) => value_ir.ty,
    };

    // RHS type before coerce — used to re-establish a concrete refinement
    // when assigning a member into an optional/union binding.
    let rhs_ty = value_ir.ty;
    let value_expr = coerce_assign(value_ir, target_ty, name, value_span)?;

    // Assignment kills prior refinements; re-refine when RHS is a concrete
    // member of a union target (e.g. `x = x + 1` after `is not None`), or a
    // more-specific subclass of monomorphic class storage (after isinstance).
    ctx.type_refinements.remove(name);
    if matches!(target_ty, ir::Ty::Union(_))
        && let Some(member) = refined_member_after_assign(rhs_ty, target_ty)
    {
        ctx.type_refinements.insert(name.to_string(), member);
    } else if let (ir::Ty::Class(dst), ir::Ty::Class(src)) = (target_ty, rhs_ty)
        && class_is_subclass(src, dst)
        && src != dst
    {
        // `x: A = B()` after peel → refine back to B for subclass fields.
        ctx.type_refinements.insert(name.to_string(), rhs_ty);
    }

    if is_global {
        if !ctx.globals.contains_key(name) {
            ctx.globals.insert(name.to_string(), target_ty);
            ctx.globals_order.push((ctx.own_global(name), target_ty));
        }
        Ok(ir::Stmt::GlobalAssign {
            name: ctx.own_global(name),
            value: value_expr,
        })
    } else if ctx.cell_locals.contains_key(name) || ctx.declared_nonlocals.contains(name) {
        // Write through cell — value is coerced to the cell element type (ToUnion
        // when the cell holds Optional/union).
        let inner = ctx.cell_locals.get(name).copied().unwrap_or(target_ty);
        ctx.cell_locals.insert(name.to_string(), inner);
        let value_expr = if value_expr.ty != inner {
            coerce_assign(value_expr, inner, name, value_span)?
        } else {
            value_expr
        };
        let cell_name = format!(".cell.{name}");
        if !ctx.locals.contains_key(&cell_name) {
            ctx.locals.insert(cell_name.clone(), ir::cell_of(inner));
            ctx.locals_order
                .push((cell_name.clone(), ir::cell_of(inner)));
        }
        Ok(ir::Stmt::CellStore {
            cell: ir::Expr {
                ty: ir::cell_of(inner),
                kind: ir::ExprKind::Local(cell_name),
            },
            value: value_expr,
        })
    } else {
        if !ctx.locals.contains_key(name) {
            ctx.locals.insert(name.to_string(), target_ty);
            ctx.locals_order.push((name.to_string(), target_ty));
        }
        Ok(ir::Stmt::Assign {
            name: name.to_string(),
            value: value_expr,
        })
    }
}

/// If `rhs` is (or promotes to) a single concrete member of `target` union,
/// return that member for flow-sensitive re-refinement after assignment.
fn refined_member_after_assign(rhs: ir::Ty, target: ir::Ty) -> Option<ir::Ty> {
    if matches!(rhs, ir::Ty::Union(_)) {
        return None;
    }
    let members = ir::flatten_union_members(target);
    if members.contains(&rhs) {
        return Some(rhs);
    }
    // Numeric promotion into a union member (bool→int, int→float, …).
    for m in members {
        if m == ir::Ty::None {
            continue;
        }
        match (rhs, m) {
            (ir::Ty::Bool, ir::Ty::Int)
            | (ir::Ty::Bool, ir::Ty::Float)
            | (ir::Ty::Int, ir::Ty::Float) => return Some(m),
            // Subclass into a Class(base) union member → refine to the subclass.
            (ir::Ty::Class(src), ir::Ty::Class(dst)) if class_is_subclass(src, dst) => {
                return Some(rhs);
            }
            _ => {}
        }
    }
    None
}

fn lower_aug_assign(
    target: &ast::AssignTarget,
    op: ast::BinOp,
    value: &ast::Expr,
    span: Span,
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    match target {
        // desugar `x op= v` into `x = x op v`
        ast::AssignTarget::Name {
            name,
            span: name_span,
        } => {
            let (current_ty, is_global) = if let Some(&t) = ctx.locals.get(name) {
                (t, false)
            } else if ctx.binds_global(name) {
                match ctx.globals.get(name) {
                    Some(&t) => (t, true),
                    Option::None => {
                        return Err(err(format!("name '{name}' is not defined"), *name_span));
                    }
                }
            } else if ctx.globals.contains_key(name) {
                // Python raises UnboundLocalError at runtime; catch it here
                return Err(err(
                    format!(
                        "cannot modify global '{name}' here; add 'global {name}' \
                         at the top of the function"
                    ),
                    *name_span,
                ));
            } else {
                return Err(err(format!("name '{name}' is not defined"), *name_span));
            };
            let left = ir::Expr {
                ty: current_ty,
                kind: if is_global {
                    ir::ExprKind::GlobalLoad(ctx.own_global(name))
                } else {
                    ir::ExprKind::Local(name.clone())
                },
            };
            let right = lower_expr(value, ctx)?;
            // set |= other → in-place update (not a new set assign).
            if matches!(current_ty, ir::Ty::Set(_)) && op == ast::BinOp::BitOr {
                if right.ty != current_ty {
                    return Err(err(
                        format!(
                            "set |= requires the same set type on the right, found {}",
                            right.ty
                        ),
                        span,
                    ));
                }
                out.push(ir::Stmt::SetUpdate {
                    set: left,
                    other: right,
                });
                return Ok(());
            }
            let combined = lower_binary(op, left, right, span)?;
            let combined = coerce_assign(combined, current_ty, name, span)?;
            out.push(if is_global {
                ir::Stmt::GlobalAssign {
                    name: ctx.own_global(name),
                    value: combined,
                }
            } else {
                ir::Stmt::Assign {
                    name: name.clone(),
                    value: combined,
                }
            });
            Ok(())
        }
        // `xs[i] op= v`: evaluate base and index once via temps
        ast::AssignTarget::Index { base, index } => {
            let (list_ir, elem, index_ir) = lower_index_target(base, index, ctx)?;
            let list_ty = list_ir.ty;
            let idx_ty = index_ir.ty;
            let base_t = ctx.fresh_temp("aug.base", list_ty);
            let idx_t = ctx.fresh_temp("aug.idx", idx_ty);
            out.push(ir::Stmt::Assign {
                name: base_t.clone(),
                value: list_ir,
            });
            out.push(ir::Stmt::Assign {
                name: idx_t.clone(),
                value: index_ir,
            });
            let base_local = ir::Expr {
                ty: list_ty,
                kind: ir::ExprKind::Local(base_t),
            };
            let idx_local = ir::Expr {
                ty: idx_ty,
                kind: ir::ExprKind::Local(idx_t),
            };
            let current = ir::Expr {
                ty: elem,
                kind: ir::ExprKind::Index {
                    base: Box::new(base_local.clone()),
                    index: Box::new(idx_local.clone()),
                },
            };
            let right = lower_expr(value, ctx)?;
            let combined = lower_binary(op, current, right, span)?;
            let combined = coerce(combined, elem, span, "item assignment").map_err(|e| {
                Diagnostic::new(
                    Phase::Semantic,
                    format!("{}; an item's type cannot change", e.message),
                    e.span,
                )
            })?;
            out.push(ir::Stmt::IndexAssign {
                base: base_local,
                index: idx_local,
                value: combined,
            });
            Ok(())
        }
        ast::AssignTarget::Attr {
            base,
            attr,
            attr_span,
        } => {
            // desugar `obj.attr op= v` into `obj.attr = obj.attr op v`
            let base_ir = lower_expr(base, ctx)?;
            let class_id = match base_ir.ty {
                ir::Ty::Class(id) => id,
                other => {
                    return Err(err(
                        format!("cannot set attribute '{attr}' on '{other}'"),
                        *attr_span,
                    ));
                }
            };
            let (field_index, field_ty) = field_index(class_id, attr).ok_or_else(|| {
                err(
                    format!(
                        "'{}' object has no attribute '{attr}'",
                        class_info(class_id)
                            .map(|c| c.name)
                            .unwrap_or_else(|| format!("class#{class_id}"))
                    ),
                    *attr_span,
                )
            })?;
            let base_ty = base_ir.ty;
            let base_t = ctx.fresh_temp("aug.obj", base_ty);
            out.push(ir::Stmt::Assign {
                name: base_t.clone(),
                value: base_ir,
            });
            let base_local = ir::Expr {
                ty: base_ty,
                kind: ir::ExprKind::Local(base_t),
            };
            let current = ir::Expr {
                ty: field_ty,
                kind: ir::ExprKind::GetField {
                    object: Box::new(base_local.clone()),
                    class_id,
                    field_index,
                },
            };
            let right = lower_expr(value, ctx)?;
            let combined = lower_binary(op, current, right, span)?;
            let combined = coerce(combined, field_ty, span, "attribute assignment")?;
            out.push(ir::Stmt::SetField {
                object: base_local,
                class_id,
                field_index,
                value: combined,
            });
            Ok(())
        }
        ast::AssignTarget::Tuple(_) | ast::AssignTarget::Starred { .. } => Err(err(
            "augmented assignment to a tuple is not supported",
            span,
        )),
    }
}

// ---- for loops ----

fn assign_target_span(target: &ast::AssignTarget) -> Span {
    match target {
        ast::AssignTarget::Name { span, .. } => *span,
        ast::AssignTarget::Index { base, index } => base.span.to(index.span),
        ast::AssignTarget::Starred { span, .. } => *span,
        ast::AssignTarget::Attr {
            base, attr_span, ..
        } => base.span.to(*attr_span),
        ast::AssignTarget::Tuple(items) => {
            let first = items
                .first()
                .map(assign_target_span)
                .unwrap_or_else(|| Span::new(0, 0));
            let last = items.last().map(assign_target_span).unwrap_or(first);
            first.to(last)
        }
    }
}

/// Bind a for-loop / comprehension element into an assignment target.
fn bind_for_target(
    target: &ast::AssignTarget,
    value: ir::Expr,
    ctx: &mut FnCtx,
) -> SResult<Vec<ir::Stmt>> {
    let mut stmts = Vec::new();
    let span = assign_target_span(target);
    lower_assign_ir(target, None, value, span, ctx, &mut stmts)?;
    Ok(stmts)
}

fn lower_for(
    target: &ast::AssignTarget,
    iter: &ast::Expr,
    body: &[ast::Stmt],
    orelse: &[ast::Stmt],
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    // `for i in range(...)` — lazy, no list is materialized
    if let ast::ExprKind::Call { func, args, .. } = &iter.kind
        && func == "range"
        && !ctx.funcs().contains_key("range")
    {
        let plain = require_plain_args(args, "range", iter.span)?;
        let plain: Vec<ast::Expr> = plain.iter().map(|e| (*e).clone()).collect();
        return lower_for_range(target, &plain, iter.span, body, orelse, ctx, out);
    }

    // general case: list/string by index, or file via readline until ""
    let seq = lower_expr(iter, ctx)?;
    match seq.ty {
        ir::Ty::File => lower_for_file(target, seq, body, orelse, ctx, out),
        ir::Ty::List(_) | ir::Ty::Str | ir::Ty::Tuple(_) => {
            lower_for_indexed(target, seq, body, orelse, ctx, out)
        }
        ir::Ty::Dict { key, .. } => {
            // `for k in d` iterates keys (insertion order)
            let keys = ir::Expr {
                ty: ir::list_of(*key),
                kind: ir::ExprKind::DictKeys(Box::new(seq)),
            };
            lower_for_indexed(target, keys, body, orelse, ctx, out)
        }
        ir::Ty::Set(elem) => {
            let els = ir::Expr {
                ty: ir::list_of(*elem),
                kind: ir::ExprKind::SetToList(Box::new(seq)),
            };
            lower_for_indexed(target, els, body, orelse, ctx, out)
        }
        ir::Ty::Generator { yield_ty } => {
            lower_for_generator(target, seq, *yield_ty, body, orelse, ctx, out)
        }
        ir::Ty::Class(id) if resolve_method(id, "__iter__").is_some() => {
            lower_for_user_iter(target, seq, id, body, orelse, iter.span, ctx, out)
        }
        other => Err(err(format!("'{other}' object is not iterable"), iter.span)),
    }
}

/// `for x in obj:` when `obj` is a class with `__iter__` / `__next__`.
/// Desugars to: `it = obj.__iter__(); while True: try: x = it.__next__(); body
/// except StopIteration: break`.
fn lower_for_user_iter(
    target: &ast::AssignTarget,
    obj: ir::Expr,
    class_id: ir::ClassId,
    body: &[ast::Stmt],
    orelse: &[ast::Stmt],
    span: Span,
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    // it = obj.__iter__()
    let it_call = lower_instance_method_call(obj, class_id, "__iter__", span, &[], ctx)?;
    let it_ty = it_call.ty;
    let ir::Ty::Class(it_id) = it_ty else {
        return Err(err(
            format!("__iter__ must return a class instance, found {it_ty}"),
            span,
        ));
    };
    if resolve_method(it_id, "__next__").is_none() {
        return Err(err("iterator from __iter__ must define __next__", span));
    }
    let it_t = ctx.fresh_temp("for.it", it_ty);
    out.push(ir::Stmt::Assign {
        name: it_t.clone(),
        value: it_call,
    });
    let it_local = ir::Expr {
        ty: it_ty,
        kind: ir::ExprKind::Local(it_t),
    };

    // Infer yield type from __next__ return.
    let next_func = resolve_method(it_id, "__next__").unwrap();
    let next_sig = method_sig_lookup(&next_func)
        .ok_or_else(|| err("internal error: missing signature for __next__", span))?;
    let yield_ty = next_sig.ret;
    if yield_ty == ir::Ty::None {
        return Err(err("__next__ must return a non-None value type", span));
    }

    let more_t = ctx.fresh_temp("for.imore", ir::Ty::Bool);
    out.push(ir::Stmt::Assign {
        name: more_t.clone(),
        value: ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ConstBool(true),
        },
    });

    // next value temp (fresh_temp registers the local).
    let nxt_t = ctx.fresh_temp("for.inext", yield_ty);
    // Bind target to establish type before body.
    let entry_ref = ctx.type_refinements.clone();
    let dummy = ir::Expr {
        ty: yield_ty,
        kind: ir::ExprKind::Local(nxt_t.clone()),
    };
    let bind = bind_for_target(target, dummy, ctx)?;
    ctx.loop_depth += 1;
    let user_body = lower_nested_block(body, ctx)?;
    ctx.loop_depth -= 1;
    restore_refinements_after_for(ctx, entry_ref, target, body, orelse);

    // try: nxt = it.__next__(); bind; body
    // except StopIteration: more = False
    let next_call = lower_instance_method_call(it_local, it_id, "__next__", span, &[], ctx)?;
    let try_body = {
        let mut b = vec![ir::Stmt::Assign {
            name: nxt_t.clone(),
            value: next_call,
        }];
        b.extend(bind);
        b.extend(user_body);
        b
    };
    let handler = (
        Some(vec![ir::ExcType::StopIteration]),
        None,
        vec![ir::Stmt::Assign {
            name: more_t.clone(),
            value: ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::ConstBool(false),
            },
        }],
    );
    let loop_body = vec![ir::Stmt::Try {
        body: try_body,
        handlers: vec![handler],
        orelse: vec![],
        finally: vec![],
    }];
    let more_local = ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Local(more_t),
    };
    push_loop_with_else(more_local, loop_body, vec![], orelse, ctx, out)?;
    Ok(())
}

/// `for x in gen:` via GeneratorNext → optional yield|None until None.
fn lower_for_generator(
    target: &ast::AssignTarget,
    gen_expr: ir::Expr,
    yield_ty: ir::Ty,
    body: &[ast::Stmt],
    orelse: &[ast::Stmt],
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    let gen_t = ctx.fresh_temp("for.gen", gen_expr.ty);
    out.push(ir::Stmt::Assign {
        name: gen_t.clone(),
        value: gen_expr,
    });
    let gen_local = ir::Expr {
        ty: ir::generator_of(yield_ty),
        kind: ir::ExprKind::Local(gen_t.clone()),
    };
    let more_t = ctx.fresh_temp("for.gmore", ir::Ty::Bool);
    out.push(ir::Stmt::Assign {
        name: more_t.clone(),
        value: ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ConstBool(true),
        },
    });
    let opt_ty = ir::optional_of(yield_ty);
    let nxt_t = ctx.fresh_temp("for.gnext", opt_ty);
    // loop body: next = GeneratorNext(g); if next is None: more=False else: bind; user
    let next_e = ir::Expr {
        ty: opt_ty,
        kind: ir::ExprKind::GeneratorNext {
            generator: Box::new(gen_local),
            send: Box::new(const_none()),
        },
    };
    let is_none = ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::IsNone {
            value: Box::new(ir::Expr {
                ty: opt_ty,
                kind: ir::ExprKind::Local(nxt_t.clone()),
            }),
            not: false,
        },
    };
    let extracted = ir::Expr {
        ty: yield_ty,
        kind: ir::ExprKind::FromUnion {
            value: Box::new(ir::Expr {
                ty: opt_ty,
                kind: ir::ExprKind::Local(nxt_t.clone()),
            }),
        },
    };
    let entry_ref = ctx.type_refinements.clone();
    let bind = bind_for_target(target, extracted, ctx)?;
    ctx.loop_depth += 1;
    let user_body = lower_nested_block(body, ctx)?;
    ctx.loop_depth -= 1;
    restore_refinements_after_for(ctx, entry_ref, target, body, orelse);
    let body_stmts = vec![
        ir::Stmt::Assign {
            name: nxt_t.clone(),
            value: next_e,
        },
        ir::Stmt::If {
            branches: vec![(
                is_none,
                vec![ir::Stmt::Assign {
                    name: more_t.clone(),
                    value: ir::Expr {
                        ty: ir::Ty::Bool,
                        kind: ir::ExprKind::ConstBool(false),
                    },
                }],
            )],
            orelse: {
                let mut b = bind;
                b.extend(user_body);
                b
            },
        },
    ];
    let cond = ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Local(more_t),
    };
    push_loop_with_else(cond, body_stmts, vec![], orelse, ctx, out)?;
    clear_orelse_assigns(ctx, orelse);
    Ok(())
}

/// After lowering a `for` body, restore entry peels and drop peels for names
/// the body (or loop target) may rebind. Zero-trip loops must not keep
/// body-only peels (e.g. `for i in range(0): x = 5` must not leave `x` as int).
fn restore_refinements_after_for(
    ctx: &mut FnCtx,
    entry: HashMap<String, ir::Ty>,
    target: &ast::AssignTarget,
    body: &[ast::Stmt],
    orelse: &[ast::Stmt],
) {
    ctx.type_refinements = entry;
    for name in assigned_names_in_stmts(body) {
        ctx.type_refinements.remove(&name);
    }
    let mut names = HashSet::new();
    assigned_names_in_target(target, &mut names);
    for name in names {
        ctx.type_refinements.remove(&name);
    }
    // `orelse` is lowered next under these peels; clear its assigns after.
    let _ = orelse;
}

fn clear_orelse_assigns(ctx: &mut FnCtx, orelse: &[ast::Stmt]) {
    for name in assigned_names_in_stmts(orelse) {
        ctx.type_refinements.remove(&name);
    }
}

/// Emit while + optional else (else runs only if no break).
fn push_loop_with_else(
    cond: ir::Expr,
    body: Vec<ir::Stmt>,
    step: Vec<ir::Stmt>,
    orelse: &[ast::Stmt],
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    if orelse.is_empty() {
        out.push(ir::Stmt::While { cond, body, step });
        return Ok(());
    }
    let else_body = lower_nested_block(orelse, ctx)?;
    // No `break` in this loop: else always runs after the loop ends (or if
    // the loop never runs). Emit else as straight-line so return-path analysis
    // sees its `return`s. The broke-flag form uses `if not broke: else` with
    // an empty orelse, which `block_returns` cannot treat as exhaustive.
    if !loop_breaks(&body) {
        out.push(ir::Stmt::While { cond, body, step });
        out.extend(else_body);
        return Ok(());
    }
    let broke = ctx.fresh_temp("broke", ir::Ty::Bool);
    out.push(ir::Stmt::Assign {
        name: broke.clone(),
        value: ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ConstBool(false),
        },
    });
    let body = rewrite_breaks_set_flag(body, &broke);
    out.push(ir::Stmt::While { cond, body, step });
    let not_broke = ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Unary {
            op: ir::UnOp::Not,
            operand: Box::new(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Local(broke),
            }),
        },
    };
    out.push(ir::Stmt::If {
        branches: vec![(not_broke, else_body)],
        orelse: vec![],
    });
    Ok(())
}

fn rewrite_breaks_set_flag(stmts: Vec<ir::Stmt>, broke: &str) -> Vec<ir::Stmt> {
    let mut out = Vec::with_capacity(stmts.len());
    for s in stmts {
        match s {
            ir::Stmt::Break => {
                out.push(ir::Stmt::Assign {
                    name: broke.to_string(),
                    value: ir::Expr {
                        ty: ir::Ty::Bool,
                        kind: ir::ExprKind::ConstBool(true),
                    },
                });
                out.push(ir::Stmt::Break);
            }
            ir::Stmt::If { branches, orelse } => {
                out.push(ir::Stmt::If {
                    branches: branches
                        .into_iter()
                        .map(|(c, b)| (c, rewrite_breaks_set_flag(b, broke)))
                        .collect(),
                    orelse: rewrite_breaks_set_flag(orelse, broke),
                });
            }
            ir::Stmt::Try {
                body,
                handlers,
                orelse,
                finally,
            } => {
                out.push(ir::Stmt::Try {
                    body: rewrite_breaks_set_flag(body, broke),
                    handlers: handlers
                        .into_iter()
                        .map(|(e, n, h)| (e, n, rewrite_breaks_set_flag(h, broke)))
                        .collect(),
                    orelse: rewrite_breaks_set_flag(orelse, broke),
                    finally: rewrite_breaks_set_flag(finally, broke),
                });
            }
            other => out.push(other),
        }
    }
    out
}

/// `for line in f:` — while more: line = readline; if not line: more=False else: bind; body
fn lower_for_file(
    target: &ast::AssignTarget,
    file: ir::Expr,
    body: &[ast::Stmt],
    orelse: &[ast::Stmt],
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    let tspan = assign_target_span(target);
    let file_t = ctx.fresh_temp("for.file", ir::Ty::File);
    out.push(ir::Stmt::Assign {
        name: file_t.clone(),
        value: file,
    });
    let file_local = ir::Expr {
        ty: ir::Ty::File,
        kind: ir::ExprKind::Local(file_t),
    };

    // avoid Break for EOF so for-else still runs on clean exhaustion
    let more_t = ctx.fresh_temp("for.more", ir::Ty::Bool);
    out.push(ir::Stmt::Assign {
        name: more_t.clone(),
        value: ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ConstBool(true),
        },
    });
    let more_local = ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Local(more_t.clone()),
    };

    // Read into a temp so EOF (empty string) is checked before unpacking.
    let line_t = ctx.fresh_temp("for.line", ir::Ty::Str);
    let line_read = ir::Expr {
        ty: ir::Ty::Str,
        kind: ir::ExprKind::FileCall {
            func: ir::FileFn::ReadLine,
            args: vec![file_local],
        },
    };
    let line_local = ir::Expr {
        ty: ir::Ty::Str,
        kind: ir::ExprKind::Local(line_t.clone()),
    };
    let entry_ref = ctx.type_refinements.clone();
    let bind = bind_for_target(target, line_local.clone(), ctx)?;

    let truthy = to_bool(line_local, tspan, ctx)?;
    let not_line = ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Unary {
            op: ir::UnOp::Not,
            operand: Box::new(truthy.clone()),
        },
    };

    ctx.loop_depth += 1;
    let user_body = lower_nested_block(body, ctx)?;
    ctx.loop_depth -= 1;
    restore_refinements_after_for(ctx, entry_ref, target, body, orelse);

    let stop = ir::Stmt::Assign {
        name: more_t,
        value: ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ConstBool(false),
        },
    };
    let mut run = bind;
    run.extend(user_body);
    let loop_body = vec![
        ir::Stmt::Assign {
            name: line_t,
            value: line_read,
        },
        ir::Stmt::If {
            branches: vec![(not_line, vec![stop]), (truthy, run)],
            orelse: vec![],
        },
    ];

    push_loop_with_else(more_local, loop_body, vec![], orelse, ctx, out)?;
    clear_orelse_assigns(ctx, orelse);
    Ok(())
}

/// `for x in xs` / `for c in s` — index from 0 to len (re-read each iteration).
fn lower_for_indexed(
    target: &ast::AssignTarget,
    seq: ir::Expr,
    body: &[ast::Stmt],
    orelse: &[ast::Stmt],
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    let tspan = assign_target_span(target);
    let elem_ty = match seq.ty {
        ir::Ty::List(e) => *e,
        ir::Ty::Str => ir::Ty::Str,
        ir::Ty::Tuple(elems) => {
            if elems.is_empty() {
                // loop body never runs; bind as int placeholder — use a dummy
                ir::Ty::Int
            } else {
                let t0 = elems[0];
                if elems.iter().all(|e| *e == t0) {
                    t0
                } else {
                    return Err(err(
                        "iterating a heterogeneous tuple is not supported yet; \
                         unpack or index with constants",
                        tspan,
                    ));
                }
            }
        }
        other => {
            return Err(err(
                format!("internal error: lower_for_indexed on {other}"),
                tspan,
            ));
        }
    };

    let seq_ty = seq.ty;
    let seq_t = ctx.fresh_temp("for.seq", seq_ty);
    let idx_t = ctx.fresh_temp("for.idx", ir::Ty::Int);
    out.push(ir::Stmt::Assign {
        name: seq_t.clone(),
        value: seq,
    });
    out.push(ir::Stmt::Assign {
        name: idx_t.clone(),
        value: int_const(0),
    });

    let seq_local = ir::Expr {
        ty: seq_ty,
        kind: ir::ExprKind::Local(seq_t),
    };
    let idx_local = ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::Local(idx_t.clone()),
    };

    // cond: idx < len(seq) — length is re-read every iteration, so
    // appending inside the loop extends it (like Python)
    let cond = ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Binary {
            op: ir::BinOp::Lt,
            left: Box::new(idx_local.clone()),
            right: Box::new(ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Len(Box::new(seq_local.clone())),
            }),
        },
    };

    // target = seq[idx] as the first statement(s) of the body
    let element = ir::Expr {
        ty: elem_ty,
        kind: ir::ExprKind::Index {
            base: Box::new(seq_local),
            index: Box::new(idx_local.clone()),
        },
    };
    let entry_ref = ctx.type_refinements.clone();
    let bind = bind_for_target(target, element, ctx)?;

    ctx.loop_depth += 1;
    let user_body = lower_nested_block(body, ctx);
    ctx.loop_depth -= 1;
    restore_refinements_after_for(ctx, entry_ref, target, body, orelse);
    let mut loop_body = bind;
    loop_body.extend(user_body?);

    let step = vec![ir::Stmt::Assign {
        name: idx_t,
        value: ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Binary {
                op: ir::BinOp::Add,
                left: Box::new(idx_local),
                right: Box::new(int_const(1)),
            },
        },
    }];

    push_loop_with_else(cond, loop_body, step, orelse, ctx, out)?;
    clear_orelse_assigns(ctx, orelse);
    Ok(())
}

fn lower_for_range(
    target: &ast::AssignTarget,
    args: &[ast::Expr],
    range_span: Span,
    body: &[ast::Stmt],
    orelse: &[ast::Stmt],
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    if args.is_empty() || args.len() > 3 {
        return Err(err(
            format!("range() takes 1 to 3 arguments ({} given)", args.len()),
            range_span,
        ));
    }

    let mut lowered: Vec<ir::Expr> = Vec::new();
    for a in args {
        let v = lower_expr(a, ctx)?;
        lowered.push(coerce(v, ir::Ty::Int, a.span, "range() argument")?);
    }
    let (start, stop, step) = match lowered.len() {
        1 => (int_const(0), lowered.remove(0), int_const(1)),
        2 => {
            let stop = lowered.remove(1);
            (lowered.remove(0), stop, int_const(1))
        }
        _ => {
            let step = lowered.remove(2);
            let stop = lowered.remove(1);
            (lowered.remove(0), stop, step)
        }
    };

    // Simple name targets: the loop variable must be an int.
    if let ast::AssignTarget::Name { name, span } = target {
        let existing_var_ty = if ctx.binds_global(name) {
            ctx.globals.get(name).copied()
        } else {
            ctx.locals.get(name).copied()
        };
        if let Some(existing) = existing_var_ty
            && existing != ir::Ty::Int
        {
            return Err(err(
                format!(
                    "loop variable '{name}' already has type {existing}, but \
                     range() yields int"
                ),
                *span,
            ));
        }
    }

    // Python semantics: iterate a hidden counter and assign the user
    // target at the top of each iteration. After exhaustion the variable
    // holds the last *yielded* value (not one past), an empty range never
    // assigns it, and mutating it inside the body cannot derail the loop.
    let stop_t = ctx.fresh_temp("range.stop", ir::Ty::Int);
    out.push(ir::Stmt::Assign {
        name: stop_t.clone(),
        value: stop,
    });
    let stop_local = ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::Local(stop_t),
    };
    let it_t = ctx.fresh_temp("range.it", ir::Ty::Int);
    out.push(ir::Stmt::Assign {
        name: it_t.clone(),
        value: start,
    });
    let it_local = ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::Local(it_t.clone()),
    };

    // constant steps get a simple condition; dynamic steps need a zero
    // check and a direction-aware condition
    let (cond, step_value) = match step.kind {
        ir::ExprKind::ConstInt(0) => {
            return Err(err("range() arg 3 must not be zero", range_span));
        }
        ir::ExprKind::ConstInt(k) => {
            let op = if k > 0 { ir::BinOp::Lt } else { ir::BinOp::Gt };
            let cond = ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Binary {
                    op,
                    left: Box::new(it_local.clone()),
                    right: Box::new(stop_local),
                },
            };
            (cond, int_const(k))
        }
        _ => {
            let step_t = ctx.fresh_temp("range.step", ir::Ty::Int);
            out.push(ir::Stmt::Assign {
                name: step_t.clone(),
                value: step,
            });
            let step_local = ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Local(step_t),
            };
            out.push(ir::Stmt::If {
                branches: vec![(
                    int_cmp(ir::BinOp::Eq, step_local.clone(), int_const(0)),
                    vec![ir::Stmt::Die(
                        "ValueError: range() arg 3 must not be zero".to_string(),
                    )],
                )],
                orelse: vec![],
            });
            // (step > 0 and it < stop) or (step < 0 and it > stop)
            let up = bool_and(
                int_cmp(ir::BinOp::Gt, step_local.clone(), int_const(0)),
                int_cmp(ir::BinOp::Lt, it_local.clone(), stop_local.clone()),
            );
            let down = bool_and(
                int_cmp(ir::BinOp::Lt, step_local.clone(), int_const(0)),
                int_cmp(ir::BinOp::Gt, it_local.clone(), stop_local),
            );
            let cond = ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Or,
                    left: Box::new(up),
                    right: Box::new(down),
                },
            };
            (cond, step_local)
        }
    };

    // target = .it as the first statement(s) of the body
    let entry_ref = ctx.type_refinements.clone();
    let bind = bind_for_target(target, it_local.clone(), ctx)?;

    ctx.loop_depth += 1;
    let user_body = lower_nested_block(body, ctx);
    ctx.loop_depth -= 1;
    restore_refinements_after_for(ctx, entry_ref, target, body, orelse);
    let mut loop_body = bind;
    loop_body.extend(user_body?);

    let step_stmt = ir::Stmt::Assign {
        name: it_t,
        value: ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Binary {
                op: ir::BinOp::Add,
                left: Box::new(it_local),
                right: Box::new(step_value),
            },
        },
    };

    push_loop_with_else(cond, loop_body, vec![step_stmt], orelse, ctx, out)?;
    clear_orelse_assigns(ctx, orelse);
    Ok(())
}

fn int_const(v: i64) -> ir::Expr {
    ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::ConstInt(v),
    }
}

fn int_cmp(op: ir::BinOp, l: ir::Expr, r: ir::Expr) -> ir::Expr {
    ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Binary {
            op,
            left: Box::new(l),
            right: Box::new(r),
        },
    }
}

fn bool_and(l: ir::Expr, r: ir::Expr) -> ir::Expr {
    ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Binary {
            op: ir::BinOp::And,
            left: Box::new(l),
            right: Box::new(r),
        },
    }
}

/// Coerce `value` for assignment into a variable of type `target`.
fn coerce_assign(value: ir::Expr, target: ir::Ty, name: &str, span: Span) -> SResult<ir::Expr> {
    coerce(value, target, span, &format!("assignment to '{name}'")).map_err(|e| {
        Diagnostic::new(
            Phase::Semantic,
            format!(
                "{}; variable '{name}' has storage type {target} (join of its assignments \
                 and annotation)",
                e.message
            ),
            e.span,
        )
    })
}

/// Wrap `value` into a union type (identity if already that union).
fn to_union(value: ir::Expr, union: ir::Ty) -> ir::Expr {
    if value.ty == union {
        return value;
    }
    ir::Expr {
        ty: union,
        kind: ir::ExprKind::ToUnion {
            value: Box::new(value),
        },
    }
}

/// Whether every member of `src` appears in `dst` (both unions or concrete).
fn union_is_subset(src: ir::Ty, dst: ir::Ty) -> bool {
    let src_ms = ir::flatten_union_members(src);
    let dst_ms = ir::flatten_union_members(dst);
    src_ms.iter().all(|m| dst_ms.iter().any(|d| d == m))
}

/// Whether `ty` can be boxed into [`ir::Ty::Any`] (has a print-tag encoding).
fn can_box_as_any(ty: ir::Ty) -> bool {
    match ty {
        ir::Ty::Int
        | ir::Ty::Float
        | ir::Ty::Bool
        | ir::Ty::Str
        | ir::Ty::None
        | ir::Ty::List(_)
        | ir::Ty::Tuple(_)
        | ir::Ty::Dict { .. }
        | ir::Ty::Set(_)
        | ir::Ty::Closure { .. }
        | ir::Ty::BoundMethod { .. }
        | ir::Ty::Generator { .. }
        | ir::Ty::Exception
        | ir::Ty::Class(_)
        | ir::Ty::Union(_)
        | ir::Ty::Any => true,
        ir::Ty::File | ir::Ty::Cell(_) => false,
    }
}

/// Insert implicit promotion casts (`bool → int → float`), union wraps, or fail.
fn coerce(value: ir::Expr, target: ir::Ty, span: Span, what: &str) -> SResult<ir::Expr> {
    if value.ty == target {
        return Ok(value);
    }
    // Concrete → Any (dynamic box).
    if target == ir::Ty::Any {
        if !can_box_as_any(value.ty) {
            return Err(err(
                format!("type mismatch in {what}: cannot store {} in Any", value.ty),
                span,
            ));
        }
        return Ok(ir::Expr {
            ty: ir::Ty::Any,
            kind: ir::ExprKind::ToAny {
                value: Box::new(value),
            },
        });
    }
    // Any → concrete (runtime tag check).
    if value.ty == ir::Ty::Any {
        if !can_box_as_any(target) {
            return Err(err(
                format!(
                    "type mismatch in {what}: cannot extract {} from Any",
                    target
                ),
                span,
            ));
        }
        // Nested Any is identity (handled by equality above).
        return Ok(ir::Expr {
            ty: target,
            kind: ir::ExprKind::FromAny {
                value: Box::new(value),
            },
        });
    }
    // Concrete numeric promotions
    match (value.ty, target) {
        (ir::Ty::Bool, ir::Ty::Int) => {
            return Ok(ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::BoolToInt(Box::new(value)),
            });
        }
        (ir::Ty::Int, ir::Ty::Float) => {
            return Ok(ir::Expr {
                ty: ir::Ty::Float,
                kind: ir::ExprKind::IntToFloat(Box::new(value)),
            });
        }
        (ir::Ty::Bool, ir::Ty::Float) => {
            let as_int = ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::BoolToInt(Box::new(value)),
            };
            return Ok(ir::Expr {
                ty: ir::Ty::Float,
                kind: ir::ExprKind::IntToFloat(Box::new(as_int)),
            });
        }
        _ => {}
    }

    // Target is a union: wrap a member or re-target a sub-union.
    if matches!(target, ir::Ty::Union(_)) {
        // Sub-union ⊆ target
        if matches!(value.ty, ir::Ty::Union(_)) && union_is_subset(value.ty, target) {
            return Ok(to_union(value, target));
        }
        // Exact member
        if ir::flatten_union_members(target).contains(&value.ty) {
            return Ok(to_union(value, target));
        }
        // Promote into a numeric member (e.g. bool → int|None) or subclass →
        // base class member (Dog into Animal|int).
        for m in ir::flatten_union_members(target) {
            if m == value.ty {
                return Ok(to_union(value, target));
            }
            // try numeric promotion into this member only
            if let Ok(promoted) = coerce_numeric_into(value.clone(), m) {
                return Ok(to_union(promoted, target));
            }
            if let (ir::Ty::Class(src), ir::Ty::Class(dst)) = (value.ty, m)
                && class_is_subclass(src, dst)
            {
                let as_base = ir::Expr {
                    ty: m,
                    kind: value.kind.clone(),
                };
                return Ok(to_union(as_base, target));
            }
        }
        return Err(err(
            format!(
                "type mismatch in {what}: expected {target}, found {}",
                value.ty
            ),
            span,
        ));
    }

    // Value is a union, target is a concrete member — reject (no runtime unwrap).
    if matches!(value.ty, ir::Ty::Union(_)) {
        return Err(err(
            format!(
                "cannot use {} as {target} in {what}; use 'is None' check or provide a \
                 default with 'or'",
                value.ty
            ),
            span,
        ));
    }

    // Closures with matching params/ret/capture env: retype for homogeneous
    // containers (call uses the object's code pointer; captures from env).
    if let (
        ir::Ty::Closure {
            params: p1,
            ret: r1,
            capture_tys: c1,
            ..
        },
        ir::Ty::Closure {
            params: p2,
            ret: r2,
            capture_tys: c2,
            ..
        },
    ) = (value.ty, target)
        && p1 == p2
        && r1 == r2
        && c1 == c2
    {
        return Ok(ir::Expr {
            ty: target,
            kind: value.kind,
        });
    }

    // Subclass instance → base class type (layout prefix; same pointer).
    if let (ir::Ty::Class(src), ir::Ty::Class(dst)) = (value.ty, target)
        && class_is_subclass(src, dst)
    {
        return Ok(ir::Expr {
            ty: target,
            kind: value.kind,
        });
    }

    // None → only ok for None target (handled by equality) or unions (above)
    Err(err(
        format!(
            "type mismatch in {what}: expected {target}, found {}",
            value.ty
        ),
        span,
    ))
}

/// Drop the leading `self` parameter from a method signature for call matching
/// (self is supplied via `extra_leading` in `lower_call_with_sig`).
fn method_user_sig(sig: &FuncSig) -> FuncSig {
    let mut s = sig.clone();
    if !s.params.is_empty() {
        s.params = s.params[1..].to_vec();
    }
    s
}

/// True when `e` is a zero-arg `super()` call expression.
fn is_zero_arg_super(e: &ast::Expr) -> bool {
    matches!(
        &e.kind,
        ast::ExprKind::Call {
            func,
            args,
            keywords,
            kwargs,
            ..
        } if func == "super" && args.is_empty() && keywords.is_empty() && kwargs.is_none()
    )
}

/// Lower `super().method(args)` to a **non-virtual** call of the parent (or
/// further ancestor) implementation, with the current method's `self`.
fn lower_super_method_call(
    method: &str,
    method_span: Span,
    args: &[ast::Expr],
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let class_id = ctx
        .current_class
        .ok_or_else(|| err("super() outside of a method is not supported", method_span))?;
    let parent_id = class_info(class_id).and_then(|i| i.parent).ok_or_else(|| {
        err(
            "super() requires a base class (this class has no parent)",
            method_span,
        )
    })?;
    let self_name = ctx
        .self_param
        .clone()
        .ok_or_else(|| err("super() outside of a method is not supported", method_span))?;

    let direct = resolve_method(parent_id, method).ok_or_else(|| {
        err(
            format!(
                "parent of '{}' has no method '{method}'",
                class_info(class_id)
                    .map(|c| c.name)
                    .unwrap_or_else(|| format!("class#{class_id}"))
            ),
            method_span,
        )
    })?;
    let sig = ctx
        .mctx
        .funcs
        .get(&direct)
        .cloned()
        .or_else(|| method_sig_lookup(&direct))
        .or_else(|| {
            for data in ctx.mctx.mods.values() {
                if let Some(s) = data.funcs.get(&direct) {
                    return Some(s.clone());
                }
            }
            None
        })
        .ok_or_else(|| {
            err(
                format!("internal error: missing signature for method '{method}'"),
                method_span,
            )
        })?;
    let user_sig = method_user_sig(&sig);

    // Load `self` (child instance) and coerce to the parent method's self type.
    let self_ir = ir::Expr {
        ty: ir::Ty::Class(class_id),
        kind: ir::ExprKind::Local(self_name),
    };
    let self_ty = sig
        .params
        .first()
        .map(|p| p.ty)
        .unwrap_or(ir::Ty::Class(parent_id));
    let self_ir = coerce(self_ir, self_ty, method_span, "super() self")?;

    let pos: Vec<ast::PosArg> = args.iter().map(|e| ast::PosArg::Pos(e.clone())).collect();
    // Non-virtual: always the parent implementation (not the child's override).
    lower_call_with_sig(
        method,
        direct,
        &user_sig,
        &pos,
        &[],
        None,
        method_span,
        ctx,
        &[self_ir],
    )
}

/// `ClassName(args)` → allocate instance and call `__init__`.
fn lower_class_construct(
    class_id: ir::ClassId,
    class_name: &str,
    args: &[ast::PosArg],
    keywords: &[ast::Keyword],
    kwargs: Option<&ast::Expr>,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    // Find most specific __init__ (walk parent chain).
    let init_func = resolve_method(class_id, "__init__");
    let obj_ty = ir::Ty::Class(class_id);
    let obj_tmp = ctx.fresh_temp("obj", obj_ty);
    let alloc = ir::Expr {
        ty: obj_ty,
        kind: ir::ExprKind::NewObject { class_id },
    };
    let mut stmts = vec![ir::Stmt::Assign {
        name: obj_tmp.clone(),
        value: alloc,
    }];
    let self_expr = ir::Expr {
        ty: obj_ty,
        kind: ir::ExprKind::Local(obj_tmp.clone()),
    };
    if let Some(init_name) = init_func {
        let sig = ctx
            .mctx
            .funcs
            .get(&init_name)
            .cloned()
            .or_else(|| method_sig_lookup(&init_name))
            .or_else(|| {
                for data in ctx.mctx.mods.values() {
                    if let Some(s) = data.funcs.get(&init_name) {
                        return Some(s.clone());
                    }
                }
                None
            })
            .ok_or_else(|| {
                err(
                    format!("cannot construct '{class_name}': __init__ signature not available"),
                    span,
                )
            })?;
        // FuncSig includes `self`; extra_leading prepends captures/self for the
        // IR call while the sig for argument matching is the remaining params.
        let user_sig = method_user_sig(&sig);
        let call = lower_call_with_sig(
            class_name, // display as ClassName(...) not __init__
            init_name,
            &user_sig,
            args,
            keywords,
            kwargs,
            span,
            ctx,
            &[self_expr],
        )?;
        stmts.push(ir::Stmt::ExprStmt(call));
    } else if !args.is_empty() || !keywords.is_empty() || kwargs.is_some() {
        return Err(err(
            format!("'{class_name}' takes no arguments (no __init__)"),
            span,
        ));
    }
    Ok(ir::Expr {
        ty: obj_ty,
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(ir::Expr {
                ty: obj_ty,
                kind: ir::ExprKind::Local(obj_tmp),
            }),
        },
    })
}

/// `obj.method(args)` for a user class instance.
fn lower_instance_method_call(
    base_ir: ir::Expr,
    class_id: ir::ClassId,
    method: &str,
    method_span: Span,
    args: &[ast::Expr],
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if method == "__init__" {
        return Err(err(
            "calling __init__ directly is not supported yet; construct with Class(...)",
            method_span,
        ));
    }
    let direct = resolve_method(class_id, method).ok_or_else(|| {
        err(
            format!(
                "'{}' object has no method '{method}'",
                class_info(class_id)
                    .map(|c| c.name)
                    .unwrap_or_else(|| format!("class#{class_id}"))
            ),
            method_span,
        )
    })?;
    let kind = method_kind_lookup(&direct);

    // Signature from the static method (self + user params); cross-module ok.
    let sig = ctx
        .mctx
        .funcs
        .get(&direct)
        .cloned()
        .or_else(|| method_sig_lookup(&direct))
        .or_else(|| {
            for data in ctx.mctx.mods.values() {
                if let Some(s) = data.funcs.get(&direct) {
                    return Some(s.clone());
                }
            }
            None
        })
        .ok_or_else(|| {
            err(
                format!("internal error: missing signature for method '{method}'"),
                method_span,
            )
        })?;

    let pos: Vec<ast::PosArg> = args.iter().map(|e| ast::PosArg::Pos(e.clone())).collect();

    // @staticmethod: no self; ignore instance (CPython still allows instance call).
    if kind == MethodKind::Static {
        return lower_call_with_sig(method, direct, &sig, &pos, &[], None, method_span, ctx, &[]);
    }

    // @classmethod on instance: pass the instance's class (static type for now).
    if kind == MethodKind::Class {
        let cls_token = ir::Expr {
            ty: ir::Ty::Class(class_id),
            // Reuse instance pointer as cls marker (construct uses class_id only).
            kind: base_ir.kind.clone(),
        };
        let user_sig = method_user_sig(&sig);
        return lower_call_with_sig(
            method,
            direct,
            &user_sig,
            &pos,
            &[],
            None,
            method_span,
            ctx,
            &[cls_token],
        );
    }

    // Virtual dispatch when subclasses may override.
    let subs = subclasses_of(class_id);
    let mut candidates: Vec<(ir::ClassId, String)> = Vec::new();
    let mut unique_funcs: HashSet<String> = HashSet::new();
    for sid in &subs {
        if let Some(func) = resolve_method(*sid, method) {
            unique_funcs.insert(func.clone());
            candidates.push((*sid, func));
        }
    }
    let virtual_dispatch = unique_funcs.len() > 1;

    // Match user args against params after `self`; prepend self via extra_leading.
    let user_sig = method_user_sig(&sig);

    let lowered = lower_call_with_sig(
        method,
        direct.clone(),
        &user_sig,
        &pos,
        &[],
        None,
        method_span,
        ctx,
        &[base_ir],
    )?;
    if !virtual_dispatch {
        return Ok(lowered);
    }
    // Replace static Call with virtual CallMethod.
    if let ir::ExprKind::Call {
        args: call_args, ..
    } = lowered.kind
    {
        return Ok(ir::Expr {
            ty: lowered.ty,
            kind: ir::ExprKind::CallMethod {
                direct_func: direct,
                candidates,
                args: call_args,
                virtual_dispatch: true,
            },
        });
    }
    Ok(lowered)
}

/// `ClassName.method(...)` for staticmethod / classmethod.
fn lower_class_name_method_call(
    class_id: ir::ClassId,
    method: &str,
    method_span: Span,
    args: &[ast::Expr],
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let direct = resolve_method(class_id, method).ok_or_else(|| {
        err(
            format!(
                "type object '{}' has no attribute '{method}'",
                class_info(class_id)
                    .map(|c| c.name)
                    .unwrap_or_else(|| format!("class#{class_id}"))
            ),
            method_span,
        )
    })?;
    let kind = method_kind_lookup(&direct);
    let sig = method_sig_lookup(&direct)
        .or_else(|| ctx.mctx.funcs.get(&direct).cloned())
        .ok_or_else(|| {
            err(
                format!("internal error: missing signature for method '{method}'"),
                method_span,
            )
        })?;
    let pos: Vec<ast::PosArg> = args.iter().map(|e| ast::PosArg::Pos(e.clone())).collect();
    match kind {
        MethodKind::Static => {
            lower_call_with_sig(method, direct, &sig, &pos, &[], None, method_span, ctx, &[])
        }
        MethodKind::Class => {
            // Pass an uninitialized object as the cls token (only used for
            // type; body `cls(...)` is rewritten via classmethod_cls).
            let cls_marker = ir::Expr {
                ty: ir::Ty::Class(class_id),
                kind: ir::ExprKind::NewObject { class_id },
            };
            let user_sig = method_user_sig(&sig);
            lower_call_with_sig(
                method,
                direct,
                &user_sig,
                &pos,
                &[],
                None,
                method_span,
                ctx,
                &[cls_marker],
            )
        }
        MethodKind::Instance | MethodKind::Property => Err(err(
            format!("instance method '{method}' must be called on an instance, not the class"),
            method_span,
        )),
    }
}

/// Numeric-only promotion of `value` into concrete `target` (no unions).
fn coerce_numeric_into(value: ir::Expr, target: ir::Ty) -> SResult<ir::Expr> {
    if value.ty == target {
        return Ok(value);
    }
    match (value.ty, target) {
        (ir::Ty::Bool, ir::Ty::Int) => Ok(ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::BoolToInt(Box::new(value)),
        }),
        (ir::Ty::Int, ir::Ty::Float) => Ok(ir::Expr {
            ty: ir::Ty::Float,
            kind: ir::ExprKind::IntToFloat(Box::new(value)),
        }),
        (ir::Ty::Bool, ir::Ty::Float) => {
            let as_int = ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::BoolToInt(Box::new(value)),
            };
            Ok(ir::Expr {
                ty: ir::Ty::Float,
                kind: ir::ExprKind::IntToFloat(Box::new(as_int)),
            })
        }
        _ => Err(err("no numeric promotion", Span { start: 0, end: 0 })),
    }
}

/// Lower an expression used as a condition; applies truthiness.
fn lower_condition(cond: &ast::Expr, ctx: &mut FnCtx) -> SResult<ir::Expr> {
    let lowered = lower_expr(cond, ctx)?;
    to_bool(lowered, cond.span, ctx)
}

/// Truthiness without class dunders (match patterns / any/all).
fn to_bool_default(value: ir::Expr, span: Span) -> SResult<ir::Expr> {
    match value.ty {
        ir::Ty::Bool => Ok(value),
        ir::Ty::None
        | ir::Ty::Int
        | ir::Ty::Float
        | ir::Ty::Str
        | ir::Ty::List(_)
        | ir::Ty::Tuple(_)
        | ir::Ty::Dict { .. }
        | ir::Ty::Set(_)
        | ir::Ty::Union(_)
        | ir::Ty::Exception
        | ir::Ty::Class(_)
        | ir::Ty::Any => Ok(ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ToBool(Box::new(value)),
        }),
        other => Err(err(
            format!("a value of type {other} cannot be used as a condition"),
            span,
        )),
    }
}

fn to_bool(value: ir::Expr, span: Span, ctx: &mut FnCtx) -> SResult<ir::Expr> {
    match value.ty {
        ir::Ty::Bool => Ok(value),
        ir::Ty::Class(id) => {
            // Prefer __bool__, else __len__ != 0, else default True (non-null instance).
            if resolve_method(id, "__bool__").is_some() {
                let call = lower_instance_method_call(value, id, "__bool__", span, &[], ctx)?;
                if call.ty != ir::Ty::Bool {
                    return Err(err("__bool__ must return bool", span));
                }
                return Ok(call);
            }
            if resolve_method(id, "__len__").is_some() {
                let call = lower_instance_method_call(value, id, "__len__", span, &[], ctx)?;
                if call.ty != ir::Ty::Int {
                    return Err(err("__len__ must return int", span));
                }
                return Ok(ir::Expr {
                    ty: ir::Ty::Bool,
                    kind: ir::ExprKind::Binary {
                        op: ir::BinOp::Ne,
                        left: Box::new(call),
                        right: Box::new(int_const(0)),
                    },
                });
            }
            Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::ToBool(Box::new(value)),
            })
        }
        _ => to_bool_default(value, span),
    }
}

fn const_none() -> ir::Expr {
    ir::Expr {
        ty: ir::Ty::None,
        kind: ir::ExprKind::ConstNone,
    }
}

fn lower_expr(expr: &ast::Expr, ctx: &mut FnCtx) -> SResult<ir::Expr> {
    match &expr.kind {
        ast::ExprKind::Int(v) => Ok(int_const(*v)),
        ast::ExprKind::IntDigits(s) => Ok(ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::ConstIntDigits(s.clone()),
        }),
        ast::ExprKind::Float(v) => Ok(ir::Expr {
            ty: ir::Ty::Float,
            kind: ir::ExprKind::ConstFloat(*v),
        }),
        ast::ExprKind::Bool(v) => Ok(ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ConstBool(*v),
        }),
        ast::ExprKind::Str(s) => Ok(ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::ConstStr(s.clone()),
        }),
        ast::ExprKind::NoneLit => Ok(const_none()),
        ast::ExprKind::Name(name) => {
            if let Some((_, storage, ty)) = ctx
                .comp_renames
                .iter()
                .rev()
                .find(|(user, _, _)| user == name)
            {
                return Ok(ir::Expr {
                    ty: *ty,
                    kind: ir::ExprKind::Local(storage.clone()),
                });
            }
            // Prefer a local rebind over the nested def (CPython: assignment shadows).
            if ctx.locals.contains_key(name) || ctx.cell_locals.contains_key(name) {
                // fall through to local/cell load below
            } else if let Some(info) = ctx.nested_funcs.get(name).cloned() {
                // First-class nested function → MakeClosure
                return make_closure_expr(&info, expr.span, ctx);
            }
            // Type refinement (narrowing): FromUnion for concrete union members;
            // class base → subclass retype after isinstance; multi-member peels
            // keep storage (tags unsafe to rematerialize).
            if let Some(nty) = ctx.type_refinements.get(name).copied() {
                if let Some(inner) = ctx.cell_locals.get(name).copied() {
                    let cell = ir::Expr {
                        ty: ir::cell_of(inner),
                        kind: ir::ExprKind::Local(format!(".cell.{name}")),
                    };
                    let loaded = ir::Expr {
                        ty: inner,
                        kind: ir::ExprKind::CellLoad(Box::new(cell)),
                    };
                    return Ok(apply_type_refinement(loaded, inner, nty));
                }
                if let Some(ty) = ctx.locals.get(name) {
                    let base = ir::Expr {
                        ty: *ty,
                        kind: ir::ExprKind::Local(name.clone()),
                    };
                    return Ok(apply_type_refinement(base, *ty, nty));
                }
                // Module-level / global name under a refinement peel.
                if let Some(ty) = ctx.globals.get(name) {
                    let base = ir::Expr {
                        ty: *ty,
                        kind: ir::ExprKind::GlobalLoad(ctx.own_global(name)),
                    };
                    return Ok(apply_type_refinement(base, *ty, nty));
                }
            }
            if let Some(inner) = ctx.cell_locals.get(name).copied() {
                // Load through cell
                let cell = ir::Expr {
                    ty: ir::cell_of(inner),
                    kind: ir::ExprKind::Local(format!(".cell.{name}")),
                };
                return Ok(ir::Expr {
                    ty: inner,
                    kind: ir::ExprKind::CellLoad(Box::new(cell)),
                });
            }
            if let Some(ty) = ctx.locals.get(name) {
                Ok(ir::Expr {
                    ty: *ty,
                    kind: ir::ExprKind::Local(name.clone()),
                })
            } else if let Some(ty) = ctx.globals.get(name) {
                // module globals are readable from any function
                Ok(ir::Expr {
                    ty: *ty,
                    kind: ir::ExprKind::GlobalLoad(ctx.own_global(name)),
                })
            } else if let Some(binding) = ctx
                .local_imports
                .get(name)
                .or_else(|| ctx.mctx.imports.get(name))
            {
                // a name brought in by `from other import ...`
                match binding {
                    ImportBinding::Symbol { module, name: real } => {
                        if let Some(data) = ctx.mctx.mods.get(module) {
                            let (om, on) = data
                                .reexports
                                .get(real)
                                .cloned()
                                .unwrap_or_else(|| (module.clone(), real.clone()));
                            if let Some(ty) = data.globals.get(real).copied().or_else(|| {
                                ctx.mctx
                                    .mods
                                    .get(&om)
                                    .and_then(|d| d.globals.get(&on).copied())
                            }) {
                                return Ok(ir::Expr {
                                    ty,
                                    kind: ir::ExprKind::GlobalLoad(qual(&om, &on)),
                                });
                            }
                            return Err(err(
                                format!(
                                    "'{name}' is a function imported from '{module}'; \
                                     call it with parentheses: '{name}(...)'"
                                ),
                                expr.span,
                            ));
                        }
                        // Parent package not yet fully lowered (partial / deferred).
                        if is_strict_package_prefix(module, ctx.mctx.module) {
                            if let Some((om, on, ty)) = resolve_parent_value(
                                ctx,
                                module,
                                real,
                                /*for_module_body*/ ctx.is_entry,
                            ) {
                                return Ok(ir::Expr {
                                    ty,
                                    kind: ir::ExprKind::GlobalLoad(qual(&om, &on)),
                                });
                            }
                            return Err(err(
                                format!(
                                    "cannot import name '{real}' from partially initialized \
                                     package '{module}' (most likely due to a circular import)"
                                ),
                                expr.span,
                            ));
                        }
                        Err(err(
                            format!("module '{module}' has no attribute '{real}'"),
                            expr.span,
                        ))
                    }
                    ImportBinding::Module(_) | ImportBinding::Sys => Err(err(
                        format!("module '{name}' is not a value; use '{name}.<name>'"),
                        expr.span,
                    )),
                    ImportBinding::Class(_) => Err(err(
                        format!(
                            "'{name}' is a class; construct with '{name}(...)' or use it as a type"
                        ),
                        expr.span,
                    )),
                }
            } else if ctx.funcs().contains_key(name) {
                Err(err(
                    format!("functions can only be called; add parentheses: '{name}(...)'"),
                    expr.span,
                ))
            } else {
                Err(err(format!("name '{name}' is not defined"), expr.span))
            }
        }
        ast::ExprKind::ListLit(items) => lower_list_lit(items, None, expr.span, ctx),
        ast::ExprKind::TupleLit(items) => lower_tuple_lit(items, None, expr.span, ctx),
        ast::ExprKind::DictLit(items) => lower_dict_lit(items, None, expr.span, ctx),
        ast::ExprKind::SetLit(items) => lower_set_lit(items, None, expr.span, ctx),
        ast::ExprKind::ListComp { elem, generators } => {
            lower_list_comp(elem, generators, expr.span, ctx)
        }
        ast::ExprKind::DictComp {
            key,
            value,
            generators,
        } => lower_dict_comp(key, value, generators, expr.span, ctx),
        ast::ExprKind::SetComp { elem, generators } => {
            lower_set_comp(elem, generators, expr.span, ctx)
        }
        ast::ExprKind::Index { base, index } => {
            let base_ir = lower_expr(base, ctx)?;
            match base_ir.ty {
                ir::Ty::List(e) => {
                    let index_ir = lower_expr(index, ctx)?;
                    let index_ir = coerce(index_ir, ir::Ty::Int, index.span, "index")?;
                    Ok(ir::Expr {
                        ty: *e,
                        kind: ir::ExprKind::Index {
                            base: Box::new(base_ir),
                            index: Box::new(index_ir),
                        },
                    })
                }
                ir::Ty::Str => {
                    let index_ir = lower_expr(index, ctx)?;
                    let index_ir = coerce(index_ir, ir::Ty::Int, index.span, "index")?;
                    Ok(ir::Expr {
                        ty: ir::Ty::Str,
                        kind: ir::ExprKind::Index {
                            base: Box::new(base_ir),
                            index: Box::new(index_ir),
                        },
                    })
                }
                ir::Ty::Tuple(elems) => {
                    let index_ir = lower_expr(index, ctx)?;
                    let index_ir = coerce(index_ir, ir::Ty::Int, index.span, "index")?;
                    // Result type: if constant index, use that element type; else
                    // require homogeneous tuple or reject.
                    let result_ty = if let ir::ExprKind::ConstInt(i) = index_ir.kind {
                        let mut idx = i;
                        if idx < 0 {
                            idx += elems.len() as i64;
                        }
                        if idx >= 0 && (idx as usize) < elems.len() {
                            elems[idx as usize]
                        } else {
                            return Err(err("tuple index out of range", index.span));
                        }
                    } else if elems.is_empty() {
                        return Err(err(
                            "cannot index empty tuple with a dynamic index",
                            base.span,
                        ));
                    } else {
                        let t0 = elems[0];
                        if elems.iter().all(|e| *e == t0) {
                            t0
                        } else {
                            return Err(err(
                                "dynamic indexing into a heterogeneous tuple is not supported; \
                                 use a constant index",
                                index.span,
                            ));
                        }
                    };
                    Ok(ir::Expr {
                        ty: result_ty,
                        kind: ir::ExprKind::Index {
                            base: Box::new(base_ir),
                            index: Box::new(index_ir),
                        },
                    })
                }
                ir::Ty::Dict { key, value } => {
                    let key_ir = lower_expr(index, ctx)?;
                    let key_ir = coerce(key_ir, *key, index.span, "dict key")?;
                    Ok(ir::Expr {
                        ty: *value,
                        kind: ir::ExprKind::Index {
                            base: Box::new(base_ir),
                            index: Box::new(key_ir),
                        },
                    })
                }
                other => Err(err(
                    format!("'{other}' object is not subscriptable"),
                    base.span,
                )),
            }
        }
        ast::ExprKind::Attribute {
            base,
            attr,
            attr_span,
        } => {
            if let ast::ExprKind::Name(alias) = &base.kind {
                if ctx.sys_alias(alias) {
                    if attr == "argv" {
                        return Ok(ir::Expr {
                            ty: ir::list_of(ir::Ty::Str),
                            kind: ir::ExprKind::Argv,
                        });
                    }
                    return Err(err(
                        format!("'sys.{attr}' is not supported yet (only sys.argv)"),
                        *attr_span,
                    ));
                }
                if alias == "sys" && ctx.module_alias(alias).is_none() {
                    return Err(err(
                        "name 'sys' is not defined; add 'import sys' at the top \
                         of the program",
                        base.span,
                    ));
                }
            }
            // `module.global` / `pkg.mod.global` from an imported module path.
            // Last-binding wins: value/function re-exports are checked before
            // treating `attr` as a submodule (same as `from pkg import attr`).
            if let Some(real) = resolve_module_path(base, ctx) {
                if let Some(data) = ctx.mctx.mods.get(&real) {
                    if let Some(ty) = data.globals.get(attr) {
                        let (om, on) = data
                            .reexports
                            .get(attr)
                            .cloned()
                            .unwrap_or_else(|| (real.clone(), attr.clone()));
                        return Ok(ir::Expr {
                            ty: *ty,
                            kind: ir::ExprKind::GlobalLoad(qual(&om, &on)),
                        });
                    }
                    if data.funcs.contains_key(attr) {
                        return Err(err(
                            format!("'{real}.{attr}' is a function; call it: '{real}.{attr}(...)'"),
                            *attr_span,
                        ));
                    }
                    // Pure submodule: not a first-class value in this surface.
                    if ctx
                        .mctx
                        .submodules
                        .get(&real)
                        .is_some_and(|kids| kids.contains_key(attr))
                    {
                        return Err(err(
                            format!(
                                "module '{real}.{attr}' is not a value; use \
                                 '{real}.{attr}.<name>' or call a function on it"
                            ),
                            *attr_span,
                        ));
                    }
                    return Err(err(
                        format!("module '{real}' has no attribute '{attr}'"),
                        *attr_span,
                    ));
                }
                // Partial package init / deferred parent access: parent not
                // fully lowered yet. Module body of child: only names assigned
                // before the child-loading import. Function bodies: full
                // parent simple-assign surface (CPython deferred lookup).
                if is_strict_package_prefix(&real, ctx.mctx.module) {
                    if let Some((om, on, ty)) = resolve_parent_value(
                        ctx,
                        &real,
                        attr,
                        /*for_module_body*/ ctx.is_entry,
                    ) {
                        return Ok(ir::Expr {
                            ty,
                            kind: ir::ExprKind::GlobalLoad(qual(&om, &on)),
                        });
                    }
                    // Mid-init submodule attr: not a first-class value here.
                    if ctx
                        .mctx
                        .submodules
                        .get(&real)
                        .is_some_and(|kids| kids.contains_key(attr))
                        && !matches!(
                            ctx.mctx.last_exports.get(&real).and_then(|e| e.get(attr)),
                            Some(LastExport::Symbol)
                        )
                    {
                        return Err(err(
                            format!(
                                "module '{real}.{attr}' is not a value; use \
                                 '{real}.{attr}.<name>' or call a function on it"
                            ),
                            *attr_span,
                        ));
                    }
                    return Err(err(
                        format!(
                            "cannot import name '{attr}' from partially initialized \
                             package '{real}' (most likely due to a circular import)"
                        ),
                        *attr_span,
                    ));
                }
                if ctx
                    .mctx
                    .submodules
                    .get(&real)
                    .is_some_and(|kids| kids.contains_key(attr))
                {
                    return Err(err(
                        format!(
                            "module '{real}.{attr}' is not a value; use \
                             '{real}.{attr}.<name>' or call a function on it"
                        ),
                        *attr_span,
                    ));
                }
                return Err(err(
                    format!("module '{real}' has no attribute '{attr}'"),
                    *attr_span,
                ));
            }
            // Instance field load: `obj.x` (or common/exclusive field on a class union).
            let base_ir = lower_expr(base, ctx)?;
            if let ir::Ty::Class(id) = base_ir.ty {
                // isinstance multi-peel keeps Class ABI; field may only exist
                // on refined subclasses (or on the shared base layout).
                let refine_ty = match &base.kind {
                    ast::ExprKind::Name(n) => ctx.type_refinements.get(n).copied(),
                    _ => None,
                };
                if let Some(rty) = refine_ty
                    && let Some((class_id, field_index, field_ty)) = common_class_field(rty, attr)
                {
                    return Ok(ir::Expr {
                        ty: field_ty,
                        kind: ir::ExprKind::GetField {
                            object: Box::new(base_ir),
                            class_id,
                            field_index,
                        },
                    });
                }
                // Exclusive field after multi-class isinstance peel: runtime type_id switch.
                if let Some(rty) = refine_ty
                    && let Some((candidates, field_ty)) = exclusive_class_field(rty, attr)
                {
                    return Ok(ir::Expr {
                        ty: field_ty,
                        kind: ir::ExprKind::GetFieldPartial {
                            object: Box::new(base_ir),
                            candidates,
                            attr: attr.clone(),
                        },
                    });
                }
                // @property: attribute load → zero-arg method call.
                if resolve_property(id, attr).is_some() {
                    return lower_instance_method_call(base_ir, id, attr, *attr_span, &[], ctx);
                }
                if let Some((field_index, field_ty)) = field_index(id, attr) {
                    return Ok(ir::Expr {
                        ty: field_ty,
                        kind: ir::ExprKind::GetField {
                            object: Box::new(base_ir),
                            class_id: id,
                            field_index,
                        },
                    });
                }
                // Method name without call → bound-method value.
                if let Some(direct) = resolve_method(id, attr) {
                    let kind = method_kind_lookup(&direct);
                    if matches!(kind, MethodKind::Static | MethodKind::Class) {
                        let what = match kind {
                            MethodKind::Static => "staticmethod",
                            MethodKind::Class => "classmethod",
                            _ => "method",
                        };
                        return Err(err(
                            format!(
                                "taking a reference to {what} '{attr}' is not supported yet; \
                                 call it directly"
                            ),
                            *attr_span,
                        ));
                    }
                    let sig = method_sig_lookup(&direct).ok_or_else(|| {
                        err(
                            format!("internal error: missing signature for '{attr}'"),
                            *attr_span,
                        )
                    })?;
                    let user_params: Vec<ir::Ty> =
                        sig.params.iter().skip(1).map(|p| p.ty).collect();
                    let mut candidates: Vec<(ir::ClassId, String)> = Vec::new();
                    let mut unique: HashSet<String> = HashSet::new();
                    for sid in subclasses_of(id) {
                        if let Some(func) = resolve_method(sid, attr) {
                            unique.insert(func.clone());
                            candidates.push((sid, func));
                        }
                    }
                    let virtual_dispatch = unique.len() > 1;
                    let bm_ty =
                        ir::bound_method_of(id, &user_params, sig.ret, &direct, virtual_dispatch);
                    return Ok(ir::Expr {
                        ty: bm_ty,
                        kind: ir::ExprKind::BindMethod {
                            object: Box::new(base_ir),
                            class_id: id,
                            method: attr.clone(),
                            direct_func: direct,
                            candidates,
                            virtual_dispatch,
                        },
                    });
                }
                return Err(err(
                    format!(
                        "'{}' object has no attribute '{attr}'",
                        class_info(id)
                            .map(|c| c.name)
                            .unwrap_or_else(|| format!("class#{id}"))
                    ),
                    *attr_span,
                ));
            }
            // True union ABI (e.g. list[Dog|Cat] elements) with a shared field.
            if let Some((class_id, field_index, field_ty)) = common_class_field(base_ir.ty, attr) {
                let obj = ir::Expr {
                    ty: ir::Ty::Class(class_id),
                    kind: ir::ExprKind::FromUnion {
                        value: Box::new(base_ir),
                    },
                };
                return Ok(ir::Expr {
                    ty: field_ty,
                    kind: ir::ExprKind::GetField {
                        object: Box::new(obj),
                        class_id,
                        field_index,
                    },
                });
            }
            // Exclusive field on a true class-union value.
            if let Some((candidates, field_ty)) = exclusive_class_field(base_ir.ty, attr) {
                let rep = candidates[0].0;
                let obj = ir::Expr {
                    ty: ir::Ty::Class(rep),
                    kind: ir::ExprKind::FromUnion {
                        value: Box::new(base_ir),
                    },
                };
                return Ok(ir::Expr {
                    ty: field_ty,
                    kind: ir::ExprKind::GetFieldPartial {
                        object: Box::new(obj),
                        candidates,
                        attr: attr.clone(),
                    },
                });
            }
            Err(err(
                "attribute access is only supported for instance fields, 'sys.argv', \
                 imported module globals, and method calls",
                *attr_span,
            ))
        }
        ast::ExprKind::MethodCall {
            base,
            method,
            method_span,
            args,
            keywords,
            kwargs,
        } => {
            // `module.func(args)` / `pkg.mod.func(args)` — cross-module call
            if let Some(real) = resolve_module_path(base, ctx) {
                return lower_module_call(
                    &real,
                    method,
                    *method_span,
                    args,
                    keywords,
                    kwargs.as_deref(),
                    ctx,
                );
            }
            if !keywords.is_empty() {
                return Err(err(
                    "keyword arguments are not supported for this method call",
                    keywords[0].name_span,
                ));
            }
            if kwargs.is_some() {
                return Err(err(
                    "** unpacking is not supported for this method call",
                    *method_span,
                ));
            }
            let plain = require_plain_args(args, method, *method_span)?;
            let args: Vec<ast::Expr> = plain.iter().map(|e| (*e).clone()).collect();
            // `super().m(...)` — static parent method call (before lowering base).
            if is_zero_arg_super(base) {
                return lower_super_method_call(method, *method_span, &args, ctx);
            }
            // `ClassName.static_or_class_method(...)` before lowering base as value.
            if let ast::ExprKind::Name(cls_name) = &base.kind
                && let Some(class_id) = lookup_class(cls_name)
            {
                return lower_class_name_method_call(class_id, method, *method_span, &args, ctx);
            }
            let base_ir = lower_expr(base, ctx)?;
            // User class instance method
            if let ir::Ty::Class(id) = base_ir.ty {
                return lower_instance_method_call(base_ir, id, method, *method_span, &args, ctx);
            }
            match base_ir.ty {
                ir::Ty::List(elem) => match method.as_str() {
                    // pop returns the removed element
                    "pop" => lower_list_pop(base_ir, *elem, &args, *method_span, ctx),
                    "index" => lower_list_index_of(base_ir, *elem, &args, *method_span, ctx),
                    "append" | "insert" | "remove" | "clear" | "sort" | "extend" => Err(err(
                        format!(
                            "list.{method}(...) returns None and cannot be used \
                             in an expression"
                        ),
                        *method_span,
                    )),
                    "copy" => {
                        if !args.is_empty() {
                            return Err(err(
                                format!("copy() takes no arguments ({} given)", args.len()),
                                *method_span,
                            ));
                        }
                        Ok(ir::Expr {
                            ty: ir::list_of(*elem),
                            kind: ir::ExprKind::ListCopy(Box::new(base_ir)),
                        })
                    }
                    _ => Err(err(
                        format!("'{}' has no method '{method}'", base_ir.ty),
                        *method_span,
                    )),
                },
                ir::Ty::Str => lower_str_method(base_ir, method, *method_span, &args, ctx),
                ir::Ty::File => {
                    if method == "close" {
                        return Err(err(
                            "file.close() returns None and cannot be used in \
                             an expression",
                            *method_span,
                        ));
                    }
                    lower_file_method(base_ir, method, *method_span, &args, ctx)
                }
                ir::Ty::Dict { key, value } => {
                    lower_dict_method(base_ir, *key, *value, method, *method_span, &args, ctx)
                }
                ir::Ty::Set(_) => match method.as_str() {
                    "add" | "remove" | "discard" | "clear" | "update" => Err(err(
                        format!(
                            "set.{method}(...) returns None and cannot be used in an \
                             expression"
                        ),
                        *method_span,
                    )),
                    "union" | "intersection" | "difference" | "symmetric_difference" => {
                        if args.len() != 1 {
                            return Err(err(
                                format!(
                                    "{method}() takes exactly one argument ({} given)",
                                    args.len()
                                ),
                                *method_span,
                            ));
                        }
                        let other = lower_expr(&args[0], ctx)?;
                        match method.as_str() {
                            "union" => lower_set_union(base_ir, other, *method_span),
                            "intersection" => {
                                lower_set_binary_op(base_ir, other, *method_span, method, |l, r| {
                                    ir::ExprKind::SetIntersect { left: l, right: r }
                                })
                            }
                            "difference" => {
                                lower_set_binary_op(base_ir, other, *method_span, method, |l, r| {
                                    ir::ExprKind::SetDiff { left: l, right: r }
                                })
                            }
                            _ => {
                                lower_set_binary_op(base_ir, other, *method_span, method, |l, r| {
                                    ir::ExprKind::SetSymDiff { left: l, right: r }
                                })
                            }
                        }
                    }
                    _ => Err(err(
                        format!(
                            "set method '{method}' is not supported yet (supported: add, \
                             remove, discard, clear, union, intersection, difference, \
                             symmetric_difference, update)"
                        ),
                        *method_span,
                    )),
                },
                ir::Ty::Generator { yield_ty } => match method.as_str() {
                    "close" => Err(err(
                        "generator.close() returns None and cannot be used in an expression",
                        *method_span,
                    )),
                    "send" => {
                        if args.len() != 1 {
                            return Err(err(
                                format!("send() takes exactly one argument ({} given)", args.len()),
                                *method_span,
                            ));
                        }
                        let send = lower_gen_send_arg(&args[0], *yield_ty, ctx)?;
                        Ok(ir::Expr {
                            ty: ir::optional_of(*yield_ty),
                            kind: ir::ExprKind::GeneratorNext {
                                generator: Box::new(base_ir),
                                send: Box::new(send),
                            },
                        })
                    }
                    "throw" => {
                        let (exc, message) = lower_gen_throw_args(&args, *method_span, ctx)?;
                        Ok(ir::Expr {
                            ty: ir::optional_of(*yield_ty),
                            kind: ir::ExprKind::GeneratorThrow {
                                generator: Box::new(base_ir),
                                exc,
                                message: Box::new(message),
                            },
                        })
                    }
                    _ => Err(err(
                        format!(
                            "generator method '{method}' is not supported yet \
                             (supported: close, send, throw)"
                        ),
                        *method_span,
                    )),
                },
                other => Err(err(
                    format!("'{other}' has no method '{method}'"),
                    *method_span,
                )),
            }
        }
        ast::ExprKind::Slice { base, lo, hi, step } => {
            let base_ir = lower_expr(base, ctx)?;
            let ty = match base_ir.ty {
                ir::Ty::Str => ir::Ty::Str,
                ir::Ty::List(e) => ir::Ty::List(e),
                other => {
                    return Err(err(format!("'{other}' object cannot be sliced"), base.span));
                }
            };
            // missing bounds are i64::MIN sentinels: their meaning depends
            // on the step's sign, resolved by the runtime like CPython
            let lo_ir = match lo {
                Some(e) => {
                    let v = lower_expr(e, ctx)?;
                    coerce(v, ir::Ty::Int, e.span, "slice bound")?
                }
                Option::None => int_const(i64::MIN),
            };
            let hi_ir = match hi {
                Some(e) => {
                    let v = lower_expr(e, ctx)?;
                    coerce(v, ir::Ty::Int, e.span, "slice bound")?
                }
                Option::None => int_const(i64::MIN),
            };
            let step_ir = match step {
                Some(e) => {
                    let v = lower_expr(e, ctx)?;
                    let v = coerce(v, ir::Ty::Int, e.span, "slice step")?;
                    if matches!(v.kind, ir::ExprKind::ConstInt(0)) {
                        return Err(err("slice step cannot be zero", e.span));
                    }
                    v
                }
                Option::None => int_const(1),
            };
            Ok(ir::Expr {
                ty,
                kind: ir::ExprKind::Slice {
                    base: Box::new(base_ir),
                    lo: Box::new(lo_ir),
                    hi: Box::new(hi_ir),
                    step: Box::new(step_ir),
                },
            })
        }
        ast::ExprKind::JoinedStr(parts) => lower_joined_str(parts, ctx),
        ast::ExprKind::Call {
            func,
            func_span,
            args,
            keywords,
            kwargs,
        } => lower_call(
            func,
            *func_span,
            args,
            keywords,
            kwargs.as_deref(),
            expr.span,
            ctx,
        ),
        ast::ExprKind::Cast { ty, arg } => {
            let value = lower_expr(arg, ctx)?;
            lower_cast(*ty, value, arg.span)
        }
        ast::ExprKind::Unary { op, operand } => {
            let value = lower_expr(operand, ctx)?;
            match op {
                ast::UnaryOp::Not => {
                    let value = to_bool(value, operand.span, ctx)?;
                    Ok(ir::Expr {
                        ty: ir::Ty::Bool,
                        kind: ir::ExprKind::Unary {
                            op: ir::UnOp::Not,
                            operand: Box::new(value),
                        },
                    })
                }
                ast::UnaryOp::Neg => {
                    let value = promote_numeric(value, operand.span, "unary '-'")?;
                    // fold negated literals into constants so negative range
                    // steps and exponents are statically visible
                    if let ir::ExprKind::ConstInt(v) = value.kind {
                        if let Some(n) = v.checked_neg() {
                            return Ok(int_const(n));
                        }
                        // i64::MIN negated needs a bigint; leave as Unary.
                        let value = int_const(v);
                        let ty = value.ty;
                        return Ok(ir::Expr {
                            ty,
                            kind: ir::ExprKind::Unary {
                                op: ir::UnOp::Neg,
                                operand: Box::new(value),
                            },
                        });
                    }
                    if let ir::ExprKind::ConstFloat(v) = value.kind {
                        return Ok(ir::Expr {
                            ty: ir::Ty::Float,
                            kind: ir::ExprKind::ConstFloat(-v),
                        });
                    }
                    let ty = value.ty;
                    Ok(ir::Expr {
                        ty,
                        kind: ir::ExprKind::Unary {
                            op: ir::UnOp::Neg,
                            operand: Box::new(value),
                        },
                    })
                }
                ast::UnaryOp::Invert => {
                    // ~x on int/bool (bool → int); result is int
                    let value = match value.ty {
                        ir::Ty::Int => value,
                        ir::Ty::Bool => ir::Expr {
                            ty: ir::Ty::Int,
                            kind: ir::ExprKind::BoolToInt(Box::new(value)),
                        },
                        other => {
                            return Err(err(
                                format!("bad operand type for unary ~: '{other}'"),
                                operand.span,
                            ));
                        }
                    };
                    if let ir::ExprKind::ConstInt(v) = value.kind {
                        return Ok(int_const(!v));
                    }
                    Ok(ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::Unary {
                            op: ir::UnOp::Invert,
                            operand: Box::new(value),
                        },
                    })
                }
            }
        }
        ast::ExprKind::Compare { first, rest } => {
            let first_ir = lower_expr(first, ctx)?;
            lower_compare_chain(first_ir, rest, expr.span, ctx)
        }
        ast::ExprKind::Binary { op, left, right } => {
            // and/or yield an operand (not always bool), with short-circuit.
            // Mid-expression refine: `x is not None and x > 0` types the RHS
            // under the left's then-refinements; `x is None or x < 0` under
            // the left's else-refinements (CPython short-circuit order).
            if matches!(op, ast::BinOp::And | ast::BinOp::Or) {
                let l = lower_expr(left, ctx)?;
                let saved = ctx.type_refinements.clone();
                let (then_ref, else_ref) = narrowing_from_condition(left, ctx);
                if *op == ast::BinOp::And {
                    for (k, v) in then_ref {
                        ctx.type_refinements.insert(k, v);
                    }
                } else {
                    for (k, v) in else_ref {
                        ctx.type_refinements.insert(k, v);
                    }
                }
                let r = lower_expr(right, ctx)?;
                ctx.type_refinements = saved;
                let (l, r, ty) = unify_and_or(l, r, expr.span)?;
                let ir_op = if *op == ast::BinOp::And {
                    ir::BinOp::And
                } else {
                    ir::BinOp::Or
                };
                return Ok(ir::Expr {
                    ty,
                    kind: ir::ExprKind::Binary {
                        op: ir_op,
                        left: Box::new(l),
                        right: Box::new(r),
                    },
                });
            }
            let l = lower_expr(left, ctx)?;
            let r = lower_expr(right, ctx)?;
            lower_binary(*op, l, r, expr.span)
        }
        ast::ExprKind::Lambda { params, body } => lower_lambda(params, body, expr.span, ctx),
        ast::ExprKind::Yield(v) => {
            let val = match v {
                Some(e) => lower_expr(e, ctx)?,
                None => const_none(),
            };
            let Some(yty) = ctx.yield_ty else {
                return Err(err(
                    "'yield' outside function — only valid in a generator function body",
                    expr.span,
                ));
            };
            let val = coerce(val, yty, expr.span, "yield value")?;
            // Yield suspends then resumes with send/next value as Optional[Y].
            let sent_ty = ir::optional_of(yty);
            Ok(ir::Expr {
                ty: sent_ty,
                kind: ir::ExprKind::Block {
                    stmts: vec![ir::Stmt::Yield(val)],
                    result: Box::new(ir::Expr {
                        ty: sent_ty,
                        kind: ir::ExprKind::GenSentValue,
                    }),
                },
            })
        }
        ast::ExprKind::YieldFrom(iter) => {
            // Desugar to iteration + yield for any iterable supported by `for`.
            let Some(yty) = ctx.yield_ty else {
                return Err(err("'yield from' outside function", expr.span));
            };
            lower_yield_from(iter, yty, expr.span, ctx)
        }
        ast::ExprKind::Starred(_) => Err(err(
            "starred expression cannot be used here (only in list displays and unpack targets)",
            expr.span,
        )),
    }
}

/// Desugar `yield from iter` for lists, tuples, strings, and generators.
fn lower_yield_from(
    iter: &ast::Expr,
    yty: ir::Ty,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    // Empty list literal: element type is the generator's yield type.
    let iter_ir = match &iter.kind {
        ast::ExprKind::ListLit(items) if items.is_empty() => {
            lower_list_lit(items, Some(yty), iter.span, ctx)?
        }
        _ => lower_expr(iter, ctx)?,
    };
    match iter_ir.ty {
        ir::Ty::List(elem) => {
            let elem = *elem;
            let seq = ctx.fresh_temp("yfs", ir::list_of(elem));
            let i = ctx.fresh_temp("yfi", ir::Ty::Int);
            let var = ctx.fresh_temp("yf", elem);
            let item = ir::Expr {
                ty: elem,
                kind: ir::ExprKind::Local(var.clone()),
            };
            let yielded = coerce(item, yty, span, "yield from element")?;
            let stmts = vec![
                ir::Stmt::Assign {
                    name: seq.clone(),
                    value: iter_ir,
                },
                ir::Stmt::Assign {
                    name: i.clone(),
                    value: int_const(0),
                },
                ir::Stmt::While {
                    cond: ir::Expr {
                        ty: ir::Ty::Bool,
                        kind: ir::ExprKind::Binary {
                            op: ir::BinOp::Lt,
                            left: Box::new(ir::Expr {
                                ty: ir::Ty::Int,
                                kind: ir::ExprKind::Local(i.clone()),
                            }),
                            right: Box::new(ir::Expr {
                                ty: ir::Ty::Int,
                                kind: ir::ExprKind::Len(Box::new(ir::Expr {
                                    ty: ir::list_of(elem),
                                    kind: ir::ExprKind::Local(seq.clone()),
                                })),
                            }),
                        },
                    },
                    body: vec![
                        ir::Stmt::Assign {
                            name: var,
                            value: ir::Expr {
                                ty: elem,
                                kind: ir::ExprKind::Index {
                                    base: Box::new(ir::Expr {
                                        ty: ir::list_of(elem),
                                        kind: ir::ExprKind::Local(seq),
                                    }),
                                    index: Box::new(ir::Expr {
                                        ty: ir::Ty::Int,
                                        kind: ir::ExprKind::Local(i.clone()),
                                    }),
                                },
                            },
                        },
                        ir::Stmt::Yield(yielded),
                    ],
                    step: vec![ir::Stmt::Assign {
                        name: i.clone(),
                        value: ir::Expr {
                            ty: ir::Ty::Int,
                            kind: ir::ExprKind::Binary {
                                op: ir::BinOp::Add,
                                left: Box::new(ir::Expr {
                                    ty: ir::Ty::Int,
                                    kind: ir::ExprKind::Local(i),
                                }),
                                right: Box::new(int_const(1)),
                            },
                        },
                    }],
                },
            ];
            Ok(ir::Expr {
                ty: ir::Ty::None,
                kind: ir::ExprKind::Block {
                    stmts,
                    result: Box::new(const_none()),
                },
            })
        }
        ir::Ty::Str => {
            // Yield each character (1-char str) when yty is str.
            if yty != ir::Ty::Str {
                return Err(err(
                    format!("yield from str requires generator yield type str, found {yty}"),
                    span,
                ));
            }
            let seq = ctx.fresh_temp("yfstr", ir::Ty::Str);
            let i = ctx.fresh_temp("yfi", ir::Ty::Int);
            let ch = ctx.fresh_temp("yfch", ir::Ty::Str);
            let stmts = vec![
                ir::Stmt::Assign {
                    name: seq.clone(),
                    value: iter_ir,
                },
                ir::Stmt::Assign {
                    name: i.clone(),
                    value: int_const(0),
                },
                ir::Stmt::While {
                    cond: ir::Expr {
                        ty: ir::Ty::Bool,
                        kind: ir::ExprKind::Binary {
                            op: ir::BinOp::Lt,
                            left: Box::new(ir::Expr {
                                ty: ir::Ty::Int,
                                kind: ir::ExprKind::Local(i.clone()),
                            }),
                            right: Box::new(ir::Expr {
                                ty: ir::Ty::Int,
                                kind: ir::ExprKind::Len(Box::new(ir::Expr {
                                    ty: ir::Ty::Str,
                                    kind: ir::ExprKind::Local(seq.clone()),
                                })),
                            }),
                        },
                    },
                    body: vec![
                        ir::Stmt::Assign {
                            name: ch.clone(),
                            value: ir::Expr {
                                ty: ir::Ty::Str,
                                kind: ir::ExprKind::Index {
                                    base: Box::new(ir::Expr {
                                        ty: ir::Ty::Str,
                                        kind: ir::ExprKind::Local(seq),
                                    }),
                                    index: Box::new(ir::Expr {
                                        ty: ir::Ty::Int,
                                        kind: ir::ExprKind::Local(i.clone()),
                                    }),
                                },
                            },
                        },
                        ir::Stmt::Yield(ir::Expr {
                            ty: ir::Ty::Str,
                            kind: ir::ExprKind::Local(ch),
                        }),
                    ],
                    step: vec![ir::Stmt::Assign {
                        name: i.clone(),
                        value: ir::Expr {
                            ty: ir::Ty::Int,
                            kind: ir::ExprKind::Binary {
                                op: ir::BinOp::Add,
                                left: Box::new(ir::Expr {
                                    ty: ir::Ty::Int,
                                    kind: ir::ExprKind::Local(i),
                                }),
                                right: Box::new(int_const(1)),
                            },
                        },
                    }],
                },
            ];
            Ok(ir::Expr {
                ty: ir::Ty::None,
                kind: ir::ExprKind::Block {
                    stmts,
                    result: Box::new(const_none()),
                },
            })
        }
        ir::Ty::Tuple(elems) => {
            let mut stmts = Vec::new();
            let tup = ctx.fresh_temp("yftup", iter_ir.ty);
            stmts.push(ir::Stmt::Assign {
                name: tup.clone(),
                value: iter_ir,
            });
            for (i, et) in elems.iter().enumerate() {
                let item = ir::Expr {
                    ty: *et,
                    kind: ir::ExprKind::Index {
                        base: Box::new(ir::Expr {
                            ty: ir::tuple_of(elems),
                            kind: ir::ExprKind::Local(tup.clone()),
                        }),
                        index: Box::new(int_const(i as i64)),
                    },
                };
                let yielded = coerce(item, yty, span, "yield from tuple element")?;
                stmts.push(ir::Stmt::Yield(yielded));
            }
            Ok(ir::Expr {
                ty: ir::Ty::None,
                kind: ir::ExprKind::Block {
                    stmts,
                    result: Box::new(const_none()),
                },
            })
        }
        ir::Ty::Generator { yield_ty } => {
            // CPython: `x = yield from g` gets StopIteration.value (None after
            // bare return / fall-off; N after `return N`). close() on the outer
            // closes the delegated generator so its finally runs.
            let gy = *yield_ty;
            let gen_t = ctx.fresh_temp("yfgen", iter_ir.ty);
            let more_t = ctx.fresh_temp("yfmore", ir::Ty::Bool);
            let opt_ty = ir::optional_of(gy);
            let nxt_t = ctx.fresh_temp("yfnxt", opt_ty);
            // Result type is Optional[yield_ty]: None when subgen ends without
            // an explicit return value; Some(v) after `return v` (v coerced to yty).
            let ret_ty = ir::optional_of(yty);
            let ret_t = ctx.fresh_temp("yfret", ret_ty);
            let none_ret = coerce(const_none(), ret_ty, span, "yield from default return")?;
            let gen_local = ir::Expr {
                ty: ir::generator_of(gy),
                kind: ir::ExprKind::Local(gen_t.clone()),
            };
            let loop_body = vec![
                ir::Stmt::Assign {
                    name: nxt_t.clone(),
                    value: ir::Expr {
                        ty: opt_ty,
                        kind: ir::ExprKind::GeneratorNext {
                            generator: Box::new(gen_local.clone()),
                            send: Box::new(const_none()),
                        },
                    },
                },
                ir::Stmt::If {
                    branches: vec![(
                        ir::Expr {
                            ty: ir::Ty::Bool,
                            kind: ir::ExprKind::IsNone {
                                value: Box::new(ir::Expr {
                                    ty: opt_ty,
                                    kind: ir::ExprKind::Local(nxt_t.clone()),
                                }),
                                not: false,
                            },
                        },
                        vec![
                            ir::Stmt::Assign {
                                name: more_t.clone(),
                                value: ir::Expr {
                                    ty: ir::Ty::Bool,
                                    kind: ir::ExprKind::ConstBool(false),
                                },
                            },
                            ir::Stmt::Assign {
                                name: ret_t.clone(),
                                value: {
                                    // GeneratorReturnValue is already Optional[Y]
                                    // (None if bare end; Some if return set).
                                    // Payload encoding uses the subgen yield type;
                                    // re-target to Optional[outer yield type].
                                    let raw = ir::Expr {
                                        ty: ir::optional_of(gy),
                                        kind: ir::ExprKind::GeneratorReturnValue(Box::new(
                                            gen_local.clone(),
                                        )),
                                    };
                                    if raw.ty == ret_ty {
                                        raw
                                    } else {
                                        // e.g. gy==yty, or both optional of same core —
                                        // coerce union members if needed.
                                        coerce(raw, ret_ty, span, "yield from return value")?
                                    }
                                },
                            },
                        ],
                    )],
                    orelse: {
                        let extracted = ir::Expr {
                            ty: gy,
                            kind: ir::ExprKind::FromUnion {
                                value: Box::new(ir::Expr {
                                    ty: opt_ty,
                                    kind: ir::ExprKind::Local(nxt_t.clone()),
                                }),
                            },
                        };
                        let yielded = coerce(extracted, yty, span, "yield from generator")?;
                        vec![ir::Stmt::Yield(yielded)]
                    },
                },
            ];
            let stmts = vec![
                ir::Stmt::Assign {
                    name: gen_t.clone(),
                    value: iter_ir,
                },
                ir::Stmt::Assign {
                    name: ret_t.clone(),
                    value: none_ret,
                },
                ir::Stmt::Assign {
                    name: more_t.clone(),
                    value: ir::Expr {
                        ty: ir::Ty::Bool,
                        kind: ir::ExprKind::ConstBool(true),
                    },
                },
                // try/finally close: CPython closes the subgen on outer close /
                // GeneratorExit and when yield-from finishes normally.
                ir::Stmt::Try {
                    body: vec![ir::Stmt::While {
                        cond: ir::Expr {
                            ty: ir::Ty::Bool,
                            kind: ir::ExprKind::Local(more_t.clone()),
                        },
                        body: loop_body,
                        step: vec![],
                    }],
                    handlers: vec![],
                    orelse: vec![],
                    finally: vec![ir::Stmt::GenClose {
                        generator: gen_local,
                    }],
                },
            ];
            Ok(ir::Expr {
                ty: ret_ty,
                kind: ir::ExprKind::Block {
                    stmts,
                    result: Box::new(ir::Expr {
                        ty: ret_ty,
                        kind: ir::ExprKind::Local(ret_t),
                    }),
                },
            })
        }
        other => Err(err(
            format!("yield from expects an iterable (list/tuple/str/generator), found {other}"),
            span,
        )),
    }
}

/// `a < b <= c`: each middle operand is bound to a temp (evaluated once)
/// and the chain becomes short-circuit `and`s, exactly like Python.
fn lower_compare_chain(
    prev: ir::Expr,
    rest: &[(ast::BinOp, ast::Expr)],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let (op, operand) = &rest[0];
    let cur = lower_expr(operand, ctx)?;

    if rest.len() == 1 {
        return lower_binary(*op, prev, cur, span);
    }

    let cur_ty = cur.ty;
    let temp = ctx.fresh_temp("cmp", cur_ty);
    let temp_local = ir::Expr {
        ty: cur_ty,
        kind: ir::ExprKind::Local(temp.clone()),
    };

    let head = lower_binary(*op, prev, temp_local.clone(), span)?;
    let tail = lower_compare_chain(temp_local, &rest[1..], span, ctx)?;

    Ok(ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Let {
            name: temp,
            value: Box::new(cur),
            body: Box::new(bool_and(head, tail)),
        },
    })
}

/// One prepared `for` level inside a list comprehension.
struct CompLevel {
    /// Stmts that run before this level's while (at the appropriate nesting).
    setup: Vec<ir::Stmt>,
    cond: ir::Expr,
    step: Vec<ir::Stmt>,
    /// Bind the iteration element into the target.
    bind: Vec<ir::Stmt>,
    /// Filters for this generator (`if` clauses), already lowered.
    ifs: Vec<ir::Expr>,
    /// Exact capacity when knowable (only used for a single unfiltered gen).
    cap: Option<ir::Expr>,
}

/// Build range/list/str loop setup for one comprehension generator.
/// Appends setup into `setup`; returns (cond, step, element, optional cap).
fn lower_comp_iter(
    iter: &ast::Expr,
    want_cap: bool,
    ctx: &mut FnCtx,
    setup: &mut Vec<ir::Stmt>,
) -> SResult<(ir::Expr, Vec<ir::Stmt>, ir::Expr, Option<ir::Expr>)> {
    if let ast::ExprKind::Call { func, args, .. } = &iter.kind
        && func == "range"
        && !ctx.funcs().contains_key("range")
    {
        let plain = require_plain_args(args, "range", iter.span)?;
        if plain.is_empty() || plain.len() > 3 {
            return Err(err(
                format!("range() takes 1 to 3 arguments ({} given)", plain.len()),
                iter.span,
            ));
        }
        let mut lowered: Vec<ir::Expr> = Vec::new();
        for a in &plain {
            let v = lower_expr(a, ctx)?;
            lowered.push(coerce(v, ir::Ty::Int, a.span, "range() argument")?);
        }
        let (start, stop, step) = match lowered.len() {
            1 => (int_const(0), lowered.remove(0), int_const(1)),
            2 => {
                let stop = lowered.remove(1);
                (lowered.remove(0), stop, int_const(1))
            }
            _ => {
                let step = lowered.remove(2);
                let stop = lowered.remove(1);
                (lowered.remove(0), stop, step)
            }
        };
        let stop_t = ctx.fresh_temp("comp.stop", ir::Ty::Int);
        setup.push(ir::Stmt::Assign {
            name: stop_t.clone(),
            value: stop,
        });
        let stop_local = ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Local(stop_t),
        };
        let it_t = ctx.fresh_temp("comp.it", ir::Ty::Int);
        setup.push(ir::Stmt::Assign {
            name: it_t.clone(),
            value: start,
        });
        let it_local = ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Local(it_t.clone()),
        };

        let (loop_cond, step_value, cap) = match step.kind {
            ir::ExprKind::ConstInt(0) => {
                return Err(err("range() arg 3 must not be zero", iter.span));
            }
            ir::ExprKind::ConstInt(1) if want_cap => {
                let cap_t = ctx.fresh_temp("comp.cap", ir::Ty::Int);
                setup.push(ir::Stmt::Assign {
                    name: cap_t.clone(),
                    value: ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::Binary {
                            op: ir::BinOp::Sub,
                            left: Box::new(stop_local.clone()),
                            right: Box::new(it_local.clone()),
                        },
                    },
                });
                let cap_local = ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Local(cap_t.clone()),
                };
                setup.push(ir::Stmt::If {
                    branches: vec![(
                        int_cmp(ir::BinOp::Lt, cap_local.clone(), int_const(0)),
                        vec![ir::Stmt::Assign {
                            name: cap_t,
                            value: int_const(0),
                        }],
                    )],
                    orelse: vec![],
                });
                (
                    int_cmp(ir::BinOp::Lt, it_local.clone(), stop_local),
                    int_const(1),
                    Some(cap_local),
                )
            }
            ir::ExprKind::ConstInt(k) => {
                let op = if k > 0 { ir::BinOp::Lt } else { ir::BinOp::Gt };
                (
                    int_cmp(op, it_local.clone(), stop_local),
                    int_const(k),
                    None,
                )
            }
            _ => {
                let step_t = ctx.fresh_temp("comp.step", ir::Ty::Int);
                setup.push(ir::Stmt::Assign {
                    name: step_t.clone(),
                    value: step,
                });
                let step_local = ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Local(step_t),
                };
                setup.push(ir::Stmt::If {
                    branches: vec![(
                        int_cmp(ir::BinOp::Eq, step_local.clone(), int_const(0)),
                        vec![ir::Stmt::Die(
                            "ValueError: range() arg 3 must not be zero".to_string(),
                        )],
                    )],
                    orelse: vec![],
                });
                let up = bool_and(
                    int_cmp(ir::BinOp::Gt, step_local.clone(), int_const(0)),
                    int_cmp(ir::BinOp::Lt, it_local.clone(), stop_local.clone()),
                );
                let down = bool_and(
                    int_cmp(ir::BinOp::Lt, step_local.clone(), int_const(0)),
                    int_cmp(ir::BinOp::Gt, it_local.clone(), stop_local),
                );
                let cond = ir::Expr {
                    ty: ir::Ty::Bool,
                    kind: ir::ExprKind::Binary {
                        op: ir::BinOp::Or,
                        left: Box::new(up),
                        right: Box::new(down),
                    },
                };
                (cond, step_local, None)
            }
        };
        let step_stmt = ir::Stmt::Assign {
            name: it_t,
            value: ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Add,
                    left: Box::new(it_local.clone()),
                    right: Box::new(step_value),
                },
            },
        };
        return Ok((loop_cond, vec![step_stmt], it_local, cap));
    }

    // list or str: index loop; optional presize via len
    let seq = lower_expr(iter, ctx)?;
    let src_elem_ty = match seq.ty {
        ir::Ty::List(e) => *e,
        ir::Ty::Str => ir::Ty::Str,
        other => {
            return Err(err(format!("'{other}' object is not iterable"), iter.span));
        }
    };
    let seq_ty = seq.ty;
    let seq_t = ctx.fresh_temp("comp.seq", seq_ty);
    setup.push(ir::Stmt::Assign {
        name: seq_t.clone(),
        value: seq,
    });
    let idx_t = ctx.fresh_temp("comp.idx", ir::Ty::Int);
    setup.push(ir::Stmt::Assign {
        name: idx_t.clone(),
        value: int_const(0),
    });
    let seq_local = ir::Expr {
        ty: seq_ty,
        kind: ir::ExprKind::Local(seq_t),
    };
    let idx_local = ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::Local(idx_t.clone()),
    };
    let cond = int_cmp(
        ir::BinOp::Lt,
        idx_local.clone(),
        ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Len(Box::new(seq_local.clone())),
        },
    );
    let element = ir::Expr {
        ty: src_elem_ty,
        kind: ir::ExprKind::Index {
            base: Box::new(seq_local.clone()),
            index: Box::new(idx_local.clone()),
        },
    };
    let step_stmt = ir::Stmt::Assign {
        name: idx_t,
        value: ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Binary {
                op: ir::BinOp::Add,
                left: Box::new(idx_local),
                right: Box::new(int_const(1)),
            },
        },
    };
    let cap = if want_cap {
        Some(ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Len(Box::new(seq_local)),
        })
    } else {
        None
    };
    Ok((cond, vec![step_stmt], element, cap))
}

/// Bind a comprehension target: simple names use hidden storage (no leak);
/// unpack / index targets bind real locals via `lower_assign_ir`.
fn bind_comp_target(
    target: &ast::AssignTarget,
    element: ir::Expr,
    ctx: &mut FnCtx,
) -> SResult<(Vec<ir::Stmt>, usize)> {
    match target {
        ast::AssignTarget::Name { name, .. } => {
            let src_elem_ty = element.ty;
            ctx.temp_counter += 1;
            let storage = format!(".comp{}.{name}", ctx.temp_counter);
            ctx.locals_order.push((storage.clone(), src_elem_ty));
            ctx.comp_renames
                .push((name.clone(), storage.clone(), src_elem_ty));
            let bind = vec![ir::Stmt::Assign {
                name: storage,
                value: element,
            }];
            Ok((bind, 1))
        }
        _ => {
            let bind = bind_for_target(target, element, ctx)?;
            Ok((bind, 0))
        }
    }
}

/// Nest `if` filters around an inner body (rightmost if is outermost? No —
/// left-to-right: first if wraps the rest).
fn wrap_comp_ifs(ifs: &[ir::Expr], inner: Vec<ir::Stmt>) -> Vec<ir::Stmt> {
    let mut body = inner;
    for c in ifs.iter().rev() {
        body = vec![ir::Stmt::If {
            branches: vec![(c.clone(), body)],
            orelse: vec![],
        }];
    }
    body
}

/// `[elem for target in iter if cond ... for ...]` desugars to nested loops
/// building a list inside an expression-level Block. Simple name targets live
/// in hidden storage (Python 3: shadow, do not leak). Unpack targets bind real
/// locals. Fast path: single generator, no filters, knowable length → presize
/// + unchecked append.
fn lower_list_comp(
    elem: &ast::Expr,
    generators: &[ast::CompFor],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if generators.is_empty() {
        return Err(err(
            "internal error: list comprehension has no generators",
            span,
        ));
    }

    let can_presize = generators.len() == 1 && generators[0].ifs.is_empty();
    let mut levels: Vec<CompLevel> = Vec::with_capacity(generators.len());
    let mut renames_pushed = 0usize;

    for (i, clause) in generators.iter().enumerate() {
        let want_cap = can_presize && i == 0;
        let mut setup = Vec::new();
        let (cond, step, element, cap) = lower_comp_iter(&clause.iter, want_cap, ctx, &mut setup)?;
        let (bind, n_renames) = bind_comp_target(&clause.target, element, ctx)?;
        renames_pushed += n_renames;
        let mut ifs = Vec::with_capacity(clause.ifs.len());
        for c in &clause.ifs {
            ifs.push(lower_condition(c, ctx)?);
        }
        levels.push(CompLevel {
            setup,
            cond,
            step,
            bind,
            ifs,
            cap,
        });
    }

    let elem_ir = lower_expr(elem, ctx);
    for _ in 0..renames_pushed {
        ctx.comp_renames.pop();
    }
    let elem_ir = elem_ir?;
    let elem_ty = elem_of(elem_ir.ty, elem.span)?;

    // ---- result list ----
    // Cap setup lives in the outermost generator's `setup`; emit that first
    // so presized ListNew can read the capacity temp.
    let presized = can_presize && levels[0].cap.is_some();
    let cap_expr = if presized {
        levels[0].cap.take().unwrap_or(int_const(4))
    } else {
        int_const(4)
    };

    let mut stmts: Vec<ir::Stmt> = Vec::new();
    // Peel outermost setup so ListNew sits between setup0 and while0.
    let outer_setup = std::mem::take(&mut levels[0].setup);

    stmts.extend(outer_setup);
    let res_t = ctx.fresh_temp("comp.res", ir::list_of(elem_ty));
    stmts.push(ir::Stmt::Assign {
        name: res_t.clone(),
        value: ir::Expr {
            ty: ir::list_of(elem_ty),
            kind: ir::ExprKind::ListNew {
                cap: Box::new(cap_expr),
            },
        },
    });
    let res_local = ir::Expr {
        ty: ir::list_of(elem_ty),
        kind: ir::ExprKind::Local(res_t.clone()),
    };

    let append = if presized {
        ir::Stmt::ListAppendUnchecked {
            list: res_local,
            value: elem_ir,
        }
    } else {
        ir::Stmt::ListAppend {
            list: res_local,
            value: elem_ir,
        }
    };

    // Nest from innermost generator outward.
    // Outermost setup already emitted; its while is built here with empty setup.
    let mut inner_body = vec![append];
    for level in levels.into_iter().rev() {
        let mut body = level.bind;
        body.extend(wrap_comp_ifs(&level.ifs, inner_body));
        let while_stmt = ir::Stmt::While {
            cond: level.cond,
            body,
            step: level.step,
        };
        let mut wrapped = level.setup;
        wrapped.push(while_stmt);
        inner_body = wrapped;
    }
    stmts.extend(inner_body);

    let _ = span;
    Ok(ir::Expr {
        ty: ir::list_of(elem_ty),
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(ir::Expr {
                ty: ir::list_of(elem_ty),
                kind: ir::ExprKind::Local(res_t),
            }),
        },
    })
}

/// Shared generator walk for set/dict comprehensions.
fn lower_comp_levels(
    generators: &[ast::CompFor],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<(Vec<CompLevel>, usize)> {
    if generators.is_empty() {
        return Err(err("internal error: comprehension has no generators", span));
    }
    let mut levels: Vec<CompLevel> = Vec::with_capacity(generators.len());
    let mut renames_pushed = 0usize;
    for clause in generators {
        let mut setup = Vec::new();
        let (cond, step, element, cap) = lower_comp_iter(&clause.iter, false, ctx, &mut setup)?;
        let (bind, n_renames) = bind_comp_target(&clause.target, element, ctx)?;
        renames_pushed += n_renames;
        let mut ifs = Vec::with_capacity(clause.ifs.len());
        for c in &clause.ifs {
            ifs.push(lower_condition(c, ctx)?);
        }
        levels.push(CompLevel {
            setup,
            cond,
            step,
            bind,
            ifs,
            cap,
        });
    }
    Ok((levels, renames_pushed))
}

fn nest_comp_body(levels: Vec<CompLevel>, inner_body: Vec<ir::Stmt>) -> Vec<ir::Stmt> {
    let mut body = inner_body;
    for level in levels.into_iter().rev() {
        let mut level_body = level.bind;
        level_body.extend(wrap_comp_ifs(&level.ifs, body));
        let while_stmt = ir::Stmt::While {
            cond: level.cond,
            body: level_body,
            step: level.step,
        };
        let mut wrapped = level.setup;
        wrapped.push(while_stmt);
        body = wrapped;
    }
    body
}

fn lower_set_comp(
    elem: &ast::Expr,
    generators: &[ast::CompFor],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let (levels, renames_pushed) = lower_comp_levels(generators, span, ctx)?;
    let elem_ir = lower_expr(elem, ctx);
    for _ in 0..renames_pushed {
        ctx.comp_renames.pop();
    }
    let elem_ir = elem_ir?;
    let elem_ty = elem_of(elem_ir.ty, elem.span)?;
    if !matches!(elem_ty, ir::Ty::Int | ir::Ty::Str) {
        return Err(err(
            format!("set comprehension elements must be int or str, found {elem_ty}"),
            elem.span,
        ));
    }
    let res_ty = ir::set_of(elem_ty);
    let res_t = ctx.fresh_temp("setcomp.res", res_ty);
    let mut stmts = vec![ir::Stmt::Assign {
        name: res_t.clone(),
        value: ir::Expr {
            ty: res_ty,
            kind: ir::ExprKind::SetNew,
        },
    }];
    let res_local = ir::Expr {
        ty: res_ty,
        kind: ir::ExprKind::Local(res_t.clone()),
    };
    let add = ir::Stmt::SetAdd {
        set: res_local,
        value: elem_ir,
    };
    stmts.extend(nest_comp_body(levels, vec![add]));
    Ok(ir::Expr {
        ty: res_ty,
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(ir::Expr {
                ty: res_ty,
                kind: ir::ExprKind::Local(res_t),
            }),
        },
    })
}

fn lower_dict_comp(
    key: &ast::Expr,
    value: &ast::Expr,
    generators: &[ast::CompFor],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let (levels, renames_pushed) = lower_comp_levels(generators, span, ctx)?;
    let key_ir = lower_expr(key, ctx);
    let val_ir = lower_expr(value, ctx);
    for _ in 0..renames_pushed {
        ctx.comp_renames.pop();
    }
    let key_ir = key_ir?;
    let val_ir = val_ir?;
    let key_ty = key_ir.ty;
    let val_ty = val_ir.ty;
    if !matches!(key_ty, ir::Ty::Int | ir::Ty::Str) {
        return Err(err(
            format!("dict comprehension keys must be int or str, found {key_ty}"),
            key.span,
        ));
    }
    let res_ty = ir::dict_of(key_ty, val_ty);
    let res_t = ctx.fresh_temp("dictcomp.res", res_ty);
    let mut stmts = vec![ir::Stmt::Assign {
        name: res_t.clone(),
        value: ir::Expr {
            ty: res_ty,
            kind: ir::ExprKind::DictNew,
        },
    }];
    let res_local = ir::Expr {
        ty: res_ty,
        kind: ir::ExprKind::Local(res_t.clone()),
    };
    let store = ir::Stmt::IndexAssign {
        base: res_local,
        index: key_ir,
        value: val_ir,
    };
    stmts.extend(nest_comp_body(levels, vec![store]));
    let _ = span;
    Ok(ir::Expr {
        ty: res_ty,
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(ir::Expr {
                ty: res_ty,
                kind: ir::ExprKind::Local(res_t),
            }),
        },
    })
}

fn lower_list_lit(
    items: &[ast::ListElem],
    expected: Option<ir::Ty>,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if items.is_empty() {
        // Unannotated empty lists default to list[Any]; annotations / storage
        // hints / append-inference supply a more specific elem type.
        let elem = expected.unwrap_or(ir::Ty::Any);
        return Ok(ir::Expr {
            ty: ir::list_of(elem),
            kind: ir::ExprKind::ListLit(vec![]),
        });
    }

    // Lower each element exactly once (side effects).
    enum LoweredElem {
        Item(ir::Expr, Span),
        Star(ir::Expr, Span),
    }
    let mut lowered: Vec<LoweredElem> = Vec::new();
    let mut has_star = false;
    for item in items {
        match item {
            ast::ListElem::Item(e) => {
                lowered.push(LoweredElem::Item(lower_expr(e, ctx)?, e.span));
            }
            ast::ListElem::Star(e) => {
                has_star = true;
                let ir_e = lower_expr(e, ctx)?;
                match ir_e.ty {
                    ir::Ty::List(_) => lowered.push(LoweredElem::Star(ir_e, e.span)),
                    other => {
                        return Err(err(
                            format!("can only unpack list in list display, found {other}"),
                            e.span,
                        ));
                    }
                }
            }
        }
    }

    let elem = match expected {
        Some(e) => e,
        None => {
            let mut ty_opt: Option<ir::Ty> = None;
            for le in &lowered {
                match le {
                    LoweredElem::Item(item, item_span) => {
                        ty_opt = Some(match ty_opt {
                            None => item.ty,
                            Some(prev) => join_elem_types(prev, item.ty).ok_or_else(|| {
                                err(
                                    format!(
                                        "list elements must share one type; found {} and {}",
                                        prev, item.ty
                                    ),
                                    *item_span,
                                )
                            })?,
                        });
                    }
                    LoweredElem::Star(item, item_span) => {
                        let ir::Ty::List(inner) = item.ty else {
                            unreachable!()
                        };
                        ty_opt = Some(match ty_opt {
                            None => *inner,
                            Some(prev) => join_elem_types(prev, *inner).ok_or_else(|| {
                                err(
                                    format!(
                                        "list elements must share one type; found {} and {}",
                                        prev, inner
                                    ),
                                    *item_span,
                                )
                            })?,
                        });
                    }
                }
            }
            let ty = ty_opt.ok_or_else(|| {
                err(
                    "cannot infer the element type of a list of only starred \
                     unpacks; annotate the variable",
                    span,
                )
            })?;
            elem_of(ty, span)?
        }
    };
    reject_exception_container_elem(elem, span, "lists")?;

    if !has_star {
        let mut coerced = Vec::new();
        for le in lowered {
            if let LoweredElem::Item(item, item_span) = le {
                coerced.push(coerce(item, elem, item_span, "list element")?);
            }
        }
        return Ok(ir::Expr {
            ty: ir::list_of(elem),
            kind: ir::ExprKind::ListLit(coerced),
        });
    }

    // Build via concat: start empty, append items / concat starred lists.
    let res_t = ctx.fresh_temp("liststar", ir::list_of(elem));
    let mut stmts = vec![ir::Stmt::Assign {
        name: res_t.clone(),
        value: ir::Expr {
            ty: ir::list_of(elem),
            kind: ir::ExprKind::ListLit(vec![]),
        },
    }];
    let res_e = || ir::Expr {
        ty: ir::list_of(elem),
        kind: ir::ExprKind::Local(res_t.clone()),
    };
    for le in lowered {
        match le {
            LoweredElem::Item(v, span) => {
                let v = coerce(v, elem, span, "list element")?;
                stmts.push(ir::Stmt::ListAppend {
                    list: res_e(),
                    value: v,
                });
            }
            LoweredElem::Star(star, span) => {
                let star = coerce(star, ir::list_of(elem), span, "starred list")?;
                stmts.push(ir::Stmt::Assign {
                    name: res_t.clone(),
                    value: ir::Expr {
                        ty: ir::list_of(elem),
                        kind: ir::ExprKind::Binary {
                            op: ir::BinOp::Add,
                            left: Box::new(res_e()),
                            right: Box::new(star),
                        },
                    },
                });
            }
        }
    }
    Ok(ir::Expr {
        ty: ir::list_of(elem),
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(res_e()),
        },
    })
}

fn lower_tuple_lit(
    items: &[ast::Expr],
    expected: Option<&[ir::Ty]>,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if items.is_empty() {
        if let Some(elems) = expected
            && !elems.is_empty()
        {
            return Err(err(
                format!(
                    "type mismatch in tuple literal: expected tuple with {} elements, got 0",
                    elems.len()
                ),
                span,
            ));
        }
        return Ok(ir::Expr {
            ty: ir::tuple_of(&[]),
            kind: ir::ExprKind::TupleLit(vec![]),
        });
    }
    let mut lowered = Vec::new();
    for item in items {
        let e = lower_expr(item, ctx)?;
        if e.ty == ir::Ty::None || e.ty == ir::Ty::File || e.ty == ir::Ty::Exception {
            return Err(err(format!("tuple elements cannot be {}", e.ty), item.span));
        }
        if matches!(e.ty, ir::Ty::Cell(_)) {
            return Err(err(format!("tuple elements cannot be {}", e.ty), item.span));
        }
        if matches!(e.ty, ir::Ty::Union(ms) if ms.contains(&ir::Ty::Exception)) {
            return Err(err(
                "tuple elements cannot be unions containing exception objects",
                item.span,
            ));
        }
        lowered.push((e, item.span));
    }
    if let Some(elems) = expected {
        if elems.len() != lowered.len() {
            return Err(err(
                format!(
                    "type mismatch in tuple literal: expected {} elements, got {}",
                    elems.len(),
                    lowered.len()
                ),
                span,
            ));
        }
        let mut coerced = Vec::new();
        let mut tys = Vec::new();
        for (i, (item, item_span)) in lowered.into_iter().enumerate() {
            let c = coerce(item, elems[i], item_span, "tuple element")?;
            tys.push(c.ty);
            coerced.push(c);
        }
        return Ok(ir::Expr {
            ty: ir::tuple_of(&tys),
            kind: ir::ExprKind::TupleLit(coerced),
        });
    }
    let tys: Vec<ir::Ty> = lowered.iter().map(|(e, _)| e.ty).collect();
    let items_ir: Vec<ir::Expr> = lowered.into_iter().map(|(e, _)| e).collect();
    Ok(ir::Expr {
        ty: ir::tuple_of(&tys),
        kind: ir::ExprKind::TupleLit(items_ir),
    })
}

fn lower_dict_lit(
    items: &[(ast::Expr, ast::Expr)],
    expected: Option<(ir::Ty, ir::Ty)>,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if items.is_empty() {
        let (k, v) = expected.ok_or_else(|| {
            err(
                "cannot infer the type of an empty dict; annotate the variable, \
                 e.g. 'd: dict[str, int] = {}'",
                span,
            )
        })?;
        check_hashable_key(k, span, "dict")?;
        return Ok(ir::Expr {
            ty: ir::dict_of(k, v),
            kind: ir::ExprKind::DictNew,
        });
    }
    let mut pairs = Vec::new();
    for (k, v) in items {
        let kr = lower_expr(k, ctx)?;
        let vr = lower_expr(v, ctx)?;
        pairs.push((kr, k.span, vr, v.span));
    }
    let (key_ty, val_ty) = match expected {
        Some((k, v)) => (k, v),
        None => {
            let mut kt = pairs[0].0.ty;
            let mut vt = pairs[0].2.ty;
            for (kr, kspan, vr, vspan) in &pairs[1..] {
                kt = join_elem_types(kt, kr.ty).ok_or_else(|| {
                    err(
                        format!("dict keys must share one type; found {kt} and {}", kr.ty),
                        *kspan,
                    )
                })?;
                vt = join_elem_types(vt, vr.ty).unwrap_or(vt);
                if vt != vr.ty {
                    // try join for values
                    vt = join_elem_types(vt, vr.ty).ok_or_else(|| {
                        err(
                            format!("dict values must share one type; found {vt} and {}", vr.ty),
                            *vspan,
                        )
                    })?;
                }
            }
            (kt, vt)
        }
    };
    check_hashable_key(key_ty, span, "dict")?;
    if val_ty == ir::Ty::File || val_ty == ir::Ty::Exception {
        return Err(err(
            format!("dict values of type {val_ty} are not supported"),
            span,
        ));
    }
    reject_exception_container_elem(val_ty, span, "dicts")?;
    let mut out_pairs = Vec::new();
    for (kr, kspan, vr, vspan) in pairs {
        let k = coerce(kr, key_ty, kspan, "dict key")?;
        let v = coerce(vr, val_ty, vspan, "dict value")?;
        out_pairs.push((k, v));
    }
    Ok(ir::Expr {
        ty: ir::dict_of(key_ty, val_ty),
        kind: ir::ExprKind::DictLit(out_pairs),
    })
}

fn lower_set_lit(
    items: &[ast::Expr],
    expected: Option<ir::Ty>,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if items.is_empty() {
        let elem = expected.ok_or_else(|| {
            err(
                "cannot infer the element type of an empty set; use set() with an \
                 annotation, e.g. 's: set[int] = set()'",
                span,
            )
        })?;
        check_hashable_key(elem, span, "set")?;
        return Ok(ir::Expr {
            ty: ir::set_of(elem),
            kind: ir::ExprKind::SetNew,
        });
    }
    let mut lowered = Vec::new();
    for item in items {
        lowered.push((lower_expr(item, ctx)?, item.span));
    }
    let elem = match expected {
        Some(e) => e,
        None => {
            let mut ty = lowered[0].0.ty;
            for (item, item_span) in &lowered[1..] {
                ty = join_elem_types(ty, item.ty).ok_or_else(|| {
                    err(
                        format!(
                            "set elements must share one type; found {} and {}",
                            ty, item.ty
                        ),
                        *item_span,
                    )
                })?;
            }
            ty
        }
    };
    check_hashable_key(elem, span, "set")?;
    let mut coerced = Vec::new();
    for (item, item_span) in lowered {
        coerced.push(coerce(item, elem, item_span, "set element")?);
    }
    Ok(ir::Expr {
        ty: ir::set_of(elem),
        kind: ir::ExprKind::SetLit(coerced),
    })
}

fn join_elem_types(a: ir::Ty, b: ir::Ty) -> Option<ir::Ty> {
    // Homogeneous closures: same params/ret and same capture env shape.
    // Erase func identity so CallClosure uses the code pointer from the heap
    // object (capture slots are still typed for unpack).
    if let (
        ir::Ty::Closure {
            params: p1,
            ret: r1,
            capture_tys: c1,
            ..
        },
        ir::Ty::Closure {
            params: p2,
            ret: r2,
            capture_tys: c2,
            ..
        },
    ) = (a, b)
    {
        if p1 == p2 && r1 == r2 && c1 == c2 {
            return Some(ir::closure_of_full(p1, *r1, c1, ""));
        }
        return None;
    }
    match (a, b) {
        _ if a == b => Some(a),
        (ir::Ty::Any, _) | (_, ir::Ty::Any) => Some(ir::Ty::Any),
        (ir::Ty::Float, ir::Ty::Int)
        | (ir::Ty::Int, ir::Ty::Float)
        | (ir::Ty::Float, ir::Ty::Bool)
        | (ir::Ty::Bool, ir::Ty::Float) => Some(ir::Ty::Float),
        (ir::Ty::Int, ir::Ty::Bool) | (ir::Ty::Bool, ir::Ty::Int) => Some(ir::Ty::Int),
        // Grow optionals/unions in homogeneous containers (list/dict values).
        _ if a == ir::Ty::None
            || b == ir::Ty::None
            || matches!(a, ir::Ty::Union(_))
            || matches!(b, ir::Ty::Union(_)) =>
        {
            Some(join_types(a, b))
        }
        _ => Option::None,
    }
}

/// Require plain (non-`*`) positional args — used by builtins and methods.
fn require_plain_args<'a>(
    args: &'a [ast::PosArg],
    what: &str,
    span: Span,
) -> SResult<Vec<&'a ast::Expr>> {
    let mut out = Vec::with_capacity(args.len());
    for a in args {
        match a {
            ast::PosArg::Pos(e) => out.push(e),
            ast::PosArg::Star(_) => {
                return Err(err(
                    format!("*{what} unpacking is not supported here"),
                    span,
                ));
            }
        }
    }
    Ok(out)
}

fn lower_arg_expr(
    arg: &ast::Expr,
    expected: ir::Ty,
    what: &str,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let a = if let (ast::ExprKind::ListLit(items), ir::Ty::List(elem)) = (&arg.kind, expected) {
        lower_list_lit(items, Some(*elem), arg.span, ctx)?
    } else {
        lower_expr(arg, ctx)?
    };
    coerce(a, expected, arg.span, what)
}

/// Type-check and lower a call to a function with a known signature.
/// `extra_leading`: capture values for nested functions (prepended to IR args).
#[allow(clippy::too_many_arguments)]
fn lower_call_with_sig(
    display: &str,
    ir_name: String,
    sig: &FuncSig,
    args: &[ast::PosArg],
    keywords: &[ast::Keyword],
    kwargs: Option<&ast::Expr>,
    span: Span,
    ctx: &mut FnCtx,
    extra_leading: &[ir::Expr],
) -> SResult<ir::Expr> {
    // Generator function call → MakeGenerator (resume body is `ir_name`).
    // Leading captures (cells/values) become the first frame locals.
    if sig.is_generator {
        if kwargs.is_some() || !keywords.is_empty() {
            return Err(err(
                "generator calls do not support keyword arguments in this subset",
                span,
            ));
        }
        let mut arg_irs: Vec<ir::Expr> = extra_leading.to_vec();
        for (i, a) in args.iter().enumerate() {
            match a {
                ast::PosArg::Pos(e) => {
                    let expected = sig
                        .params
                        .get(i)
                        .map(|p| p.ty)
                        .ok_or_else(|| err("too many arguments for generator", span))?;
                    arg_irs.push(lower_arg_expr(
                        e,
                        expected,
                        &format!("argument {} of '{display}'", i + 1),
                        ctx,
                    )?);
                }
                ast::PosArg::Star(_) => {
                    return Err(err(
                        "starred arguments not supported for generator calls yet",
                        span,
                    ));
                }
            }
        }
        let user_args = arg_irs.len() - extra_leading.len();
        if user_args != sig.params.len() {
            // fill defaults
            for p in sig.params.iter().skip(user_args) {
                if let Some(d) = &p.default {
                    arg_irs.push(lower_expr(d, ctx)?);
                } else {
                    return Err(err(
                        format!(
                            "'{display}' expected {} argument(s), got {}",
                            sig.params.len(),
                            user_args
                        ),
                        span,
                    ));
                }
            }
        }
        let yty = sig.yield_ty.unwrap_or(ir::Ty::Int);
        // Prefer exact frame size recorded after lowering the resume function;
        // fall back to a generous estimate if call precedes that (nested order).
        let nlocals = if sig.gen_frame_slots > 0 {
            sig.gen_frame_slots
        } else {
            (arg_irs.len() as i64) + 128
        };
        return Ok(ir::Expr {
            ty: ir::generator_of(yty),
            kind: ir::ExprKind::MakeGenerator {
                func: ir_name,
                code_from: None,
                args: arg_irs,
                nlocals,
            },
        });
    }

    let n = sig.params.len();
    let has_vararg = sig.vararg.is_some();
    let has_kwarg = sig.kwarg.is_some();

    // Expand positionals and *unpacks into a sequence of IR exprs for fixed
    // params, plus a list of IR exprs that feed *args.
    let mut fixed_slots: Vec<Option<ir::Expr>> = (0..n).map(|_| None).collect();
    let mut filled = vec![false; n];
    let mut vararg_items: Vec<ir::Expr> = Vec::new();
    let mut positional_count = 0usize; // how many fixed slots filled by position
    let mut star_prelude: Vec<ir::Stmt> = Vec::new();

    for arg in args {
        match arg {
            ast::PosArg::Pos(e) => {
                if positional_count < n {
                    let expected = sig.params[positional_count].ty;
                    let a = lower_arg_expr(
                        e,
                        expected,
                        &format!("argument {} of '{display}'", positional_count + 1),
                        ctx,
                    )?;
                    fixed_slots[positional_count] = Some(a);
                    filled[positional_count] = true;
                    positional_count += 1;
                } else if has_vararg {
                    let elem = sig.vararg.as_ref().unwrap().ty;
                    let a = lower_arg_expr(
                        e,
                        elem,
                        &format!(
                            "*{} element of '{display}'",
                            sig.vararg.as_ref().unwrap().name
                        ),
                        ctx,
                    )?;
                    vararg_items.push(a);
                } else {
                    return Err(err(
                        format!(
                            "function '{display}' takes {} argument(s) but more were given",
                            n
                        ),
                        e.span,
                    ));
                }
            }
            ast::PosArg::Star(e) => {
                let seq = lower_expr(e, ctx)?;
                let elem = match seq.ty {
                    ir::Ty::List(el) => *el,
                    other => {
                        return Err(err(
                            format!("* unpacking expects a list, found {other}"),
                            e.span,
                        ));
                    }
                };
                let remaining_fixed = n.saturating_sub(positional_count);
                let seq_t = ctx.fresh_temp("star", seq.ty);
                star_prelude.push(ir::Stmt::Assign {
                    name: seq_t.clone(),
                    value: seq,
                });
                let len_e = ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Len(Box::new(ir::Expr {
                        ty: ir::list_of(elem),
                        kind: ir::ExprKind::Local(seq_t.clone()),
                    })),
                };
                if remaining_fixed == 0 {
                    if !has_vararg {
                        return Err(err(
                            format!(
                                "function '{display}' takes {} argument(s); \
                                 cannot *unpack extra values",
                                n
                            ),
                            e.span,
                        ));
                    }
                    let want = sig.vararg.as_ref().unwrap().ty;
                    if elem != want {
                        return Err(err(
                            format!(
                                "* unpacking element type {elem} does not match \
                                 *{}: {want}",
                                sig.vararg.as_ref().unwrap().name
                            ),
                            e.span,
                        ));
                    }
                    // entire list goes to *args
                    vararg_items.push(ir::Expr {
                        ty: ir::list_of(elem),
                        kind: ir::ExprKind::Local(seq_t),
                    });
                } else {
                    let min_needed = remaining_fixed as i64;
                    let check = if has_vararg {
                        ir::Stmt::If {
                            branches: vec![(
                                ir::Expr {
                                    ty: ir::Ty::Bool,
                                    kind: ir::ExprKind::Binary {
                                        op: ir::BinOp::Lt,
                                        left: Box::new(len_e.clone()),
                                        right: Box::new(int_const(min_needed)),
                                    },
                                },
                                vec![ir::Stmt::Die(format!(
                                    "TypeError: {display}() missing arguments after * unpack"
                                ))],
                            )],
                            orelse: vec![],
                        }
                    } else {
                        ir::Stmt::If {
                            branches: vec![(
                                ir::Expr {
                                    ty: ir::Ty::Bool,
                                    kind: ir::ExprKind::Binary {
                                        op: ir::BinOp::Ne,
                                        left: Box::new(len_e.clone()),
                                        right: Box::new(int_const(min_needed)),
                                    },
                                },
                                vec![ir::Stmt::Die(format!(
                                    "TypeError: {display}() argument count after * unpack mismatch"
                                ))],
                            )],
                            orelse: vec![],
                        }
                    };
                    star_prelude.push(check);
                    for i in 0..remaining_fixed {
                        let expected = sig.params[positional_count].ty;
                        if elem != expected
                            && !(expected == ir::Ty::Float
                                && matches!(elem, ir::Ty::Int | ir::Ty::Bool))
                            && !(expected == ir::Ty::Int && elem == ir::Ty::Bool)
                        {
                            return Err(err(
                                format!(
                                    "* unpacking element type {elem} does not match \
                                     parameter type {expected}"
                                ),
                                e.span,
                            ));
                        }
                        let item = ir::Expr {
                            ty: elem,
                            kind: ir::ExprKind::Index {
                                base: Box::new(ir::Expr {
                                    ty: ir::list_of(elem),
                                    kind: ir::ExprKind::Local(seq_t.clone()),
                                }),
                                index: Box::new(int_const(i as i64)),
                            },
                        };
                        let item = coerce(
                            item,
                            expected,
                            e.span,
                            &format!("argument {} of '{display}'", positional_count + 1),
                        )?;
                        let tmp = ctx.fresh_temp("sarg", expected);
                        star_prelude.push(ir::Stmt::Assign {
                            name: tmp.clone(),
                            value: item,
                        });
                        fixed_slots[positional_count] = Some(ir::Expr {
                            ty: expected,
                            kind: ir::ExprKind::Local(tmp),
                        });
                        filled[positional_count] = true;
                        positional_count += 1;
                    }
                    if has_vararg {
                        let want = sig.vararg.as_ref().unwrap().ty;
                        if elem != want {
                            return Err(err(
                                format!(
                                    "* unpacking element type {elem} does not match \
                                     *{}: {want}",
                                    sig.vararg.as_ref().unwrap().name
                                ),
                                e.span,
                            ));
                        }
                        let rest = ir::Expr {
                            ty: ir::list_of(elem),
                            kind: ir::ExprKind::Slice {
                                base: Box::new(ir::Expr {
                                    ty: ir::list_of(elem),
                                    kind: ir::ExprKind::Local(seq_t.clone()),
                                }),
                                lo: Box::new(int_const(remaining_fixed as i64)),
                                hi: Box::new(int_const(i64::MIN)),
                                step: Box::new(int_const(1)),
                            },
                        };
                        let rest_t = ctx.fresh_temp("srest", rest.ty);
                        star_prelude.push(ir::Stmt::Assign {
                            name: rest_t.clone(),
                            value: rest,
                        });
                        vararg_items.push(ir::Expr {
                            ty: ir::list_of(elem),
                            kind: ir::ExprKind::Local(rest_t),
                        });
                    }
                }
            }
        }
    }

    // Keywords for fixed params; extras go to **kwargs.
    let mut kwarg_pairs: Vec<(ir::Expr, ir::Expr)> = Vec::new();
    for kw in keywords {
        if let Some(idx) = sig.params.iter().position(|p| p.name == kw.name) {
            if filled[idx] {
                return Err(err(
                    format!(
                        "function '{display}' got multiple values for argument '{name}'",
                        name = kw.name
                    ),
                    kw.name_span,
                ));
            }
            let expected = sig.params[idx].ty;
            let a = lower_arg_expr(
                &kw.value,
                expected,
                &format!("argument '{name}' of '{display}'", name = kw.name),
                ctx,
            )?;
            fixed_slots[idx] = Some(a);
            filled[idx] = true;
        } else if has_kwarg {
            let val_ty = sig.kwarg.as_ref().unwrap().ty;
            let v = lower_arg_expr(
                &kw.value,
                val_ty,
                &format!(
                    "**{} value of '{display}'",
                    sig.kwarg.as_ref().unwrap().name
                ),
                ctx,
            )?;
            let k = ir::Expr {
                ty: ir::Ty::Str,
                kind: ir::ExprKind::ConstStr(kw.name.clone()),
            };
            kwarg_pairs.push((k, v));
        } else {
            return Err(err(
                format!(
                    "function '{display}' got an unexpected keyword argument '{name}'",
                    name = kw.name
                ),
                kw.name_span,
            ));
        }
    }

    // **kwargs mapping unpack
    let mut kwarg_dict_extra: Option<ir::Expr> = None;
    if let Some(kd) = kwargs {
        let d = lower_expr(kd, ctx)?;
        match d.ty {
            ir::Ty::Dict { key, value } if *key == ir::Ty::Str => {
                if has_kwarg {
                    let want = sig.kwarg.as_ref().unwrap().ty;
                    if *value != want {
                        return Err(err(
                            format!(
                                "** unpacking value type {value} does not match **{}: {want}",
                                sig.kwarg.as_ref().unwrap().name
                            ),
                            kd.span,
                        ));
                    }
                    // Merge: for each still-unfilled fixed param, try dict get.
                    // Then remaining keys must only go to kwargs — without full
                    // dynamic key scan, we only support **d when no fixed params
                    // remain unfilled OR all unfilled fixed have defaults and
                    // **d feeds kwargs only (no overlap). Simpler rule:
                    // **d only fills kwargs dict; fixed params must already be filled.
                    for (i, p) in sig.params.iter().enumerate() {
                        if !filled[i] {
                            // try to pull from d via ConstStr key — runtime KeyError if missing and no default
                            // Use DictGet with a sentinel? Better: DictPop / index.
                            // If has default, use DictGet(key, default); else Index.
                            let key_e = ir::Expr {
                                ty: ir::Ty::Str,
                                kind: ir::ExprKind::ConstStr(p.name.clone()),
                            };
                            if let Some(def) = &p.default {
                                let def_v = lower_arg_expr(
                                    def,
                                    p.ty,
                                    &format!("default for parameter '{}' of '{display}'", p.name),
                                    ctx,
                                )?;
                                let got = ir::Expr {
                                    ty: p.ty,
                                    kind: ir::ExprKind::DictGet {
                                        dict: Box::new(d.clone()),
                                        key: Box::new(key_e),
                                        default: Box::new(def_v),
                                    },
                                };
                                fixed_slots[i] = Some(got);
                                filled[i] = true;
                            } else {
                                let got = ir::Expr {
                                    ty: p.ty,
                                    kind: ir::ExprKind::Index {
                                        base: Box::new(d.clone()),
                                        index: Box::new(key_e),
                                    },
                                };
                                // coerce if needed - type already matches want
                                fixed_slots[i] = Some(got);
                                filled[i] = true;
                            }
                        }
                    }
                    kwarg_dict_extra = Some(d);
                } else {
                    // No **param: **d must supply remaining fixed params by name only.
                    for (i, p) in sig.params.iter().enumerate() {
                        if filled[i] {
                            continue;
                        }
                        if *value != p.ty {
                            return Err(err(
                                format!(
                                    "** unpacking value type {value} does not match \
                                     parameter '{}': {}",
                                    p.name, p.ty
                                ),
                                kd.span,
                            ));
                        }
                        let key_e = ir::Expr {
                            ty: ir::Ty::Str,
                            kind: ir::ExprKind::ConstStr(p.name.clone()),
                        };
                        if let Some(def) = &p.default {
                            let def_v = lower_arg_expr(
                                def,
                                p.ty,
                                &format!("default for parameter '{}' of '{display}'", p.name),
                                ctx,
                            )?;
                            fixed_slots[i] = Some(ir::Expr {
                                ty: p.ty,
                                kind: ir::ExprKind::DictGet {
                                    dict: Box::new(d.clone()),
                                    key: Box::new(key_e),
                                    default: Box::new(def_v),
                                },
                            });
                        } else {
                            fixed_slots[i] = Some(ir::Expr {
                                ty: p.ty,
                                kind: ir::ExprKind::Index {
                                    base: Box::new(d.clone()),
                                    index: Box::new(key_e),
                                },
                            });
                        }
                        filled[i] = true;
                    }
                }
            }
            other => {
                return Err(err(
                    format!("** unpacking expects dict[str, T], found {other}"),
                    kd.span,
                ));
            }
        }
    } else if kwargs.is_some() {
        // handled
    }

    let mut lowered_args: Vec<ir::Expr> = extra_leading.to_vec();
    for (i, p) in sig.params.iter().enumerate() {
        if let Some(a) = fixed_slots[i].take() {
            lowered_args.push(a);
            continue;
        }
        if let Some(def) = &p.default {
            let a = lower_arg_expr(
                def,
                p.ty,
                &format!(
                    "default for parameter '{name}' of '{display}'",
                    name = p.name
                ),
                ctx,
            )?;
            lowered_args.push(a);
        } else {
            return Err(err(
                format!(
                    "function '{display}' missing required argument '{name}'",
                    name = p.name
                ),
                span,
            ));
        }
    }

    if let Some(va) = &sig.vararg {
        let list_ty = ir::list_of(va.ty);
        // Pack vararg_items: mix of scalar elems and whole lists (from *unpack).
        let packed = if vararg_items.is_empty() {
            ir::Expr {
                ty: list_ty,
                kind: ir::ExprKind::ListLit(vec![]),
            }
        } else {
            // Start with empty or first list, concat/append rest.
            let mut acc: Option<ir::Expr> = None;
            for item in vararg_items {
                if item.ty == list_ty {
                    acc = Some(match acc {
                        None => item,
                        Some(a) => ir::Expr {
                            ty: list_ty,
                            kind: ir::ExprKind::Binary {
                                op: ir::BinOp::Add,
                                left: Box::new(a),
                                right: Box::new(item),
                            },
                        },
                    });
                } else {
                    // scalar: append via list + [item]
                    let one = ir::Expr {
                        ty: list_ty,
                        kind: ir::ExprKind::ListLit(vec![item]),
                    };
                    acc = Some(match acc {
                        None => one,
                        Some(a) => ir::Expr {
                            ty: list_ty,
                            kind: ir::ExprKind::Binary {
                                op: ir::BinOp::Add,
                                left: Box::new(a),
                                right: Box::new(one),
                            },
                        },
                    });
                }
            }
            acc.unwrap()
        };
        lowered_args.push(packed);
    }

    if let Some(kw) = &sig.kwarg {
        let dict_ty = ir::dict_of(ir::Ty::Str, kw.ty);
        let base = if kwarg_pairs.is_empty() {
            ir::Expr {
                ty: dict_ty,
                kind: ir::ExprKind::DictNew,
            }
        } else {
            ir::Expr {
                ty: dict_ty,
                kind: ir::ExprKind::DictLit(kwarg_pairs),
            }
        };
        let dict_expr = if let Some(extra) = kwarg_dict_extra {
            // Merge explicit kwargs over **d: start from **d, then set pairs.
            // Without a dict-merge primitive, if both present and pairs non-empty,
            // build from pairs only when extra is empty-keys case; else error if both.
            if matches!(base.kind, ir::ExprKind::DictNew) {
                extra
            } else if matches!(&extra.kind, ir::ExprKind::DictNew)
                || matches!(&extra.kind, ir::ExprKind::DictLit(p) if p.is_empty())
            {
                base
            } else {
                // Prefer explicit keyword pairs; ignore overlapping ** keys (CPython
                // errors on duplicates). Documented subset: **d alone or keywords alone.
                return Err(err(
                    format!(
                        "function '{display}': combining keyword arguments with ** unpacking \
                         is not supported yet; use one or the other"
                    ),
                    span,
                ));
            }
        } else {
            base
        };
        lowered_args.push(dict_expr);
    }

    let call = ir::Expr {
        ty: sig.ret,
        kind: ir::ExprKind::Call {
            func: ir_name,
            args: lowered_args,
        },
    };
    if star_prelude.is_empty() {
        Ok(call)
    } else {
        Ok(ir::Expr {
            ty: sig.ret,
            kind: ir::ExprKind::Block {
                stmts: star_prelude,
                result: Box::new(call),
            },
        })
    }
}

/// Resolve a value attribute on a parent package that is not yet in `mods`.
/// Returns `(origin_module, origin_name, ty)` for the IR global load.
/// `for_module_body`: child module top-level (partial only); otherwise deferred
/// full surface including re-exports (function bodies after parent finishes).
fn resolve_parent_value(
    ctx: &FnCtx,
    parent: &str,
    name: &str,
    for_module_body: bool,
) -> Option<(String, String, ir::Ty)> {
    if for_module_body {
        if let Some(ty) = ctx
            .mctx
            .partial_parent_globals(parent)
            .and_then(|g| g.get(name).copied())
        {
            return Some((parent.to_string(), name.to_string(), ty));
        }
        if ctx
            .mctx
            .partial_parent_reexports(parent)
            .is_some_and(|s| s.contains(name))
        {
            return resolve_reexport_value(ctx, parent, name);
        }
        return None;
    }

    // Deferred: last from-import re-export wins over earlier own assign.
    if ctx
        .mctx
        .reexport_origins
        .get(parent)
        .is_some_and(|m| m.contains_key(name))
        && let Some(got) = resolve_reexport_value(ctx, parent, name)
    {
        return Some(got);
    }
    if let Some(ty) = ctx
        .mctx
        .package_final_values
        .get(parent)
        .and_then(|g| g.get(name).copied())
        .or_else(|| {
            ctx.mctx
                .partial_parent_globals(parent)
                .and_then(|g| g.get(name).copied())
        })
    {
        return Some((parent.to_string(), name.to_string(), ty));
    }
    resolve_reexport_value(ctx, parent, name)
}

/// Follow `reexport_origins` (and finished `mods` reexports) to a loadable value.
fn resolve_reexport_value(
    ctx: &FnCtx,
    module: &str,
    name: &str,
) -> Option<(String, String, ir::Ty)> {
    let mut m = module.to_string();
    let mut n = name.to_string();
    for _ in 0..32 {
        if let Some((om, on)) = ctx
            .mctx
            .reexport_origins
            .get(&m)
            .and_then(|map| map.get(&n))
            .cloned()
        {
            m = om;
            n = on;
            continue;
        }
        if let Some(data) = ctx.mctx.mods.get(&m) {
            if let Some((om, on)) = data.reexports.get(&n).cloned() {
                m = om;
                n = on;
                continue;
            }
            if let Some(ty) = data.globals.get(&n).copied() {
                return Some((m, n, ty));
            }
            return None;
        }
        if let Some(ty) = ctx
            .mctx
            .package_final_values
            .get(&m)
            .and_then(|g| g.get(&n))
            .copied()
        {
            return Some((m, n, ty));
        }
        return None;
    }
    None
}

/// Resolve a parent function while the parent is mid-init / not in `mods`.
/// Returns `(ir_func_name, sig)`.
fn resolve_parent_func(
    ctx: &FnCtx,
    parent: &str,
    name: &str,
    for_module_body: bool,
) -> Option<(String, FuncSig)> {
    if for_module_body {
        if ctx
            .mctx
            .partial_parent_funcs(parent)
            .is_some_and(|s| s.contains(name))
            && let Some(sig) = ctx
                .mctx
                .all_own_funcs
                .get(parent)
                .and_then(|f| f.get(name).cloned())
        {
            return Some((qual(parent, name), sig));
        }
        if ctx
            .mctx
            .partial_parent_reexports(parent)
            .is_some_and(|s| s.contains(name))
        {
            return resolve_reexport_func(ctx, parent, name);
        }
        return None;
    }

    // Deferred: re-export last wins.
    if ctx
        .mctx
        .reexport_origins
        .get(parent)
        .is_some_and(|m| m.contains_key(name))
        && let Some(got) = resolve_reexport_func(ctx, parent, name)
    {
        return Some(got);
    }
    if let Some(sig) = ctx
        .mctx
        .all_own_funcs
        .get(parent)
        .and_then(|f| f.get(name).cloned())
    {
        return Some((qual(parent, name), sig));
    }
    resolve_reexport_func(ctx, parent, name)
}

fn resolve_reexport_func(ctx: &FnCtx, module: &str, name: &str) -> Option<(String, FuncSig)> {
    let mut m = module.to_string();
    let mut n = name.to_string();
    for _ in 0..32 {
        if let Some((om, on)) = ctx
            .mctx
            .reexport_origins
            .get(&m)
            .and_then(|map| map.get(&n))
            .cloned()
        {
            m = om;
            n = on;
            continue;
        }
        if let Some(data) = ctx.mctx.mods.get(&m) {
            if let Some((om, on)) = data.reexports.get(&n).cloned() {
                m = om;
                n = on;
                continue;
            }
            if let Some(sig) = data.funcs.get(&n).cloned() {
                return Some((qual(&m, &n), sig));
            }
            return None;
        }
        if let Some(sig) = ctx
            .mctx
            .all_own_funcs
            .get(&m)
            .and_then(|f| f.get(&n))
            .cloned()
        {
            return Some((qual(&m, &n), sig));
        }
        return None;
    }
    None
}

/// `module.func(args)` — a call into another module (including re-exports).
fn lower_module_call(
    real: &str,
    method: &str,
    method_span: Span,
    args: &[ast::PosArg],
    keywords: &[ast::Keyword],
    kwargs: Option<&ast::Expr>,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    // `mod.Class(...)` construction.
    if let Some(class_id) = lookup_class_in_module(real, method).or_else(|| {
        ctx.mctx
            .mods
            .get(real)
            .and_then(|d| d.classes.get(method).copied())
    }) {
        return lower_class_construct(class_id, method, args, keywords, kwargs, method_span, ctx);
    }
    // json.dumps is polymorphic: special-case before signature matching.
    if real == "json" && method == "dumps" {
        if kwargs.is_some() {
            return Err(err("json.dumps() does not take **kwargs", method_span));
        }
        if !keywords.is_empty() {
            return Err(err(
                "json.dumps() does not take keyword arguments",
                keywords[0].name_span,
            ));
        }
        let plain = require_plain_args(args, "json.dumps", method_span)?;
        if plain.len() != 1 {
            return Err(err(
                format!(
                    "json.dumps() takes exactly one argument ({} given)",
                    plain.len()
                ),
                method_span,
            ));
        }
        return lower_json_dumps(plain[0], ctx);
    }

    let Some(data) = ctx.mctx.mods.get(real) else {
        // Parent package mid-init has no ModuleData yet.
        if is_strict_package_prefix(real, ctx.mctx.module) {
            if let Some((ir_name, sig)) =
                resolve_parent_func(ctx, real, method, /*for_module_body*/ ctx.is_entry)
            {
                return lower_call_with_sig(
                    method,
                    ir_name,
                    &sig,
                    args,
                    keywords,
                    kwargs,
                    method_span,
                    ctx,
                    &[],
                );
            }
            return Err(err(
                format!(
                    "cannot import name '{method}' from partially initialized \
                     package '{real}' (most likely due to a circular import)"
                ),
                method_span,
            ));
        }
        return Err(err(
            format!("module '{real}' has no attribute '{method}'"),
            method_span,
        ));
    };
    let (om, on) = data
        .reexports
        .get(method)
        .cloned()
        .unwrap_or_else(|| (real.to_string(), method.to_string()));
    if let Some(sig) = data.funcs.get(method).cloned().or_else(|| {
        ctx.mctx
            .mods
            .get(&om)
            .and_then(|d| d.funcs.get(&on).cloned())
    }) {
        return lower_call_with_sig(
            method,
            qual(&om, &on),
            &sig,
            args,
            keywords,
            kwargs,
            method_span,
            ctx,
            &[],
        );
    }
    if data.globals.contains_key(method) {
        return Err(err(
            format!("'{real}.{method}' is a value, not a function"),
            method_span,
        ));
    }
    Err(err(
        format!("module '{real}' has no attribute '{method}'"),
        method_span,
    ))
}

fn lower_json_dumps(arg: &ast::Expr, ctx: &mut FnCtx) -> SResult<ir::Expr> {
    let v = lower_expr(arg, ctx)?;
    if !json_dumps_supported(v.ty) {
        return Err(err(
            format!(
                "json.dumps() does not support type {} (supported: int, float, bool, str, \
                 list/dict of those with str keys)",
                v.ty
            ),
            arg.span,
        ));
    }
    Ok(ir::Expr {
        ty: ir::Ty::Str,
        kind: ir::ExprKind::JsonDumps(Box::new(v)),
    })
}

fn json_dumps_supported(ty: ir::Ty) -> bool {
    match ty {
        ir::Ty::Int | ir::Ty::Float | ir::Ty::Bool | ir::Ty::Str => true,
        ir::Ty::List(e) => json_dumps_supported(*e),
        ir::Ty::Dict { key, value } => *key == ir::Ty::Str && json_dumps_supported(*value),
        _ => false,
    }
}

/// Drop flow-sensitive refinements for cell-backed names that a nested call
/// might overwrite via `nonlocal` (caller must not keep a stale concrete peel).
fn invalidate_cell_refinements_for_call(ctx: &mut FnCtx, info: &NestedFnInfo) {
    for (i, (name, _)) in info.captures.iter().enumerate() {
        if info.capture_is_cell.get(i).copied().unwrap_or(false) {
            ctx.type_refinements.remove(name);
        }
    }
}

/// `fs[0](x)` / general callable expression: first arg of synthetic `.call`.
fn lower_value_call(
    args: &[ast::PosArg],
    keywords: &[ast::Keyword],
    kwargs: Option<&ast::Expr>,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.is_empty() {
        return Err(err("internal: .call missing callee", span));
    }
    let callee_ast = match &args[0] {
        ast::PosArg::Pos(e) => e,
        ast::PosArg::Star(_) => {
            return Err(err("cannot call a starred expression", span));
        }
    };
    let callee = lower_expr(callee_ast, ctx)?;
    let user_args = &args[1..];
    match callee.ty {
        ir::Ty::Closure { .. } => {
            lower_call_closure_value(&callee, user_args, keywords, kwargs, span, ctx)
        }
        other => Err(err(
            format!("'{other}' object is not callable"),
            callee_ast.span,
        )),
    }
}

/// Invoke a first-class closure (or generator function closure) value.
fn lower_call_closure_value(
    clos_expr: &ir::Expr,
    args: &[ast::PosArg],
    keywords: &[ast::Keyword],
    kwargs: Option<&ast::Expr>,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let ir::Ty::Closure {
        params: cparams,
        ret: cret,
        capture_tys,
        func: ir_func,
    } = clos_expr.ty
    else {
        return Err(err("internal: expected closure type", span));
    };
    invalidate_all_cell_refinements(ctx);
    let mut arg_irs = Vec::new();
    for (i, a) in args.iter().enumerate() {
        match a {
            ast::PosArg::Pos(e) => {
                let expected = cparams.get(i).copied().unwrap_or(ir::Ty::None);
                let v = lower_expr(e, ctx)?;
                let v = coerce(v, expected, e.span, "argument")?;
                arg_irs.push(v);
            }
            ast::PosArg::Star(_) => {
                return Err(err(
                    "starred arguments not supported when calling a closure value",
                    span,
                ));
            }
        }
    }
    if !keywords.is_empty() || kwargs.is_some() {
        return Err(err(
            "keyword arguments not supported when calling a closure value",
            span,
        ));
    }
    if arg_irs.len() < cparams.len() {
        let defs: Option<Vec<(ir::Ty, Option<ast::Expr>)>> = ctx
            .nested_funcs
            .values()
            .find(|i| i.ir_name == ir_func)
            .map(|i| {
                i.sig
                    .params
                    .iter()
                    .map(|p| (p.ty, p.default.clone()))
                    .collect()
            })
            .or_else(|| lookup_closure_defaults(ir_func));
        if let Some(defs) = defs {
            for i in arg_irs.len()..cparams.len() {
                match defs.get(i) {
                    Some((ty, Some(d))) => {
                        arg_irs.push(lower_closure_default(d, *ty, span, ctx)?);
                    }
                    _ => {
                        return Err(err(
                            format!(
                                "closure takes {} argument(s) but {} were given",
                                cparams.len(),
                                arg_irs.len()
                            ),
                            span,
                        ));
                    }
                }
            }
        } else {
            return Err(err(
                format!(
                    "closure takes {} argument(s) but {} were given",
                    cparams.len(),
                    arg_irs.len()
                ),
                span,
            ));
        }
    } else if arg_irs.len() > cparams.len() {
        return Err(err(
            format!(
                "closure takes {} argument(s) but {} were given",
                cparams.len(),
                arg_irs.len()
            ),
            span,
        ));
    }
    // Generator function value: MakeGenerator with captures + args as frame locals.
    if let ir::Ty::Generator { yield_ty } = *cret {
        let is_gen = ctx
            .nested_funcs
            .values()
            .find(|i| i.ir_name == ir_func)
            .map(|i| i.sig.is_generator)
            .unwrap_or(true);
        if is_gen {
            let mut frame_args = Vec::new();
            // Unpack captures from the closure env into frame slots.
            for (i, cty) in capture_tys.iter().enumerate() {
                frame_args.push(ir::Expr {
                    ty: *cty,
                    kind: ir::ExprKind::ClosureCap {
                        closure: Box::new(clos_expr.clone()),
                        index: i as i64,
                        cap_ty: *cty,
                    },
                });
            }
            frame_args.extend(arg_irs);
            let nlocals = (frame_args.len() as i64) + 128;
            // Erased func name (homogeneous list of gens) → code from closure.
            let code_from = if ir_func.is_empty() {
                Some(Box::new(clos_expr.clone()))
            } else {
                None
            };
            return Ok(ir::Expr {
                ty: ir::generator_of(*yield_ty),
                kind: ir::ExprKind::MakeGenerator {
                    func: ir_func.to_string(),
                    code_from,
                    args: frame_args,
                    nlocals,
                },
            });
        }
    }
    Ok(ir::Expr {
        ty: *cret,
        kind: ir::ExprKind::CallClosure {
            closure: Box::new(clos_expr.clone()),
            args: arg_irs,
            capture_tys: capture_tys.to_vec(),
            func: ir_func.to_string(),
        },
    })
}

/// Conservatively clear all cell refinements (unknown callees / CallClosure).
fn invalidate_all_cell_refinements(ctx: &mut FnCtx) {
    let names: Vec<String> = ctx.cell_locals.keys().cloned().collect();
    for n in names {
        ctx.type_refinements.remove(&n);
    }
}

fn lower_call(
    func: &str,
    func_span: Span,
    args: &[ast::PosArg],
    keywords: &[ast::Keyword],
    kwargs: Option<&ast::Expr>,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    // Synthetic value-call from parser: `.call(callee, *user_args)`.
    if func == ".call" {
        return lower_value_call(args, keywords, kwargs, span, ctx);
    }
    if func == "super" {
        if !args.is_empty() || !keywords.is_empty() || kwargs.is_some() {
            return Err(err(
                "two-arg super() is not supported yet; use zero-arg super().method(...)",
                span,
            ));
        }
        return Err(err(
            "super() must be used as super().method(...); bare super() values are not supported",
            span,
        ));
    }
    // Inside @classmethod: `cls(...)` constructs the owning class.
    if let Some((cls_name, class_id)) = ctx.classmethod_cls.clone()
        && func == cls_name
    {
        return lower_class_construct(class_id, func, args, keywords, kwargs, span, ctx);
    }
    // Class construction: `Point(1, 2)` → allocate + `__init__`.
    // Bare name in current module, or imported class binding.
    if let Some(class_id) = lookup_class(func) {
        return lower_class_construct(class_id, func, args, keywords, kwargs, span, ctx);
    }
    if let Some(ImportBinding::Class(class_id)) = ctx
        .local_imports
        .get(func)
        .or_else(|| ctx.mctx.imports.get(func))
        .cloned()
    {
        return lower_class_construct(class_id, func, args, keywords, kwargs, span, ctx);
    }
    // `from m import C` may still be Symbol if registered as value export.
    if let Some(ImportBinding::Symbol { module, name }) = ctx
        .local_imports
        .get(func)
        .or_else(|| ctx.mctx.imports.get(func))
        .cloned()
        && let Some(class_id) = lookup_class_in_module(&module, &name)
    {
        return lower_class_construct(class_id, &name, args, keywords, kwargs, span, ctx);
    }
    // Call through a local/global of closure type first (local rebind shadows nested def).
    let closure_ty = ctx
        .locals
        .get(func)
        .copied()
        .or_else(|| ctx.globals.get(func).copied());
    // nested function in this function (only if name was not rebound to a local)
    if !ctx.locals.contains_key(func)
        && let Some(info) = ctx.nested_funcs.get(func).cloned()
    {
        // Nested body may CellStore captures; drop stale peels before the call.
        invalidate_cell_refinements_for_call(ctx, &info);
        let mut leading = Vec::new();
        for (i, (name, ty)) in info.captures.iter().enumerate() {
            let is_cell = info.capture_is_cell.get(i).copied().unwrap_or(false);
            if is_cell {
                let cell_name = format!(".cell.{name}");
                // Caller must have the cell (own local or capture param).
                if !ctx.locals.contains_key(&cell_name) {
                    return Err(err(
                        format!(
                            "cannot call nested function '{func}' from here: it captures \
                             cell '{name}' which is not available in this scope"
                        ),
                        span,
                    ));
                }
                leading.push(ir::Expr {
                    ty: ir::cell_of(*ty),
                    kind: ir::ExprKind::Local(cell_name),
                });
            } else if ctx.cell_locals.contains_key(name)
                && ctx.locals.contains_key(&format!(".cell.{name}"))
            {
                // Callee expects by-value but caller only has the cell — load it.
                let cell_name = format!(".cell.{name}");
                let cell = ir::Expr {
                    ty: ir::cell_of(*ty),
                    kind: ir::ExprKind::Local(cell_name),
                };
                leading.push(ir::Expr {
                    ty: *ty,
                    kind: ir::ExprKind::CellLoad(Box::new(cell)),
                });
            } else {
                let Some(local_ty) = ctx.locals.get(name).copied() else {
                    return Err(err(
                        format!(
                            "cannot call nested function '{func}': free variable '{name}' \
                             is not in scope here"
                        ),
                        span,
                    ));
                };
                if local_ty != *ty {
                    return Err(err(
                        format!(
                            "capture type mismatch for '{name}': expected {ty}, found {local_ty}"
                        ),
                        span,
                    ));
                }
                leading.push(ir::Expr {
                    ty: *ty,
                    kind: ir::ExprKind::Local(name.clone()),
                });
            }
        }
        return lower_call_with_sig(
            func,
            info.ir_name,
            &info.sig,
            args,
            keywords,
            kwargs,
            span,
            ctx,
            &leading,
        );
    }
    // Call through a local/global of closure type: `f(x)` where f is a closure value
    if let Some(ty) = closure_ty
        && let ir::Ty::Closure { .. } = ty
    {
        let clos_expr = if ctx.locals.contains_key(func) {
            ir::Expr {
                ty,
                kind: ir::ExprKind::Local(func.to_string()),
            }
        } else {
            ir::Expr {
                ty,
                kind: ir::ExprKind::GlobalLoad(ctx.own_global(func)),
            }
        };
        return lower_call_closure_value(&clos_expr, args, keywords, kwargs, span, ctx);
    }
    // Bound method value: `f = obj.m; f(args)`
    if let Some(ty) = closure_ty
        && let ir::Ty::BoundMethod {
            params,
            ret,
            func: direct,
            is_virtual: virt,
            class_id,
        } = ty
    {
        let bound = if ctx.locals.contains_key(func) {
            ir::Expr {
                ty,
                kind: ir::ExprKind::Local(func.to_string()),
            }
        } else {
            ir::Expr {
                ty,
                kind: ir::ExprKind::GlobalLoad(ctx.own_global(func)),
            }
        };
        let plain = require_plain_args(args, "bound method", span)?;
        if plain.len() != params.len() {
            return Err(err(
                format!(
                    "bound method takes {} argument(s) ({} given)",
                    params.len(),
                    plain.len()
                ),
                span,
            ));
        }
        if !keywords.is_empty() || kwargs.is_some() {
            return Err(err(
                "keyword arguments on bound methods are not supported yet",
                span,
            ));
        }
        let mut arg_irs = Vec::new();
        for (i, a) in plain.iter().enumerate() {
            let v = lower_expr(a, ctx)?;
            arg_irs.push(coerce(v, params[i], a.span, "bound method argument")?);
        }
        let mut candidates = Vec::new();
        if virt {
            for sid in subclasses_of(class_id) {
                // method short name is the last segment of direct... use resolve from class
                if let Some(info) = class_info(class_id) {
                    let mname = direct.rsplit('.').next().unwrap_or(direct);
                    if let Some(func) = resolve_method(sid, mname) {
                        candidates.push((sid, func));
                    }
                    let _ = info;
                }
            }
        }
        return Ok(ir::Expr {
            ty: *ret,
            kind: ir::ExprKind::CallBoundMethod {
                bound: Box::new(bound),
                args: arg_irs,
                direct_func: direct.to_string(),
                candidates,
                virtual_dispatch: virt,
            },
        });
    }
    // a function defined in this module
    if ctx.funcs().contains_key(func) {
        let sig = ctx.funcs().get(func).cloned().unwrap();
        return lower_call_with_sig(
            func,
            ctx.own_func(func),
            &sig,
            args,
            keywords,
            kwargs,
            span,
            ctx,
            &[],
        );
    }
    // a function pulled in by `from other import func` (incl. re-exports)
    if let Some(ImportBinding::Symbol { module, name }) = ctx
        .local_imports
        .get(func)
        .or_else(|| ctx.mctx.imports.get(func))
        .cloned()
    {
        if module == "json" && name == "dumps" {
            if kwargs.is_some() {
                return Err(err("json.dumps() does not take **kwargs", span));
            }
            if !keywords.is_empty() {
                return Err(err(
                    "json.dumps() does not take keyword arguments",
                    keywords[0].name_span,
                ));
            }
            let plain = require_plain_args(args, "json.dumps", span)?;
            if plain.len() != 1 {
                return Err(err(
                    format!(
                        "json.dumps() takes exactly one argument ({} given)",
                        plain.len()
                    ),
                    span,
                ));
            }
            return lower_json_dumps(plain[0], ctx);
        }
        if let Some(data) = ctx.mctx.mods.get(&module) {
            let (om, on) = data
                .reexports
                .get(&name)
                .cloned()
                .unwrap_or_else(|| (module.clone(), name.clone()));
            if let Some(sig) = data.funcs.get(&name).cloned().or_else(|| {
                ctx.mctx
                    .mods
                    .get(&om)
                    .and_then(|d| d.funcs.get(&on).cloned())
            }) {
                return lower_call_with_sig(
                    func,
                    qual(&om, &on),
                    &sig,
                    args,
                    keywords,
                    kwargs,
                    span,
                    ctx,
                    &[],
                );
            }
            return Err(err(
                format!("'{func}' is a value imported from '{module}', not a function"),
                func_span,
            ));
        }
        if is_strict_package_prefix(&module, ctx.mctx.module) {
            if let Some((ir_name, sig)) =
                resolve_parent_func(ctx, &module, &name, /*for_module_body*/ ctx.is_entry)
            {
                return lower_call_with_sig(
                    func,
                    ir_name,
                    &sig,
                    args,
                    keywords,
                    kwargs,
                    span,
                    ctx,
                    &[],
                );
            }
            return Err(err(
                format!(
                    "cannot import name '{name}' from partially initialized \
                     package '{module}' (most likely due to a circular import)"
                ),
                func_span,
            ));
        }
        return Err(err(
            format!("'{func}' is a value imported from '{module}', not a function"),
            func_span,
        ));
    }
    // a module alias used as if it were a function
    if ctx.module_alias(func).is_some() || ctx.sys_alias(func) {
        return Err(err(
            format!("'{func}' is a module, not a function"),
            func_span,
        ));
    }
    if kwargs.is_some() {
        return Err(err(format!("'{func}()' does not take **kwargs"), span));
    }
    // `enumerate(..., start=n)` is the only builtin keyword we accept here.
    if func != "enumerate"
        && let Some(kw) = keywords.first()
    {
        return Err(err(
            format!("'{func}()' does not take keyword arguments"),
            kw.name_span,
        ));
    }
    let plain = require_plain_args(args, func, span)?;
    let args = plain;
    {
        match func {
            "print" => Err(err(
                "print(...) does not return a value and cannot be used \
                     in an expression",
                span,
            )),
            "set" => {
                if args.is_empty() {
                    return Err(err(
                        "set() requires a type annotation on the target, e.g. \
                         's: set[int] = set()'",
                        span,
                    ));
                }
                if args.len() != 1 {
                    return Err(err(
                        format!("set() takes at most 1 argument ({} given)", args.len()),
                        span,
                    ));
                }
                let arg = lower_expr(args[0], ctx)?;
                lower_set_ctor(arg, args[0].span)
            }
            "list" => {
                if args.len() != 1 {
                    return Err(err(
                        format!(
                            "list() takes exactly one argument ({} given); \
                             empty list() is not supported — use []",
                            args.len()
                        ),
                        span,
                    ));
                }
                let arg = lower_expr(args[0], ctx)?;
                lower_list_ctor(arg, args[0].span)
            }
            "dict" => {
                if args.is_empty() {
                    return Err(err(
                        "dict() requires a type annotation on the target, e.g. \
                         'd: dict[str, int] = {}'",
                        span,
                    ));
                }
                if args.len() != 1 {
                    return Err(err(
                        format!("dict() takes at most 1 argument ({} given)", args.len()),
                        span,
                    ));
                }
                let arg = lower_expr(args[0], ctx)?;
                lower_dict_ctor(arg, args[0].span)
            }
            "tuple" => {
                if args.len() != 1 {
                    return Err(err(
                        format!("tuple() takes exactly one argument ({} given)", args.len()),
                        span,
                    ));
                }
                let arg = lower_expr(args[0], ctx)?;
                lower_tuple_ctor(arg, args[0].span)
            }
            "len" => {
                if args.len() != 1 {
                    return Err(err(
                        format!("len() takes exactly one argument ({} given)", args.len()),
                        span,
                    ));
                }
                let arg = lower_expr(args[0], ctx)?;
                if let ir::Ty::Class(id) = arg.ty {
                    if resolve_method(id, "__len__").is_some() {
                        let call =
                            lower_instance_method_call(arg, id, "__len__", args[0].span, &[], ctx)?;
                        if call.ty != ir::Ty::Int {
                            return Err(err("__len__ must return int", args[0].span));
                        }
                        return Ok(call);
                    }
                    return Err(err(
                        format!("object of type '{}' has no len()", arg.ty),
                        args[0].span,
                    ));
                }
                if !matches!(
                    arg.ty,
                    ir::Ty::Str
                        | ir::Ty::List(_)
                        | ir::Ty::Tuple(_)
                        | ir::Ty::Dict { .. }
                        | ir::Ty::Set(_)
                ) {
                    return Err(err(
                        format!("object of type '{}' has no len()", arg.ty),
                        args[0].span,
                    ));
                }
                Ok(ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Len(Box::new(arg)),
                })
            }
            "abs" => {
                if args.len() != 1 {
                    return Err(err(
                        format!("abs() takes exactly one argument ({} given)", args.len()),
                        span,
                    ));
                }
                let arg = lower_expr(args[0], ctx)?;
                // bool → int (abs(True) is 1); int/float keep their type
                let arg = match arg.ty {
                    ir::Ty::Bool => ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::BoolToInt(Box::new(arg)),
                    },
                    ir::Ty::Int | ir::Ty::Float => arg,
                    other => {
                        return Err(err(
                            format!("bad operand type for abs(): '{other}'"),
                            args[0].span,
                        ));
                    }
                };
                Ok(ir::Expr {
                    ty: arg.ty,
                    kind: ir::ExprKind::Abs(Box::new(arg)),
                })
            }
            "min" | "max" => {
                if args.len() == 1 {
                    // Iterable form: list of numbers (int/float/bool).
                    let arg = lower_expr(args[0], ctx)?;
                    let elem = match arg.ty {
                        ir::Ty::List(e) => *e,
                        other => {
                            return Err(err(
                                format!(
                                    "{func}() iterable form expects a list of numbers, found {other}"
                                ),
                                args[0].span,
                            ));
                        }
                    };
                    match elem {
                        ir::Ty::Int | ir::Ty::Float | ir::Ty::Bool => {
                            let kind = if func == "min" {
                                ir::ExprKind::MinList(Box::new(arg))
                            } else {
                                ir::ExprKind::MaxList(Box::new(arg))
                            };
                            Ok(ir::Expr { ty: elem, kind })
                        }
                        other => Err(err(
                            format!(
                                "{func}() is only supported for list[int], list[float], \
                                 and list[bool], found list[{other}]"
                            ),
                            args[0].span,
                        )),
                    }
                } else if args.len() == 2 {
                    let left = lower_expr(args[0], ctx)?;
                    let right = lower_expr(args[1], ctx)?;
                    let (left, right, ty) = unify_numeric(left, right, span, &format!("{func}()"))?;
                    let kind = if func == "min" {
                        ir::ExprKind::Min {
                            left: Box::new(left),
                            right: Box::new(right),
                        }
                    } else {
                        ir::ExprKind::Max {
                            left: Box::new(left),
                            right: Box::new(right),
                        }
                    };
                    Ok(ir::Expr { ty, kind })
                } else {
                    Err(err(
                        format!(
                            "{func}() takes 1 or 2 arguments ({} given); \
                             key=/default= are not supported yet",
                            args.len()
                        ),
                        span,
                    ))
                }
            }
            "sum" => {
                if args.len() != 1 {
                    return Err(err(
                        format!(
                            "sum() takes exactly 1 argument ({} given); \
                             start= is not supported yet",
                            args.len()
                        ),
                        span,
                    ));
                }
                let arg = lower_expr(args[0], ctx)?;
                let elem = match arg.ty {
                    ir::Ty::List(e) => *e,
                    other => {
                        return Err(err(
                            format!("sum() expects a list of numbers, found {other}"),
                            args[0].span,
                        ));
                    }
                };
                match elem {
                    ir::Ty::Int | ir::Ty::Float => Ok(ir::Expr {
                        ty: elem,
                        kind: ir::ExprKind::Sum(Box::new(arg)),
                    }),
                    other => Err(err(
                        format!(
                            "sum() is only supported for list[int] and list[float], \
                             found list[{other}]"
                        ),
                        args[0].span,
                    )),
                }
            }
            "sorted" => {
                if args.len() != 1 {
                    return Err(err(
                        format!(
                            "sorted() takes exactly 1 argument ({} given); \
                             key=/reverse= are not supported yet",
                            args.len()
                        ),
                        span,
                    ));
                }
                let arg = lower_expr(args[0], ctx)?;
                let elem = match arg.ty {
                    ir::Ty::List(e) => *e,
                    other => {
                        return Err(err(
                            format!("sorted() expects a list, found {other}"),
                            args[0].span,
                        ));
                    }
                };
                ensure_sortable_list_elem(elem, args[0].span)?;
                // copy via `xs * 1`, sort the copy, yield it
                let ty = arg.ty;
                let tmp = ctx.fresh_temp("sorted", ty);
                let copy = ir::Expr {
                    ty,
                    kind: ir::ExprKind::Binary {
                        op: ir::BinOp::Mul,
                        left: Box::new(arg),
                        right: Box::new(int_const(1)),
                    },
                };
                let local = |name: String| ir::Expr {
                    ty,
                    kind: ir::ExprKind::Local(name),
                };
                Ok(ir::Expr {
                    ty,
                    kind: ir::ExprKind::Block {
                        stmts: vec![
                            ir::Stmt::Assign {
                                name: tmp.clone(),
                                value: copy,
                            },
                            ir::Stmt::ListSort {
                                list: local(tmp.clone()),
                            },
                        ],
                        result: Box::new(local(tmp)),
                    },
                })
            }
            "range" => Err(err(
                "range(...) is only supported as the iterable of a 'for' loop",
                span,
            )),
            "input" => {
                let prompt = match args.as_slice() {
                    [] => Option::None,
                    [p] => {
                        let v = lower_expr(p, ctx)?;
                        if v.ty != ir::Ty::Str {
                            return Err(err(
                                format!(
                                    "input() prompt must be a str, found {} \
                                     (wrap it in str(...))",
                                    v.ty
                                ),
                                p.span,
                            ));
                        }
                        Some(Box::new(v))
                    }
                    _ => {
                        return Err(err(
                            format!("input() takes at most one argument ({} given)", args.len()),
                            span,
                        ));
                    }
                };
                Ok(ir::Expr {
                    ty: ir::Ty::Str,
                    kind: ir::ExprKind::Input { prompt },
                })
            }
            "open" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(err(
                        format!("open() takes 1 or 2 arguments ({} given)", args.len()),
                        span,
                    ));
                }
                let path = lower_expr(args[0], ctx)?;
                if path.ty != ir::Ty::Str {
                    return Err(err(
                        format!("open() path must be a str, found {}", path.ty),
                        args[0].span,
                    ));
                }
                let mode = match args.get(1) {
                    Some(m) => {
                        let v = lower_expr(m, ctx)?;
                        if v.ty != ir::Ty::Str {
                            return Err(err(
                                format!("open() mode must be a str, found {}", v.ty),
                                m.span,
                            ));
                        }
                        // constant modes are validated now, like Python would
                        // at runtime
                        if let ir::ExprKind::ConstStr(mode_s) = &v.kind
                            && !matches!(mode_s.as_str(), "r" | "w" | "a")
                        {
                            return Err(err(
                                format!(
                                    "invalid mode: '{mode_s}' (supported: 'r', \
                                     'w', 'a')"
                                ),
                                m.span,
                            ));
                        }
                        v
                    }
                    Option::None => ir::Expr {
                        ty: ir::Ty::Str,
                        kind: ir::ExprKind::ConstStr("r".to_string()),
                    },
                };
                Ok(ir::Expr {
                    ty: ir::Ty::File,
                    kind: ir::ExprKind::Open {
                        path: Box::new(path),
                        mode: Box::new(mode),
                    },
                })
            }
            "isinstance" => lower_isinstance(&args, span, ctx),
            "any" => lower_any_all(true, &args, span, ctx),
            "all" => lower_any_all(false, &args, span, ctx),
            "enumerate" => lower_enumerate_expr(&args, keywords, span, ctx),
            "zip" => lower_zip_expr(&args, span, ctx),
            "reversed" => lower_reversed_expr(&args, span, ctx),
            _ => Err(err(format!("function '{func}' is not defined"), func_span)),
        }
    }
}

/// Type-name patterns accepted by `isinstance(x, T)` / `isinstance(x, (T1, T2))`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IsInstancePat {
    Int,
    Float,
    Bool,
    Str,
    List,
    Tuple,
    Dict,
    Set,
    None,
    /// Exception hierarchy type (`ValueError`, `OSError`, `Exception`, …).
    Exc(ir::ExcType),
    /// User class (with inheritance).
    Class(ir::ClassId),
}

fn parse_isinstance_type_arg(e: &ast::Expr) -> SResult<Vec<IsInstancePat>> {
    match &e.kind {
        ast::ExprKind::Name(n) => Ok(vec![name_to_isinstance_pat(n, e.span)?]),
        ast::ExprKind::NoneLit => Ok(vec![IsInstancePat::None]),
        ast::ExprKind::TupleLit(items) => {
            if items.is_empty() {
                return Err(err("isinstance() type tuple must be non-empty", e.span));
            }
            let mut out = Vec::new();
            for it in items {
                out.extend(parse_isinstance_type_arg(it)?);
            }
            Ok(out)
        }
        // type(None) if written as a call — not supported; require None or name.
        _ => Err(err(
            "isinstance() second argument must be a type name (int, float, bool, str, \
             list, tuple, dict, set, None, a class name, or an exception type) or a \
             tuple of those — not a variable or expression",
            e.span,
        )),
    }
}

fn name_to_isinstance_pat(name: &str, span: Span) -> SResult<IsInstancePat> {
    match name {
        "int" => Ok(IsInstancePat::Int),
        "float" => Ok(IsInstancePat::Float),
        "bool" => Ok(IsInstancePat::Bool),
        "str" => Ok(IsInstancePat::Str),
        "list" => Ok(IsInstancePat::List),
        "tuple" => Ok(IsInstancePat::Tuple),
        "dict" => Ok(IsInstancePat::Dict),
        "set" => Ok(IsInstancePat::Set),
        "None" => Ok(IsInstancePat::None),
        other => {
            if let Some(id) = lookup_class(other) {
                return Ok(IsInstancePat::Class(id));
            }
            // Exception types: ValueError, OSError, Exception, GeneratorExit, …
            match name_to_exc_type(other, span) {
                Ok(t) => Ok(IsInstancePat::Exc(t)),
                Err(_) => Err(err(
                    format!(
                        "isinstance() does not support type '{name}' (supported: int, float, bool, \
                         str, list, tuple, dict, set, None, class names, and exception types)"
                    ),
                    span,
                )),
            }
        }
    }
}

fn isinstance_pat_matches(ty: ir::Ty, pat: IsInstancePat) -> bool {
    match pat {
        IsInstancePat::Int => matches!(ty, ir::Ty::Int | ir::Ty::Bool), // CPython: bool ⊂ int
        IsInstancePat::Float => matches!(ty, ir::Ty::Float),
        IsInstancePat::Bool => matches!(ty, ir::Ty::Bool),
        IsInstancePat::Str => matches!(ty, ir::Ty::Str),
        IsInstancePat::List => matches!(ty, ir::Ty::List(_)),
        IsInstancePat::Tuple => matches!(ty, ir::Ty::Tuple(_)),
        IsInstancePat::Dict => matches!(ty, ir::Ty::Dict { .. }),
        IsInstancePat::Set => matches!(ty, ir::Ty::Set(_)),
        IsInstancePat::None => matches!(ty, ir::Ty::None),
        // Exception instances always need a runtime type-tag check.
        IsInstancePat::Exc(_) => false,
        // Class: static fold only when monomorphic and exact/subclass.
        IsInstancePat::Class(want) => match ty {
            ir::Ty::Class(got) => class_is_subclass(got, want),
            _ => false,
        },
    }
}

/// Map an `isinstance` type pattern to a storage type for bare-param inference.
/// Containers (`list`/`tuple`/`dict`/`set`) are skipped — no element type.
/// Multi-pat tuples are joined by the caller into a union (not float-promoted).
fn isinstance_pat_to_ty(pat: IsInstancePat) -> Option<ir::Ty> {
    match pat {
        IsInstancePat::Int => Some(ir::Ty::Int),
        IsInstancePat::Float => Some(ir::Ty::Float),
        IsInstancePat::Bool => Some(ir::Ty::Bool),
        IsInstancePat::Str => Some(ir::Ty::Str),
        IsInstancePat::List
        | IsInstancePat::Tuple
        | IsInstancePat::Dict
        | IsInstancePat::Set
        | IsInstancePat::None => None,
        IsInstancePat::Exc(_) => Some(ir::Ty::Exception),
        IsInstancePat::Class(id) => Some(ir::Ty::Class(id)),
    }
}

fn isinstance_pat_tag(pat: IsInstancePat) -> Option<i32> {
    match pat {
        IsInstancePat::Int => Some(0),
        IsInstancePat::Float => Some(1),
        IsInstancePat::Bool => Some(2),
        IsInstancePat::Str => Some(3),
        IsInstancePat::List => Some(4), // any list: tag % 8 == 4
        IsInstancePat::Tuple => Some(5),
        IsInstancePat::Dict => Some(6),
        IsInstancePat::Set => Some(7),
        IsInstancePat::None => Some(-1),
        IsInstancePat::Exc(_) | IsInstancePat::Class(_) => None,
    }
}

fn lower_isinstance(args: &[&ast::Expr], span: Span, ctx: &mut FnCtx) -> SResult<ir::Expr> {
    if args.len() != 2 {
        return Err(err(
            format!(
                "isinstance() takes exactly 2 arguments ({} given)",
                args.len()
            ),
            span,
        ));
    }
    let value = lower_expr(args[0], ctx)?;
    // Use storage type (unwrap FromUnion peel) so unions see the full tag set.
    let value = match value.kind {
        ir::ExprKind::FromUnion { value: inner } => *inner,
        _ => value,
    };
    let pats = parse_isinstance_type_arg(args[1])?;
    let exc_filters: Vec<i32> = pats
        .iter()
        .filter_map(|p| match p {
            IsInstancePat::Exc(t) => Some(t.tag()),
            _ => None,
        })
        .collect();
    let class_filters: Vec<ir::ClassId> = pats
        .iter()
        .filter_map(|p| match p {
            IsInstancePat::Class(id) => Some(*id),
            _ => None,
        })
        .collect();
    let value_pats: Vec<IsInstancePat> = pats
        .iter()
        .copied()
        .filter(|p| !matches!(p, IsInstancePat::Exc(_) | IsInstancePat::Class(_)))
        .collect();
    let bool_is_int = value_pats.contains(&IsInstancePat::Int);

    // Exception objects: runtime hierarchy check (tag lives inside the object).
    if value.ty == ir::Ty::Exception {
        if exc_filters.is_empty() {
            // isinstance(exc, int) etc. is always false.
            return Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::ConstBool(false),
            });
        }
        return Ok(ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ExcIsInstance {
                value: Box::new(value),
                filters: exc_filters,
            },
        });
    }

    // Dynamic Any: runtime print-tag / class-id check (no static fold).
    if value.ty == ir::Ty::Any {
        let type_tags: Vec<i32> = value_pats
            .iter()
            .filter_map(|p| isinstance_pat_tag(*p))
            .collect();
        if type_tags.is_empty() && exc_filters.is_empty() && class_filters.is_empty() {
            return Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::ConstBool(false),
            });
        }
        return Ok(ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::IsInstance {
                value: Box::new(value),
                type_tags,
                bool_is_int,
                exc_filters,
                class_filters,
            },
        });
    }

    // User class instances: inheritance check (static when possible).
    if let ir::Ty::Class(got) = value.ty {
        if class_filters.is_empty() {
            // isinstance(obj, int) etc. is always false for instances.
            let hit = value_pats
                .iter()
                .any(|p| isinstance_pat_matches(value.ty, *p));
            return Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::ConstBool(hit),
            });
        }
        // got is subclass of want → always True (exact or more specific static).
        if class_filters
            .iter()
            .any(|&want| class_is_subclass(got, want))
        {
            return Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::ConstBool(true),
            });
        }
        // want is a subclass of got → runtime (value may be that subclass).
        let runtime: Vec<ir::ClassId> = class_filters
            .iter()
            .copied()
            .filter(|&want| class_is_subclass(want, got))
            .collect();
        if runtime.is_empty() {
            return Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::ConstBool(false),
            });
        }
        // OR of ClassIsInstance checks.
        let mut acc: Option<ir::Expr> = None;
        for want in runtime {
            let check = ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::ClassIsInstance {
                    value: Box::new(value.clone()),
                    class_id: want,
                },
            };
            acc = Some(match acc {
                None => check,
                Some(prev) => ir::Expr {
                    ty: ir::Ty::Bool,
                    kind: ir::ExprKind::Binary {
                        op: ir::BinOp::Or,
                        left: Box::new(prev),
                        right: Box::new(check),
                    },
                },
            });
        }
        return Ok(acc.unwrap());
    }

    // Static fold when monomorphic (non-exception, non-class).
    if !matches!(value.ty, ir::Ty::Union(_)) {
        // Non-exception values are never instances of exception types.
        let hit = value_pats
            .iter()
            .any(|p| isinstance_pat_matches(value.ty, *p))
            || class_filters.iter().any(
                |&want| matches!(value.ty, ir::Ty::Class(got) if class_is_subclass(got, want)),
            );
        return Ok(ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ConstBool(hit),
        });
    }

    // Union: runtime member-index test; Exception/Class members use hierarchy.
    let type_tags: Vec<i32> = value_pats
        .iter()
        .filter_map(|p| isinstance_pat_tag(*p))
        .collect();
    let has_exc_member = matches!(value.ty, ir::Ty::Union(ms) if ms.contains(&ir::Ty::Exception));
    let exc_filters = if has_exc_member {
        exc_filters
    } else {
        Vec::new()
    };
    let has_class_member = matches!(
        value.ty,
        ir::Ty::Union(ms) if ms.iter().any(|m| matches!(m, ir::Ty::Class(_)))
    );
    let class_filters = if has_class_member {
        class_filters
    } else {
        Vec::new()
    };
    if type_tags.is_empty() && exc_filters.is_empty() && class_filters.is_empty() {
        return Ok(ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ConstBool(false),
        });
    }
    Ok(ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::IsInstance {
            value: Box::new(value),
            type_tags,
            bool_is_int,
            exc_filters,
            class_filters,
        },
    })
}

/// `any(xs)` / `all(xs)` — list first; also str/tuple/set. Empty any→False, all→True.
fn lower_any_all(
    is_any: bool,
    args: &[&ast::Expr],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let name = if is_any { "any" } else { "all" };
    if args.len() != 1 {
        return Err(err(
            format!("{name}() takes exactly one argument ({} given)", args.len()),
            span,
        ));
    }
    let seq = lower_expr(args[0], ctx)?;
    match seq.ty {
        ir::Ty::List(_) | ir::Ty::Str | ir::Ty::Tuple(_) | ir::Ty::Set(_) => {}
        other => {
            return Err(err(
                format!("{name}() expects a list, str, tuple, or set, found {other}"),
                args[0].span,
            ));
        }
    }
    // Desugar to a loop with ToBool.
    let seq_ty = seq.ty;
    let seq_t = ctx.fresh_temp(&format!("{name}.seq"), seq_ty);
    let i_t = ctx.fresh_temp(&format!("{name}.i"), ir::Ty::Int);
    let acc_t = ctx.fresh_temp(&format!("{name}.acc"), ir::Ty::Bool);
    let init_acc = ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::ConstBool(!is_any), // any→False, all→True
    };
    let mut stmts = vec![
        ir::Stmt::Assign {
            name: seq_t.clone(),
            value: seq,
        },
        ir::Stmt::Assign {
            name: i_t.clone(),
            value: int_const(0),
        },
        ir::Stmt::Assign {
            name: acc_t.clone(),
            value: init_acc,
        },
    ];
    // For set: materialize to list.
    let (iter_ty, iter_expr) = if let ir::Ty::Set(elem) = seq_ty {
        let list_ty = ir::list_of(*elem);
        let lt = ctx.fresh_temp(&format!("{name}.els"), list_ty);
        stmts.push(ir::Stmt::Assign {
            name: lt.clone(),
            value: ir::Expr {
                ty: list_ty,
                kind: ir::ExprKind::SetToList(Box::new(ir::Expr {
                    ty: seq_ty,
                    kind: ir::ExprKind::Local(seq_t.clone()),
                })),
            },
        });
        (
            list_ty,
            ir::Expr {
                ty: list_ty,
                kind: ir::ExprKind::Local(lt),
            },
        )
    } else {
        (
            seq_ty,
            ir::Expr {
                ty: seq_ty,
                kind: ir::ExprKind::Local(seq_t.clone()),
            },
        )
    };
    let n_t = ctx.fresh_temp(&format!("{name}.n"), ir::Ty::Int);
    stmts.push(ir::Stmt::Assign {
        name: n_t.clone(),
        value: ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Len(Box::new(iter_expr.clone())),
        },
    });
    let elem_ty = match iter_ty {
        ir::Ty::List(e) => *e,
        ir::Ty::Str => ir::Ty::Str,
        ir::Ty::Tuple(es) if !es.is_empty() && es.iter().all(|e| e == &es[0]) => es[0],
        ir::Ty::Tuple(_) => {
            // Heterogeneous: index returns union of members — use first for ToBool via each.
            // Use a loose approach: load as... we need per-index. For simplicity,
            // only homogeneous tuples for any/all for now; hetero → error.
            return Err(err(
                format!("{name}() on heterogeneous tuples is not supported yet"),
                args[0].span,
            ));
        }
        _ => unreachable!(),
    };
    let cond = ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Binary {
            op: ir::BinOp::Lt,
            left: Box::new(ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Local(i_t.clone()),
            }),
            right: Box::new(ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Local(n_t),
            }),
        },
    };
    let elem = ir::Expr {
        ty: elem_ty,
        kind: ir::ExprKind::Index {
            base: Box::new(iter_expr),
            index: Box::new(ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Local(i_t.clone()),
            }),
        },
    };
    let truth = to_bool_default(elem, span)?;
    let update = if is_any {
        // if truth: acc = True
        ir::Stmt::If {
            branches: vec![(
                truth,
                vec![ir::Stmt::Assign {
                    name: acc_t.clone(),
                    value: ir::Expr {
                        ty: ir::Ty::Bool,
                        kind: ir::ExprKind::ConstBool(true),
                    },
                }],
            )],
            orelse: vec![],
        }
    } else {
        // if not truth: acc = False
        ir::Stmt::If {
            branches: vec![(
                ir::Expr {
                    ty: ir::Ty::Bool,
                    kind: ir::ExprKind::Unary {
                        op: ir::UnOp::Not,
                        operand: Box::new(truth),
                    },
                },
                vec![ir::Stmt::Assign {
                    name: acc_t.clone(),
                    value: ir::Expr {
                        ty: ir::Ty::Bool,
                        kind: ir::ExprKind::ConstBool(false),
                    },
                }],
            )],
            orelse: vec![],
        }
    };
    let step = ir::Stmt::Assign {
        name: i_t.clone(),
        value: ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Binary {
                op: ir::BinOp::Add,
                left: Box::new(ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Local(i_t.clone()),
                }),
                right: Box::new(int_const(1)),
            },
        },
    };
    stmts.push(ir::Stmt::While {
        cond,
        body: vec![update],
        step: vec![step],
    });
    Ok(ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Local(acc_t),
            }),
        },
    })
}

fn lower_enumerate_expr(
    args: &[&ast::Expr],
    keywords: &[ast::Keyword],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if !keywords.is_empty() {
        // Optional start= — support if keyword is start.
        if keywords.len() == 1 && keywords[0].name == "start" {
            // handled below
        } else {
            return Err(err(
                "enumerate() only supports the optional start= keyword",
                keywords[0].name_span,
            ));
        }
    }
    if args.is_empty() || args.len() > 1 {
        return Err(err(
            format!(
                "enumerate() takes 1 positional argument ({} given)",
                args.len()
            ),
            span,
        ));
    }
    let seq = lower_expr(args[0], ctx)?;
    let start = if let Some(kw) = keywords.iter().find(|k| k.name == "start") {
        let s = lower_expr(&kw.value, ctx)?;
        coerce(s, ir::Ty::Int, kw.value.span, "enumerate start")?
    } else {
        int_const(0)
    };
    let elem_ty = match seq.ty {
        ir::Ty::List(e) => *e,
        ir::Ty::Str => ir::Ty::Str,
        ir::Ty::Tuple(es) if !es.is_empty() && es.iter().all(|e| e == &es[0]) => es[0],
        ir::Ty::Tuple(_) => {
            return Err(err(
                "enumerate() on heterogeneous tuples is not supported yet",
                args[0].span,
            ));
        }
        other => {
            return Err(err(
                format!("enumerate() expects a list, str, or tuple, found {other}"),
                args[0].span,
            ));
        }
    };
    let pair_ty = ir::tuple_of(&[ir::Ty::Int, elem_ty]);
    let out_ty = ir::list_of(pair_ty);
    // Materialize list of (i, x).
    let seq_t = ctx.fresh_temp("enum.seq", seq.ty);
    let out_t = ctx.fresh_temp("enum.out", out_ty);
    let i_t = ctx.fresh_temp("enum.i", ir::Ty::Int);
    let n_t = ctx.fresh_temp("enum.n", ir::Ty::Int);
    let mut stmts = vec![
        ir::Stmt::Assign {
            name: seq_t.clone(),
            value: seq.clone(),
        },
        ir::Stmt::Assign {
            name: out_t.clone(),
            value: ir::Expr {
                ty: out_ty,
                kind: ir::ExprKind::ListNew {
                    cap: Box::new(ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::Len(Box::new(ir::Expr {
                            ty: seq.ty,
                            kind: ir::ExprKind::Local(seq_t.clone()),
                        })),
                    }),
                },
            },
        },
        ir::Stmt::Assign {
            name: i_t.clone(),
            value: int_const(0),
        },
        ir::Stmt::Assign {
            name: n_t.clone(),
            value: ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Len(Box::new(ir::Expr {
                    ty: seq.ty,
                    kind: ir::ExprKind::Local(seq_t.clone()),
                })),
            },
        },
    ];
    let idx_t = ctx.fresh_temp("enum.idx", ir::Ty::Int);
    stmts.push(ir::Stmt::Assign {
        name: idx_t.clone(),
        value: start,
    });
    let cond = ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Binary {
            op: ir::BinOp::Lt,
            left: Box::new(ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Local(i_t.clone()),
            }),
            right: Box::new(ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Local(n_t),
            }),
        },
    };
    let elem = ir::Expr {
        ty: elem_ty,
        kind: ir::ExprKind::Index {
            base: Box::new(ir::Expr {
                ty: seq.ty,
                kind: ir::ExprKind::Local(seq_t),
            }),
            index: Box::new(ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Local(i_t.clone()),
            }),
        },
    };
    let pair = ir::Expr {
        ty: pair_ty,
        kind: ir::ExprKind::TupleLit(vec![
            ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Local(idx_t.clone()),
            },
            elem,
        ]),
    };
    let body = vec![
        ir::Stmt::ListAppend {
            list: ir::Expr {
                ty: out_ty,
                kind: ir::ExprKind::Local(out_t.clone()),
            },
            value: pair,
        },
        ir::Stmt::Assign {
            name: idx_t.clone(),
            value: ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Add,
                    left: Box::new(ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::Local(idx_t),
                    }),
                    right: Box::new(int_const(1)),
                },
            },
        },
    ];
    let step = ir::Stmt::Assign {
        name: i_t.clone(),
        value: ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Binary {
                op: ir::BinOp::Add,
                left: Box::new(ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Local(i_t),
                }),
                right: Box::new(int_const(1)),
            },
        },
    };
    stmts.push(ir::Stmt::While {
        cond,
        body,
        step: vec![step],
    });
    Ok(ir::Expr {
        ty: out_ty,
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(ir::Expr {
                ty: out_ty,
                kind: ir::ExprKind::Local(out_t),
            }),
        },
    })
}

fn lower_zip_expr(args: &[&ast::Expr], span: Span, ctx: &mut FnCtx) -> SResult<ir::Expr> {
    if args.len() != 2 {
        return Err(err(
            format!(
                "zip() takes exactly 2 arguments in this subset ({} given)",
                args.len()
            ),
            span,
        ));
    }
    let a = lower_expr(args[0], ctx)?;
    let b = lower_expr(args[1], ctx)?;
    let elem_a = match a.ty {
        ir::Ty::List(e) => *e,
        ir::Ty::Str => ir::Ty::Str,
        ir::Ty::Tuple(es) if !es.is_empty() && es.iter().all(|e| e == &es[0]) => es[0],
        other => {
            return Err(err(
                format!("zip() expects list/str/homogeneous tuple, found {other}"),
                args[0].span,
            ));
        }
    };
    let elem_b = match b.ty {
        ir::Ty::List(e) => *e,
        ir::Ty::Str => ir::Ty::Str,
        ir::Ty::Tuple(es) if !es.is_empty() && es.iter().all(|e| e == &es[0]) => es[0],
        other => {
            return Err(err(
                format!("zip() expects list/str/homogeneous tuple, found {other}"),
                args[1].span,
            ));
        }
    };
    let pair_ty = ir::tuple_of(&[elem_a, elem_b]);
    let out_ty = ir::list_of(pair_ty);
    let a_t = ctx.fresh_temp("zip.a", a.ty);
    let b_t = ctx.fresh_temp("zip.b", b.ty);
    let out_t = ctx.fresh_temp("zip.out", out_ty);
    let i_t = ctx.fresh_temp("zip.i", ir::Ty::Int);
    let na = ctx.fresh_temp("zip.na", ir::Ty::Int);
    let nb = ctx.fresh_temp("zip.nb", ir::Ty::Int);
    let n_t = ctx.fresh_temp("zip.n", ir::Ty::Int);
    let mut stmts = vec![
        ir::Stmt::Assign {
            name: a_t.clone(),
            value: a.clone(),
        },
        ir::Stmt::Assign {
            name: b_t.clone(),
            value: b.clone(),
        },
        ir::Stmt::Assign {
            name: na.clone(),
            value: ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Len(Box::new(ir::Expr {
                    ty: a.ty,
                    kind: ir::ExprKind::Local(a_t.clone()),
                })),
            },
        },
        ir::Stmt::Assign {
            name: nb.clone(),
            value: ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Len(Box::new(ir::Expr {
                    ty: b.ty,
                    kind: ir::ExprKind::Local(b_t.clone()),
                })),
            },
        },
        ir::Stmt::Assign {
            name: n_t.clone(),
            value: ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Min {
                    left: Box::new(ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::Local(na),
                    }),
                    right: Box::new(ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::Local(nb),
                    }),
                },
            },
        },
        ir::Stmt::Assign {
            name: out_t.clone(),
            value: ir::Expr {
                ty: out_ty,
                kind: ir::ExprKind::ListNew {
                    cap: Box::new(ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::Local(n_t.clone()),
                    }),
                },
            },
        },
        ir::Stmt::Assign {
            name: i_t.clone(),
            value: int_const(0),
        },
    ];
    let cond = ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Binary {
            op: ir::BinOp::Lt,
            left: Box::new(ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Local(i_t.clone()),
            }),
            right: Box::new(ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Local(n_t),
            }),
        },
    };
    let ea = ir::Expr {
        ty: elem_a,
        kind: ir::ExprKind::Index {
            base: Box::new(ir::Expr {
                ty: a.ty,
                kind: ir::ExprKind::Local(a_t),
            }),
            index: Box::new(ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Local(i_t.clone()),
            }),
        },
    };
    let eb = ir::Expr {
        ty: elem_b,
        kind: ir::ExprKind::Index {
            base: Box::new(ir::Expr {
                ty: b.ty,
                kind: ir::ExprKind::Local(b_t),
            }),
            index: Box::new(ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Local(i_t.clone()),
            }),
        },
    };
    let body = vec![ir::Stmt::ListAppend {
        list: ir::Expr {
            ty: out_ty,
            kind: ir::ExprKind::Local(out_t.clone()),
        },
        value: ir::Expr {
            ty: pair_ty,
            kind: ir::ExprKind::TupleLit(vec![ea, eb]),
        },
    }];
    let step = ir::Stmt::Assign {
        name: i_t.clone(),
        value: ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Binary {
                op: ir::BinOp::Add,
                left: Box::new(ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Local(i_t),
                }),
                right: Box::new(int_const(1)),
            },
        },
    };
    stmts.push(ir::Stmt::While {
        cond,
        body,
        step: vec![step],
    });
    Ok(ir::Expr {
        ty: out_ty,
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(ir::Expr {
                ty: out_ty,
                kind: ir::ExprKind::Local(out_t),
            }),
        },
    })
}

fn lower_reversed_expr(args: &[&ast::Expr], span: Span, ctx: &mut FnCtx) -> SResult<ir::Expr> {
    if args.len() != 1 {
        return Err(err(
            format!(
                "reversed() takes exactly one argument ({} given)",
                args.len()
            ),
            span,
        ));
    }
    let seq = lower_expr(args[0], ctx)?;
    match seq.ty {
        ir::Ty::List(elem) => {
            // Materialize a new list in reverse order.
            let out_ty = ir::list_of(*elem);
            let seq_t = ctx.fresh_temp("rev.seq", seq.ty);
            let out_t = ctx.fresh_temp("rev.out", out_ty);
            let i_t = ctx.fresh_temp("rev.i", ir::Ty::Int);
            let n_t = ctx.fresh_temp("rev.n", ir::Ty::Int);
            let mut stmts = vec![
                ir::Stmt::Assign {
                    name: seq_t.clone(),
                    value: seq.clone(),
                },
                ir::Stmt::Assign {
                    name: n_t.clone(),
                    value: ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::Len(Box::new(ir::Expr {
                            ty: seq.ty,
                            kind: ir::ExprKind::Local(seq_t.clone()),
                        })),
                    },
                },
                ir::Stmt::Assign {
                    name: out_t.clone(),
                    value: ir::Expr {
                        ty: out_ty,
                        kind: ir::ExprKind::ListNew {
                            cap: Box::new(ir::Expr {
                                ty: ir::Ty::Int,
                                kind: ir::ExprKind::Local(n_t.clone()),
                            }),
                        },
                    },
                },
                ir::Stmt::Assign {
                    name: i_t.clone(),
                    value: ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::Binary {
                            op: ir::BinOp::Sub,
                            left: Box::new(ir::Expr {
                                ty: ir::Ty::Int,
                                kind: ir::ExprKind::Local(n_t),
                            }),
                            right: Box::new(int_const(1)),
                        },
                    },
                },
            ];
            let cond = ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Ge,
                    left: Box::new(ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::Local(i_t.clone()),
                    }),
                    right: Box::new(int_const(0)),
                },
            };
            let elem_e = ir::Expr {
                ty: *elem,
                kind: ir::ExprKind::Index {
                    base: Box::new(ir::Expr {
                        ty: seq.ty,
                        kind: ir::ExprKind::Local(seq_t),
                    }),
                    index: Box::new(ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::Local(i_t.clone()),
                    }),
                },
            };
            let body = vec![ir::Stmt::ListAppend {
                list: ir::Expr {
                    ty: out_ty,
                    kind: ir::ExprKind::Local(out_t.clone()),
                },
                value: elem_e,
            }];
            let step = ir::Stmt::Assign {
                name: i_t.clone(),
                value: ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Binary {
                        op: ir::BinOp::Sub,
                        left: Box::new(ir::Expr {
                            ty: ir::Ty::Int,
                            kind: ir::ExprKind::Local(i_t),
                        }),
                        right: Box::new(int_const(1)),
                    },
                },
            };
            stmts.push(ir::Stmt::While {
                cond,
                body,
                step: vec![step],
            });
            Ok(ir::Expr {
                ty: out_ty,
                kind: ir::ExprKind::Block {
                    stmts,
                    result: Box::new(ir::Expr {
                        ty: out_ty,
                        kind: ir::ExprKind::Local(out_t),
                    }),
                },
            })
        }
        ir::Ty::Str => {
            // Reverse via slice [::-1] (i64::MIN = missing bound).
            Ok(ir::Expr {
                ty: ir::Ty::Str,
                kind: ir::ExprKind::Slice {
                    base: Box::new(seq),
                    lo: Box::new(int_const(i64::MIN)),
                    hi: Box::new(int_const(i64::MIN)),
                    step: Box::new(int_const(-1)),
                },
            })
        }
        ir::Ty::Tuple(es) if !es.is_empty() && es.iter().all(|e| e == &es[0]) => {
            // Materialize list (document: reversed tuple → list).
            let elem = es[0];
            let as_list = ir::Expr {
                ty: ir::list_of(elem),
                // Build list by indexing — reuse list path via temp materialize.
                kind: ir::ExprKind::ListNew {
                    cap: Box::new(ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::Len(Box::new(seq.clone())),
                    }),
                },
            };
            // Fall through: convert tuple to list then reverse.
            let list_ty = ir::list_of(elem);
            let seq_t = ctx.fresh_temp("rev.tup", seq.ty);
            let list_t = ctx.fresh_temp("rev.list", list_ty);
            let i_t = ctx.fresh_temp("rev.i", ir::Ty::Int);
            let n_t = ctx.fresh_temp("rev.n", ir::Ty::Int);
            let mut stmts = vec![
                ir::Stmt::Assign {
                    name: seq_t.clone(),
                    value: seq.clone(),
                },
                ir::Stmt::Assign {
                    name: n_t.clone(),
                    value: ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::Len(Box::new(ir::Expr {
                            ty: seq.ty,
                            kind: ir::ExprKind::Local(seq_t.clone()),
                        })),
                    },
                },
                ir::Stmt::Assign {
                    name: list_t.clone(),
                    value: as_list,
                },
                ir::Stmt::Assign {
                    name: i_t.clone(),
                    value: int_const(0),
                },
            ];
            let cond = ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Lt,
                    left: Box::new(ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::Local(i_t.clone()),
                    }),
                    right: Box::new(ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::Local(n_t.clone()),
                    }),
                },
            };
            let body = vec![ir::Stmt::ListAppend {
                list: ir::Expr {
                    ty: list_ty,
                    kind: ir::ExprKind::Local(list_t.clone()),
                },
                value: ir::Expr {
                    ty: elem,
                    kind: ir::ExprKind::Index {
                        base: Box::new(ir::Expr {
                            ty: seq.ty,
                            kind: ir::ExprKind::Local(seq_t),
                        }),
                        index: Box::new(ir::Expr {
                            ty: ir::Ty::Int,
                            kind: ir::ExprKind::Local(i_t.clone()),
                        }),
                    },
                },
            }];
            let step = ir::Stmt::Assign {
                name: i_t.clone(),
                value: ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Binary {
                        op: ir::BinOp::Add,
                        left: Box::new(ir::Expr {
                            ty: ir::Ty::Int,
                            kind: ir::ExprKind::Local(i_t),
                        }),
                        right: Box::new(int_const(1)),
                    },
                },
            };
            stmts.push(ir::Stmt::While {
                cond,
                body,
                step: vec![step],
            });
            // Now reverse the list via recursive call-like desugar — index reverse.
            let out_t = ctx.fresh_temp("rev.out", list_ty);
            let j_t = ctx.fresh_temp("rev.j", ir::Ty::Int);
            stmts.push(ir::Stmt::Assign {
                name: out_t.clone(),
                value: ir::Expr {
                    ty: list_ty,
                    kind: ir::ExprKind::ListNew {
                        cap: Box::new(ir::Expr {
                            ty: ir::Ty::Int,
                            kind: ir::ExprKind::Local(n_t.clone()),
                        }),
                    },
                },
            });
            stmts.push(ir::Stmt::Assign {
                name: j_t.clone(),
                value: ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Binary {
                        op: ir::BinOp::Sub,
                        left: Box::new(ir::Expr {
                            ty: ir::Ty::Int,
                            kind: ir::ExprKind::Local(n_t),
                        }),
                        right: Box::new(int_const(1)),
                    },
                },
            });
            let cond2 = ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Ge,
                    left: Box::new(ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::Local(j_t.clone()),
                    }),
                    right: Box::new(int_const(0)),
                },
            };
            let body2 = vec![ir::Stmt::ListAppend {
                list: ir::Expr {
                    ty: list_ty,
                    kind: ir::ExprKind::Local(out_t.clone()),
                },
                value: ir::Expr {
                    ty: elem,
                    kind: ir::ExprKind::Index {
                        base: Box::new(ir::Expr {
                            ty: list_ty,
                            kind: ir::ExprKind::Local(list_t),
                        }),
                        index: Box::new(ir::Expr {
                            ty: ir::Ty::Int,
                            kind: ir::ExprKind::Local(j_t.clone()),
                        }),
                    },
                },
            }];
            let step2 = ir::Stmt::Assign {
                name: j_t.clone(),
                value: ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Binary {
                        op: ir::BinOp::Sub,
                        left: Box::new(ir::Expr {
                            ty: ir::Ty::Int,
                            kind: ir::ExprKind::Local(j_t),
                        }),
                        right: Box::new(int_const(1)),
                    },
                },
            };
            stmts.push(ir::Stmt::While {
                cond: cond2,
                body: body2,
                step: vec![step2],
            });
            Ok(ir::Expr {
                ty: list_ty,
                kind: ir::ExprKind::Block {
                    stmts,
                    result: Box::new(ir::Expr {
                        ty: list_ty,
                        kind: ir::ExprKind::Local(out_t),
                    }),
                },
            })
        }
        other => Err(err(
            format!("reversed() expects a list, str, or homogeneous tuple, found {other}"),
            args[0].span,
        )),
    }
}

fn lower_cast(ty: ast::TypeName, value: ir::Expr, span: Span) -> SResult<ir::Expr> {
    match ty {
        ast::TypeName::Int => match value.ty {
            ir::Ty::Int => Ok(value),
            ir::Ty::Float => Ok(ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::FloatToInt(Box::new(value)),
            }),
            ir::Ty::Bool => Ok(ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::BoolToInt(Box::new(value)),
            }),
            other => Err(err(format!("int() cannot convert {other}"), span)),
        },
        ast::TypeName::Float => match value.ty {
            ir::Ty::Float => Ok(value),
            ir::Ty::Int | ir::Ty::Bool => {
                let as_int = if value.ty == ir::Ty::Bool {
                    ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::BoolToInt(Box::new(value)),
                    }
                } else {
                    value
                };
                Ok(ir::Expr {
                    ty: ir::Ty::Float,
                    kind: ir::ExprKind::IntToFloat(Box::new(as_int)),
                })
            }
            other => Err(err(format!("float() cannot convert {other}"), span)),
        },
        ast::TypeName::Bool => match value.ty {
            ir::Ty::Bool => Ok(value),
            ir::Ty::Int
            | ir::Ty::Float
            | ir::Ty::Str
            | ir::Ty::List(_)
            | ir::Ty::Tuple(_)
            | ir::Ty::Dict { .. }
            | ir::Ty::Set(_)
            | ir::Ty::Exception
            | ir::Ty::Class(_)
            | ir::Ty::Any => Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::ToBool(Box::new(value)),
            }),
            other => Err(err(format!("bool() cannot convert {other}"), span)),
        },
        ast::TypeName::Str => match value.ty {
            ir::Ty::Str => Ok(value),
            ir::Ty::Int => Ok(ir::Expr {
                ty: ir::Ty::Str,
                kind: ir::ExprKind::IntToStr(Box::new(value)),
            }),
            ir::Ty::Float => Ok(ir::Expr {
                ty: ir::Ty::Str,
                kind: ir::ExprKind::FloatToStr(Box::new(value)),
            }),
            ir::Ty::Bool => Ok(ir::Expr {
                ty: ir::Ty::Str,
                kind: ir::ExprKind::BoolToStr(Box::new(value)),
            }),
            ir::Ty::Exception => Ok(ir::Expr {
                ty: ir::Ty::Str,
                kind: ir::ExprKind::ExcToStr(Box::new(value)),
            }),
            ir::Ty::Class(id) => lower_class_to_str(value, id, span),
            other => Err(err(format!("str() cannot convert {other} yet"), span)),
        },
        ast::TypeName::List(_) => Err(err("list(...) conversions are not supported", span)),
        ast::TypeName::Tuple(_) => Err(err("tuple(...) conversions are not supported", span)),
        ast::TypeName::Dict { .. } => Err(err(
            "dict(...) conversions are not supported; use a literal or annotate '{}'",
            span,
        )),
        ast::TypeName::Set(_) => Err(err(
            "set(iterable) is not supported yet; use a set literal or set() with annotation",
            span,
        )),
        ast::TypeName::File => Err(err(
            "file() is not a conversion; use open(path) to open a file",
            span,
        )),
        ast::TypeName::None => Err(err("None is not a conversion", span)),
        ast::TypeName::Union(_) => Err(err("union types are not a conversion", span)),
        ast::TypeName::Class(_) => Err(err(
            "class types are not a conversion (construct with ClassName(...))",
            span,
        )),
        ast::TypeName::Any => {
            // `Any(x)` is not a real CPython builtin; allow as annotation-style
            // cast helper: coerce value into dynamic Any.
            coerce(value, ir::Ty::Any, span, "Any(...) cast")
        }
    }
}

/// Lower `f"…"` / nested format-spec joined strings to concat of pieces.
fn lower_joined_str(parts: &[ast::FStringPart], ctx: &mut FnCtx<'_>) -> SResult<ir::Expr> {
    let mut result: Option<ir::Expr> = Option::None;
    for part in parts {
        let piece = match part {
            ast::FStringPart::Literal(s) => ir::Expr {
                ty: ir::Ty::Str,
                kind: ir::ExprKind::ConstStr(s.clone()),
            },
            ast::FStringPart::Expr {
                expr: e,
                conversion,
                format_spec,
            } => {
                let v = lower_expr(e, ctx)?;
                let converted = lower_fstring_conversion(v, *conversion, e.span)?;
                match format_spec {
                    None => {
                        // No `:` → `str(value)` (after optional conversion).
                        if converted.ty == ir::Ty::Str {
                            converted
                        } else {
                            lower_cast(ast::TypeName::Str, converted, e.span)?
                        }
                    }
                    Some(spec) => {
                        let spec_ir = lower_expr(spec, ctx)?;
                        if spec_ir.ty != ir::Ty::Str {
                            return Err(err(
                                format!(
                                    "f-string format specifier must be str, got {}",
                                    spec_ir.ty
                                ),
                                e.span,
                            ));
                        }
                        // Static check: only scalar types we can format.
                        match converted.ty {
                            ir::Ty::Int | ir::Ty::Float | ir::Ty::Bool | ir::Ty::Str => {}
                            other => {
                                return Err(err(
                                    format!("format() cannot convert {other} yet"),
                                    e.span,
                                ));
                            }
                        }
                        ir::Expr {
                            ty: ir::Ty::Str,
                            kind: ir::ExprKind::FormatValue {
                                value: Box::new(converted),
                                spec: Box::new(spec_ir),
                            },
                        }
                    }
                }
            }
        };
        result = Some(match result {
            Option::None => piece,
            Some(acc) => ir::Expr {
                ty: ir::Ty::Str,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Add,
                    left: Box::new(acc),
                    right: Box::new(piece),
                },
            },
        });
    }
    Ok(result.unwrap_or(ir::Expr {
        ty: ir::Ty::Str,
        kind: ir::ExprKind::ConstStr(String::new()),
    }))
}

/// Apply f-string `!s` / `!r` / `!a` conversion (or leave value unchanged).
fn lower_fstring_conversion(
    value: ir::Expr,
    conversion: Option<ast::FStringConversion>,
    span: Span,
) -> SResult<ir::Expr> {
    let Some(conv) = conversion else {
        return Ok(value);
    };
    match conv {
        ast::FStringConversion::Str => {
            if value.ty == ir::Ty::Str {
                Ok(value)
            } else {
                lower_cast(ast::TypeName::Str, value, span)
            }
        }
        ast::FStringConversion::Repr => lower_repr_like(value, false, span),
        ast::FStringConversion::Ascii => lower_repr_like(value, true, span),
    }
}

/// `repr` / `ascii` for supported scalars. For int/float/bool, both match
/// `str()`. For str, emit [`ir::ExprKind::StrRepr`] / [`ir::ExprKind::StrAscii`].
fn lower_repr_like(value: ir::Expr, ascii: bool, span: Span) -> SResult<ir::Expr> {
    match value.ty {
        ir::Ty::Str => Ok(ir::Expr {
            ty: ir::Ty::Str,
            kind: if ascii {
                ir::ExprKind::StrAscii(Box::new(value))
            } else {
                ir::ExprKind::StrRepr(Box::new(value))
            },
        }),
        ir::Ty::Int | ir::Ty::Float | ir::Ty::Bool => {
            // CPython: repr(42) == '42', ascii same as repr for these.
            lower_cast(ast::TypeName::Str, value, span)
        }
        other => Err(err(
            format!(
                "{}() cannot convert {other} yet",
                if ascii { "ascii" } else { "repr" }
            ),
            span,
        )),
    }
}

/// bool → int; int/float pass through; anything else is an error.
fn promote_numeric(value: ir::Expr, span: Span, what: &str) -> SResult<ir::Expr> {
    match value.ty {
        ir::Ty::Int | ir::Ty::Float => Ok(value),
        ir::Ty::Bool => Ok(ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::BoolToInt(Box::new(value)),
        }),
        other => Err(err(
            format!("{what} is not supported for values of type {other}"),
            span,
        )),
    }
}

/// Promote both operands to a common numeric type (int unless either side
/// is float).
fn unify_numeric(
    l: ir::Expr,
    r: ir::Expr,
    span: Span,
    what: &str,
) -> SResult<(ir::Expr, ir::Expr, ir::Ty)> {
    let l = promote_numeric(l, span, what)?;
    let r = promote_numeric(r, span, what)?;
    match (l.ty, r.ty) {
        (ir::Ty::Int, ir::Ty::Int) => Ok((l, r, ir::Ty::Int)),
        (ir::Ty::Float, ir::Ty::Float) => Ok((l, r, ir::Ty::Float)),
        (ir::Ty::Int, ir::Ty::Float) => {
            let l = ir::Expr {
                ty: ir::Ty::Float,
                kind: ir::ExprKind::IntToFloat(Box::new(l)),
            };
            Ok((l, r, ir::Ty::Float))
        }
        (ir::Ty::Float, ir::Ty::Int) => {
            let r = ir::Expr {
                ty: ir::Ty::Float,
                kind: ir::ExprKind::IntToFloat(Box::new(r)),
            };
            Ok((l, r, ir::Ty::Float))
        }
        _ => unreachable!("promote_numeric only returns int/float"),
    }
}

/// Join two types for `and`/`or`: equal → that type; both numeric → promote;
/// either side `Any` → `Any`; provisional `list[Any]` yields to a more specific
/// `list[T]`; otherwise flatten into a union of all atomic members.
fn join_types(a: ir::Ty, b: ir::Ty) -> ir::Ty {
    if a == b {
        return a;
    }
    if a == ir::Ty::Any || b == ir::Ty::Any {
        return ir::Ty::Any;
    }
    // Empty-list default `list[Any]` is provisional: join with `list[T]` → `list[T]`.
    match (a, b) {
        (ir::Ty::List(e), ir::Ty::List(f)) if *e == ir::Ty::Any => {
            return ir::list_of(*f);
        }
        (ir::Ty::List(e), ir::Ty::List(f)) if *f == ir::Ty::Any => {
            return ir::list_of(*e);
        }
        _ => {}
    }
    let a_num = matches!(a, ir::Ty::Bool | ir::Ty::Int | ir::Ty::Float);
    let b_num = matches!(b, ir::Ty::Bool | ir::Ty::Int | ir::Ty::Float);
    if a_num && b_num {
        // same rules as unify_numeric without building exprs
        return match (a, b) {
            (ir::Ty::Float, _) | (_, ir::Ty::Float) => ir::Ty::Float,
            (ir::Ty::Int, _) | (_, ir::Ty::Int) => ir::Ty::Int,
            _ => ir::Ty::Bool,
        };
    }
    let mut members = ir::flatten_union_members(a);
    members.extend(ir::flatten_union_members(b));
    ir::union_of(&members)
}

/// Unify operand types for `and`/`or`: same type, numeric promote, or union.
fn unify_and_or(l: ir::Expr, r: ir::Expr, span: Span) -> SResult<(ir::Expr, ir::Expr, ir::Ty)> {
    // Same type: keep as-is (including both bool — do not promote to int).
    if l.ty == r.ty {
        let ty = l.ty;
        return Ok((l, r, ty));
    }
    // Differing numeric sides: bool/int/float promote like other operators.
    let l_num = matches!(l.ty, ir::Ty::Bool | ir::Ty::Int | ir::Ty::Float);
    let r_num = matches!(r.ty, ir::Ty::Bool | ir::Ty::Int | ir::Ty::Float);
    if l_num && r_num {
        return unify_numeric(l, r, span, "'and'/'or'");
    }
    // Otherwise form a union and coerce both sides to it.
    let result_ty = join_types(l.ty, r.ty);
    let l = coerce(l, result_ty, span, "'and'/'or' left operand")?;
    let r = coerce(r, result_ty, span, "'and'/'or' right operand")?;
    Ok((l, r, result_ty))
}

/// Math stdlib unary intrinsic name → IR op (bodies replaced when lowering
/// functions in the `math` module).
fn math_intrinsic(name: &str) -> Option<ir::MathOp> {
    Some(match name {
        "sqrt" => ir::MathOp::Sqrt,
        "sin" => ir::MathOp::Sin,
        "cos" => ir::MathOp::Cos,
        "tan" => ir::MathOp::Tan,
        "log" => ir::MathOp::Log,
        "log10" => ir::MathOp::Log10,
        "exp" => ir::MathOp::Exp,
        "floor" => ir::MathOp::Floor,
        "ceil" => ir::MathOp::Ceil,
        "fabs" => ir::MathOp::Fabs,
        _ => return None,
    })
}

/// Drop a single `FromUnion` peel so `is None` / `is not None` always see
/// storage (union) tags. Flow-sensitive refinements retype loads via
/// `FromUnion` for arithmetic; if that peeled value were used here, codegen
/// would constant-fold the check (`int is not None` → true) and `while
/// x is not None: x = None` would infinite-loop.
fn unwrap_from_union_peel(e: ir::Expr) -> ir::Expr {
    match e.kind {
        ir::ExprKind::FromUnion { value } => *value,
        _ => e,
    }
}

/// `expr is None` / `expr is not None`, or pointer identity for heap objects
/// (lists, dicts, sets, tuples, str, closures, generators, files).
fn lower_is_none(op: ast::BinOp, l: ir::Expr, r: ir::Expr, span: Span) -> SResult<ir::Expr> {
    let not = matches!(op, ast::BinOp::IsNot);
    let l = unwrap_from_union_peel(l);
    let r = unwrap_from_union_peel(r);
    let l_none = matches!(l.kind, ir::ExprKind::ConstNone) || l.ty == ir::Ty::None;
    let r_none = matches!(r.kind, ir::ExprKind::ConstNone) || r.ty == ir::Ty::None;
    match (l_none, r_none) {
        (false, true) => Ok(ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::IsNone {
                value: Box::new(l),
                not,
            },
        }),
        (true, false) => Ok(ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::IsNone {
                value: Box::new(r),
                not,
            },
        }),
        (true, true) => {
            // `None is None` → True; `None is not None` → False
            Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::ConstBool(!not),
            })
        }
        (false, false) => {
            // Pointer identity for same-type heap objects; same-binding locals
            // of int/float/bool compare by value-as-slot (not CPython int interning).
            if l.ty != r.ty {
                return Err(err(
                    format!(
                        "'is' / 'is not' require the same type on both sides \
                         (found {} and {})",
                        l.ty, r.ty
                    ),
                    span,
                ));
            }
            let ptr_like = matches!(
                l.ty,
                ir::Ty::Str
                    | ir::Ty::List(_)
                    | ir::Ty::Tuple(_)
                    | ir::Ty::Dict { .. }
                    | ir::Ty::Set(_)
                    | ir::Ty::File
                    | ir::Ty::Closure { .. }
                    | ir::Ty::Generator { .. }
                    | ir::Ty::Cell(_)
                    | ir::Ty::Exception
                    | ir::Ty::Class(_)
            );
            if !ptr_like
                && !matches!(
                    l.ty,
                    ir::Ty::Int | ir::Ty::Float | ir::Ty::Bool | ir::Ty::Union(_)
                )
            {
                return Err(err(
                    format!("'is' / 'is not' is not supported for type {}", l.ty),
                    span,
                ));
            }
            Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::IsIdentity {
                    left: Box::new(l),
                    right: Box::new(r),
                    not,
                },
            })
        }
    }
}

fn lower_binary(op: ast::BinOp, l: ir::Expr, r: ir::Expr, span: Span) -> SResult<ir::Expr> {
    let describe = format!("operator '{op}'");

    // membership tests work on str and list; check before type dispatch
    if matches!(op, ast::BinOp::In | ast::BinOp::NotIn) {
        return lower_contains(op, l, r, span);
    }

    // `is` / `is not` — only `… is None` / `… is not None` (either side).
    if matches!(op, ast::BinOp::Is | ast::BinOp::IsNot) {
        return lower_is_none(op, l, r, span);
    }

    // ---- set algebra before bitwise int ops ----
    if matches!((l.ty, r.ty), (ir::Ty::Set(_), ir::Ty::Set(_))) {
        match op {
            ast::BinOp::BitOr => return lower_set_union(l, r, span),
            ast::BinOp::BitAnd => {
                return lower_set_binary_op(l, r, span, "intersection", |left, right| {
                    ir::ExprKind::SetIntersect { left, right }
                });
            }
            ast::BinOp::Sub => {
                return lower_set_binary_op(l, r, span, "difference", |left, right| {
                    ir::ExprKind::SetDiff { left, right }
                });
            }
            ast::BinOp::BitXor => {
                return lower_set_binary_op(l, r, span, "symmetric_difference", |left, right| {
                    ir::ExprKind::SetSymDiff { left, right }
                });
            }
            _ => {}
        }
    }

    // ---- string operations ----
    if l.ty == ir::Ty::Str || r.ty == ir::Ty::Str {
        return lower_str_binary(op, l, r, span);
    }
    // ---- list + / * ----
    if matches!(l.ty, ir::Ty::List(_)) || matches!(r.ty, ir::Ty::List(_)) {
        return lower_list_binary(op, l, r, span);
    }
    // ---- tuple equality ----
    if matches!(l.ty, ir::Ty::Tuple(_)) || matches!(r.ty, ir::Ty::Tuple(_)) {
        return lower_tuple_binary(op, l, r, span);
    }

    match op {
        ast::BinOp::Add
        | ast::BinOp::Sub
        | ast::BinOp::Mul
        | ast::BinOp::FloorDiv
        | ast::BinOp::Mod => {
            let (l, r, ty) = unify_numeric(l, r, span, &describe)?;
            let ir_op = match op {
                ast::BinOp::Add => ir::BinOp::Add,
                ast::BinOp::Sub => ir::BinOp::Sub,
                ast::BinOp::Mul => ir::BinOp::Mul,
                ast::BinOp::FloorDiv => ir::BinOp::FloorDiv,
                ast::BinOp::Mod => ir::BinOp::Mod,
                _ => unreachable!(),
            };
            Ok(ir::Expr {
                ty,
                kind: ir::ExprKind::Binary {
                    op: ir_op,
                    left: Box::new(l),
                    right: Box::new(r),
                },
            })
        }
        // int ** int stays int, except a negative constant exponent which
        // is a float in Python (2 ** -1 == 0.5); dynamic negative exponents
        // trap at runtime. Floats use llvm.pow.
        ast::BinOp::Pow => {
            let (l, r, ty) = unify_numeric(l, r, span, &describe)?;
            let (l, r, ty) =
                if ty == ir::Ty::Int && matches!(r.kind, ir::ExprKind::ConstInt(k) if k < 0) {
                    let to_float = |e: ir::Expr| ir::Expr {
                        ty: ir::Ty::Float,
                        kind: ir::ExprKind::IntToFloat(Box::new(e)),
                    };
                    (to_float(l), to_float(r), ir::Ty::Float)
                } else {
                    (l, r, ty)
                };
            Ok(ir::Expr {
                ty,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Pow,
                    left: Box::new(l),
                    right: Box::new(r),
                },
            })
        }
        // true division always produces float (Python semantics)
        ast::BinOp::Div => {
            let (l, r, _) = unify_numeric(l, r, span, &describe)?;
            let to_float = |e: ir::Expr| {
                if e.ty == ir::Ty::Float {
                    e
                } else {
                    ir::Expr {
                        ty: ir::Ty::Float,
                        kind: ir::ExprKind::IntToFloat(Box::new(e)),
                    }
                }
            };
            Ok(ir::Expr {
                ty: ir::Ty::Float,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Div,
                    left: Box::new(to_float(l)),
                    right: Box::new(to_float(r)),
                },
            })
        }
        ast::BinOp::Eq
        | ast::BinOp::NotEq
        | ast::BinOp::Lt
        | ast::BinOp::LtEq
        | ast::BinOp::Gt
        | ast::BinOp::GtEq => {
            let (l, r, _) = unify_numeric(l, r, span, &describe)?;
            Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Binary {
                    op: comparison_ir_op(op),
                    left: Box::new(l),
                    right: Box::new(r),
                },
            })
        }
        ast::BinOp::And | ast::BinOp::Or => {
            unreachable!("and/or are handled in lower_expr")
        }
        ast::BinOp::In | ast::BinOp::NotIn => {
            unreachable!("in/not-in are handled above")
        }
        ast::BinOp::Is | ast::BinOp::IsNot => {
            unreachable!("is/is not are handled above")
        }
        ast::BinOp::BitAnd
        | ast::BinOp::BitOr
        | ast::BinOp::BitXor
        | ast::BinOp::LShift
        | ast::BinOp::RShift => lower_bitwise(op, l, r, span),
    }
}

/// Bitwise ops on int/bool. Bool &/|/^ bool stays bool (CPython); otherwise int.
fn lower_bitwise(op: ast::BinOp, l: ir::Expr, r: ir::Expr, span: Span) -> SResult<ir::Expr> {
    let both_bool = l.ty == ir::Ty::Bool && r.ty == ir::Ty::Bool;
    let keep_bool = both_bool
        && matches!(
            op,
            ast::BinOp::BitAnd | ast::BinOp::BitOr | ast::BinOp::BitXor
        );
    let to_int = |e: ir::Expr, side: &str| -> SResult<ir::Expr> {
        match e.ty {
            ir::Ty::Int => Ok(e),
            ir::Ty::Bool => Ok(ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::BoolToInt(Box::new(e)),
            }),
            other => Err(err(
                format!(
                    "unsupported operand type(s) for '{op}': {side} is {other} (need int or bool)"
                ),
                span,
            )),
        }
    };
    let l = to_int(l, "left")?;
    let r = to_int(r, "right")?;
    let ir_op = match op {
        ast::BinOp::BitAnd => ir::BinOp::BitAnd,
        ast::BinOp::BitOr => ir::BinOp::BitOr,
        ast::BinOp::BitXor => ir::BinOp::BitXor,
        ast::BinOp::LShift => ir::BinOp::LShift,
        ast::BinOp::RShift => ir::BinOp::RShift,
        _ => unreachable!(),
    };
    // constant fold only when the result still fits in i64 (bigint shifts leave runtime)
    if let (ir::ExprKind::ConstInt(a), ir::ExprKind::ConstInt(b)) = (&l.kind, &r.kind) {
        let folded: Option<i64> = match ir_op {
            ir::BinOp::BitAnd => Some(a & b),
            ir::BinOp::BitOr => Some(a | b),
            ir::BinOp::BitXor => Some(a ^ b),
            ir::BinOp::LShift => {
                if *b < 0 {
                    return Err(err("negative shift count", span));
                }
                if *b >= 63 {
                    None // may need bigint (e.g. 1<<100)
                } else if *b == 0 {
                    Some(*a)
                } else {
                    // only fold when no overflow beyond i64
                    let sh = *b as u32;
                    if *a >= 0 {
                        a.checked_shl(sh).filter(|&v| v >> sh == *a)
                    } else if sh < 63 {
                        // small negative << k that still fits in i64
                        Some(a.wrapping_shl(sh))
                    } else {
                        None
                    }
                }
            }
            ir::BinOp::RShift => {
                if *b < 0 {
                    return Err(err("negative shift count", span));
                }
                if *b >= 63 {
                    Some(if *a < 0 { -1 } else { 0 })
                } else {
                    Some(*a >> (*b as u32))
                }
            }
            _ => unreachable!(),
        };
        if let Some(v) = folded {
            if keep_bool {
                return Ok(ir::Expr {
                    ty: ir::Ty::Bool,
                    kind: ir::ExprKind::ConstBool(v != 0),
                });
            }
            return Ok(int_const(v));
        }
    }
    let result = ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::Binary {
            op: ir_op,
            left: Box::new(l),
            right: Box::new(r),
        },
    };
    if keep_bool {
        Ok(ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ToBool(Box::new(result)),
        })
    } else {
        Ok(result)
    }
}

fn comparison_ir_op(op: ast::BinOp) -> ir::BinOp {
    match op {
        ast::BinOp::Eq => ir::BinOp::Eq,
        ast::BinOp::NotEq => ir::BinOp::Ne,
        ast::BinOp::Lt => ir::BinOp::Lt,
        ast::BinOp::LtEq => ir::BinOp::Le,
        ast::BinOp::Gt => ir::BinOp::Gt,
        ast::BinOp::GtEq => ir::BinOp::Ge,
        _ => unreachable!("not a comparison"),
    }
}

/// `needle in haystack` / `not in`: substring, list/tuple/set membership, dict keys.
fn lower_contains(op: ast::BinOp, l: ir::Expr, r: ir::Expr, span: Span) -> SResult<ir::Expr> {
    let needle = match r.ty {
        ir::Ty::Str => {
            if l.ty != ir::Ty::Str {
                return Err(err(
                    format!("'in <str>' requires a str on the left, found {}", l.ty),
                    span,
                ));
            }
            l
        }
        ir::Ty::List(ir::Ty::List(_)) => {
            return Err(err("'in' is not supported for lists of lists yet", span));
        }
        ir::Ty::List(elem) => coerce(l, *elem, span, "'in' operand")?,
        ir::Ty::Dict { key, .. } => coerce(l, *key, span, "'in' dict key")?,
        ir::Ty::Set(elem) => coerce(l, *elem, span, "'in' set element")?,
        ir::Ty::Tuple(elems) => {
            // Homogeneous: coerce to that elem type. Heterogeneous: keep needle
            // as-is; runtime only compares elements with a matching tag.
            let mut uniq: Vec<ir::Ty> = Vec::new();
            for e in elems.iter() {
                if !uniq.iter().any(|u| u == e) {
                    uniq.push(*e);
                }
            }
            if uniq.len() == 1 {
                coerce(l, uniq[0], span, "'in' tuple operand")?
            } else {
                // Accept needle if it is comparable to any element type.
                let ok = uniq.iter().any(|e| {
                    l.ty == *e
                        || matches!(
                            (l.ty, *e),
                            (ir::Ty::Bool, ir::Ty::Int)
                                | (ir::Ty::Int, ir::Ty::Float)
                                | (ir::Ty::Bool, ir::Ty::Float)
                        )
                });
                if !ok && !uniq.is_empty() {
                    return Err(err(
                        format!(
                            "'in' tuple operand type {} is not compatible with any element type",
                            l.ty
                        ),
                        span,
                    ));
                }
                l
            }
        }
        other => {
            return Err(err(format!("'{other}' does not support 'in'"), span));
        }
    };
    let contains = ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Contains {
            needle: Box::new(needle),
            haystack: Box::new(r),
        },
    };
    if op == ast::BinOp::NotIn {
        return Ok(ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::Unary {
                op: ir::UnOp::Not,
                operand: Box::new(contains),
            },
        });
    }
    Ok(contains)
}

fn lower_tuple_binary(op: ast::BinOp, l: ir::Expr, r: ir::Expr, span: Span) -> SResult<ir::Expr> {
    match op {
        ast::BinOp::Eq | ast::BinOp::NotEq => match (l.ty, r.ty) {
            (ir::Ty::Tuple(a), ir::Ty::Tuple(b)) if a == b => Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Binary {
                    op: if matches!(op, ast::BinOp::Eq) {
                        ir::BinOp::Eq
                    } else {
                        ir::BinOp::Ne
                    },
                    left: Box::new(l),
                    right: Box::new(r),
                },
            }),
            (ir::Ty::Tuple(_), ir::Ty::Tuple(_)) => {
                Err(err(format!("cannot compare {} and {}", l.ty, r.ty), span))
            }
            _ => Err(err(
                format!("'{}' is not comparable with '{}'", l.ty, r.ty),
                span,
            )),
        },
        other => Err(err(
            format!("operator '{other}' is not supported for tuples yet"),
            span,
        )),
    }
}

fn lower_list_binary(op: ast::BinOp, l: ir::Expr, r: ir::Expr, span: Span) -> SResult<ir::Expr> {
    match op {
        // xs + ys — same element type required
        ast::BinOp::Add => match (l.ty, r.ty) {
            (ir::Ty::List(a), ir::Ty::List(b)) if a == b => Ok(ir::Expr {
                ty: ir::Ty::List(a),
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Add,
                    left: Box::new(l),
                    right: Box::new(r),
                },
            }),
            (ir::Ty::List(a), ir::Ty::List(b)) => Err(err(
                format!("cannot concatenate list[{a}] and list[{b}]"),
                span,
            )),
            _ => {
                let other = if matches!(l.ty, ir::Ty::List(_)) {
                    &r.ty
                } else {
                    &l.ty
                };
                Err(err(
                    format!("can only concatenate list (not \"{other}\") to list"),
                    span,
                ))
            }
        },
        // xs * n / n * xs — count normalized to the right
        ast::BinOp::Mul => {
            let (xs, n) = match (l.ty, r.ty) {
                (ir::Ty::List(_), _) => (l, r),
                (_, ir::Ty::List(_)) => (r, l),
                _ => unreachable!("lower_list_binary only when a side is list"),
            };
            let n = promote_numeric(n, span, "list repetition")?;
            if n.ty != ir::Ty::Int {
                return Err(err("a list can only be multiplied by an int", span));
            }
            Ok(ir::Expr {
                ty: xs.ty,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Mul,
                    left: Box::new(xs),
                    right: Box::new(n),
                },
            })
        }
        // element-wise equality (same list element type)
        ast::BinOp::Eq | ast::BinOp::NotEq => match (l.ty, r.ty) {
            (ir::Ty::List(a), ir::Ty::List(b)) if a == b => Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Binary {
                    op: if matches!(op, ast::BinOp::Eq) {
                        ir::BinOp::Eq
                    } else {
                        ir::BinOp::Ne
                    },
                    left: Box::new(l),
                    right: Box::new(r),
                },
            }),
            (ir::Ty::List(a), ir::Ty::List(b)) => {
                Err(err(format!("cannot compare list[{a}] and list[{b}]"), span))
            }
            _ => Err(err(
                format!("'{}' is not comparable with '{}'", l.ty, r.ty),
                span,
            )),
        },
        other => Err(err(
            format!("operator '{other}' is not supported for lists yet"),
            span,
        )),
    }
}

fn lower_str_binary(op: ast::BinOp, l: ir::Expr, r: ir::Expr, span: Span) -> SResult<ir::Expr> {
    match op {
        // "a" + "b"
        ast::BinOp::Add if l.ty == ir::Ty::Str && r.ty == ir::Ty::Str => Ok(ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::Binary {
                op: ir::BinOp::Add,
                left: Box::new(l),
                right: Box::new(r),
            },
        }),
        ast::BinOp::Add => Err(err(
            "can only concatenate str to str; use str(...) to convert",
            span,
        )),
        // "ab" * 3 / 3 * "ab" — the count is normalized to the right
        ast::BinOp::Mul => {
            let (s, n) = if l.ty == ir::Ty::Str { (l, r) } else { (r, l) };
            let n = promote_numeric(n, span, "string repetition")?;
            if n.ty != ir::Ty::Int {
                return Err(err("a string can only be multiplied by an int", span));
            }
            Ok(ir::Expr {
                ty: ir::Ty::Str,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Mul,
                    left: Box::new(s),
                    right: Box::new(n),
                },
            })
        }
        // lexicographic comparisons
        ast::BinOp::Eq
        | ast::BinOp::NotEq
        | ast::BinOp::Lt
        | ast::BinOp::LtEq
        | ast::BinOp::Gt
        | ast::BinOp::GtEq => {
            if l.ty != ir::Ty::Str || r.ty != ir::Ty::Str {
                return Err(err(
                    format!("'{}' is not comparable with '{}'", l.ty, r.ty),
                    span,
                ));
            }
            Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Binary {
                    op: comparison_ir_op(op),
                    left: Box::new(l),
                    right: Box::new(r),
                },
            })
        }
        other => Err(err(
            format!("operator '{other}' is not supported for str"),
            span,
        )),
    }
}

// ---- return-path analysis ----

fn block_returns(stmts: &[ir::Stmt]) -> bool {
    stmts.iter().any(stmt_returns)
}

fn stmt_returns(stmt: &ir::Stmt) -> bool {
    match stmt {
        ir::Stmt::Return(_) => true,
        // Die / Raise exit the process or transfer; cannot fall through
        ir::Stmt::Die(_) | ir::Stmt::Raise { .. } => true,
        ir::Stmt::If { branches, orelse } => {
            !orelse.is_empty()
                && branches.iter().all(|(_, body)| block_returns(body))
                && block_returns(orelse)
        }
        // `while True:` without a break never falls through
        ir::Stmt::While { cond, body, .. } => {
            matches!(cond.kind, ir::ExprKind::ConstBool(true)) && !loop_breaks(body)
        }
        // No fall-through past the try. Finally runs on every exit; if it
        // never falls through (return/raise), the try never falls through.
        // Otherwise combine body + handlers (raise in body alone is not
        // enough when a handler can fall through).
        ir::Stmt::Try {
            body,
            handlers,
            orelse,
            finally,
        } => {
            if block_returns(finally) {
                return true;
            }
            // Normal completion runs orelse; return from body skips it.
            if block_returns(body) {
                if !block_may_raise(body) {
                    return true;
                }
                return handlers.iter().all(|(_, _, h)| block_returns(h));
            }
            // Body can fall through → else must return on that path.
            if !block_returns(orelse) {
                return false;
            }
            if !block_may_raise(body) {
                return true;
            }
            handlers.iter().all(|(_, _, h)| block_returns(h))
        }
        _ => false,
    }
}

/// Conservative: body may transfer via raise/die (so except handlers matter).
fn block_may_raise(stmts: &[ir::Stmt]) -> bool {
    stmts.iter().any(|s| match s {
        ir::Stmt::Raise { .. } | ir::Stmt::Die(_) => true,
        ir::Stmt::If { branches, orelse } => {
            branches.iter().any(|(_, b)| block_may_raise(b)) || block_may_raise(orelse)
        }
        ir::Stmt::While { body, step, .. } => block_may_raise(body) || block_may_raise(step),
        ir::Stmt::Try {
            body,
            handlers,
            orelse,
            finally,
        } => {
            block_may_raise(body)
                || handlers.iter().any(|(_, _, h)| block_may_raise(h))
                || block_may_raise(orelse)
                || block_may_raise(finally)
        }
        _ => false,
    })
}

/// Does this loop body contain a `break` for *this* loop (not a nested one)?
fn loop_breaks(stmts: &[ir::Stmt]) -> bool {
    stmts.iter().any(|s| match s {
        ir::Stmt::Break => true,
        ir::Stmt::If { branches, orelse } => {
            branches.iter().any(|(_, b)| loop_breaks(b)) || loop_breaks(orelse)
        }
        // a break inside a nested while belongs to that while
        ir::Stmt::While { .. } => false,
        ir::Stmt::Try {
            body,
            handlers,
            orelse,
            finally,
        } => {
            loop_breaks(body)
                || handlers.iter().any(|(_, _, h)| loop_breaks(h))
                || loop_breaks(orelse)
                || loop_breaks(finally)
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze_src(src: &str) -> SResult<ir::Module> {
        let module = parser::parse(src).expect("parse failed");
        analyze(&module)
    }

    fn analyze_ok(src: &str) -> ir::Module {
        match analyze_src(src) {
            Ok(m) => m,
            Err(e) => panic!(
                "analyze failed: {}\n{}",
                e.message,
                e.render("test.py", src)
            ),
        }
    }

    fn analyze_err(src: &str) -> Diagnostic {
        analyze_src(src).expect_err("expected a semantic error")
    }

    fn find_func<'a>(m: &'a ir::Module, name: &str) -> &'a ir::Function {
        m.funcs.iter().find(|f| f.name == name).unwrap()
    }

    #[test]
    fn lowers_fib() {
        let src = "\
def fib(n: int) -> int:
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)

print(fib(10))
";
        let m = analyze_ok(src);
        assert_eq!(m.entry, ENTRY_NAME);
        let fib = find_func(&m, "fib");
        assert_eq!(fib.ret, ir::Ty::Int);
        let entry = find_func(&m, ENTRY_NAME);
        assert!(matches!(entry.body[0], ir::Stmt::Print(_)));
    }

    #[test]
    fn function_and_module_docstrings_are_ok() {
        // First statement that is a string literal (docstring) is a no-op
        // expression statement — not a bare-name / empty-body error. `__doc__`
        // is not stored; runtime effect matches programs that only use docs.
        let m = analyze_ok(
            "\
\"\"\"module documentation\"\"\"

def f() -> int:
    \"\"\"function documentation\"\"\"
    return 42

print(f())
",
        );
        let f = find_func(&m, "f");
        // docstring + return
        assert!(
            f.body.len() >= 2,
            "expected docstring ExprStmt then return, got {:?}",
            f.body
        );
        assert!(
            matches!(
                &f.body[0],
                ir::Stmt::ExprStmt(ir::Expr {
                    kind: ir::ExprKind::ConstStr(s),
                    ..
                }) if s == "function documentation"
            ),
            "first body stmt should be the docstring ConstStr, got {:?}",
            f.body[0]
        );
        assert!(matches!(f.body[1], ir::Stmt::Return(Some(_))));

        let entry = find_func(&m, ENTRY_NAME);
        // module docstring then print(f())
        assert!(
            matches!(
                &entry.body[0],
                ir::Stmt::ExprStmt(ir::Expr {
                    kind: ir::ExprKind::ConstStr(s),
                    ..
                }) if s == "module documentation"
            ),
            "entry should start with module docstring, got {:?}",
            entry.body[0]
        );
    }

    #[test]
    fn triple_quoted_string_value_lowers() {
        let m = analyze_ok("s = \"\"\"a\nb\"\"\"\nprint(s)\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
            panic!("expected GlobalAssign, got {:?}", entry.body[0]);
        };
        assert!(
            matches!(&value.kind, ir::ExprKind::ConstStr(s) if s == "a\nb"),
            "{:?}",
            value.kind
        );
    }

    #[test]
    fn int_promotes_to_float_in_mixed_arithmetic() {
        let m = analyze_ok("x = 1 + 2.5\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
            panic!("expected Assign");
        };
        assert_eq!(value.ty, ir::Ty::Float);
    }

    #[test]
    fn true_division_yields_float() {
        let m = analyze_ok("x = 7 / 2\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
            panic!("expected Assign");
        };
        assert_eq!(value.ty, ir::Ty::Float);
    }

    #[test]
    fn pow_int_stays_int_pow_float_is_float() {
        let m = analyze_ok("a = 2 ** 10\nb = 2.0 ** 10\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Int);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Float);
    }

    #[test]
    fn chained_comparison_lowers_to_let_and() {
        let m = analyze_ok("x = 1\nb = 0 < x < 10\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
            panic!("expected Assign");
        };
        assert_eq!(value.ty, ir::Ty::Bool);
        let ir::ExprKind::Let { body, .. } = &value.kind else {
            panic!("expected Let, got {:?}", value.kind);
        };
        assert!(matches!(
            body.kind,
            ir::ExprKind::Binary {
                op: ir::BinOp::And,
                ..
            }
        ));
    }

    #[test]
    fn str_variables_and_concat() {
        let m = analyze_ok("s = \"ab\"\nt = s + \"c\"\nu = s * 3\n");
        let entry = find_func(&m, ENTRY_NAME);
        assert_eq!(m.globals[0], ("s".to_string(), ir::Ty::Str));
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Str);
    }

    #[test]
    fn str_comparisons_are_bool() {
        let m = analyze_ok("b = \"a\" < \"b\"\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Bool);
    }

    #[test]
    fn str_isdigit_is_bool() {
        let m = analyze_ok("b = \"42\".isdigit()\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Bool);
        assert!(matches!(
            &value.kind,
            ir::ExprKind::StrCall {
                func: ir::StrFn::IsDigit,
                ..
            }
        ));
    }

    #[test]
    fn str_rfind_is_int() {
        let m = analyze_ok("i = \"banana\".rfind(\"an\")\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Int);
        assert!(matches!(
            &value.kind,
            ir::ExprKind::StrCall {
                func: ir::StrFn::RFind,
                ..
            }
        ));
    }

    #[test]
    fn str_isalpha_isspace_case_are_bool() {
        let m = analyze_ok(
            "a = \"ab\".isalpha()\nb = \" \\t\".isspace()\n\
             c = \"AB\".isupper()\nd = \"ab\".islower()\n",
        );
        let entry = find_func(&m, ENTRY_NAME);
        for (i, want) in [
            ir::StrFn::IsAlpha,
            ir::StrFn::IsSpace,
            ir::StrFn::IsUpper,
            ir::StrFn::IsLower,
        ]
        .into_iter()
        .enumerate()
        {
            let ir::Stmt::GlobalAssign { value, .. } = &entry.body[i] else {
                panic!("body[{i}]");
            };
            assert_eq!(value.ty, ir::Ty::Bool);
            assert!(matches!(
                &value.kind,
                ir::ExprKind::StrCall { func, .. } if *func == want
            ));
        }
    }

    #[test]
    fn abs_int_float_and_bool() {
        let m = analyze_ok("a = abs(-5)\nb = abs(-2.5)\nc = abs(True)\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Int);
        assert!(matches!(value.kind, ir::ExprKind::Abs(_)));
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Float);
        assert!(matches!(value.kind, ir::ExprKind::Abs(_)));
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[2] else {
            panic!();
        };
        // bool promotes to int, then abs
        assert_eq!(value.ty, ir::Ty::Int);
        assert!(matches!(value.kind, ir::ExprKind::Abs(_)));
    }

    #[test]
    fn abs_rejects_str() {
        let e = analyze_err("x = abs(\"nope\")\n");
        assert!(
            e.message.contains("bad operand type for abs()"),
            "{}",
            e.message
        );
    }

    #[test]
    fn min_max_unify_numeric() {
        let m = analyze_ok("a = min(-3, 2)\nb = max(1, 1.5)\nc = min(True, 0)\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Int);
        assert!(matches!(value.kind, ir::ExprKind::Min { .. }));
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Float);
        assert!(matches!(value.kind, ir::ExprKind::Max { .. }));
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[2] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Int);
        assert!(matches!(value.kind, ir::ExprKind::Min { .. }));
    }

    #[test]
    fn min_rejects_str() {
        let e = analyze_err("x = min(1, \"nope\")\n");
        assert!(
            e.message.contains("min()") && e.message.contains("str"),
            "{}",
            e.message
        );
    }

    #[test]
    fn min_arity() {
        let e = analyze_err("x = min()\n");
        assert!(
            e.message.contains("1 or 2 arguments") || e.message.contains("takes"),
            "{}",
            e.message
        );
    }

    #[test]
    fn min_list_form() {
        let m = analyze_ok("a = min([3, 1, 4])\nb = max([1.5, -2.0])\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Int);
        assert!(matches!(value.kind, ir::ExprKind::MinList(_)));
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Float);
        assert!(matches!(value.kind, ir::ExprKind::MaxList(_)));
    }

    #[test]
    fn sum_list_int_and_float() {
        let m = analyze_ok("a = sum([1, 2, 3])\nb = sum([1.5, 2.5])\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Int);
        assert!(matches!(value.kind, ir::ExprKind::Sum(_)));
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Float);
        assert!(matches!(value.kind, ir::ExprKind::Sum(_)));
    }

    #[test]
    fn sum_rejects_str_list() {
        let e = analyze_err("x = sum([\"a\", \"b\"])\n");
        assert!(
            e.message.contains("sum()") && e.message.contains("list[str]"),
            "{}",
            e.message
        );
    }

    #[test]
    fn str_cast_and_index_and_len() {
        let m = analyze_ok("s = str(42)\nc = s[0]\nn = len(s)\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
            panic!();
        };
        assert!(matches!(value.kind, ir::ExprKind::IntToStr(_)));
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Str);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[2] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Int);
    }

    #[test]
    fn list_literal_infers_type_and_promotes() {
        let m = analyze_ok("xs = [1, 2.5, True]\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
            panic!();
        };
        assert_eq!(value.ty, ir::list_of(ir::Ty::Float));
    }

    #[test]
    fn empty_list_defaults_to_list_any() {
        let m = analyze_ok("xs = []\nprint(len(xs))\n");
        let entry = find_func(&m, ENTRY_NAME);
        // First global assign is xs = [] with list[Any].
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
            panic!("expected GlobalAssign, got {:?}", entry.body[0]);
        };
        assert_eq!(value.ty, ir::list_of(ir::Ty::Any));
        analyze_ok("xs: list[int] = []\n");
    }

    #[test]
    fn any_annotation_and_coerce() {
        let m = analyze_ok(
            "\
x: Any = 1
y: int = x
print(y)
",
        );
        let entry = find_func(&m, ENTRY_NAME);
        assert!(entry.body.iter().any(|s| matches!(
            s,
            ir::Stmt::GlobalAssign { name, value, .. }
                if name == "x" && value.ty == ir::Ty::Any
        )));
    }

    #[test]
    fn exclusive_field_after_multi_isinstance() {
        let m = analyze_ok(
            "\
class A:
    def __init__(self):
        self.a = 1
class B(A):
    def __init__(self):
        self.a = 1
        self.b = 2
class C(A):
    def __init__(self):
        self.a = 1
        self.c = 3
def f(x: A):
    if isinstance(x, (B, C)):
        return x.b
    return 0
print(f(B()))
",
        );
        let f = find_func(&m, "f");
        // Body should contain GetFieldPartial for exclusive .b
        fn has_partial(stmts: &[ir::Stmt]) -> bool {
            for s in stmts {
                match s {
                    ir::Stmt::Return(Some(e))
                    | ir::Stmt::ExprStmt(e)
                    | ir::Stmt::Assign { value: e, .. } => {
                        if expr_has_partial(e) {
                            return true;
                        }
                    }
                    ir::Stmt::If { branches, orelse } => {
                        for (_, b) in branches {
                            if has_partial(b) {
                                return true;
                            }
                        }
                        if has_partial(orelse) {
                            return true;
                        }
                    }
                    _ => {}
                }
            }
            false
        }
        fn expr_has_partial(e: &ir::Expr) -> bool {
            match &e.kind {
                ir::ExprKind::GetFieldPartial { .. } => true,
                ir::ExprKind::Block { stmts, result } => {
                    has_partial(stmts) || expr_has_partial(result)
                }
                _ => false,
            }
        }
        assert!(has_partial(&f.body), "expected GetFieldPartial in f body");
    }

    #[test]
    fn list_index_and_assignment() {
        let m = analyze_ok("xs = [1, 2]\ny = xs[0]\nxs[1] = 5\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Int);
        assert!(matches!(entry.body[2], ir::Stmt::IndexAssign { .. }));
    }

    #[test]
    fn list_append_becomes_stmt() {
        let m = analyze_ok("xs = [1]\nxs.append(2)\n");
        let entry = find_func(&m, ENTRY_NAME);
        assert!(matches!(entry.body[1], ir::Stmt::ListAppend { .. }));
    }

    #[test]
    fn list_insert_remove_clear_and_index() {
        let m = analyze_ok(
            "xs = [1, 2, 3]\nxs.insert(1, 9)\nxs.remove(2)\n\
             i = xs.index(9)\nxs.clear()\n",
        );
        let entry = find_func(&m, ENTRY_NAME);
        assert!(matches!(entry.body[1], ir::Stmt::ListInsert { .. }));
        assert!(matches!(entry.body[2], ir::Stmt::ListRemove { .. }));
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[3] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Int);
        assert!(matches!(value.kind, ir::ExprKind::ListIndexOf { .. }));
        assert!(matches!(entry.body[4], ir::Stmt::ListClear { .. }));
    }

    #[test]
    fn list_concat_and_repeat() {
        let m = analyze_ok("a = [1] + [2, 3]\nb = [1, 2] * 3\nc = 2 * [9]\n");
        let entry = find_func(&m, ENTRY_NAME);
        for i in 0..3 {
            let ir::Stmt::GlobalAssign { value, .. } = &entry.body[i] else {
                panic!();
            };
            assert_eq!(value.ty, ir::list_of(ir::Ty::Int));
            assert!(matches!(
                value.kind,
                ir::ExprKind::Binary {
                    op: ir::BinOp::Add | ir::BinOp::Mul,
                    ..
                }
            ));
        }
    }

    #[test]
    fn list_concat_rejects_mixed_elem() {
        let e = analyze_err("x = [1] + [1.5]\n");
        assert!(e.message.contains("concatenate"), "{}", e.message);
    }

    #[test]
    fn list_eq_is_bool() {
        let m = analyze_ok("b = [1, 2] == [1, 2]\nc = [1] != [2]\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Bool);
        assert!(matches!(
            value.kind,
            ir::ExprKind::Binary {
                op: ir::BinOp::Eq,
                ..
            }
        ));
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
            panic!();
        };
        assert!(matches!(
            value.kind,
            ir::ExprKind::Binary {
                op: ir::BinOp::Ne,
                ..
            }
        ));
    }

    #[test]
    fn list_sort_stmt_and_sorted_builtin() {
        let m = analyze_ok("xs = [3, 1]\nxs.sort()\nys = sorted([2, 1])\n");
        let entry = find_func(&m, ENTRY_NAME);
        assert!(matches!(entry.body[1], ir::Stmt::ListSort { .. }));
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[2] else {
            panic!();
        };
        assert_eq!(value.ty, ir::list_of(ir::Ty::Int));
        assert!(matches!(value.kind, ir::ExprKind::Block { .. }));
    }

    #[test]
    fn augmented_index_assignment_uses_temps() {
        let m = analyze_ok("xs = [1, 2]\nxs[0] += 5\n");
        let entry = find_func(&m, ENTRY_NAME);
        // Assign(list), Assign(.aug.base), Assign(.aug.idx), IndexAssign
        assert_eq!(entry.body.len(), 4);
        assert!(matches!(entry.body[3], ir::Stmt::IndexAssign { .. }));
    }

    #[test]
    fn for_range_desugars_to_while_with_step() {
        let m = analyze_ok("for i in range(10):\n    print(i)\n");
        let entry = find_func(&m, ENTRY_NAME);
        // Assign(.range.stop), Assign(i), While
        let ir::Stmt::While { step, .. } = &entry.body[2] else {
            panic!("expected While, got {:?}", entry.body[2]);
        };
        assert_eq!(step.len(), 1);
    }

    #[test]
    fn for_range_zero_step_is_compile_error() {
        let e = analyze_err("for i in range(0, 10, 0):\n    print(i)\n");
        assert!(e.message.contains("zero"), "{}", e.message);
    }

    #[test]
    fn for_over_file_desugars_to_while_more() {
        let m = analyze_ok("f = open(\"x\")\nfor line in f:\n    print(line)\n");
        let entry = find_func(&m, ENTRY_NAME);
        // open, file temp, more=True, While more (EOF uses flag, not break)
        assert!(
            entry.body.iter().any(|s| matches!(
                s,
                ir::Stmt::While {
                    cond: ir::Expr {
                        kind: ir::ExprKind::Local(name),
                        ..
                    },
                    ..
                } if name.contains("more")
            )),
            "expected while-more for file iteration, body={:?}",
            entry.body
        );
    }

    #[test]
    fn for_else_without_break_is_straight_line() {
        // No break in body → else is appended (not if-not-broke), so return
        // analysis can see else `return`s.
        let m = analyze_ok(
            "\
for i in range(2):
    pass
else:
    print(1)
",
        );
        let entry = find_func(&m, ENTRY_NAME);
        assert!(
            entry
                .body
                .iter()
                .any(|s| matches!(s, ir::Stmt::While { .. })),
            "{:?}",
            entry.body
        );
        // No broke-flag If: else is straight-line after the while.
        assert!(
            !entry.body.iter().any(|s| matches!(
                s,
                ir::Stmt::If {
                    branches,
                    ..
                } if matches!(
                    branches[0].0.kind,
                    ir::ExprKind::Unary { op: ir::UnOp::Not, .. }
                )
            )),
            "expected straight-line for-else when no break, body={:?}",
            entry.body
        );
    }

    #[test]
    fn while_else_without_break_is_straight_line() {
        let m = analyze_ok(
            "\
n = 0
while n < 1:
    n = n + 1
else:
    print(1)
",
        );
        let entry = find_func(&m, ENTRY_NAME);
        assert!(
            entry
                .body
                .iter()
                .any(|s| matches!(s, ir::Stmt::While { .. })),
            "{:?}",
            entry.body
        );
        // broke flag only when the body can `break`.
        assert!(
            !entry.body.iter().any(|s| matches!(
                s,
                ir::Stmt::Assign {
                    value: ir::Expr {
                        kind: ir::ExprKind::ConstBool(false),
                        ..
                    },
                    ..
                }
            )),
            "expected no broke=False when body has no break, body={:?}",
            entry.body
        );
    }

    #[test]
    fn while_else_with_break_uses_broke_flag() {
        let m = analyze_ok(
            "\
n = 0
while n < 3:
    n = n + 1
    break
else:
    print(1)
",
        );
        let entry = find_func(&m, ENTRY_NAME);
        assert!(
            entry.body.iter().any(|s| matches!(
                s,
                ir::Stmt::If {
                    branches,
                    ..
                } if matches!(
                    branches[0].0.kind,
                    ir::ExprKind::Unary { op: ir::UnOp::Not, .. }
                )
            )),
            "expected if-not-broke when body can break, body={:?}",
            entry.body
        );
    }

    #[test]
    fn for_without_else_has_no_broke_flag() {
        let m = analyze_ok("for i in range(2):\n    print(i)\n");
        let entry = find_func(&m, ENTRY_NAME);
        // no ConstBool false assign for broke
        let bool_false_assigns = entry
            .body
            .iter()
            .filter(|s| {
                matches!(
                    s,
                    ir::Stmt::Assign {
                        value: ir::Expr {
                            kind: ir::ExprKind::ConstBool(false),
                            ..
                        },
                        ..
                    }
                )
            })
            .count();
        assert_eq!(bool_false_assigns, 0, "{:?}", entry.body);
    }

    #[test]
    fn file_typed_param_and_return() {
        let m = analyze_ok(
            "\
def first(f: file) -> str:
    return f.readline()

def wrap(path: str) -> file:
    return open(path)

f = open(\"x\")
print(first(f))
",
        );
        let first = find_func(&m, "first");
        assert_eq!(first.params[0].1, ir::Ty::File);
        assert_eq!(first.ret, ir::Ty::Str);
        let wrap = find_func(&m, "wrap");
        assert_eq!(wrap.ret, ir::Ty::File);
    }

    #[test]
    fn multi_assign_binds_both() {
        let m = analyze_ok("a = b = 1\n");
        let entry = find_func(&m, ENTRY_NAME);
        // temp + two global assigns (right-to-left)
        assert!(entry.body.len() >= 3);
        assert!(
            m.globals.iter().any(|(n, t)| n == "a" && *t == ir::Ty::Int),
            "{:?}",
            m.globals
        );
        assert!(
            m.globals.iter().any(|(n, t)| n == "b" && *t == ir::Ty::Int),
            "{:?}",
            m.globals
        );
    }

    #[test]
    fn defaults_and_keyword_args() {
        let m = analyze_ok(
            "\
def f(a: int, b: int = 2) -> int:
    return a + b
print(f(1))
print(f(1, b=3))
",
        );
        let entry = find_func(&m, ENTRY_NAME);
        assert!(matches!(
            &entry.body[0],
            ir::Stmt::Print(args) if args.len() == 1
                && matches!(args[0].kind, ir::ExprKind::Call { ref args, .. } if args.len() == 2)
        ));
    }

    #[test]
    fn missing_required_after_kw_is_error() {
        let e = analyze_err("def f(a: int, b: int = 1) -> int:\n    return a\nprint(f(b=2))\n");
        assert!(
            e.message.contains("missing required argument 'a'"),
            "{}",
            e.message
        );
    }

    #[test]
    fn for_over_list_binds_elem_type() {
        let m = analyze_ok("for x in [1.5, 2.5]:\n    print(x)\n");
        let _ = find_func(&m, ENTRY_NAME);
        let has_x_float = m
            .globals
            .iter()
            .any(|(n, t)| n == "x" && *t == ir::Ty::Float);
        assert!(has_x_float, "globals: {:?}", m.globals);
    }

    #[test]
    fn for_over_str_binds_str() {
        let m = analyze_ok("for c in \"abc\":\n    print(c)\n");
        let _ = find_func(&m, ENTRY_NAME);
        let has_c_str = m.globals.iter().any(|(n, t)| n == "c" && *t == ir::Ty::Str);
        assert!(has_c_str, "globals: {:?}", m.globals);
    }

    #[test]
    fn for_over_int_is_error() {
        let e = analyze_err("for x in 42:\n    print(x)\n");
        assert!(e.message.contains("not iterable"), "{}", e.message);
    }

    #[test]
    fn range_outside_for_is_error() {
        let e = analyze_err("xs = range(10)\n");
        assert!(e.message.contains("for"), "{}", e.message);
    }

    #[test]
    fn str_and_int_concat_is_error() {
        let e = analyze_err("x = \"a\" + 1\n");
        assert!(e.message.contains("concatenate"), "{}", e.message);
    }

    #[test]
    fn str_item_assignment_is_error() {
        let e = analyze_err("s = \"ab\"\ns[0] = \"c\"\n");
        assert!(e.message.contains("immutable"), "{}", e.message);
    }

    #[test]
    fn heterogeneous_list_is_error() {
        let e = analyze_err("xs = [1, \"a\"]\n");
        assert!(e.message.contains("share one type"), "{}", e.message);
    }

    #[test]
    fn error_no_entry_point() {
        let e = analyze_err("def helper() -> int:\n    return 1\n");
        assert!(e.message.contains("entry point"), "{}", e.message);
    }

    #[test]
    fn error_variable_changes_type() {
        // Numeric multi-assign joins (int + float → float storage), so that
        // path is allowed. Incompatible non-numeric reassign still errors when
        // the joined storage cannot accept the RHS without a prior join pass
        // seeing both — here a later bool into a str-only binding.
        let e = analyze_err(
            "\
def f():
    x: str = \"a\"
    x = 1
    return x
print(f())
",
        );
        assert!(
            e.message.contains("type mismatch")
                || e.message.contains("storage type")
                || e.message.contains("expected str"),
            "{}",
            e.message
        );
    }

    #[test]
    fn multi_assign_numeric_promotes() {
        // int then float → float storage (join_types numeric promotion).
        let m = analyze_ok("x = 1\nx = 2.5\nprint(x)\n");
        assert!(
            m.globals
                .iter()
                .any(|(n, t)| n == "x" && *t == ir::Ty::Float),
            "{:?}",
            m.globals
        );
    }

    #[test]
    fn error_undefined_name() {
        let e = analyze_err("x = y + 1\n");
        assert!(e.message.contains("not defined"), "{}", e.message);
    }

    #[test]
    fn error_missing_return_path() {
        let e = analyze_err("def f(a: int) -> int:\n    if a:\n        return 1\n");
        assert!(e.message.contains("without a return"), "{}", e.message);
    }

    #[test]
    fn error_break_outside_loop() {
        let e = analyze_err("break\n");
        assert!(e.message.contains("outside"), "{}", e.message);
    }

    #[test]
    fn str_truthiness_works() {
        analyze_ok("s = \"x\"\nif s:\n    print(1)\n");
        analyze_ok("xs = [1]\nwhile xs:\n    break\n");
    }

    #[test]
    fn entry_calls_main_when_no_script() {
        let m = analyze_ok("def main():\n    print(1)\n");
        let entry = find_func(&m, ENTRY_NAME);
        assert!(matches!(
            &entry.body[0],
            ir::Stmt::ExprStmt(ir::Expr {
                kind: ir::ExprKind::Call { func, .. },
                ..
            }) if func == "main"
        ));
    }

    #[test]
    fn list_params_and_returns() {
        analyze_ok(
            "\
def total(xs: list[int]) -> int:
    t = 0
    for x in xs:
        t += x
    return t

print(total([1, 2, 3]))
",
        );
    }

    #[test]
    fn empty_list_arg_uses_param_type() {
        analyze_ok(
            "\
def count(xs: list[str]) -> int:
    return len(xs)

print(count([]))
",
        );
    }

    #[test]
    fn cannot_redefine_builtins() {
        let e = analyze_err("def len(x: int) -> int:\n    return x\nprint(len(1))\n");
        assert!(e.message.contains("builtin"), "{}", e.message);
    }

    #[test]
    fn slices_type_correctly() {
        let m = analyze_ok("s = \"hello\"\nt = s[1:3]\nxs = [1, 2, 3]\nys = xs[:2]\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Str);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[3] else {
            panic!();
        };
        assert_eq!(value.ty, ir::list_of(ir::Ty::Int));
        // missing bounds become i64::MIN sentinels, missing step becomes 1
        let ir::ExprKind::Slice { lo, hi, step, .. } = &value.kind else {
            panic!();
        };
        assert!(matches!(lo.kind, ir::ExprKind::ConstInt(i64::MIN)));
        assert!(matches!(hi.kind, ir::ExprKind::ConstInt(_)));
        assert!(matches!(step.kind, ir::ExprKind::ConstInt(1)));
    }

    #[test]
    fn error_slicing_an_int() {
        let e = analyze_err("x = 5\ny = x[1:2]\n");
        assert!(e.message.contains("sliced"), "{}", e.message);
    }

    #[test]
    fn contains_types_correctly() {
        let m = analyze_ok("b = \"ell\" in \"hello\"\nc = 2 in [1, 2]\nd = 5 not in [1, 2]\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
            panic!();
        };
        assert!(matches!(value.kind, ir::ExprKind::Contains { .. }));
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[2] else {
            panic!();
        };
        // not in == Not(Contains)
        assert!(matches!(
            &value.kind,
            ir::ExprKind::Unary { op: ir::UnOp::Not, operand }
                if matches!(operand.kind, ir::ExprKind::Contains { .. })
        ));
    }

    #[test]
    fn contains_coerces_needle_to_elem_type() {
        let m = analyze_ok("b = 1 in [1.5, 2.5]\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
            panic!();
        };
        let ir::ExprKind::Contains { needle, .. } = &value.kind else {
            panic!();
        };
        assert_eq!(needle.ty, ir::Ty::Float);
    }

    #[test]
    fn error_int_in_str() {
        let e = analyze_err("b = 5 in \"hello\"\n");
        assert!(e.message.contains("str on the left"), "{}", e.message);
    }

    #[test]
    fn pop_returns_element_type() {
        let m = analyze_ok("xs = [1.5]\nx = xs.pop()\ny = xs.pop(0)\nxs.pop()\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Float);
        assert!(matches!(value.kind, ir::ExprKind::ListPop { .. }));
        // statement-position pop is allowed and discarded
        assert!(matches!(entry.body[3], ir::Stmt::ExprStmt(_)));
    }

    #[test]
    fn fstring_lowers_to_concat_with_conversions() {
        let m = analyze_ok("x = 42\ns = f\"x={x}!\"\nprint(s)\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Str);
        // somewhere in the tree there must be an IntToStr conversion
        fn has_int_to_str(e: &ir::Expr) -> bool {
            match &e.kind {
                ir::ExprKind::IntToStr(_) => true,
                ir::ExprKind::Binary { left, right, .. } => {
                    has_int_to_str(left) || has_int_to_str(right)
                }
                _ => false,
            }
        }
        assert!(has_int_to_str(value), "{value:?}");
    }

    #[test]
    fn fstring_format_spec_lowers_to_format_value() {
        let m = analyze_ok("x = 3.14159\ns = f\"{x:.2f}\"\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
            panic!();
        };
        fn has_fmt(e: &ir::Expr) -> bool {
            match &e.kind {
                ir::ExprKind::FormatValue { value, spec } => {
                    value.ty == ir::Ty::Float
                        && matches!(&spec.kind, ir::ExprKind::ConstStr(s) if s == ".2f")
                }
                ir::ExprKind::Binary { left, right, .. } => has_fmt(left) || has_fmt(right),
                _ => false,
            }
        }
        assert!(has_fmt(value), "{value:?}");
    }

    #[test]
    fn fstring_format_spec_on_int() {
        let m = analyze_ok("n = 2\ns = f\"{n:.2f}\"\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
            panic!();
        };
        fn has_int_fmt(e: &ir::Expr) -> bool {
            match &e.kind {
                ir::ExprKind::FormatValue { value, .. } => value.ty == ir::Ty::Int,
                ir::ExprKind::Binary { left, right, .. } => has_int_fmt(left) || has_int_fmt(right),
                _ => false,
            }
        }
        assert!(has_int_fmt(value), "{value:?}");
    }

    #[test]
    fn fstring_repr_conversion_on_str() {
        let m = analyze_ok("s = \"hi\"\nt = f\"{s!r}\"\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
            panic!();
        };
        fn has_repr(e: &ir::Expr) -> bool {
            match &e.kind {
                ir::ExprKind::StrRepr(_) => true,
                ir::ExprKind::Binary { left, right, .. } => has_repr(left) || has_repr(right),
                _ => false,
            }
        }
        assert!(has_repr(value), "{value:?}");
    }

    #[test]
    fn error_fstring_of_list() {
        let e = analyze_err("xs = [1]\ns = f\"{xs}\"\nprint(s)\n");
        assert!(e.message.contains("convert"), "{}", e.message);
    }

    #[test]
    fn tuple_literal_and_unpack() {
        let m = analyze_ok("a, b = 1, 2\nprint(a)\n");
        let entry = find_func(&m, ENTRY_NAME);
        assert!(
            entry
                .body
                .iter()
                .any(|s| matches!(s, ir::Stmt::Assign { .. }))
        );
    }

    #[test]
    fn tuple_unpack_length_mismatch_is_error() {
        let e = analyze_err("a, b = (1,)\n");
        assert!(e.message.contains("not enough values"), "{}", e.message);
        let e = analyze_err("a, b = (1, 2, 3)\n");
        assert!(e.message.contains("too many values"), "{}", e.message);
    }

    #[test]
    fn dict_key_type_rejected_in_annotation() {
        let e = analyze_err("d: dict[float, int] = {}\n");
        assert!(
            e.message.contains("not supported") || e.message.contains("only int"),
            "{}",
            e.message
        );
    }

    #[test]
    fn dict_bare_get_returns_optional() {
        // Bare get(key) → Optional[V] (None on miss).
        let m = analyze_ok("d: dict[str, int] = {\"a\": 1}\nx = d.get(\"a\")\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
            panic!("{:?}", entry.body);
        };
        assert_eq!(value.ty, ir::optional_of(ir::Ty::Int));
        assert!(matches!(value.kind, ir::ExprKind::DictGet { .. }));
    }

    #[test]
    fn optional_assign_and_reject_as_int() {
        let m = analyze_ok("x: int | None = None\nx = 5\n");
        let entry = find_func(&m, ENTRY_NAME);
        assert!(
            entry
                .body
                .iter()
                .any(|s| matches!(s, ir::Stmt::GlobalAssign { .. }))
        );
        let e = analyze_err("x: int | None = None\ny: int = x\n");
        assert!(
            e.message.contains("cannot use")
                || e.message.contains("is None")
                || e.message.contains("type mismatch")
                || e.message.contains("None"),
            "{}",
            e.message
        );
    }

    #[test]
    fn is_none_lowers() {
        let m = analyze_ok("x: int | None = 1\nb = x is None\nc = x is not None\n");
        let entry = find_func(&m, ENTRY_NAME);
        let has_is = entry.body.iter().any(|s| match s {
            ir::Stmt::GlobalAssign { value, .. } => {
                matches!(value.kind, ir::ExprKind::IsNone { .. })
            }
            _ => false,
        });
        assert!(has_is, "expected IsNone in IR: {:?}", entry.body);
    }

    #[test]
    fn set_empty_needs_annotation() {
        let e = analyze_err("s = set()\n");
        assert!(
            e.message.contains("annotation") || e.message.contains("set()"),
            "{}",
            e.message
        );
    }

    #[test]
    fn class_point_lowers_with_fields_and_method() {
        let m = analyze_ok(
            "\
class Point:
    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y
    def sum(self) -> int:
        return self.x + self.y
p = Point(1, 2)
print(p.sum())
",
        );
        assert!(!m.classes.is_empty());
        assert_eq!(m.classes[0].name, "Point");
        assert!(m.classes[0].fields.iter().any(|(n, _)| n == "x"));
        assert!(
            m.funcs
                .iter()
                .any(|f| f.name == "Point.__init__" || f.name.ends_with("Point.__init__"))
        );
        assert!(
            m.funcs
                .iter()
                .any(|f| f.name == "Point.sum" || f.name.ends_with("Point.sum"))
        );
    }

    #[test]
    fn class_multi_base_rejected() {
        let e = analyze_err(
            "\
class A:
    pass
class B:
    pass
class C(A, B):
    pass
",
        );
        assert!(
            e.message
                .contains("multiple inheritance is not supported yet"),
            "{}",
            e.message
        );
    }

    #[test]
    fn class_incompatible_override_rejected() {
        let e = analyze_err(
            "\
class A:
    def m(self) -> int:
        return 1
class B(A):
    def m(self) -> str:
        return \"x\"
",
        );
        assert!(
            e.message.contains("incompatible return type"),
            "{}",
            e.message
        );
    }

    #[test]
    fn hetero_tuple_for_is_error() {
        let e = analyze_err("t = (1, \"a\")\nfor x in t:\n    print(x)\n");
        assert!(e.message.contains("heterogeneous"), "{}", e.message);
    }

    #[test]
    fn raise_and_try_lower() {
        let m = analyze_ok(
            "\
try:
    raise ValueError(\"x\")
except ValueError as e:
    print(e)
finally:
    print(\"f\")
",
        );
        let entry = find_func(&m, ENTRY_NAME);
        assert!(entry.body.iter().any(|s| matches!(s, ir::Stmt::Try { .. })));
        // `as e` binds a first-class exception object, not str.
        let e_ty = entry.locals.iter().find(|(n, _)| n == "e").map(|(_, t)| *t);
        assert_eq!(e_ty, Some(ir::Ty::Exception), "locals: {:?}", entry.locals);
    }

    #[test]
    fn except_exception_and_isinstance_lower() {
        let m = analyze_ok(
            "\
try:
    raise FileNotFoundError(\"x\")
except OSError as e:
    print(isinstance(e, OSError))
    print(str(e))
try:
    raise ValueError(\"v\")
except Exception:
    print(\"ok\")
",
        );
        let entry = find_func(&m, ENTRY_NAME);
        assert!(entry.body.iter().any(|s| matches!(s, ir::Stmt::Try { .. })));
        fn walk_expr(e: &ir::Expr, f: &mut dyn FnMut(&ir::Expr)) {
            f(e);
            match &e.kind {
                ir::ExprKind::ExcIsInstance { value, .. }
                | ir::ExprKind::ExcToStr(value)
                | ir::ExprKind::ToBool(value) => walk_expr(value, f),
                ir::ExprKind::IsInstance { value, .. } => walk_expr(value, f),
                _ => {}
            }
        }
        fn walk_stmt(s: &ir::Stmt, f: &mut dyn FnMut(&ir::Expr)) {
            match s {
                ir::Stmt::Print(args) => {
                    for a in args {
                        walk_expr(a, f);
                    }
                }
                ir::Stmt::Try {
                    body,
                    handlers,
                    orelse,
                    finally,
                } => {
                    for st in body.iter().chain(orelse).chain(finally) {
                        walk_stmt(st, f);
                    }
                    for (_, _, hb) in handlers {
                        for st in hb {
                            walk_stmt(st, f);
                        }
                    }
                }
                _ => {}
            }
        }
        let mut saw_exc_isinstance = false;
        let mut saw_exc_to_str = false;
        for s in &entry.body {
            walk_stmt(s, &mut |e| match &e.kind {
                ir::ExprKind::ExcIsInstance { .. } => saw_exc_isinstance = true,
                ir::ExprKind::ExcToStr(_) => saw_exc_to_str = true,
                _ => {}
            });
        }
        assert!(
            saw_exc_isinstance,
            "expected ExcIsInstance in {:?}",
            entry.body
        );
        assert!(saw_exc_to_str, "expected ExcToStr in {:?}", entry.body);
    }

    #[test]
    fn exception_in_list_is_error() {
        let e = analyze_err(
            "\
try:
    raise ValueError(\"x\")
except ValueError as e:
    xs = [e]
",
        );
        assert!(
            e.message.contains("exception") && e.message.contains("list"),
            "{}",
            e.message
        );
    }

    #[test]
    fn raise_counts_as_return_path() {
        let m = analyze_ok(
            "\
def f(x: int) -> int:
    if x < 0:
        raise ValueError(\"neg\")
    return x

print(f(1))
",
        );
        assert_eq!(find_func(&m, "f").ret, ir::Ty::Int);
    }

    #[test]
    fn try_raise_except_pass_missing_return() {
        // body raises but except falls through — not a valid -> int path
        let e = analyze_err(
            "\
def f() -> int:
    try:
        raise ValueError(\"x\")
    except ValueError:
        pass
print(1)
",
        );
        assert!(
            e.message.contains("return") || e.message.contains("end of its body"),
            "{}",
            e.message
        );
    }

    #[test]
    fn try_return_is_ok_with_dead_handler() {
        let m = analyze_ok(
            "\
def f() -> int:
    try:
        return 1
    except ValueError:
        pass
print(f())
",
        );
        assert_eq!(find_func(&m, "f").ret, ir::Ty::Int);
    }

    #[test]
    fn tuple_membership_lowers() {
        let m = analyze_ok("print(1 in (1, 2))\n");
        let entry = find_func(&m, ENTRY_NAME);
        let has = entry.body.iter().any(|s| match s {
            ir::Stmt::Print(args) => args
                .iter()
                .any(|a| matches!(a.kind, ir::ExprKind::Contains { .. })),
            _ => false,
        });
        assert!(
            has,
            "expected Contains for tuple membership: {:?}",
            entry.body
        );
    }

    #[test]
    fn isinstance_bool_is_int() {
        let m = analyze_ok("print(isinstance(True, int))\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::Print(args) = &entry.body[0] else {
            panic!("{:?}", entry.body);
        };
        assert!(
            matches!(args[0].kind, ir::ExprKind::ConstBool(true)),
            "{:?}",
            args[0]
        );
    }

    #[test]
    fn isinstance_rejects_variable_type() {
        let e = analyze_err("t = 1\nprint(isinstance(1, t))\n");
        assert!(
            e.message.contains("type name")
                || e.message.contains("not a variable")
                || e.message.contains("does not support type"),
            "{}",
            e.message
        );
    }

    #[test]
    fn multi_assign_joins_to_union() {
        let m = analyze_ok(
            "\
def f():
    x = 1
    x = \"a\"
    return x
print(f())
",
        );
        let f = find_func(&m, "f");
        // local x should be int|str
        let x_ty = f.locals.iter().find(|(n, _)| n == "x").map(|(_, t)| *t);
        assert!(
            x_ty.is_some_and(|t| matches!(t, ir::Ty::Union(_))),
            "expected union local, got {x_ty:?} in {:?}",
            f.locals
        );
    }

    #[test]
    fn bare_param_infers_int() {
        let m = analyze_ok(
            "\
def f(x):
    return x + 1
print(f(2))
",
        );
        let f = find_func(&m, "f");
        assert_eq!(f.params[0].1, ir::Ty::Int);
    }

    #[test]
    fn bare_param_infers_from_isinstance() {
        let m = analyze_ok(
            "\
def f(x):
    if isinstance(x, int):
        return x + 1
    return 0
print(f(3))
",
        );
        let f = find_func(&m, "f");
        assert_eq!(f.params[0].1, ir::Ty::Int);
    }

    #[test]
    fn bare_param_multi_isinstance_needs_annotation() {
        let e = analyze_err(
            "\
def f(x):
    if isinstance(x, (int, float)):
        return x + 1
    return 0
print(f(3))
",
        );
        assert!(
            e.message.contains("missing a type annotation"),
            "{}",
            e.message
        );
    }

    #[test]
    fn bare_param_isinstance_list_needs_annotation() {
        let e = analyze_err(
            "\
def f(x):
    if isinstance(x, list):
        return len(x)
    return 0
print(f([1]))
",
        );
        assert!(
            e.message.contains("missing a type annotation"),
            "{}",
            e.message
        );
    }

    #[test]
    fn and_chain_isinstance_keeps_more_specific_class() {
        let m = analyze_ok(
            "\
class A:
    def __init__(self):
        self.a = 1
class B(A):
    def __init__(self):
        self.a = 1
        self.b = 2
class C(B):
    def __init__(self):
        self.a = 1
        self.b = 2
        self.c = 3
def f(x: A):
    if isinstance(x, C) and isinstance(x, B):
        return x.c
    return 0
print(f(C()))
",
        );
        let f = find_func(&m, "f");
        assert_eq!(f.ret, ir::Ty::Int);
    }

    #[test]
    fn bare_param_infers_str_from_method() {
        let m = analyze_ok(
            "\
def f(x):
    return x.upper()
print(f(\"hi\"))
",
        );
        let f = find_func(&m, "f");
        assert_eq!(f.params[0].1, ir::Ty::Str);
    }

    #[test]
    fn empty_list_from_append_infers_elem() {
        let m = analyze_ok(
            "\
def f():
    xs = []
    xs.append(1)
    return xs
print(f())
",
        );
        let f = find_func(&m, "f");
        assert_eq!(f.ret, ir::list_of(ir::Ty::Int));
    }

    #[test]
    fn isinstance_subclass_peel_allows_subclass_field() {
        let m = analyze_ok(
            "\
class A:
    def __init__(self):
        self.a = 1
class B(A):
    def __init__(self):
        self.a = 1
        self.b = 2
def f(x: A):
    if isinstance(x, B):
        return x.b
    return x.a
print(f(B()))
",
        );
        let f = find_func(&m, "f");
        assert_eq!(f.params[0].1, ir::Ty::Class(0));
        assert_eq!(f.ret, ir::Ty::Int);
    }

    #[test]
    fn subclass_into_base_union_ok() {
        let m = analyze_ok(
            "\
class A:
    def __init__(self):
        self.a = 1
class B(A):
    def __init__(self):
        self.a = 1
x: A | int = B()
print(x)
",
        );
        let entry = find_func(&m, ENTRY_NAME);
        assert!(
            entry.body.iter().any(|s| matches!(
                s,
                ir::Stmt::GlobalAssign { name, .. } if name == "x"
            )),
            "{:?}",
            entry.body
        );
    }

    #[test]
    fn raise_new_exc_types() {
        let m = analyze_ok(
            "\
try:
    raise FileNotFoundError(\"missing\")
except FileNotFoundError as e:
    print(e)
try:
    raise OverflowError(\"big\")
except (OverflowError, ValueError):
    print(\"ov\")
",
        );
        let entry = find_func(&m, ENTRY_NAME);
        assert!(entry.body.iter().any(|s| matches!(s, ir::Stmt::Try { .. })));
    }

    #[test]
    fn import_bind_helpers() {
        assert_eq!(import_bind_name("pkg.mod", &None), "pkg");
        assert_eq!(import_bind_name("pkg.mod", &Some("m".into())), "m");
        assert_eq!(import_bound_module("pkg.mod", &None), "pkg");
        assert_eq!(import_bound_module("pkg.mod", &Some("m".into())), "pkg.mod");
    }

    #[test]
    fn submodule_map_links_parents() {
        let names = vec![
            "pkg".into(),
            "pkg.mod".into(),
            "pkg.sub".into(),
            "pkg.sub.m".into(),
        ];
        let map = build_submodule_map(&names);
        assert_eq!(map["pkg"]["mod"], "pkg.mod");
        assert_eq!(map["pkg"]["sub"], "pkg.sub");
        assert_eq!(map["pkg.sub"]["m"], "pkg.sub.m");
    }

    #[test]
    fn multi_module_reexport_analyze() {
        let mod_ast = parser::parse("VAL = 3\ndef f() -> int:\n    return VAL\n").unwrap();
        let pkg_ast = parser::parse("from pkg.mod import f, VAL\n").unwrap();
        let main_ast = parser::parse(
            "import pkg\nprint(pkg.VAL, pkg.f())\nfrom pkg import f as g\nprint(g())\n",
        )
        .unwrap();
        let m = analyze_program(&[
            ModuleInput {
                name: "pkg.mod".into(),
                ast: &mod_ast,
            },
            ModuleInput {
                name: "pkg".into(),
                ast: &pkg_ast,
            },
            ModuleInput {
                name: ENTRY_NAME.into(),
                ast: &main_ast,
            },
        ])
        .expect("reexport program should analyze");
        // Re-exported call should target origin IR name pkg.mod.f
        let entry = find_func(&m, ENTRY_NAME);
        let has_origin_call = entry.body.iter().any(|s| match s {
            ir::Stmt::Print(args) => args.iter().any(|a| {
                matches!(
                    &a.kind,
                    ir::ExprKind::Call { func, .. } if func == "pkg.mod.f"
                )
            }),
            ir::Stmt::ExprStmt(e) => matches!(
                &e.kind,
                ir::ExprKind::Call { func, .. } if func == "pkg.mod.f" || func == "pkg.__init__" || func == "pkg.mod.__init__"
            ),
            _ => false,
        });
        assert!(
            has_origin_call
                || entry.body.iter().any(|s| matches!(
                    s,
                    ir::Stmt::Print(args) if args.iter().any(|a| matches!(
                        &a.kind,
                        ir::ExprKind::Call { func, .. } if func.contains("f")
                    ))
                )),
            "expected call through re-export; body={:?}",
            entry.body
        );
    }

    #[test]
    fn mixed_class_list_from_empty_is_union() {
        let m = analyze_ok(
            "\
class Dog:
    def __init__(self):
        self.name = \"d\"
class Cat:
    def __init__(self):
        self.name = \"c\"
xs = []
xs.append(Dog())
xs.append(Cat())
print(xs[0].name)
print(len(xs))
",
        );
        let xs = m.globals.iter().find(|(n, _)| n == "xs").map(|(_, t)| *t);
        assert!(
            matches!(xs, Some(ir::Ty::List(e)) if matches!(e, ir::Ty::Union(_))),
            "expected list[Dog|Cat], got {:?}",
            xs
        );
    }
}
