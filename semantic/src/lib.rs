//! Semantic analysis: name resolution, type checking, and lowering the AST
//! into the typed IR.
//!
//! Typing rules (a statically-typed subset of Python, mypy-flavored):
//! - `int`, `float`, `bool` values; `bool` is assignable to `int`, and
//!   `int`/`bool` are assignable to `float` (implicit promotion casts are
//!   inserted).
//! - a variable's type is fixed by its first assignment and cannot change.
//! - `/` is true division and always produces `float`; `//` and `%` follow
//!   Python's floored semantics.
//! - conditions accept any numeric/bool value (truthiness).
//! - `and`/`or`/`not` operate on truthiness and produce `bool`.
//! - string literals are only supported as `print` arguments.
//!
//! The program entry is the top-level script statements; if there are none
//! but a zero-parameter `main` is defined, `main()` is called instead.

use std::collections::HashMap;

use common::{Diagnostic, Phase, Span};
use parser::ast;

pub fn ping() -> String {
    String::from("pong")
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
        ast::TypeName::None => ir::Ty::None,
    }
}

#[derive(Debug, Clone)]
struct FuncSig {
    params: Vec<ir::Ty>,
    ret: ir::Ty,
    span: Span,
}

/// Analyze a parsed module and lower it to IR.
pub fn analyze(module: &ast::Module) -> SResult<ir::Module> {
    let mut funcs: HashMap<String, FuncSig> = HashMap::new();
    let mut func_order: Vec<&ast::FuncDef> = Vec::new();
    let mut script: Vec<&ast::Stmt> = Vec::new();

    // pass 1: collect signatures so functions can call forward references
    for stmt in &module.body {
        match &stmt.kind {
            ast::StmtKind::FuncDef(f) => {
                if funcs.contains_key(&f.name) {
                    return Err(err(
                        format!("function '{}' is defined more than once", f.name),
                        f.span,
                    ));
                }
                let mut params = Vec::new();
                for p in &f.params {
                    let ty = resolve_type(p.ty);
                    if ty == ir::Ty::None {
                        return Err(err(
                            format!("parameter '{}' cannot have type None", p.name),
                            p.span,
                        ));
                    }
                    params.push(ty);
                }
                let ret = f.ret.map(resolve_type).unwrap_or(ir::Ty::None);
                funcs.insert(
                    f.name.clone(),
                    FuncSig {
                        params,
                        ret,
                        span: f.span,
                    },
                );
                func_order.push(f);
            }
            _ => script.push(stmt),
        }
    }

    if funcs.contains_key("print") {
        let f = func_order.iter().find(|f| f.name == "print").unwrap();
        return Err(err("cannot redefine the builtin 'print'", f.span));
    }

    // pass 2: lower every user function
    let mut lowered = Vec::new();
    for f in &func_order {
        lowered.push(lower_function(f, &funcs)?);
    }

    // pass 3: build the entry function from top-level statements
    let entry_body: Vec<ast::Stmt> = script.iter().map(|s| (*s).clone()).collect();
    let entry = if !entry_body.is_empty() {
        let entry_def = ast::FuncDef {
            name: ENTRY_NAME.to_string(),
            params: vec![],
            ret: None,
            body: entry_body,
            span: Span::default(),
        };
        lower_function(&entry_def, &funcs)?
    } else if let Some(sig) = funcs.get("main") {
        // no top-level code: call main() if it takes no arguments
        if !sig.params.is_empty() {
            return Err(err(
                "main() is used as the entry point and cannot take parameters",
                sig.span,
            ));
        }
        ir::Function {
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
        }
    } else {
        return Err(err(
            "program has no entry point: add top-level statements or define main()",
            Span::default(),
        ));
    };

    lowered.push(entry);

    Ok(ir::Module {
        funcs: lowered,
        entry: ENTRY_NAME.to_string(),
    })
}

struct FnCtx<'a> {
    funcs: &'a HashMap<String, FuncSig>,
    fn_name: String,
    ret: ir::Ty,
    locals: HashMap<String, ir::Ty>,
    locals_order: Vec<(String, ir::Ty)>,
    loop_depth: usize,
}

fn lower_function(f: &ast::FuncDef, funcs: &HashMap<String, FuncSig>) -> SResult<ir::Function> {
    let mut params = Vec::new();
    let mut ctx = FnCtx {
        funcs,
        fn_name: f.name.clone(),
        ret: f.ret.map(resolve_type).unwrap_or(ir::Ty::None),
        locals: HashMap::new(),
        locals_order: Vec::new(),
        loop_depth: 0,
    };

    for p in &f.params {
        let ty = resolve_type(p.ty);
        if ctx.locals.insert(p.name.clone(), ty).is_some() {
            return Err(err(
                format!("duplicate parameter '{}'", p.name),
                p.span,
            ));
        }
        params.push((p.name.clone(), ty));
    }

    let body = lower_block(&f.body, &mut ctx)?;

    // every path through a value-returning function must return
    if ctx.ret != ir::Ty::None && !block_returns(&body) {
        return Err(err(
            format!(
                "function '{}' is declared to return {} but can reach the end \
                 of its body without a return statement",
                f.name, ctx.ret
            ),
            f.span,
        ));
    }

    Ok(ir::Function {
        name: f.name.clone(),
        params,
        ret: ctx.ret,
        locals: ctx.locals_order,
        body,
    })
}

fn lower_block(stmts: &[ast::Stmt], ctx: &mut FnCtx) -> SResult<Vec<ir::Stmt>> {
    let mut out = Vec::new();
    for stmt in stmts {
        if let Some(lowered) = lower_stmt(stmt, ctx)? {
            out.push(lowered);
        }
    }
    Ok(out)
}

fn lower_stmt(stmt: &ast::Stmt, ctx: &mut FnCtx) -> SResult<Option<ir::Stmt>> {
    match &stmt.kind {
        ast::StmtKind::FuncDef(f) => Err(err(
            format!(
                "nested function definitions are not supported yet ('{}')",
                f.name
            ),
            f.span,
        )),
        ast::StmtKind::Pass => Ok(None),
        ast::StmtKind::Break => {
            if ctx.loop_depth == 0 {
                return Err(err("'break' outside of a loop", stmt.span));
            }
            Ok(Some(ir::Stmt::Break))
        }
        ast::StmtKind::Continue => {
            if ctx.loop_depth == 0 {
                return Err(err("'continue' outside of a loop", stmt.span));
            }
            Ok(Some(ir::Stmt::Continue))
        }
        ast::StmtKind::Return(value) => {
            match (value, ctx.ret) {
                (None, ir::Ty::None) => Ok(Some(ir::Stmt::Return(None))),
                (None, expected) => Err(err(
                    format!(
                        "function '{}' must return a value of type {}",
                        ctx.fn_name, expected
                    ),
                    stmt.span,
                )),
                (Some(e), ir::Ty::None) => {
                    // `return None` is fine in a None function
                    if matches!(e.kind, ast::ExprKind::NoneLit) {
                        return Ok(Some(ir::Stmt::Return(None)));
                    }
                    Err(err(
                        format!(
                            "function '{}' does not declare a return type; \
                             annotate it (e.g. 'def {}(...) -> int:') to return a value",
                            ctx.fn_name, ctx.fn_name
                        ),
                        e.span,
                    ))
                }
                (Some(e), expected) => {
                    let value = lower_expr(e, ctx)?;
                    let value = coerce(value, expected, e.span, "return value")?;
                    Ok(Some(ir::Stmt::Return(Some(value))))
                }
            }
        }
        ast::StmtKind::Assign {
            name,
            name_span,
            annotation,
            value,
        } => {
            let lowered = lower_assign(name, *name_span, *annotation, value, ctx)?;
            Ok(Some(lowered))
        }
        ast::StmtKind::AugAssign {
            name,
            name_span,
            op,
            value,
        } => {
            // desugar `x op= v` into `x = x op v`, reusing the binary logic
            let current_ty = *ctx.locals.get(name).ok_or_else(|| {
                err(format!("name '{name}' is not defined"), *name_span)
            })?;
            let left = ir::Expr {
                ty: current_ty,
                kind: ir::ExprKind::Local(name.clone()),
            };
            let right = lower_expr(value, ctx)?;
            let combined = lower_binary(*op, left, right, stmt.span)?;
            let combined = coerce_assign(combined, current_ty, name, stmt.span)?;
            Ok(Some(ir::Stmt::Assign {
                name: name.clone(),
                value: combined,
            }))
        }
        ast::StmtKind::ExprStmt(e) => {
            // print is a statement-level builtin
            if let ast::ExprKind::Call { func, args, .. } = &e.kind
                && func == "print"
            {
                let mut lowered_args = Vec::new();
                for (i, arg) in args.iter().enumerate() {
                    let a = lower_expr(arg, ctx)?;
                    if a.ty == ir::Ty::None {
                        return Err(err(
                            format!("print argument {} has no value (returns None)", i + 1),
                            arg.span,
                        ));
                    }
                    lowered_args.push(a);
                }
                return Ok(Some(ir::Stmt::Print(lowered_args)));
            }
            let lowered = lower_expr(e, ctx)?;
            Ok(Some(ir::Stmt::ExprStmt(lowered)))
        }
        ast::StmtKind::If { branches, orelse } => {
            let mut lowered_branches = Vec::new();
            for (cond, body) in branches {
                let c = lower_condition(cond, ctx)?;
                let b = lower_block(body, ctx)?;
                lowered_branches.push((c, b));
            }
            let lowered_orelse = lower_block(orelse, ctx)?;
            Ok(Some(ir::Stmt::If {
                branches: lowered_branches,
                orelse: lowered_orelse,
            }))
        }
        ast::StmtKind::While { cond, body } => {
            let c = lower_condition(cond, ctx)?;
            ctx.loop_depth += 1;
            let b = lower_block(body, ctx);
            ctx.loop_depth -= 1;
            Ok(Some(ir::Stmt::While { cond: c, body: b? }))
        }
    }
}

fn lower_assign(
    name: &str,
    name_span: Span,
    annotation: Option<ast::TypeName>,
    value: &ast::Expr,
    ctx: &mut FnCtx,
) -> SResult<ir::Stmt> {
    if ctx.funcs.contains_key(name) {
        return Err(err(
            format!("'{name}' is a function and cannot be assigned to"),
            name_span,
        ));
    }

    let lowered = lower_expr(value, ctx)?;

    let target_ty = match (annotation, ctx.locals.get(name).copied()) {
        (Some(ann), existing) => {
            let ann_ty = resolve_type(ann);
            if ann_ty == ir::Ty::None {
                return Err(err("cannot declare a variable of type None", name_span));
            }
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
        (None, None) => match lowered.ty {
            ir::Ty::None => {
                return Err(err(
                    format!(
                        "cannot assign to '{name}': the expression has no value \
                         (returns None)"
                    ),
                    value.span,
                ));
            }
            ir::Ty::Str => {
                return Err(err(
                    "str variables are not supported yet; string literals can \
                     only be printed",
                    value.span,
                ));
            }
            ty => ty,
        },
    };

    let value_expr = coerce_assign(lowered, target_ty, name, value.span)?;

    if !ctx.locals.contains_key(name) {
        ctx.locals.insert(name.to_string(), target_ty);
        ctx.locals_order.push((name.to_string(), target_ty));
    }

    Ok(ir::Stmt::Assign {
        name: name.to_string(),
        value: value_expr,
    })
}

/// Coerce `value` for assignment into a variable of type `target`.
fn coerce_assign(
    value: ir::Expr,
    target: ir::Ty,
    name: &str,
    span: Span,
) -> SResult<ir::Expr> {
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

/// Insert implicit promotion casts (`bool → int → float`) or fail.
fn coerce(value: ir::Expr, target: ir::Ty, span: Span, what: &str) -> SResult<ir::Expr> {
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
        (found, expected) => Err(err(
            format!("type mismatch in {what}: expected {expected}, found {found}"),
            span,
        )),
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
        ir::Ty::Int | ir::Ty::Float => Ok(ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ToBool(Box::new(value)),
        }),
        other => Err(err(
            format!("a value of type {other} cannot be used as a condition"),
            span,
        )),
    }
}

fn lower_expr(expr: &ast::Expr, ctx: &mut FnCtx) -> SResult<ir::Expr> {
    match &expr.kind {
        ast::ExprKind::Int(v) => Ok(ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::ConstInt(*v),
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
        ast::ExprKind::NoneLit => Err(err(
            "'None' cannot be used in an expression here",
            expr.span,
        )),
        ast::ExprKind::Name(name) => {
            if let Some(ty) = ctx.locals.get(name) {
                Ok(ir::Expr {
                    ty: *ty,
                    kind: ir::ExprKind::Local(name.clone()),
                })
            } else if ctx.funcs.contains_key(name) {
                Err(err(
                    format!("functions can only be called; add parentheses: '{name}(...)'"),
                    expr.span,
                ))
            } else {
                Err(err(format!("name '{name}' is not defined"), expr.span))
            }
        }
        ast::ExprKind::Call {
            func,
            func_span,
            args,
        } => {
            if func == "print" {
                return Err(err(
                    "print(...) does not return a value and cannot be used \
                     in an expression",
                    expr.span,
                ));
            }
            let sig = ctx
                .funcs
                .get(func)
                .cloned()
                .ok_or_else(|| err(format!("function '{func}' is not defined"), *func_span))?;
            if args.len() != sig.params.len() {
                return Err(err(
                    format!(
                        "function '{func}' takes {} argument(s) but {} were given",
                        sig.params.len(),
                        args.len()
                    ),
                    expr.span,
                ));
            }
            let mut lowered_args = Vec::new();
            for (i, (arg, &expected)) in args.iter().zip(&sig.params).enumerate() {
                let a = lower_expr(arg, ctx)?;
                let a = coerce(
                    a,
                    expected,
                    arg.span,
                    &format!("argument {} of '{func}'", i + 1),
                )?;
                lowered_args.push(a);
            }
            Ok(ir::Expr {
                ty: sig.ret,
                kind: ir::ExprKind::Call {
                    func: func.clone(),
                    args: lowered_args,
                },
            })
        }
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
                    let ty = value.ty;
                    Ok(ir::Expr {
                        ty,
                        kind: ir::ExprKind::Unary {
                            op: ir::UnOp::Neg,
                            operand: Box::new(value),
                        },
                    })
                }
            }
        }
        ast::ExprKind::Binary { op, left, right } => {
            // and/or take truthiness operands and short-circuit
            if matches!(op, ast::BinOp::And | ast::BinOp::Or) {
                let l = lower_condition(left, ctx)?;
                let r = lower_condition(right, ctx)?;
                let ir_op = if *op == ast::BinOp::And {
                    ir::BinOp::And
                } else {
                    ir::BinOp::Or
                };
                return Ok(ir::Expr {
                    ty: ir::Ty::Bool,
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
            ir::Ty::Int | ir::Ty::Float => Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::ToBool(Box::new(value)),
            }),
            other => Err(err(format!("bool() cannot convert {other}"), span)),
        },
        ast::TypeName::None => Err(err("None is not a conversion", span)),
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

fn lower_binary(op: ast::BinOp, l: ir::Expr, r: ir::Expr, span: Span) -> SResult<ir::Expr> {
    let describe = format!("operator '{op}'");

    // friendlier message for strings before the generic numeric error
    if l.ty == ir::Ty::Str || r.ty == ir::Ty::Str {
        return Err(err(
            format!(
                "{describe} is not supported for str yet; string literals can \
                 only be printed"
            ),
            span,
        ));
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
            let ir_op = match op {
                ast::BinOp::Eq => ir::BinOp::Eq,
                ast::BinOp::NotEq => ir::BinOp::Ne,
                ast::BinOp::Lt => ir::BinOp::Lt,
                ast::BinOp::LtEq => ir::BinOp::Le,
                ast::BinOp::Gt => ir::BinOp::Gt,
                ast::BinOp::GtEq => ir::BinOp::Ge,
                _ => unreachable!(),
            };
            Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Binary {
                    op: ir_op,
                    left: Box::new(l),
                    right: Box::new(r),
                },
            })
        }
        ast::BinOp::And | ast::BinOp::Or => {
            unreachable!("and/or are handled in lower_expr")
        }
    }
}

// ---- return-path analysis ----

fn block_returns(stmts: &[ir::Stmt]) -> bool {
    stmts.iter().any(stmt_returns)
}

fn stmt_returns(stmt: &ir::Stmt) -> bool {
    match stmt {
        ir::Stmt::Return(_) => true,
        ir::Stmt::If { branches, orelse } => {
            !orelse.is_empty()
                && branches.iter().all(|(_, body)| block_returns(body))
                && block_returns(orelse)
        }
        // `while True:` without a break never falls through
        ir::Stmt::While { cond, body } => {
            matches!(cond.kind, ir::ExprKind::ConstBool(true)) && !loop_breaks(body)
        }
        _ => false,
    }
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
            Err(e) => panic!("analyze failed: {}\n{}", e.message, e.render("test.py", src)),
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
        assert_eq!(fib.params, vec![("n".to_string(), ir::Ty::Int)]);
        let entry = find_func(&m, ENTRY_NAME);
        assert!(matches!(entry.body[0], ir::Stmt::Print(_)));
    }

    #[test]
    fn int_promotes_to_float_in_mixed_arithmetic() {
        let m = analyze_ok("x = 1 + 2.5\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::Assign { value, .. } = &entry.body[0] else {
            panic!("expected Assign");
        };
        assert_eq!(value.ty, ir::Ty::Float);
        let ir::ExprKind::Binary { left, .. } = &value.kind else {
            panic!("expected Binary");
        };
        assert!(matches!(left.kind, ir::ExprKind::IntToFloat(_)));
    }

    #[test]
    fn true_division_yields_float() {
        let m = analyze_ok("x = 7 / 2\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::Assign { value, .. } = &entry.body[0] else {
            panic!("expected Assign");
        };
        assert_eq!(value.ty, ir::Ty::Float);
    }

    #[test]
    fn floor_division_of_ints_stays_int() {
        let m = analyze_ok("x = 7 // 2\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::Assign { value, .. } = &entry.body[0] else {
            panic!("expected Assign");
        };
        assert_eq!(value.ty, ir::Ty::Int);
    }

    #[test]
    fn int_condition_gets_truthiness_cast() {
        let m = analyze_ok("x = 5\nwhile x:\n    x = x - 1\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::While { cond, .. } = &entry.body[1] else {
            panic!("expected While");
        };
        assert_eq!(cond.ty, ir::Ty::Bool);
        assert!(matches!(cond.kind, ir::ExprKind::ToBool(_)));
    }

    #[test]
    fn locals_are_collected_with_types() {
        let m = analyze_ok("x = 1\ny = 2.5\nb = True\n");
        let entry = find_func(&m, ENTRY_NAME);
        assert_eq!(
            entry.locals,
            vec![
                ("x".to_string(), ir::Ty::Int),
                ("y".to_string(), ir::Ty::Float),
                ("b".to_string(), ir::Ty::Bool),
            ]
        );
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
    fn error_wrong_arg_count() {
        let e = analyze_err("def f(a: int) -> int:\n    return a\nf(1, 2)\n");
        assert!(e.message.contains("argument"), "{}", e.message);
    }

    #[test]
    fn error_wrong_arg_type() {
        let e = analyze_err("def f(a: int) -> int:\n    return a\nf(1.5)\n");
        assert!(e.message.contains("mismatch"), "{}", e.message);
    }

    #[test]
    fn error_missing_return_path() {
        let e = analyze_err("def f(a: int) -> int:\n    if a:\n        return 1\n");
        assert!(e.message.contains("without a return"), "{}", e.message);
    }

    #[test]
    fn while_true_counts_as_returning() {
        analyze_ok("def f() -> int:\n    while True:\n        pass\nf()\n");
    }

    #[test]
    fn while_true_with_break_does_not_return() {
        let e = analyze_err("def f() -> int:\n    while True:\n        break\nf()\n");
        assert!(e.message.contains("without a return"), "{}", e.message);
    }

    #[test]
    fn error_break_outside_loop() {
        let e = analyze_err("break\n");
        assert!(e.message.contains("outside"), "{}", e.message);
    }

    #[test]
    fn error_nested_def() {
        let e = analyze_err("def f():\n    def g():\n        pass\n    pass\nf()\n");
        assert!(e.message.contains("nested"), "{}", e.message);
    }

    #[test]
    fn error_str_arithmetic() {
        let e = analyze_err("x = \"a\" + 1\n");
        assert!(e.message.contains("str"), "{}", e.message);
    }

    #[test]
    fn error_print_in_expression() {
        let e = analyze_err("x = print(1)\n");
        assert!(e.message.contains("print"), "{}", e.message);
    }

    #[test]
    fn error_return_value_from_none_function() {
        let e = analyze_err("def f():\n    return 5\nf()\n");
        assert!(e.message.contains("return type"), "{}", e.message);
    }

    #[test]
    fn bool_assignable_to_int() {
        let m = analyze_ok("x: int = True\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::Assign { value, .. } = &entry.body[0] else {
            panic!("expected Assign");
        };
        assert_eq!(value.ty, ir::Ty::Int);
        assert!(matches!(value.kind, ir::ExprKind::BoolToInt(_)));
    }

    #[test]
    fn casts_lower_correctly() {
        let m = analyze_ok("x = int(2.9)\ny = float(3)\nb = bool(0)\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::Assign { value, .. } = &entry.body[0] else {
            panic!();
        };
        assert!(matches!(value.kind, ir::ExprKind::FloatToInt(_)));
        let ir::Stmt::Assign { value, .. } = &entry.body[1] else {
            panic!();
        };
        assert!(matches!(value.kind, ir::ExprKind::IntToFloat(_)));
        let ir::Stmt::Assign { value, .. } = &entry.body[2] else {
            panic!();
        };
        assert!(matches!(value.kind, ir::ExprKind::ToBool(_)));
    }

    #[test]
    fn aug_assign_desugars() {
        let m = analyze_ok("x = 1\nx += 2\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::Assign { name, value } = &entry.body[1] else {
            panic!("expected Assign");
        };
        assert_eq!(name, "x");
        assert!(matches!(
            value.kind,
            ir::ExprKind::Binary { op: ir::BinOp::Add, .. }
        ));
    }

    #[test]
    fn error_aug_div_changes_int_type() {
        // x /= 2 would turn an int into a float
        let e = analyze_err("x = 4\nx /= 2\n");
        assert!(e.message.contains("fixed"), "{}", e.message);
    }

    #[test]
    fn and_or_produce_bool() {
        let m = analyze_ok("x = 1 and 2.5 or True\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::Assign { value, .. } = &entry.body[0] else {
            panic!("expected Assign");
        };
        assert_eq!(value.ty, ir::Ty::Bool);
    }

    #[test]
    fn print_accepts_mixed_args() {
        let m = analyze_ok("print(1, 2.5, True, \"label\")\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::Print(args) = &entry.body[0] else {
            panic!("expected Print");
        };
        assert_eq!(args.len(), 4);
        assert_eq!(args[3].ty, ir::Ty::Str);
    }
}
