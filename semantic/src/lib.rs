//! Semantic analysis: name resolution, type checking, and lowering the AST
//! into the typed IR.
//!
//! Typing rules (a statically-typed subset of Python, mypy-flavored):
//! - `int`, `float`, `bool`, `str`, `list[T]` values; `bool` is assignable
//!   to `int`, and `int`/`bool` are assignable to `float` (implicit
//!   promotion casts are inserted).
//! - a variable's type is fixed by its first assignment and cannot change.
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
    }
}

/// Resolve a parameter's type: explicit annotation, else infer from a
/// constant/simple default, else diagnostic.
fn resolve_param_ty(p: &ast::Param) -> SResult<ir::Ty> {
    if let Some(t) = p.ty {
        return resolve_type_checked(t, p.span);
    }
    if let Some(d) = &p.default {
        return infer_ty_from_default(d);
    }
    Err(err(
        format!(
            "parameter '{}' is missing a type annotation and has no default to \
             infer from (e.g. '{}: int' or '{}: int = 0')",
            p.name, p.name, p.name
        ),
        p.span,
    ))
}

/// Infer a type from a simple default expression (literals and short forms).
fn infer_ty_from_default(expr: &ast::Expr) -> SResult<ir::Ty> {
    match &expr.kind {
        ast::ExprKind::Int(_) => Ok(ir::Ty::Int),
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
        ast::ExprKind::ListLit(items) if items.is_empty() => Err(err(
            "cannot infer type of default []; annotate the parameter \
             (e.g. 'xs: list[int] = []')",
            expr.span,
        )),
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
            Ok(ir::list_of(elem))
        }
        ast::TypeName::Tuple(elems) => {
            let mut ts = Vec::with_capacity(elems.len());
            for e in elems {
                let t = resolve_type_checked(*e, span)?;
                if t == ir::Ty::None || matches!(t, ir::Ty::Union(_)) || t == ir::Ty::File {
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
        other => Ok(resolve_type(other)),
    }
}

fn elem_of(ty: ir::Ty, span: Span) -> SResult<ir::Ty> {
    match ty {
        ir::Ty::File => Err(err("files cannot be stored in lists yet", span)),
        // Pure None list elements are allowed only as part of a union annotation
        // path; a bare None elem type is rejected when building untyped lists.
        other => Ok(other),
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

fn extract_union_member(value: ir::Expr, member: ir::Ty, _span: Span) -> SResult<ir::Expr> {
    Ok(ir::Expr {
        ty: member,
        kind: ir::ExprKind::FromUnion {
            value: Box::new(value),
        },
    })
}

/// Flow-sensitive narrowing for `x is None` / `x is not None` / `not (x is None)`.
/// Also peels `A and B` / `A or B` so `x is not None and flag` narrows `x` in
/// the then-arm (and complementary or-arms). Returns (then, else) maps.
fn narrowing_from_condition(
    cond: &ast::Expr,
    ctx: &FnCtx,
) -> (HashMap<String, ir::Ty>, HashMap<String, ir::Ty>) {
    let mut then_m = HashMap::new();
    let mut else_m = HashMap::new();
    match &cond.kind {
        ast::ExprKind::Unary {
            op: ast::UnaryOp::Not,
            operand,
        } => {
            let (t, e) = narrowing_from_condition(operand, ctx);
            return (e, t);
        }
        // `A and B`: then sees both then-refs; else is not a simple complement
        // (A may be true and B false), so only then-refs are applied.
        ast::ExprKind::Binary {
            op: ast::BinOp::And,
            left,
            right,
        } => {
            let (lt, _) = narrowing_from_condition(left, ctx);
            let (rt, _) = narrowing_from_condition(right, ctx);
            then_m.extend(lt);
            then_m.extend(rt);
            return (then_m, else_m);
        }
        // `A or B`: else sees both else-refs (both failed); then is mixed.
        ast::ExprKind::Binary {
            op: ast::BinOp::Or,
            left,
            right,
        } => {
            let (_, le) = narrowing_from_condition(left, ctx);
            let (_, re) = narrowing_from_condition(right, ctx);
            else_m.extend(le);
            else_m.extend(re);
            return (then_m, else_m);
        }
        ast::ExprKind::Binary {
            op: op @ (ast::BinOp::Is | ast::BinOp::IsNot),
            left,
            right,
        } => {
            let not = matches!(op, ast::BinOp::IsNot);
            let (name, name_ty) = match (&left.kind, &right.kind) {
                (ast::ExprKind::Name(n), ast::ExprKind::NoneLit) => (
                    n.as_str(),
                    ctx.locals
                        .get(n)
                        .copied()
                        .or_else(|| ctx.cell_locals.get(n).copied())
                        .or_else(|| {
                            // Module Optionals: free reads (no `global` needed)
                            // and explicit `global` / entry use GlobalLoad.
                            if !ctx.locals.contains_key(n) && !ctx.cell_locals.contains_key(n) {
                                ctx.globals.get(n).copied()
                            } else {
                                None
                            }
                        }),
                ),
                (ast::ExprKind::NoneLit, ast::ExprKind::Name(n)) => (
                    n.as_str(),
                    ctx.locals
                        .get(n)
                        .copied()
                        .or_else(|| ctx.cell_locals.get(n).copied())
                        .or_else(|| {
                            if !ctx.locals.contains_key(n) && !ctx.cell_locals.contains_key(n) {
                                ctx.globals.get(n).copied()
                            } else {
                                None
                            }
                        }),
                ),
                _ => return (then_m, else_m),
            };
            let Some(storage_ty) = name_ty else {
                return (then_m, else_m);
            };
            // Prefer an active refinement when present (nested checks).
            let ty = ctx
                .type_refinements
                .get(name)
                .copied()
                .unwrap_or(storage_ty);
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
}

/// A fully analyzed module's exported surface, for cross-module lookup.
struct ModuleData {
    funcs: HashMap<String, FuncSig>,
    globals: HashMap<String, ir::Ty>,
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
const BUILTINS: [&str; 11] = [
    "print", "len", "range", "input", "open", "abs", "min", "max", "sum", "sorted", "set",
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
            let mut params = Vec::new();
            let mut seen_names = std::collections::HashSet::new();
            for p in &f.params {
                let ty = resolve_param_ty(p)?;
                if ty == ir::Ty::None {
                    return Err(err(
                        format!("parameter '{}' cannot have type None", p.name),
                        p.span,
                    ));
                }
                if !seen_names.insert(p.name.clone()) {
                    return Err(err(
                        format!("duplicate parameter name '{}'", p.name),
                        p.span,
                    ));
                }
                params.push(ParamSig {
                    name: p.name.clone(),
                    ty,
                    default: p.default.clone(),
                });
            }
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
        ast::ExprKind::Int(_) => Some(ir::Ty::Int),
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
        ast::ExprKind::Call { func, .. } => match known_rets.get(func).copied() {
            Some(ir::Ty::None) => None, // still unknown / void
            Some(t) => Some(t),
            None => None,
        },
        ast::ExprKind::Cast { ty, .. } => resolve_type_checked(*ty, e.span).ok(),
        _ => None,
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
    let mut export = assigned.clone();
    loop {
        let mut changed = false;
        for m in modules {
            for stmt in &m.ast.body {
                let ast::StmtKind::FromImport {
                    module: src, names, ..
                } = &stmt.kind
                else {
                    continue;
                };
                for (name, alias, _) in names {
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
        ast::ExprKind::Int(_) => Some(ir::Ty::Int),
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
            module: src, names, ..
        } => {
            if src == child || is_strict_package_prefix(child, src) {
                return true;
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
                    names: imported, ..
                } = &stmt.kind
                {
                    for (name, alias, _) in imported {
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
                    module: src, names, ..
                } => {
                    for (name, alias, _) in names {
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
            ast::StmtKind::FromImport { names, .. } => {
                for (name, alias, _) in names {
                    let bound = alias.as_ref().unwrap_or(name);
                    if bound == local {
                        last_import = true;
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
    let mut export = own_funcs.clone();
    loop {
        let mut changed = false;
        for m in modules {
            for stmt in &m.ast.body {
                let ast::StmtKind::FromImport {
                    module: src, names, ..
                } = &stmt.kind
                else {
                    continue;
                };
                for (name, alias, _) in names {
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
        ast::AssignTarget::Index { .. } => {}
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
fn collect_imports(
    module: &ast::Module,
    self_name: &str,
    last_exports: &HashMap<String, HashMap<String, LastExport>>,
    export_funcs: &HashMap<String, HashMap<String, FuncSig>>,
    export_values: &HashMap<String, std::collections::HashSet<String>>,
    submodules: &HashMap<String, HashMap<String, String>>,
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
                span,
                ..
            } => {
                if m == "sys" {
                    return Err(err(
                        "'from sys import ...' is not supported; use 'import sys' \
                         and 'sys.argv'",
                        *span,
                    ));
                }
                for (name, alias, nspan) in names {
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
                            imports.insert(
                                local.clone(),
                                ImportBinding::Symbol {
                                    module: m.clone(),
                                    name: name.clone(),
                                },
                            );
                            if m == self_name || is_func || is_value {
                                self_value_bound.insert(local);
                            }
                        }
                        None => {
                            if let Some(full) = sub_full {
                                imports.insert(local.clone(), ImportBinding::Module(full));
                            } else if is_func || is_value {
                                imports.insert(
                                    local.clone(),
                                    ImportBinding::Symbol {
                                        module: m.clone(),
                                        name: name.clone(),
                                    },
                                );
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
fn collect_imports_block(
    stmts: &[ast::Stmt],
    self_name: &str,
    last_exports: &HashMap<String, HashMap<String, LastExport>>,
    export_funcs: &HashMap<String, HashMap<String, FuncSig>>,
    export_values: &HashMap<String, std::collections::HashSet<String>>,
    submodules: &HashMap<String, HashMap<String, String>>,
    self_value_bound: &HashSet<String>,
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
    let _ = (
        self_name,
        last_exports,
        export_funcs,
        export_values,
        submodules,
        self_value_bound,
    );
    collect_imports(
        &m,
        self_name,
        last_exports,
        export_funcs,
        export_values,
        submodules,
    )
}

/// Analyze a whole program: several modules with cross-file imports.
/// `modules` is in topological order (dependencies first, root last);
/// diagnostics are tagged with the module index as their file id.
pub fn analyze_program(modules: &[ModuleInput]) -> SResult<ir::Module> {
    assert!(!modules.is_empty(), "a program needs at least one module");
    clear_closure_defaults();
    let root_idx = modules.len() - 1;

    // pass 1: every module's own signatures and assignment-name surface
    let mut own_funcs: HashMap<String, HashMap<String, FuncSig>> = HashMap::new();
    let mut all_orders: Vec<Vec<&ast::FuncDef>> = Vec::new();
    let mut assigned_names: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    let module_names: Vec<String> = modules.iter().map(|m| m.name.clone()).collect();
    let submodules = build_submodule_map(&module_names);
    for (i, m) in modules.iter().enumerate() {
        let (funcs, order) = collect_sigs(m.ast).map_err(|d| d.with_file(i))?;
        own_funcs.insert(m.name.clone(), funcs);
        all_orders.push(order);
        assigned_names.insert(m.name.clone(), collect_assigned_names(m.ast));
    }
    // pass 1b: export surface includes package re-exports; last-binding map
    let export_funcs = expand_export_funcs(modules, &own_funcs);
    let export_values = expand_export_values(modules, &assigned_names, &export_funcs, &submodules);
    let last_exports = compute_last_exports(modules, &submodules, &export_funcs, &export_values);
    let partial_prelim = build_partial_prelim(modules);
    let partial_funcs = build_partial_funcs(modules);
    let partial_reexports = build_partial_reexports(modules);
    let package_final_values = build_package_final_values(modules);

    // pass 2: import bindings (validated against the export surface)
    let mut all_imports: Vec<HashMap<String, ImportBinding>> = Vec::new();
    for (i, m) in modules.iter().enumerate() {
        let imports = collect_imports(
            m.ast,
            &m.name,
            &last_exports,
            &export_funcs,
            &export_values,
            &submodules,
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
            .filter(|s| !matches!(s.kind, ast::StmtKind::FuncDef(_)))
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
            reexports: HashMap::new(),
        };
        // Dependencies are already in `mods` (topo order), so re-exports can
        // resolve origin types/sigs. Parent packages that import children are
        // lowered after those children.
        apply_reexports(&mut data, &own_func_names, &all_imports[i], &mods, m.ast);
        mods.insert(m.name.clone(), data);
    }

    Ok(ir::Module {
        funcs: out_funcs,
        globals: out_globals,
        entry: ENTRY_NAME.to_string(),
    })
}

/// Seed module globals from simple top-level assignments (literals / names)
/// so function bodies can type-check against them before init is lowered.
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
    for st in script {
        if let ast::StmtKind::Assign {
            targets,
            annotation,
            value,
        } = &st.kind
        {
            let ty = annotation
                .as_ref()
                .and_then(|t| resolve_type_checked(*t, st.span).ok())
                .or_else(|| seed_ty_from_expr(value));
            let Some(ty) = ty else {
                continue;
            };
            for t in targets {
                if let ast::AssignTarget::Name { name, .. } = t {
                    use std::collections::hash_map::Entry;
                    if let Entry::Vacant(e) = globals.entry(name.clone()) {
                        e.insert(ty);
                        globals_order.push((own(name), ty));
                    }
                }
            }
        }
    }
}

fn seed_ty_from_expr(e: &ast::Expr) -> Option<ir::Ty> {
    match &e.kind {
        ast::ExprKind::Int(_) => Some(ir::Ty::Int),
        ast::ExprKind::Float(_) => Some(ir::Ty::Float),
        ast::ExprKind::Bool(_) => Some(ir::Ty::Bool),
        ast::ExprKind::Str(_) | ast::ExprKind::JoinedStr(_) => Some(ir::Ty::Str),
        ast::ExprKind::NoneLit => Some(ir::Ty::None),
        ast::ExprKind::Unary {
            op: ast::UnaryOp::Neg | ast::UnaryOp::Invert,
            operand,
        } => seed_ty_from_expr(operand),
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
    /// Nesting depth of `finally` blocks (yield in finally is rejected until
    /// exit-state is persisted across suspend like try phase).
    finally_depth: usize,
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
    lower_function_inner(
        f,
        mctx,
        globals,
        globals_order,
        is_entry,
        None,
        HashMap::new(),
    )
}

/// `capture_params`: leading params for nested functions (free vars), already typed.
/// `seed_nested`: sibling (and self) nested functions visible for calls.
fn lower_function_inner(
    f: &ast::FuncDef,
    mctx: &ModuleCtx,
    globals: &mut HashMap<String, ir::Ty>,
    globals_order: &mut Vec<(String, ir::Ty)>,
    is_entry: bool,
    capture_params: Option<Vec<(String, ir::Ty)>>,
    seed_nested: HashMap<String, NestedFnInfo>,
) -> SResult<(ir::Function, Vec<ir::Function>)> {
    let mut params = Vec::new();
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
        finally_depth: 0,
        local_imports: HashMap::new(),
        sibling_nonlocal_names: HashSet::new(),
        cell_candidates: HashSet::new(),
        late_bind_tys: HashMap::new(),
        outer_body_assigned: HashSet::new(),
        lowered_nested: HashSet::new(),
    };

    // Nonlocals declared in nested defs — free captures of these use cells.
    ctx.sibling_nonlocal_names = collect_nested_nonlocals(&f.body);
    ctx.cell_candidates = collect_cell_candidate_names(&f.body);
    ctx.outer_body_assigned = assigned_names_in_stmts(&f.body);
    // Types for late free captures (nested def before assignment).
    ctx.late_bind_tys = infer_late_bind_types(&f.body, &f.params, ctx.globals);

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

    for p in &f.params {
        let ty = resolve_param_ty(p)?;
        if ctx.locals.insert(p.name.clone(), ty).is_some() {
            return Err(err(format!("duplicate parameter '{}'", p.name), p.span));
        }
        params.push((p.name.clone(), ty));
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
    let mut params = Vec::new();
    let mut seen = HashSet::new();
    for p in &f.params {
        let ty = resolve_param_ty(p)?;
        if !seen.insert(p.name.clone()) {
            return Err(err(
                format!("duplicate parameter name '{}'", p.name),
                p.span,
            ));
        }
        params.push(ParamSig {
            name: p.name.clone(),
            ty,
            default: p.default.clone(),
        });
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
        ast::ExprKind::Int(_) => Some(ir::Ty::Int),
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

    // Build nested signature (params / *args / **kwargs).
    let mut params = Vec::new();
    let mut seen = HashSet::new();
    for p in &f.params {
        let ty = resolve_param_ty(p)?;
        if ty == ir::Ty::None {
            return Err(err(
                format!("parameter '{}' cannot have type None", p.name),
                p.span,
            ));
        }
        if !seen.insert(p.name.clone()) {
            return Err(err(
                format!("duplicate parameter name '{}'", p.name),
                p.span,
            ));
        }
        params.push(ParamSig {
            name: p.name.clone(),
            ty,
            default: p.default.clone(),
        });
    }
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
        ast::ExprKind::ListComp {
            elem, iter, cond, ..
        } => {
            walk_expr_for_lambdas(elem, out);
            walk_expr_for_lambdas(iter, out);
            if let Some(c) = cond {
                walk_expr_for_lambdas(c, out);
            }
        }
        ast::ExprKind::JoinedStr(parts) => {
            for p in parts {
                if let ast::FStringPart::Expr { expr, .. } = p {
                    walk_expr_for_lambdas(expr, out);
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
            var, body, orelse, ..
        } => {
            out.insert(var.clone());
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
        ast::AssignTarget::Index { .. } => {}
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
        ast::ExprKind::ListComp {
            elem, iter, cond, ..
        } => {
            collect_called_func_names_in_expr(elem, out);
            collect_called_func_names_in_expr(iter, out);
            if let Some(c) = cond {
                collect_called_func_names_in_expr(c, out);
            }
        }
        ast::ExprKind::Cast { arg, .. } => collect_called_func_names_in_expr(arg, out),
        ast::ExprKind::JoinedStr(parts) => {
            for p in parts {
                if let ast::FStringPart::Expr { expr, .. } = p {
                    collect_called_func_names_in_expr(expr, out);
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
            iter, body, orelse, ..
        } => {
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
        ast::ExprKind::ListComp {
            elem, iter, cond, ..
        } => {
            collect_used_names_in_expr(elem, out);
            collect_used_names_in_expr(iter, out);
            if let Some(c) = cond {
                collect_used_names_in_expr(c, out);
            }
        }
        ast::ExprKind::JoinedStr(parts) => {
            for p in parts {
                if let ast::FStringPart::Expr { expr, .. } = p {
                    collect_used_names_in_expr(expr, out);
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
            span,
            ..
        } => {
            let _ = span;
            // package / module body, then any submodules pulled in by name
            if !module.is_empty() && module != "sys" {
                out.extend(init_calls_for(module));
            }
            for (name, alias, nspan) in names {
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
            // evaluates for side effects). Non-None send / throw still out.
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
                        if *existing != ir::Ty::Str {
                            ctx.try_depth -= 1;
                            return Err(err(
                                format!(
                                    "type mismatch in assignment to '{n}': expected \
                                     {existing}, found str"
                                ),
                                *span,
                            ));
                        }
                    } else {
                        ctx.locals.insert(n.clone(), ir::Ty::Str);
                        ctx.locals_order.push((n.clone(), ir::Ty::Str));
                    }
                    Some(n.clone())
                } else {
                    None
                };
                let body_h = lower_nested_block(&h.body, ctx)?;
                handlers_ir.push((h.exc.map(ast_exc_to_ir), name, body_h));
            }
            let orelse_ir = lower_nested_block(orelse, ctx)?;
            ctx.finally_depth += 1;
            let finally_ir = lower_nested_block(finally, ctx)?;
            ctx.finally_depth -= 1;
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
            var,
            var_span,
            iter,
            body,
            orelse,
        } => lower_for(var, *var_span, iter, body, orelse, ctx, out),
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
            let left = to_bool(subject.clone(), span)?;
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
    let base_ir = lower_expr(base, ctx)?;
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
            // send(None) ≡ next(g); non-None send is not supported yet.
            if args.len() != 1 {
                return Err(err(
                    format!("send() takes exactly one argument ({} given)", args.len()),
                    method_span,
                ));
            }
            let v = lower_expr(&args[0], ctx)?;
            if !matches!(v.kind, ir::ExprKind::ConstNone) && v.ty != ir::Ty::None {
                return Err(err(
                    "generator.send(value) only supports send(None) in this subset \
                     (equivalent to next()); non-None send is not supported yet",
                    args[0].span,
                ));
            }
            // Discard the next value (statement form).
            let next = ir::Expr {
                ty: ir::optional_of(yield_ty),
                kind: ir::ExprKind::GeneratorNext(Box::new(base_ir)),
            };
            Ok(ir::Stmt::ExprStmt(next))
        }
        "throw" => Err(err("generator.throw() is not supported yet", method_span)),
        _ => Err(err(
            format!("generator method '{method}' is not supported yet (supported: close, send)"),
            method_span,
        )),
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
        "get" | "pop" | "keys" | "values" | "items" => {
            let call = lower_dict_method(base_ir, key_ty, val_ty, method, method_span, args, ctx)?;
            Ok(ir::Stmt::ExprStmt(call))
        }
        _ => Err(err(
            format!(
                "dict method '{method}' is not supported yet (supported: get, pop, \
                 keys, values, items, clear)"
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
        _ => Err(err(
            format!(
                "set method '{method}' is not supported yet (supported: add, remove, \
                 discard, clear)"
            ),
            method_span,
        )),
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
        _ => Err(err(
            format!(
                "dict method '{method}' is not supported yet (supported: get, pop, \
                 keys, values, items, clear)"
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
    // Propagate expected types into empty / typed literals
    let lowered = match (&value.kind, ann_ty) {
        (ast::ExprKind::ListLit(items), Some(ir::Ty::List(elem))) => {
            lower_list_lit(items, Some(*elem), value.span, ctx)?
        }
        (ast::ExprKind::DictLit(items), Some(ir::Ty::Dict { key, value: val })) => {
            lower_dict_lit(items, Some((*key, *val)), value.span, ctx)?
        }
        (ast::ExprKind::SetLit(items), Some(ir::Ty::Set(elem))) => {
            lower_set_lit(items, Some(*elem), value.span, ctx)?
        }
        (ast::ExprKind::TupleLit(items), Some(ir::Ty::Tuple(elems))) => {
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
    let existing = if is_global {
        ctx.globals.get(name).copied()
    } else if let Some(&t) = ctx.cell_locals.get(name) {
        Some(t)
    } else {
        ctx.locals.get(name).copied()
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
    // member of a union target (e.g. `x = x + 1` after `is not None`).
    ctx.type_refinements.remove(name);
    if matches!(target_ty, ir::Ty::Union(_))
        && let Some(member) = refined_member_after_assign(rhs_ty, target_ty)
    {
        ctx.type_refinements.insert(name.to_string(), member);
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
        ast::AssignTarget::Tuple(_) | ast::AssignTarget::Starred { .. } => Err(err(
            "augmented assignment to a tuple is not supported",
            span,
        )),
    }
}

// ---- for loops ----

fn lower_for(
    var: &str,
    var_span: Span,
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
        return lower_for_range(var, var_span, &plain, iter.span, body, orelse, ctx, out);
    }

    // general case: list/string by index, or file via readline until ""
    let seq = lower_expr(iter, ctx)?;
    match seq.ty {
        ir::Ty::File => lower_for_file(var, var_span, seq, body, orelse, ctx, out),
        ir::Ty::List(_) | ir::Ty::Str | ir::Ty::Tuple(_) => {
            lower_for_indexed(var, var_span, seq, body, orelse, ctx, out)
        }
        ir::Ty::Dict { key, .. } => {
            // `for k in d` iterates keys (insertion order)
            let keys = ir::Expr {
                ty: ir::list_of(*key),
                kind: ir::ExprKind::DictKeys(Box::new(seq)),
            };
            lower_for_indexed(var, var_span, keys, body, orelse, ctx, out)
        }
        ir::Ty::Set(elem) => {
            let els = ir::Expr {
                ty: ir::list_of(*elem),
                kind: ir::ExprKind::SetToList(Box::new(seq)),
            };
            lower_for_indexed(var, var_span, els, body, orelse, ctx, out)
        }
        ir::Ty::Generator { yield_ty } => {
            lower_for_generator(var, var_span, seq, *yield_ty, body, orelse, ctx, out)
        }
        other => Err(err(format!("'{other}' object is not iterable"), iter.span)),
    }
}

/// `for x in gen:` via GeneratorNext → optional yield|None until None.
#[allow(clippy::too_many_arguments)]
fn lower_for_generator(
    var: &str,
    var_span: Span,
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
        kind: ir::ExprKind::GeneratorNext(Box::new(gen_local)),
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
    let bind = bind_name(var, var_span, None, extracted, var_span, ctx)?;
    ctx.loop_depth += 1;
    let user_body = lower_nested_block(body, ctx)?;
    ctx.loop_depth -= 1;
    restore_refinements_after_for(ctx, entry_ref, var, body, orelse);
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
                let mut b = vec![bind];
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
/// the body (or loop variable) may rebind. Zero-trip loops must not keep
/// body-only peels (e.g. `for i in range(0): x = 5` must not leave `x` as int).
fn restore_refinements_after_for(
    ctx: &mut FnCtx,
    entry: HashMap<String, ir::Ty>,
    var: &str,
    body: &[ast::Stmt],
    orelse: &[ast::Stmt],
) {
    ctx.type_refinements = entry;
    for name in assigned_names_in_stmts(body) {
        ctx.type_refinements.remove(&name);
    }
    ctx.type_refinements.remove(var);
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

/// `for line in f:` — while more: line = readline; if not line: more=False else: body
fn lower_for_file(
    var: &str,
    var_span: Span,
    file: ir::Expr,
    body: &[ast::Stmt],
    orelse: &[ast::Stmt],
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
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

    let line = ir::Expr {
        ty: ir::Ty::Str,
        kind: ir::ExprKind::FileCall {
            func: ir::FileFn::ReadLine,
            args: vec![file_local],
        },
    };
    let entry_ref = ctx.type_refinements.clone();
    let bind = bind_name(var, var_span, None, line, var_span, ctx)?;

    let line_local = if let ir::Stmt::Assign { name, .. } = &bind {
        ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::Local(name.clone()),
        }
    } else if let ir::Stmt::GlobalAssign { name, .. } = &bind {
        ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::GlobalLoad(name.clone()),
        }
    } else {
        return Err(err(
            "internal error: for-file loop variable binding",
            var_span,
        ));
    };
    let truthy = to_bool(line_local, var_span)?;
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
    restore_refinements_after_for(ctx, entry_ref, var, body, orelse);

    let stop = ir::Stmt::Assign {
        name: more_t,
        value: ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ConstBool(false),
        },
    };
    let loop_body = vec![
        bind,
        ir::Stmt::If {
            branches: vec![(not_line, vec![stop]), (truthy, user_body)],
            orelse: vec![],
        },
    ];

    push_loop_with_else(more_local, loop_body, vec![], orelse, ctx, out)?;
    clear_orelse_assigns(ctx, orelse);
    Ok(())
}

/// `for x in xs` / `for c in s` — index from 0 to len (re-read each iteration).
fn lower_for_indexed(
    var: &str,
    var_span: Span,
    seq: ir::Expr,
    body: &[ast::Stmt],
    orelse: &[ast::Stmt],
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
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
                        var_span,
                    ));
                }
            }
        }
        other => {
            return Err(err(
                format!("internal error: lower_for_indexed on {other}"),
                var_span,
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

    // var = seq[idx] as the first statement of the body
    let element = ir::Expr {
        ty: elem_ty,
        kind: ir::ExprKind::Index {
            base: Box::new(seq_local),
            index: Box::new(idx_local.clone()),
        },
    };
    let entry_ref = ctx.type_refinements.clone();
    let bind = bind_name(var, var_span, None, element, var_span, ctx)?;

    ctx.loop_depth += 1;
    let user_body = lower_nested_block(body, ctx);
    ctx.loop_depth -= 1;
    restore_refinements_after_for(ctx, entry_ref, var, body, orelse);
    let mut loop_body = vec![bind];
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

#[allow(clippy::too_many_arguments)]
fn lower_for_range(
    var: &str,
    var_span: Span,
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

    // the loop variable must be an int (wherever it is bound)
    let existing_var_ty = if ctx.binds_global(var) {
        ctx.globals.get(var).copied()
    } else {
        ctx.locals.get(var).copied()
    };
    if let Some(existing) = existing_var_ty
        && existing != ir::Ty::Int
    {
        return Err(err(
            format!(
                "loop variable '{var}' already has type {existing}, but \
                 range() yields int"
            ),
            var_span,
        ));
    }

    // Python semantics: iterate a hidden counter and assign the user
    // variable at the top of each iteration. After exhaustion the variable
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

    // var = .it as the first statement of the body
    let entry_ref = ctx.type_refinements.clone();
    let bind = bind_name(var, var_span, None, it_local.clone(), var_span, ctx)?;

    ctx.loop_depth += 1;
    let user_body = lower_nested_block(body, ctx);
    ctx.loop_depth -= 1;
    restore_refinements_after_for(ctx, entry_ref, var, body, orelse);
    let mut loop_body = vec![bind];
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
                "{}; a variable's type is fixed by its first assignment",
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

/// Insert implicit promotion casts (`bool → int → float`), union wraps, or fail.
fn coerce(value: ir::Expr, target: ir::Ty, span: Span, what: &str) -> SResult<ir::Expr> {
    if value.ty == target {
        return Ok(value);
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
        // Promote into a numeric member (e.g. bool → int|None)
        for m in ir::flatten_union_members(target) {
            if m == value.ty {
                return Ok(to_union(value, target));
            }
            // try numeric promotion into this member only
            if let Ok(promoted) = coerce_numeric_into(value.clone(), m) {
                return Ok(to_union(promoted, target));
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

    // Closures with matching params/ret and empty captures: retype for
    // homogeneous containers (call uses the object's code pointer).
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
        && c1.is_empty()
        && c2.is_empty()
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
    to_bool(lowered, cond.span)
}

fn to_bool(value: ir::Expr, span: Span) -> SResult<ir::Expr> {
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
        | ir::Ty::Union(_) => Ok(ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ToBool(Box::new(value)),
        }),
        other => Err(err(
            format!("a value of type {other} cannot be used as a condition"),
            span,
        )),
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
            // Type refinement (narrowing): only peel a *single* concrete member
            // via FromUnion. Multi-member refined unions keep storage type
            // (retyping Local without remapping tags is unsafe).
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
                    if nty != inner
                        && matches!(inner, ir::Ty::Union(_))
                        && !matches!(nty, ir::Ty::Union(_))
                    {
                        return extract_union_member(loaded, nty, expr.span);
                    }
                    return Ok(loaded);
                }
                if let Some(ty) = ctx.locals.get(name) {
                    let base = ir::Expr {
                        ty: *ty,
                        kind: ir::ExprKind::Local(name.clone()),
                    };
                    if nty != *ty
                        && matches!(ty, ir::Ty::Union(_))
                        && !matches!(nty, ir::Ty::Union(_))
                    {
                        return extract_union_member(base, nty, expr.span);
                    }
                    // Multi-member peel: keep storage union type.
                    return Ok(base);
                }
                // Module-level / global name under a refinement peel.
                if let Some(ty) = ctx.globals.get(name) {
                    let base = ir::Expr {
                        ty: *ty,
                        kind: ir::ExprKind::GlobalLoad(ctx.own_global(name)),
                    };
                    if nty != *ty
                        && matches!(ty, ir::Ty::Union(_))
                        && !matches!(nty, ir::Ty::Union(_))
                    {
                        return extract_union_member(base, nty, expr.span);
                    }
                    return Ok(base);
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
        ast::ExprKind::ListComp {
            elem,
            var,
            var_span,
            iter,
            cond,
        } => lower_list_comp(elem, var, *var_span, iter, cond.as_deref(), expr.span, ctx),
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
            Err(err(
                "attribute access is only supported for 'sys.argv', imported \
                 module globals, and method calls",
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
            let base_ir = lower_expr(base, ctx)?;
            match base_ir.ty {
                ir::Ty::List(elem) => match method.as_str() {
                    // pop returns the removed element
                    "pop" => lower_list_pop(base_ir, *elem, &args, *method_span, ctx),
                    "index" => lower_list_index_of(base_ir, *elem, &args, *method_span, ctx),
                    "append" | "insert" | "remove" | "clear" | "sort" => Err(err(
                        format!(
                            "list.{method}(...) returns None and cannot be used \
                             in an expression"
                        ),
                        *method_span,
                    )),
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
                    "add" | "remove" | "discard" | "clear" => Err(err(
                        format!(
                            "set.{method}(...) returns None and cannot be used in an \
                             expression"
                        ),
                        *method_span,
                    )),
                    _ => Err(err(
                        format!(
                            "set method '{method}' is not supported yet (supported: add, \
                             remove, discard, clear)"
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
                        let v = lower_expr(&args[0], ctx)?;
                        if !matches!(v.kind, ir::ExprKind::ConstNone) && v.ty != ir::Ty::None {
                            return Err(err(
                                "generator.send(value) only supports send(None) in this subset",
                                args[0].span,
                            ));
                        }
                        // send(None) → yield_ty | None (like next)
                        Ok(ir::Expr {
                            ty: ir::optional_of(*yield_ty),
                            kind: ir::ExprKind::GeneratorNext(Box::new(base_ir)),
                        })
                    }
                    "throw" => Err(err("generator.throw() is not supported yet", *method_span)),
                    _ => Err(err(
                        format!(
                            "generator method '{method}' is not supported yet \
                             (supported: close, send)"
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
        ast::ExprKind::JoinedStr(parts) => {
            let mut result: Option<ir::Expr> = Option::None;
            for part in parts {
                let piece = match part {
                    ast::FStringPart::Literal(s) => ir::Expr {
                        ty: ir::Ty::Str,
                        kind: ir::ExprKind::ConstStr(s.clone()),
                    },
                    ast::FStringPart::Expr { expr: e, format } => {
                        let v = lower_expr(e, ctx)?;
                        match format {
                            None => lower_cast(ast::TypeName::Str, v, e.span)?,
                            Some(ast::FStringFormat::DotNf { precision }) => {
                                lower_float_format(v, *precision, e.span)?
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
                    let value = to_bool(value, operand.span)?;
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
                    match value.kind {
                        ir::ExprKind::ConstInt(v) => return Ok(int_const(v.wrapping_neg())),
                        ir::ExprKind::ConstFloat(v) => {
                            return Ok(ir::Expr {
                                ty: ir::Ty::Float,
                                kind: ir::ExprKind::ConstFloat(-v),
                            });
                        }
                        _ => {}
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
            if ctx.finally_depth > 0 {
                return Err(err("yield in finally is not supported yet", expr.span));
            }
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
            // Represent as a Block that executes Yield stmt and produces None
            // (yield expressions are not used as values in this subset except
            // as expression statements).
            Ok(ir::Expr {
                ty: ir::Ty::None,
                kind: ir::ExprKind::Block {
                    stmts: vec![ir::Stmt::Yield(val)],
                    result: Box::new(const_none()),
                },
            })
        }
        ast::ExprKind::YieldFrom(iter) => {
            // Desugar to iteration + yield for any iterable supported by `for`.
            if ctx.finally_depth > 0 {
                return Err(err("yield from in finally is not supported yet", expr.span));
            }
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
                        kind: ir::ExprKind::GeneratorNext(Box::new(gen_local.clone())),
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

/// `[elem for var in iter if cond]` desugars to a loop building a list
/// inside an expression-level Block. The variable lives in a hidden
/// storage slot (Python 3: it shadows but does not leak). Fast path:
/// when the produced count is knowable (list/str sources always; range
/// with a constant step of 1), the result is allocated at full capacity
/// and appended without per-element capacity checks.
fn lower_list_comp(
    elem: &ast::Expr,
    var: &str,
    var_span: Span,
    iter: &ast::Expr,
    cond: Option<&ast::Expr>,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let mut stmts: Vec<ir::Stmt> = Vec::new();

    // ---- source setup: yields (per-iteration element expr, loop parts) ----
    struct Loop {
        cond: ir::Expr,
        step: Vec<ir::Stmt>,
        element: ir::Expr,
        /// exact or upper-bound capacity when knowable
        cap: Option<ir::Expr>,
    }

    let looping: Loop = if let ast::ExprKind::Call { func, args, .. } = &iter.kind
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
        stmts.push(ir::Stmt::Assign {
            name: stop_t.clone(),
            value: stop,
        });
        let stop_local = ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Local(stop_t),
        };
        let it_t = ctx.fresh_temp("comp.it", ir::Ty::Int);
        stmts.push(ir::Stmt::Assign {
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
            ir::ExprKind::ConstInt(1) => {
                // presize: cap = max(0, stop - it)
                let cap_t = ctx.fresh_temp("comp.cap", ir::Ty::Int);
                stmts.push(ir::Stmt::Assign {
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
                stmts.push(ir::Stmt::If {
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
                stmts.push(ir::Stmt::Assign {
                    name: step_t.clone(),
                    value: step,
                });
                let step_local = ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Local(step_t),
                };
                stmts.push(ir::Stmt::If {
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
        Loop {
            cond: loop_cond,
            step: vec![step_stmt],
            element: it_local,
            cap,
        }
    } else {
        // list or str: index loop, exact/upper-bound presize via len
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
        stmts.push(ir::Stmt::Assign {
            name: seq_t.clone(),
            value: seq,
        });
        let idx_t = ctx.fresh_temp("comp.idx", ir::Ty::Int);
        stmts.push(ir::Stmt::Assign {
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
        // exact capacity without a filter, upper bound with one
        let cap = ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Len(Box::new(seq_local)),
        };
        Loop {
            cond,
            step: vec![step_stmt],
            element,
            cap: Some(cap),
        }
    };

    // ---- hidden storage for the loop variable ----
    let src_elem_ty = looping.element.ty;
    ctx.temp_counter += 1;
    let storage = format!(".comp{}.{var}", ctx.temp_counter);
    ctx.locals_order.push((storage.clone(), src_elem_ty));

    ctx.comp_renames
        .push((var.to_string(), storage.clone(), src_elem_ty));
    let elem_ir = lower_expr(elem, ctx);
    let cond_ir = match cond {
        Some(c) => lower_condition(c, ctx).map(Some),
        Option::None => Ok(Option::None),
    };
    ctx.comp_renames.pop();
    let elem_ir = elem_ir?;
    let cond_ir = cond_ir?;

    let elem_ty = elem_of(elem_ir.ty, elem.span)?;
    let _ = var_span;

    // ---- result list ----
    let presized = looping.cap.is_some();
    let cap_expr = looping.cap.unwrap_or(int_const(4));
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

    // ---- loop body: var = element; [if cond:] append(elem) ----
    let append = if presized {
        ir::Stmt::ListAppendUnchecked {
            list: res_local.clone(),
            value: elem_ir,
        }
    } else {
        ir::Stmt::ListAppend {
            list: res_local.clone(),
            value: elem_ir,
        }
    };
    let mut body = vec![ir::Stmt::Assign {
        name: storage,
        value: looping.element,
    }];
    match cond_ir {
        Some(c) => body.push(ir::Stmt::If {
            branches: vec![(c, vec![append])],
            orelse: vec![],
        }),
        Option::None => body.push(append),
    }

    stmts.push(ir::Stmt::While {
        cond: looping.cond,
        body,
        step: looping.step,
    });

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

fn lower_list_lit(
    items: &[ast::ListElem],
    expected: Option<ir::Ty>,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if items.is_empty() {
        let elem = expected.ok_or_else(|| {
            err(
                "cannot infer the element type of an empty list; annotate the \
                 variable, e.g. 'xs: list[int] = []'",
                span,
            )
        })?;
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
        if e.ty == ir::Ty::None || e.ty == ir::Ty::File {
            return Err(err(format!("tuple elements cannot be {}", e.ty), item.span));
        }
        if matches!(e.ty, ir::Ty::Cell(_)) {
            return Err(err(format!("tuple elements cannot be {}", e.ty), item.span));
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
    if val_ty == ir::Ty::File {
        return Err(err(
            format!("dict values of type {val_ty} are not supported"),
            span,
        ));
    }
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
    // Homogeneous closures: same params/ret. Erase capture/func identity so
    // CallClosure uses the code pointer from the heap object.
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
        if p1 == p2 && r1 == r2 && c1.is_empty() && c2.is_empty() {
            return Some(ir::closure_of_full(p1, *r1, &[], ""));
        }
        // Same captures shape (rare homogeneous list of identical nested) —
        // only when both are capture-free for container storage.
        return None;
    }
    match (a, b) {
        _ if a == b => Some(a),
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
    if let Some(kw) = keywords.first() {
        return Err(err(
            format!("'{func}()' does not take keyword arguments"),
            kw.name_span,
        ));
    }
    if kwargs.is_some() {
        return Err(err(format!("'{func}()' does not take **kwargs"), span));
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
            "set" => Err(err(
                "set() requires a type annotation on the target, e.g. \
                 's: set[int] = set()'",
                span,
            )),
            "len" => {
                if args.len() != 1 {
                    return Err(err(
                        format!("len() takes exactly one argument ({} given)", args.len()),
                        span,
                    ));
                }
                let arg = lower_expr(args[0], ctx)?;
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
            _ => Err(err(format!("function '{func}' is not defined"), func_span)),
        }
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
            | ir::Ty::Set(_) => Ok(ir::Expr {
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
    }
}

/// f-string `{x:.Nf}`: format int/float/bool as fixed-point (CPython-compatible).
fn lower_float_format(value: ir::Expr, precision: u32, span: Span) -> SResult<ir::Expr> {
    let as_float = match value.ty {
        ir::Ty::Float => value,
        ir::Ty::Int => ir::Expr {
            ty: ir::Ty::Float,
            kind: ir::ExprKind::IntToFloat(Box::new(value)),
        },
        ir::Ty::Bool => {
            let as_int = ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::BoolToInt(Box::new(value)),
            };
            ir::Expr {
                ty: ir::Ty::Float,
                kind: ir::ExprKind::IntToFloat(Box::new(as_int)),
            }
        }
        other => {
            return Err(err(
                format!("Unknown format code 'f' for object of type '{other}'"),
                span,
            ));
        }
    };
    Ok(ir::Expr {
        ty: ir::Ty::Str,
        kind: ir::ExprKind::FloatFormat {
            value: Box::new(as_float),
            precision,
        },
    })
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
/// otherwise flatten into a union of all atomic members.
fn join_types(a: ir::Ty, b: ir::Ty) -> ir::Ty {
    if a == b {
        return a;
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
    // constant fold
    if let (ir::ExprKind::ConstInt(a), ir::ExprKind::ConstInt(b)) = (&l.kind, &r.kind) {
        let v = match ir_op {
            ir::BinOp::BitAnd => a & b,
            ir::BinOp::BitOr => a | b,
            ir::BinOp::BitXor => a ^ b,
            ir::BinOp::LShift => {
                if *b < 0 {
                    return Err(err("negative shift count", span));
                }
                if *b >= 64 {
                    0
                } else {
                    a.wrapping_shl(*b as u32)
                }
            }
            ir::BinOp::RShift => {
                if *b < 0 {
                    return Err(err("negative shift count", span));
                }
                if *b >= 64 {
                    if *a < 0 { -1 } else { 0 }
                } else {
                    // Arithmetic right shift (CPython); wrapping_shr is logical.
                    *a >> (*b as u32)
                }
            }
            _ => unreachable!(),
        };
        if keep_bool {
            return Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::ConstBool(v != 0),
            });
        }
        return Ok(int_const(v));
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

/// `needle in haystack` / `not in`: substring, list/set membership, dict keys.
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
        ir::Ty::Tuple(_) => {
            return Err(err("membership test on tuples is not supported yet", span));
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
    fn empty_list_needs_annotation() {
        let e = analyze_err("xs = []\n");
        assert!(e.message.contains("annotate"), "{}", e.message);
        analyze_ok("xs: list[int] = []\n");
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
        let e = analyze_err("x = 1\nx = 2.5\n");
        assert!(e.message.contains("fixed"), "{}", e.message);
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
    fn fstring_dot_nf_lowers_to_float_format() {
        let m = analyze_ok("x = 3.14159\ns = f\"{x:.2f}\"\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
            panic!();
        };
        fn has_fmt(e: &ir::Expr) -> bool {
            match &e.kind {
                ir::ExprKind::FloatFormat { precision: 2, .. } => true,
                ir::ExprKind::Binary { left, right, .. } => has_fmt(left) || has_fmt(right),
                _ => false,
            }
        }
        assert!(has_fmt(value), "{value:?}");
    }

    #[test]
    fn fstring_dot_nf_promotes_int() {
        let m = analyze_ok("n = 2\ns = f\"{n:.2f}\"\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
            panic!();
        };
        fn has_int_to_float_fmt(e: &ir::Expr) -> bool {
            match &e.kind {
                ir::ExprKind::FloatFormat {
                    value,
                    precision: 2,
                } => {
                    matches!(value.kind, ir::ExprKind::IntToFloat(_))
                }
                ir::ExprKind::Binary { left, right, .. } => {
                    has_int_to_float_fmt(left) || has_int_to_float_fmt(right)
                }
                _ => false,
            }
        }
        assert!(has_int_to_float_fmt(value), "{value:?}");
    }

    #[test]
    fn error_fstring_of_list() {
        let e = analyze_err("xs = [1]\ns = f\"{xs}\"\nprint(s)\n");
        assert!(e.message.contains("convert"), "{}", e.message);
    }

    #[test]
    fn error_fstring_dot_nf_on_str() {
        let e = analyze_err("s = \"hi\"\nt = f\"{s:.2f}\"\n");
        assert!(e.message.contains("format code"), "{}", e.message);
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
    fn tuple_membership_not_supported_yet() {
        let e = analyze_err("print(1 in (1, 2))\n");
        assert!(e.message.contains("not supported yet"), "{}", e.message);
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
}
