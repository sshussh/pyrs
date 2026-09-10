//! Signature collection and local/parameter/return type inference.

use std::collections::{HashMap, HashSet};

use parser::ast;

use crate::prelude::*;

/// Collect a module's function signatures (with builtin-shadowing and
/// duplicate checks).
pub(crate) fn collect_sigs(
    module: &ast::Module,
) -> SResult<(HashMap<String, FuncSig>, Vec<&ast::FuncDef>)> {
    let mut funcs: HashMap<String, FuncSig> = HashMap::new();
    let mut order: Vec<&ast::FuncDef> = Vec::new();
    for stmt in &module.body {
        if let ast::StmtKind::FuncDef(f) = &stmt.kind {
            if f.decorators.len() > 1 {
                return Err(err(
                    "stacked function decorators are not supported yet",
                    f.decorators[1].span,
                ));
            }
            if f.decorators.len() == 1 {
                let d = &f.decorators[0];
                // Class method decorators only valid on methods, not free funcs.
                if matches!(d.name.as_str(), "staticmethod" | "classmethod" | "property") {
                    return Err(err(
                        format!("@{} is only valid on methods inside a class body", d.name),
                        d.span,
                    ));
                }
                // Single bare-name decorator is applied at module init (see below).
            }
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
                // Prefer the annotated return type as the yield element;
                // otherwise infer from the first yield. These must agree with
                // lower_function, or a call site sees a different element type
                // than the body produced.
                let y = generator_yield_ty(&f.name, ret, &f.body, &f.params);
                ret = ir::generator_of(y);
                Some(y)
            } else {
                None
            };
            funcs.insert(
                f.name.clone(),
                FuncSig {
                    params,
                    posonly_end: f.posonly_end,
                    kwonly_start: f.kwonly_start,
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
pub(crate) fn pre_infer_module_returns(
    order: &[&ast::FuncDef],
    funcs: &mut HashMap<String, FuncSig>,
) {
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
pub(crate) fn try_infer_ret_from_ast_body(
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

pub(crate) fn collect_ast_return_tys(
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
pub(crate) fn try_type_ast_expr(
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
                // `a @ b` is whatever the class's `__matmul__` returns.
                MatMul => None,
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
                    // Element joining, not scalar storage joining: keep mixed
                    // numerics as a union so the literal's values survive.
                    Some(prev) => seed_join(prev, t)?,
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
pub(crate) fn try_infer_param_from_body(
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

pub(crate) fn collect_param_constraints(
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
            | ast::StmtKind::Raise {
                message: Some(e), ..
            } => {
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
            // Nested defs: scan bodies for free uses of outer bare params
            // (e.g. decorator `def deco(f): def g(x): return f(x)+1`).
            ast::StmtKind::FuncDef(f) => {
                for p in &f.params {
                    if let Some(d) = &p.default {
                        collect_param_constraints_expr(name, d, params, bare, out);
                    }
                }
                collect_param_constraints(name, &f.body, params, bare, out);
            }
            _ => {}
        }
    }
}

pub(crate) fn collect_param_constraints_expr(
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
            // Bare param called as a function: monomorphic Closure[args → int].
            // Ret defaults to Int (common for arithmetic wrappers / decorators).
            if func == name {
                let mut ptys = Vec::new();
                for a in args {
                    if let ast::PosArg::Pos(e) = a {
                        ptys.push(
                            try_type_ast_expr(e, params, &HashMap::new()).unwrap_or(ir::Ty::Int),
                        );
                    }
                }
                out.push(ir::closure_of(&ptys, ir::Ty::Int));
            }
            if (func == "len"
                || func == "abs"
                || func == "sum"
                || func == "sorted"
                || func == "round"
                || func == "ord"
                || func == "chr"
                || func == "hex"
                || func == "bin"
                || func == "oct"
                || func == "divmod"
                || func == "pow")
                && let Some(ast::PosArg::Pos(ae)) = args.first()
                && matches!(&ae.kind, ast::ExprKind::Name(n) if n == name)
            {
                match func.as_str() {
                    "len" => {
                        // Ambiguous container — do not constrain alone.
                    }
                    "abs" | "sum" | "round" | "divmod" | "pow" => out.push(ir::Ty::Int),
                    "ord" => out.push(ir::Ty::Str),
                    "chr" | "hex" | "bin" | "oct" => out.push(ir::Ty::Int),
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
                    "append" | "pop" | "insert" | "remove" | "clear" | "sort" | "index"
                    | "count" | "reverse" => {
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
                    "upper" | "lower" | "strip" | "split" | "rsplit" | "startswith"
                    | "endswith" | "find" | "replace" | "join" | "removeprefix"
                    | "removesuffix" | "partition" | "rpartition" => {
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
pub(crate) fn collect_joined_local_types(
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
pub(crate) fn collect_empty_list_assign_names(stmts: &[ast::Stmt], out: &mut HashSet<String>) {
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
pub(crate) fn collect_list_append_elem_hints(
    stmts: &[ast::Stmt],
    env: &HashMap<String, ir::Ty>,
    out: &mut HashMap<String, ir::Ty>,
) {
    collect_list_append_elem_hints_scoped(stmts, env, out, &HashSet::new());
}

/// `shadowed`: names assigned in an enclosing nested-def scope (block free-var
/// appends from filling outer empty lists when the nested function rebinds the name).
pub(crate) fn collect_list_append_elem_hints_scoped(
    stmts: &[ast::Stmt],
    env: &HashMap<String, ir::Ty>,
    out: &mut HashMap<String, ir::Ty>,
    shadowed: &HashSet<String>,
) {
    for st in stmts {
        match &st.kind {
            ast::StmtKind::ExprStmt(e)
            | ast::StmtKind::Return(Some(e))
            | ast::StmtKind::Raise {
                message: Some(e), ..
            } => {
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

pub(crate) fn collect_list_append_elem_hints_expr(
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
pub(crate) fn fill_empty_list_types_from_appends(
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

pub(crate) fn collect_assign_types_in(
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
pub(crate) fn static_dunder_all(module: &ast::Module) -> Option<Result<Vec<String>, ()>> {
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

pub(crate) fn string_lit_sequence(e: &ast::Expr) -> Result<Vec<String>, ()> {
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
