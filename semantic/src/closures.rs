//! Nested functions, lambdas, generator expressions, and closure defaults.

use common::Span;
use parser::ast;

use crate::prelude::*;

/// Nested function visible only inside its enclosing function.
#[derive(Debug, Clone)]
pub(crate) struct NestedFnInfo {
    /// Fully-qualified IR name (`outer.inner` or `mod.outer.inner`).
    pub(crate) ir_name: String,
    /// Signature of the nested function **without** capture parameters.
    pub(crate) sig: FuncSig,
    /// Outer locals/params captured as leading IR params (cell ptr or value).
    pub(crate) captures: Vec<(String, ir::Ty)>,
    /// Parallel to captures: true if the capture is a cell pointer.
    pub(crate) capture_is_cell: Vec<bool>,
    /// True when this nested function uses the closure calling convention
    /// (env pointer first) rather than plain leading value params.
    #[allow(dead_code)]
    pub(crate) uses_env: bool,
}

pub(crate) fn make_closure_expr(
    info: &NestedFnInfo,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
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

///
/// Computed by binding the loop targets exactly as `lower_list_comp` does and
/// then typing the element — the only way to know it, since the targets exist
/// only inside the generator. The IR this builds is thrown away; it runs
/// before the synthesized function is lowered, purely to type it.
/// The type a generator expression yields.
///
/// Computed by binding the loop targets exactly as `lower_list_comp` does and
/// then typing the element — the only way to know it, since the targets exist
/// only inside the generator. The IR this builds is thrown away; it runs
/// before the synthesized function is lowered, purely to type it.
pub(crate) fn gen_exp_elem_ty(
    elem: &ast::Expr,
    generators: &[ast::CompFor],
    ctx: &mut FnCtx,
) -> SResult<ir::Ty> {
    // Cell inits belong to the real lowering, not to this probe.
    let saved_cells = std::mem::take(&mut ctx.pending_cell_inits);
    let mut renames_pushed = 0usize;
    let result = (|ctx: &mut FnCtx| -> SResult<ir::Ty> {
        for clause in generators {
            let mut setup = Vec::new();
            let parts = lower_comp_iter(&clause.iter, false, ctx, &mut setup)?;
            let (_, n_renames) = bind_comp_target(&clause.target, parts.element, ctx)?;
            renames_pushed += n_renames;
        }
        Ok(lower_expr(elem, ctx)?.ty)
    })(ctx);
    for _ in 0..renames_pushed {
        ctx.comp_renames.pop();
    }
    ctx.pending_cell_inits = saved_cells;
    result
}

/// What a generator expression evaluates at creation, and how the body names
/// it afterwards.
pub(crate) struct GenExpHoist {
    /// Parameters the synthesized generator function takes.
    pub(crate) params: Vec<String>,
    /// Temps in the enclosing scope holding each argument.
    pub(crate) args: Vec<String>,
    /// Statements binding those temps, emitted before the generator is made.
    pub(crate) setup: Vec<ir::Stmt>,
    /// The iterable as the body should write it, in terms of `params`.
    pub(crate) iter: ast::Expr,
}

/// Evaluate a generator expression's outermost iterable at creation time.
///
/// Returns `None` only for an iterable that is neither a value nor a form
/// this knows how to take apart; such an iterable stays in the body and is
/// evaluated on first advance, which is the divergence this exists to avoid.
pub(crate) fn hoist_genexp_iter(
    iter: &ast::Expr,
    iter_param: &str,
    ctx: &mut FnCtx,
) -> SResult<Option<GenExpHoist>> {
    if let Ok(value) = lower_expr(iter, ctx) {
        set_synth_param_ty(iter_param, value.ty);
        let temp = ctx.fresh_temp("genexp.iter", value.ty);
        return Ok(Some(GenExpHoist {
            params: vec![iter_param.to_string()],
            args: vec![temp.clone()],
            setup: vec![ir::Stmt::Assign { name: temp, value }],
            iter: ast::Expr {
                kind: ast::ExprKind::Name(iter_param.to_string()),
                span: iter.span,
            },
        }));
    }

    // `range(...)` is not a value, so hoist its operands and rebuild the call
    // inside the body. Anything else is left where it is.
    let ast::ExprKind::Call { func, args, .. } = &iter.kind else {
        return Ok(Option::None);
    };
    if func != "range" || ctx.funcs().contains_key("range") {
        return Ok(Option::None);
    }
    let Ok(plain) = require_plain_args(args, "range", iter.span) else {
        return Ok(Option::None);
    };
    if plain.is_empty() || plain.len() > 3 {
        return Ok(Option::None);
    }

    let mut hoist = GenExpHoist {
        params: Vec::new(),
        args: Vec::new(),
        setup: Vec::new(),
        iter: ast::Expr {
            kind: ast::ExprKind::Name(String::new()),
            span: iter.span,
        },
    };
    let mut rebuilt = Vec::new();
    for (i, arg) in plain.iter().enumerate() {
        let value = lower_expr(arg, ctx)?;
        let value = coerce(value, ir::Ty::Int, arg.span, "range() argument")?;
        let param = format!("{iter_param}{i}");
        set_synth_param_ty(&param, ir::Ty::Int);
        let temp = ctx.fresh_temp("genexp.range", ir::Ty::Int);
        hoist.setup.push(ir::Stmt::Assign {
            name: temp.clone(),
            value,
        });
        rebuilt.push(ast::PosArg::Pos(ast::Expr {
            kind: ast::ExprKind::Name(param.clone()),
            span: arg.span,
        }));
        hoist.params.push(param);
        hoist.args.push(temp);
    }
    hoist.iter = ast::Expr {
        kind: ast::ExprKind::Call {
            func: "range".to_string(),
            func_span: iter.span,
            args: rebuilt,
            keywords: Vec::new(),
            kwargs: Option::None,
        },
        span: iter.span,
    };
    Ok(Some(hoist))
}

/// Lower `(elem for target in iter if cond ...)` to a synthesized nested
/// generator function, then call it.
///
/// Desugaring at the *AST* level rather than building IR directly is what
/// makes this small: the nested-def path already handles free-variable
/// capture, `stmts_have_yield` already recognises the body as a generator,
/// and the ordinary call path already produces the generator object. Nothing
/// new is needed in the IR.
///
/// The nesting is inside-out, so `(x + y for x in xs for y in ys)` becomes
/// `for x in xs: for y in ys: yield x + y`, which is Python's order.
pub(crate) fn lower_gen_exp(
    elem: &ast::Expr,
    generators: &[ast::CompFor],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if generators.is_empty() {
        return Err(err(
            "internal error: generator expression has no generators",
            span,
        ));
    }
    ctx.temp_counter += 1;
    let name = format!(".genexp{}", ctx.temp_counter);
    let iter_param = format!(".genexp{}.iter", ctx.temp_counter);

    // The outermost iterable is evaluated here, into a temp, and passed as a
    // real argument rather than captured. Two things fall out of that. It is
    // evaluated once, before the generator runs, which is CPython's rule that
    // the outermost iterable is consumed eagerly at creation. And it is not a
    // free variable, which matters at module level: the iterable is usually a
    // global there, and a function cannot capture one, so `sum(x for x in xs)`
    // at top level would otherwise fail where the same line inside a function
    // works.
    // The element type has to be known before the body is lowered, because
    // the yield sites are checked against it.
    let yield_ty = gen_exp_elem_ty(elem, generators, ctx)?;
    set_synth_yield_ty(&name, yield_ty);

    // CPython evaluates the *outermost* iterable when the generator
    // expression is created, not on first advance. That is what this hoist
    // is for, and it has two forms because not every iterable is a
    // first-class value here.
    //
    // When it lowers, the whole thing becomes one argument. When it does not
    // -- `range(...)` is the only such form left -- its **operands** are
    // hoisted instead and the form is rebuilt inside the body from
    // parameters, so `(x for x in range(bound()))` still calls `bound()` at
    // creation. Probing rather than enumerating keeps this correct as more
    // iterables become values.
    let hoisted = hoist_genexp_iter(&generators[0].iter, &iter_param, ctx)?;

    // Innermost first: `yield elem`, wrapped by each clause's filters, then by
    // that clause's `for`, working outward.
    let mut body = vec![ast::Stmt {
        kind: ast::StmtKind::ExprStmt(ast::Expr {
            kind: ast::ExprKind::Yield(Some(Box::new(elem.clone()))),
            span: elem.span,
        }),
        span: elem.span,
    }];
    for (i, clause) in generators.iter().enumerate().rev() {
        for cond in clause.ifs.iter().rev() {
            body = vec![ast::Stmt {
                kind: ast::StmtKind::If {
                    branches: vec![(cond.clone(), body)],
                    orelse: Vec::new(),
                },
                span: cond.span,
            }];
        }
        let iter = match (i, &hoisted) {
            (0, Some(h)) => h.iter.clone(),
            _ => clause.iter.clone(),
        };
        body = vec![ast::Stmt {
            kind: ast::StmtKind::For {
                target: clause.target.clone(),
                iter,
                body,
                orelse: Vec::new(),
            },
            span,
        }];
    }

    let fd = ast::FuncDef {
        name: name.clone(),
        params: hoisted
            .iter()
            .flat_map(|h| h.params.iter())
            .map(|name| ast::Param {
                name: name.clone(),
                ty: Option::None,
                span,
                default: Option::None,
            })
            .collect(),
        // Compiler-synthesized: no `/` or `*` markers.
        posonly_end: 0,
        kwonly_start: Option::None,
        vararg: Option::None,
        kwarg: Option::None,
        ret: Option::None,
        body,
        span,
        decorators: Vec::new(),
    };
    lower_nested_func_def(&fd, ctx)?;

    // Same cell-init flush the lambda and FuncDef statement paths do: without
    // it a captured free variable's cell is still null when the generator runs.
    let mut inits = std::mem::take(&mut ctx.pending_cell_inits);
    freeze_nested_defaults(&name, span, ctx, &mut inits)?;

    let call = lower_expr(
        &ast::Expr {
            kind: ast::ExprKind::Call {
                func: name,
                func_span: span,
                args: hoisted
                    .iter()
                    .flat_map(|h| h.args.iter())
                    .map(|temp| {
                        ast::PosArg::Pos(ast::Expr {
                            kind: ast::ExprKind::Name(temp.clone()),
                            span,
                        })
                    })
                    .collect(),
                keywords: Vec::new(),
                kwargs: Option::None,
            },
            span,
        },
        ctx,
    )?;
    let mut stmts = Vec::new();
    if let Some(h) = hoisted {
        stmts.extend(h.setup);
    }
    stmts.extend(inits);
    Ok(ir::Expr {
        ty: call.ty,
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(call),
        },
    })
}

/// Lower `lambda params: body` to a nested function + MakeClosure.
pub(crate) fn lower_lambda(
    params: &[ast::Param],
    body: &ast::Expr,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    lower_lambda_typed(params, body, &[], span, ctx)
}

/// Lower a lambda, with types for its leading parameters supplied by the
/// context that consumes it.
///
/// A lambda's parameters are almost never annotated -- `key=lambda s: len(s)`
/// is the whole point -- and the body alone cannot always type them: `len(s)`
/// says nothing about `s`. Where the consumer knows (a `key=` argument knows
/// the element type it will pass), it says so here. Otherwise the same
/// body-usage inference nested `def`s get applies, which handles the common
/// `lambda a: a + 1`.
pub(crate) fn lower_lambda_typed(
    params: &[ast::Param],
    body: &ast::Expr,
    param_tys: &[ir::Ty],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    ctx.temp_counter += 1;
    let name = format!(".lambda{}", ctx.temp_counter);
    // A lambda's parameters carry the names the user wrote, so a hint left in
    // the shared map would apply to any later parameter of the same name.
    // Scope it: set, lower, restore.
    let mut hinted: Vec<(String, Option<ir::Ty>)> = Vec::new();
    for (i, p) in params.iter().enumerate() {
        // A written annotation wins; the hint only fills a bare parameter.
        if p.ty.is_none()
            && let Some(t) = param_tys.get(i)
        {
            hinted.push((p.name.clone(), synth_param_ty(&p.name)));
            set_synth_param_ty(&p.name, *t);
        }
    }
    // Leave return unannotated; lower_function infers from the return stmt body.
    let body_stmt = ast::Stmt {
        kind: ast::StmtKind::Return(Some(body.clone())),
        span: body.span,
    };
    let fd = ast::FuncDef {
        name: name.clone(),
        params: params.to_vec(),
        // Compiler-synthesized: no `/` or `*` markers.
        posonly_end: 0,
        kwonly_start: Option::None,
        vararg: None,
        kwarg: None,
        ret: None,
        body: vec![body_stmt],
        span,
        decorators: Vec::new(),
    };
    let lowered = lower_nested_func_def(&fd, ctx);
    for (name, prev) in hinted {
        restore_synth_param_ty(&name, prev);
    }
    lowered?;
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
pub(crate) fn freeze_nested_defaults(
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

pub(crate) fn default_is_literal(e: &ast::Expr) -> bool {
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
pub(crate) fn lower_closure_default(
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

pub(crate) fn first_return_ty(stmts: &[ir::Stmt]) -> Option<ir::Ty> {
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
