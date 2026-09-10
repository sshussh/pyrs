//! Function and nested-def lowering, capture analysis, and block helpers.

use std::collections::{HashMap, HashSet};

use common::Span;
use parser::ast;

use crate::prelude::*;

pub(crate) fn lower_function(
    f: &ast::FuncDef,
    mctx: &ModuleCtx,
    globals: &mut HashMap<String, ir::Ty>,
    globals_order: &mut Vec<(String, ir::Ty)>,
    is_entry: bool,
) -> SResult<(ir::Function, Vec<ir::Function>)> {
    lower_function_with_class(f, mctx, globals, globals_order, is_entry, None)
}

/// Like [`lower_function`], but records the owning class for zero-arg `super()`.
pub(crate) fn lower_function_with_class(
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
pub(crate) fn lower_function_inner(
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
    // Detect method kind when this is a method IR name.
    let method_kind = if current_class.is_some() {
        method_kind_lookup(&f.name)
    } else {
        MethodKind::Instance
    };
    let is_classmethod = method_kind == MethodKind::Class;
    // Instance / property methods use self for super(); staticmethods do not.
    let self_param = if current_class.is_some()
        && matches!(method_kind, MethodKind::Instance | MethodKind::Property)
    {
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
        handler_depth: 0,
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
        // Yield type: a non-None return annotation names it directly.
        // Without one, take the first `yield` of a literal or an annotated
        // parameter -- defaulting to Int made `def g(): yield "a"` a hard
        // error, so an unannotated generator could only ever yield ints.
        let yty = generator_yield_ty(&f.name, ctx.ret, &f.body, &f.params);
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
    } else if mctx.module == "os.path" && f.name == "_stat_kind" {
        if params.len() != 1 || params[0].1 != ir::Ty::Str || ctx.ret != ir::Ty::Int {
            return Err(err(
                "os.path._stat_kind must take one str and return int".to_string(),
                f.span,
            ));
        }
        let arg = ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::Local(params[0].0.clone()),
        };
        vec![ir::Stmt::Return(Some(ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::OsStatKind(Box::new(arg)),
        }))]
    } else if mctx.module == "os.path" && f.name == "_getcwd" {
        // Same primitive as `os.getcwd`; `os.path` cannot import `os`, which
        // imports it.
        if !params.is_empty() || ctx.ret != ir::Ty::Str {
            return Err(err(
                "os.path._getcwd must take no parameters and return str".to_string(),
                f.span,
            ));
        }
        vec![ir::Stmt::Return(Some(ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::OsGetcwd,
        }))]
    } else if (mctx.module == "os" || mctx.module == "os.path") && f.name == "_environ" {
        let want = ir::dict_of(ir::Ty::Str, ir::Ty::Str);
        if !params.is_empty() || ctx.ret != want {
            return Err(err(
                "os._environ must take no parameters and return dict[str, str]".to_string(),
                f.span,
            ));
        }
        vec![ir::Stmt::Return(Some(ir::Expr {
            ty: want,
            kind: ir::ExprKind::OsEnviron,
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

pub(crate) fn stmts_have_yield(stmts: &[ast::Stmt]) -> bool {
    stmts.iter().any(stmt_has_yield)
}

/// The yield type of a generator function.
///
/// `-> Iterator[int]` already *is* the generator type, so it is unwrapped
/// rather than wrapped again; a plain `-> int` names the yield type directly
/// (the older spelling this subset accepted); with no annotation the first
/// `yield` decides. The four places that build a generator's signature have to
/// agree, or a call site sees a different element type than the body produces.
pub(crate) fn generator_yield_ty(
    name: &str,
    ret: ir::Ty,
    body: &[ast::Stmt],
    params: &[ast::Param],
) -> ir::Ty {
    if let Some(t) = synth_yield_ty(name) {
        return t;
    }
    match ret {
        ir::Ty::Generator { yield_ty } => *yield_ty,
        ir::Ty::None => first_yield_ty(body, params).unwrap_or(ir::Ty::Int),
        other => other,
    }
}

/// The type of the first `yield <expr>` in a body, for an unannotated
/// generator. Only literals and annotated parameters are consulted: this runs
/// before the body is lowered and before locals exist, so anything else stays
/// unknown and the caller keeps its default.
pub(crate) fn first_yield_ty(stmts: &[ast::Stmt], params: &[ast::Param]) -> Option<ir::Ty> {
    fn from_expr(e: &ast::Expr, params: &[ast::Param]) -> Option<ir::Ty> {
        match &e.kind {
            ast::ExprKind::Yield(Some(v)) => literal_expr_ty(v).or_else(|| match &v.kind {
                ast::ExprKind::Name(n) => params
                    .iter()
                    .find(|p| &p.name == n)
                    .and_then(|p| p.ty.as_ref())
                    .map(|t| resolve_type(*t)),
                _ => Option::None,
            }),
            ast::ExprKind::Yield(Option::None) => Option::None,
            _ => Option::None,
        }
    }
    fn walk(stmts: &[ast::Stmt], params: &[ast::Param]) -> Option<ir::Ty> {
        for st in stmts {
            let found = match &st.kind {
                ast::StmtKind::ExprStmt(e) | ast::StmtKind::Return(Some(e)) => from_expr(e, params),
                ast::StmtKind::Assign { value, .. } => from_expr(value, params),
                ast::StmtKind::If { branches, orelse } => branches
                    .iter()
                    .find_map(|(_, b)| walk(b, params))
                    .or_else(|| walk(orelse, params)),
                ast::StmtKind::While { body, orelse, .. }
                | ast::StmtKind::For { body, orelse, .. } => {
                    walk(body, params).or_else(|| walk(orelse, params))
                }
                ast::StmtKind::Try {
                    body,
                    handlers,
                    orelse,
                    finally,
                } => walk(body, params)
                    .or_else(|| handlers.iter().find_map(|h| walk(&h.body, params)))
                    .or_else(|| walk(orelse, params))
                    .or_else(|| walk(finally, params)),
                ast::StmtKind::With { body, .. } => walk(body, params),
                _ => Option::None,
            };
            if found.is_some() {
                return found;
            }
        }
        Option::None
    }
    walk(stmts, params)
}

pub(crate) fn stmt_has_yield(st: &ast::Stmt) -> bool {
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

pub(crate) fn expr_has_yield(e: &ast::Expr) -> bool {
    match &e.kind {
        ast::ExprKind::Yield(_) | ast::ExprKind::YieldFrom(_) => true,
        ast::ExprKind::IfExp { test, body, orelse } => {
            expr_has_yield(test) || expr_has_yield(body) || expr_has_yield(orelse)
        }
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

pub(crate) fn lower_block(stmts: &[ast::Stmt], ctx: &mut FnCtx) -> SResult<Vec<ir::Stmt>> {
    let mut out = Vec::new();
    for stmt in stmts {
        lower_stmt(stmt, ctx, &mut out)?;
    }
    Ok(out)
}

/// Pre-scan nested `def`s at one nesting level and register provisional
/// signatures so forward / mutual sibling calls type-check.
pub(crate) fn pre_register_nested_sigs(stmts: &[ast::Stmt], ctx: &mut FnCtx) -> SResult<()> {
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

pub(crate) fn pre_register_one_nested_sig(f: &ast::FuncDef, ctx: &mut FnCtx) -> SResult<()> {
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
        let y = generator_yield_ty(&f.name, ret, &f.body, &f.params);
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
        posonly_end: f.posonly_end,
        kwonly_start: f.kwonly_start,
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
pub(crate) fn complete_nested_captures(stmts: &[ast::Stmt], ctx: &mut FnCtx) -> SResult<()> {
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

pub(crate) fn collect_nested_func_defs(stmts: &[ast::Stmt], out: &mut Vec<ast::FuncDef>) {
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

pub(crate) type NestedCaptures = (Vec<(String, ir::Ty)>, Vec<bool>);

/// Capture list for a nested def (own free vars + sibling-threaded cells).
pub(crate) fn analyze_nested_captures(f: &ast::FuncDef, ctx: &FnCtx) -> SResult<NestedCaptures> {
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

pub(crate) fn free_names_used_in_stmts(stmts: &[ast::Stmt]) -> HashSet<String> {
    let mut s = HashSet::new();
    for st in stmts {
        collect_used_names_in_stmt(st, &mut s);
    }
    s
}

/// Infer types of simple assignments for late free-var cell allocation.
pub(crate) fn infer_late_bind_types(
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

pub(crate) fn infer_late_bind_types_in(
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

pub(crate) fn guess_expr_ty(
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
                    Some(prev) => seed_join(prev, t)?,
                });
            }
            ety.map(ir::list_of)
        }
        _ => None,
    }
}

/// Ensure a late-bound free var has an unbound cell (NameError on load until assign).
pub(crate) fn ensure_cell_unbound(
    ctx: &mut FnCtx,
    name: &str,
    ty: ir::Ty,
    span: Span,
) -> SResult<Option<ir::Stmt>> {
    if ctx.cell_locals.contains_key(name) {
        return Ok(None);
    }
    // At module scope the binding this cell stands for is a *global*, so the
    // assignment writes the global and the cell stays empty — the closure then
    // fails at run time with a NameError about a free variable. Reject it here
    // instead. A top-level `def` is unaffected: it is a module function that
    // reads the global directly, not a capture.
    if ctx.is_entry {
        return Err(err(
            format!(
                "'{name}' is a module-level variable and cannot be captured by a \
                 closure here (lambda, generator expression, or a function \
                 defined inside an expression). Move the code into a function, \
                 or pass '{name}' in as an argument"
            ),
            span,
        ));
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
pub(crate) fn lower_nested_func_def(f: &ast::FuncDef, ctx: &mut FnCtx) -> SResult<()> {
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
        let y = generator_yield_ty(&f.name, ret, &f.body, &f.params);
        ret = ir::generator_of(y);
        Some(y)
    } else {
        None
    };
    let sig = FuncSig {
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
    // The body is lowered under the mangled IR name, so a synthesized yield
    // type registered against the user-visible name has to follow it.
    if let Some(t) = synth_yield_ty(&f.name) {
        set_synth_yield_ty(&ir_name, t);
    }
    let nested_def = ast::FuncDef {
        name: ir_name.clone(),
        params: f.params.clone(),
        // A nested `def f(a, *, b)` keeps its own boundaries.
        posonly_end: f.posonly_end,
        kwonly_start: f.kwonly_start,
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

pub(crate) fn collect_nonlocals_in_stmts(stmts: &[ast::Stmt]) -> HashSet<String> {
    let mut s = HashSet::new();
    for st in stmts {
        collect_nonlocals_in_stmt(st, &mut s);
    }
    s
}

/// Nonlocal names declared in nested function bodies (not this function's own).
pub(crate) fn collect_nested_nonlocals(stmts: &[ast::Stmt]) -> HashSet<String> {
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

pub(crate) fn collect_nonlocals_in_stmt(st: &ast::Stmt, out: &mut HashSet<String>) {
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
pub(crate) fn merge_fallthrough_refinements(
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
pub(crate) fn collect_cell_candidate_names(stmts: &[ast::Stmt]) -> HashSet<String> {
    let mut out = HashSet::new();
    collect_cell_candidates_in_stmts(stmts, &mut out);
    out
}

pub(crate) fn collect_cell_candidates_in_stmts(stmts: &[ast::Stmt], out: &mut HashSet<String>) {
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

pub(crate) fn collect_cell_candidates_in_target(t: &ast::AssignTarget, out: &mut HashSet<String>) {
    match t {
        ast::AssignTarget::Index { base, index } => {
            collect_cell_candidates_in_expr(base, out);
            collect_cell_candidates_in_expr(index, out);
        }
        ast::AssignTarget::Slice {
            base, lo, hi, step, ..
        } => {
            collect_cell_candidates_in_expr(base, out);
            if let Some(e) = lo {
                collect_cell_candidates_in_expr(e, out);
            }
            if let Some(e) = hi {
                collect_cell_candidates_in_expr(e, out);
            }
            if let Some(e) = step {
                collect_cell_candidates_in_expr(e, out);
            }
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

pub(crate) fn collect_cell_candidates_in_expr(e: &ast::Expr, out: &mut HashSet<String>) {
    walk_expr_for_lambdas(e, out);
}

pub(crate) fn walk_expr_for_lambdas(e: &ast::Expr, out: &mut HashSet<String>) {
    match &e.kind {
        ast::ExprKind::IfExp { test, body, orelse } => {
            walk_expr_for_lambdas(test, out);
            walk_expr_for_lambdas(body, out);
            walk_expr_for_lambdas(orelse, out);
        }
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

pub(crate) fn add_nested_free_names(f: &ast::FuncDef, out: &mut HashSet<String>) {
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
pub(crate) fn ensure_cell(
    ctx: &mut FnCtx,
    name: &str,
    ty: ir::Ty,
    span: Span,
) -> SResult<Option<ir::Stmt>> {
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

pub(crate) fn assigned_names_in_stmts(stmts: &[ast::Stmt]) -> HashSet<String> {
    let mut s = HashSet::new();
    for st in stmts {
        assigned_names_in_stmt(st, &mut s);
    }
    s
}

pub(crate) fn assigned_names_in_stmt(st: &ast::Stmt, out: &mut HashSet<String>) {
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

pub(crate) fn assigned_names_in_target(t: &ast::AssignTarget, out: &mut HashSet<String>) {
    match t {
        ast::AssignTarget::Name { name, .. } => {
            out.insert(name.clone());
        }
        ast::AssignTarget::Index { .. }
        | ast::AssignTarget::Slice { .. }
        | ast::AssignTarget::Attr { .. } => {}
        ast::AssignTarget::Tuple(ts) => {
            for t in ts {
                assigned_names_in_target(t, out);
            }
        }
        ast::AssignTarget::Starred { target, .. } => assigned_names_in_target(target, out),
    }
}

pub(crate) fn collect_used_names_in_stmts(stmts: &[ast::Stmt], out: &mut HashSet<String>) {
    for st in stmts {
        collect_used_names_in_stmt(st, out);
    }
}

/// Bare-name call targets in a statement list (for sibling nested capture threading).
pub(crate) fn collect_called_func_names_in_stmts(stmts: &[ast::Stmt], out: &mut HashSet<String>) {
    for st in stmts {
        collect_called_func_names_in_stmt(st, out);
    }
}

pub(crate) fn collect_called_func_names_in_stmt(st: &ast::Stmt, out: &mut HashSet<String>) {
    match &st.kind {
        ast::StmtKind::Assign { value, .. }
        | ast::StmtKind::AugAssign { value, .. }
        | ast::StmtKind::ExprStmt(value)
        | ast::StmtKind::Return(Some(value))
        | ast::StmtKind::Raise {
            message: Some(value),
            ..
        } => {
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

pub(crate) fn collect_called_func_names_in_expr(e: &ast::Expr, out: &mut HashSet<String>) {
    match &e.kind {
        ast::ExprKind::IfExp { test, body, orelse } => {
            collect_called_func_names_in_expr(test, out);
            collect_called_func_names_in_expr(body, out);
            collect_called_func_names_in_expr(orelse, out);
        }
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

pub(crate) fn collect_used_names_in_stmt(st: &ast::Stmt, out: &mut HashSet<String>) {
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
        ast::StmtKind::Raise {
            message: Some(message),
            ..
        } => {
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

pub(crate) fn collect_used_names_in_target_read(t: &ast::AssignTarget, out: &mut HashSet<String>) {
    match t {
        ast::AssignTarget::Name { .. } => {}
        ast::AssignTarget::Index { base, index } => {
            collect_used_names_in_expr(base, out);
            collect_used_names_in_expr(index, out);
        }
        ast::AssignTarget::Slice {
            base, lo, hi, step, ..
        } => {
            collect_used_names_in_expr(base, out);
            if let Some(e) = lo {
                collect_used_names_in_expr(e, out);
            }
            if let Some(e) = hi {
                collect_used_names_in_expr(e, out);
            }
            if let Some(e) = step {
                collect_used_names_in_expr(e, out);
            }
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

pub(crate) fn collect_used_names_in_expr(e: &ast::Expr, out: &mut HashSet<String>) {
    match &e.kind {
        ast::ExprKind::Name(n) => {
            out.insert(n.clone());
        }
        ast::ExprKind::IfExp { test, body, orelse } => {
            collect_used_names_in_expr(test, out);
            collect_used_names_in_expr(body, out);
            collect_used_names_in_expr(orelse, out);
        }
        ast::ExprKind::Call {
            func,
            args,
            keywords,
            kwargs,
            ..
        } => {
            // Free function names used as callees must be captured (e.g. `f(x)` in a nested def).
            if func != ".call" && !func.contains('.') {
                out.insert(func.clone());
            }
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
pub(crate) fn lower_nested_block(stmts: &[ast::Stmt], ctx: &mut FnCtx) -> SResult<Vec<ir::Stmt>> {
    lower_block(stmts, ctx)
}
