//! Type resolution, parameter/function signatures, and unsupported-feature tables.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use common::{Diagnostic, Span};
use parser::ast;

use crate::prelude::*;

pub(crate) fn resolve_type(ty: ast::TypeName) -> ir::Ty {
    match ty {
        ast::TypeName::Int => ir::Ty::Int,
        ast::TypeName::Float => ir::Ty::Float,
        ast::TypeName::Bool => ir::Ty::Bool,
        ast::TypeName::Str => ir::Ty::Str,
        ast::TypeName::File => ir::Ty::File,
        ast::TypeName::Any => ir::Ty::Any,
        ast::TypeName::Iterator(t) => ir::generator_of(resolve_type(*t)),
        // A capture-free closure: the shape a module-level function or a
        // non-capturing lambda takes in value position. Captures live in the
        // closure object and a caller reads them by static type, so an
        // annotation — which cannot know them — describes exactly the
        // capture-free case, and `coerce` checks that.
        ast::TypeName::Callable { params, ret } => {
            let ps: Vec<ir::Ty> = params.iter().copied().map(resolve_type).collect();
            ir::closure_of(&ps, resolve_type(*ret))
        }
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
pub(crate) fn resolve_param_ty_opt(p: &ast::Param) -> SResult<Option<ir::Ty>> {
    if let Some(t) = p.ty {
        return Ok(Some(resolve_type_checked(t, p.span)?));
    }
    if let Some(d) = &p.default {
        return Ok(Some(infer_ty_from_default(d)?));
    }
    Ok(None)
}

/// Resolve a parameter's type, requiring annotation/default (no body inference).
pub(crate) fn resolve_param_ty(p: &ast::Param) -> SResult<ir::Ty> {
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
pub(crate) fn bare_param_infer_err(p: &ast::Param) -> Diagnostic {
    err(
        format!(
            "parameter '{}' is missing a type annotation; could not infer a unique \
             type from the function body (add e.g. '{}: int')",
            p.name, p.name
        ),
        p.span,
    )
}

thread_local! {
    /// Types for compiler-synthesized parameters, keyed by their unique names.
    ///
    /// A generator expression passes its outermost iterable as a real
    /// parameter, and that parameter's type is whatever the iterable
    /// expression lowered to — something neither an annotation nor the
    /// syntactic default inference can recover. The names are dot-prefixed and
    /// unique, so they cannot collide with anything a program can write.
    pub(crate) static SYNTH_PARAM_TYS: RefCell<HashMap<String, ir::Ty>> = RefCell::new(HashMap::new());
}

/// A Python feature PyRs knows about and does not support.
///
/// [docs/GUIDE.md](../../docs/GUIDE.md) promises that "unsupported Python
/// features produce parse/semantic errors that name the feature". For the
/// most common ones they did not: `eval(x)` reported `function 'eval' is not
/// defined`, indistinguishable from a typo, and
/// `if __name__ == "__main__":` — the single most common idiom in Python —
/// reported `name '__name__' is not defined`.
///
/// Each entry says what the feature is and what to do instead. The fallback
/// messages stay for genuine typos, which is most of what they see.
pub(crate) fn unsupported_feature(name: &str) -> Option<&'static str> {
    Some(match name {
        // Dynamism the closed-world model rules out entirely.
        "eval" | "exec" => {
            "eval() and exec() are not supported: PyRs is closed-world and \
             compiles ahead of time, so there is no interpreter to run a \
             string. Run the program with --compat if it needs one"
        }
        "compile" | "__import__" => {
            "runtime compilation and dynamic import are not supported: PyRs \
             resolves the whole import graph at compile time. Use --compat"
        }
        "getattr" | "setattr" | "hasattr" | "delattr" | "vars" | "dir" => {
            "attribute reflection is not supported: instance fields are \
             resolved statically, so an attribute named at run time cannot be \
             looked up. Use a dict, or --compat"
        }
        "globals" | "locals" => {
            "globals() and locals() are not supported: there is no name table \
             at run time. Pass the values you need as arguments"
        }
        "type" => {
            "type() is not supported: classes are not first-class values \
             here. Use isinstance(x, C) to test, or a field for a tag"
        }
        "callable" | "issubclass" => {
            "callable() and issubclass() are not supported: they need \
             first-class class and function objects, which this subset does \
             not have"
        }
        // Types with no representation yet.
        "bytes" | "bytearray" | "memoryview" => {
            "bytes, bytearray and memoryview are not supported yet: str is \
             UTF-8 text and there is no binary sequence type. Text files and \
             str cover most uses; otherwise --compat"
        }
        "complex" => "complex numbers are not supported yet: int and float only",
        "frozenset" => {
            "frozenset is not supported yet: use a set, which is not hashable \
             here either, or a sorted tuple as a key"
        }
        "slice" => {
            "slice objects are not supported: write the slice inline, as \
             xs[a:b], rather than building one"
        }
        // Typing constructs that are annotation-only or absent.
        "TypeVar" | "ParamSpec" | "TypeVarTuple" => {
            "generics are not supported: TypeVar has no meaning in a \
             monomorphic subset. A parameter's type is inferred from the body \
             when it is unambiguous"
        }
        "NamedTuple" | "TypedDict" | "dataclass" => {
            "NamedTuple, TypedDict and dataclasses are not supported yet: \
             declare a class with annotated fields assigned in __init__"
        }
        "id" | "hash" => {
            "id() and hash() are not supported: object identity is not \
             stable here and there is no general hash protocol"
        }
        "iter" => {
            "iter() is not supported yet: iterate directly with a for loop, \
             or use next() on a generator"
        }
        "open_binary" | "breakpoint" | "help" | "input_raw" => return Option::None,
        _ => return Option::None,
    })
}

/// The module-level names that are Python's but not values here.
pub(crate) fn unsupported_dunder(name: &str) -> Option<&'static str> {
    Some(match name {
        "__file__" | "__package__" | "__doc__" | "__spec__" => {
            "module attributes like __file__ and __package__ are not \
             supported yet: there is no module object at run time"
        }
        "__slots__" => {
            "__slots__ is not supported: every class already has a fixed set \
             of fields, declared by assignment in __init__"
        }
        "__dict__" => {
            "__dict__ is not supported: instance fields are resolved \
             statically and there is no attribute dictionary"
        }
        _ => return Option::None,
    })
}

/// `sys.stderr` / `sys.stdout` written as a `print(file=...)` destination.
///
/// Returns whether it is stderr. Neither is a value anywhere else: there is
/// no file object behind them, and `print` is the only thing that can name
/// one.
pub(crate) fn stream_keyword(value: &ast::Expr, ctx: &FnCtx) -> Option<bool> {
    let ast::ExprKind::Attribute { base, attr, .. } = &value.kind else {
        return Option::None;
    };
    let ast::ExprKind::Name(alias) = &base.kind else {
        return Option::None;
    };
    if !ctx.sys_alias(alias) {
        return Option::None;
    }
    match attr.as_str() {
        "stderr" => Some(true),
        "stdout" => Some(false),
        _ => Option::None,
    }
}

pub(crate) fn set_synth_param_ty(name: &str, ty: ir::Ty) {
    SYNTH_PARAM_TYS.with(|m| m.borrow_mut().insert(name.to_string(), ty));
}

pub(crate) fn synth_param_ty(name: &str) -> Option<ir::Ty> {
    SYNTH_PARAM_TYS.with(|m| m.borrow().get(name).copied())
}

/// Restore (or remove) an entry, so a hint scoped to one lambda cannot leak
/// onto an unrelated parameter that happens to share its name.
pub(crate) fn restore_synth_param_ty(name: &str, prev: Option<ir::Ty>) {
    SYNTH_PARAM_TYS.with(|m| match prev {
        Some(t) => {
            m.borrow_mut().insert(name.to_string(), t);
        }
        Option::None => {
            m.borrow_mut().remove(name);
        }
    });
}

pub(crate) fn clear_synth_param_tys() {
    SYNTH_PARAM_TYS.with(|m| m.borrow_mut().clear());
}

thread_local! {
    /// Yield types for compiler-synthesized generator functions, by name.
    ///
    /// A generator expression's element type depends on the loop targets,
    /// which exist only inside the synthesized body — so neither a return
    /// annotation nor the first-yield scan can recover it. The lowering
    /// computes it up front, by binding the targets exactly as a list
    /// comprehension would, and leaves it here.
    pub(crate) static SYNTH_YIELD_TYS: RefCell<HashMap<String, ir::Ty>> = RefCell::new(HashMap::new());
}

pub(crate) fn set_synth_yield_ty(name: &str, ty: ir::Ty) {
    SYNTH_YIELD_TYS.with(|m| m.borrow_mut().insert(name.to_string(), ty));
}

pub(crate) fn synth_yield_ty(name: &str) -> Option<ir::Ty> {
    SYNTH_YIELD_TYS.with(|m| m.borrow().get(name).copied())
}

/// Resolve all formal params, inferring bare ones monomorphically from body usage.
pub(crate) fn resolve_params_with_body_infer(
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
        let ty = match synth_param_ty(&p.name) {
            Some(t) => t,
            Option::None => match resolve_param_ty_opt(p)? {
                Some(t) => t,
                Option::None => {
                    bare_idxs.push(params.len());
                    ir::Ty::Int // placeholder
                }
            },
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
pub(crate) fn infer_ty_from_default(expr: &ast::Expr) -> SResult<ir::Ty> {
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
pub(crate) fn resolve_type_checked(ty: ast::TypeName, span: Span) -> SResult<ir::Ty> {
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
                    unsupported_feature(name)
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            format!("unknown type '{name}' (not a builtin or defined class)")
                        }),
                    span,
                ));
            };
            Ok(ir::Ty::Class(id))
        }
        other => Ok(resolve_type(other)),
    }
}

pub(crate) fn elem_of(ty: ir::Ty, span: Span) -> SResult<ir::Ty> {
    match ty {
        ir::Ty::File => Err(err("files cannot be stored in lists yet", span)),
        // Exception objects are allowed as list/tuple elements (v0.24).
        other => Ok(other),
    }
}

/// Reject types that still cannot live in containers (files only for now).
pub(crate) fn reject_exception_container_elem(ty: ir::Ty, span: Span, what: &str) -> SResult<()> {
    match ty {
        ir::Ty::File => Err(err(
            format!("file objects cannot be stored in {what} yet"),
            span,
        )),
        ir::Ty::Union(ms) if ms.iter().any(|m| matches!(m, ir::Ty::File)) => Err(err(
            format!("unions containing file objects cannot be stored in {what} yet"),
            span,
        )),
        _ => Ok(()),
    }
}

/// Keys for dict/set. `bool` is excluded deliberately even though it hashes:
/// CPython's `True == 1` means a bool key would collide with an int one, and
/// this subset does not model that.
pub(crate) fn check_hashable_key(ty: ir::Ty, span: Span, what: &str) -> SResult<()> {
    if is_hashable_key_ty(ty) {
        return Ok(());
    }
    Err(err(
        format!(
            "{what} keys/elements of type {other} are not supported yet \
             (int, str, or a tuple of those)",
            other = ty
        ),
        span,
    ))
}

/// Hashable as a dict/set key: int, str, and tuples of hashable things.
pub(crate) fn is_hashable_key_ty(ty: ir::Ty) -> bool {
    match ty {
        ir::Ty::Int | ir::Ty::Str => true,
        ir::Ty::Tuple(elems) => elems.iter().all(|e| is_hashable_key_ty(*e)),
        _ => false,
    }
}

/// Resolve an exception type written in `raise` / `except`.
///
/// Builtins win over a user class of the same name, matching the shadowing a
/// program would get from CPython's builtins scope. This runs in the semantic
/// phase rather than the parser because only here is the set of
/// `class E(Exception)` declarations known.
pub(crate) fn resolve_exc_name(e: &ast::ExcName) -> SResult<ir::ExcType> {
    if let Ok(builtin) = name_to_exc_type(&e.name, e.span) {
        return Ok(builtin);
    }
    // Same module context bare class lookups use.
    let module = with_class_env(|env| env.current_module.clone());
    if let Some(tag) = lookup_exc_class(&module, &e.name) {
        return Ok(ir::ExcType::User(tag));
    }
    Err(err(
        format!(
            "unknown exception type '{}'; define it with \
             `class {}(Exception): pass`, or use a builtin ({})",
            e.name,
            e.name,
            ir::ExcType::all_names()
        ),
        e.span,
    ))
}

#[derive(Debug, Clone)]
pub(crate) struct ParamSig {
    pub(crate) name: String,
    pub(crate) ty: ir::Ty,
    /// Cloned AST default; lowered at each call site when the arg is omitted.
    pub(crate) default: Option<ast::Expr>,
}

#[derive(Debug, Clone)]
pub(crate) struct FuncSig {
    pub(crate) params: Vec<ParamSig>,
    /// Number of leading params that may only be passed positionally (`/`).
    pub(crate) posonly_end: usize,
    /// Index at which keyword-only params begin (`*` / `*args`), or `None`
    /// when the signature has neither. See `ast::FuncDef::kwonly_start` for
    /// why this is an `Option` and not an index with a `0` default.
    pub(crate) kwonly_start: Option<usize>,
    /// `*args: T` — element type `T`; IR param is `list[T]`.
    pub(crate) vararg: Option<ParamSig>,
    /// `**kwargs: T` — value type `T`; IR param is `dict[str, T]`.
    pub(crate) kwarg: Option<ParamSig>,
    pub(crate) ret: ir::Ty,
    pub(crate) span: Span,
    /// True when the function body contains `yield` (returns a generator).
    pub(crate) is_generator: bool,
    /// Element type yielded when `is_generator`.
    pub(crate) yield_ty: Option<ir::Ty>,
    /// Frame slot count for generator resume (params + locals); 0 if unknown.
    pub(crate) gen_frame_slots: i64,
}
