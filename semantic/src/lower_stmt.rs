//! Statement lowering (control flow, assignments, with/match, method stmts).

use std::collections::{HashMap, HashSet};

use common::{Diagnostic, Phase, Span};
use parser::ast;

use crate::prelude::*;

pub(crate) fn lower_stmt(
    stmt: &ast::Stmt,
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    match &stmt.kind {
        ast::StmtKind::Reraise => {
            // CPython raises `RuntimeError: No active exception to re-raise`;
            // there is nothing to re-raise here either, and a compile error
            // says so before the program runs.
            if ctx.handler_depth == 0 {
                return Err(err(
                    "bare 'raise' is only valid inside an 'except' handler; \
                     there is no active exception to re-raise here",
                    stmt.span,
                ));
            }
            out.push(ir::Stmt::Reraise);
            Ok(())
        }
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
                // Annotation-only modules have no body to run.
                if is_typing_module(module) {
                    continue;
                }
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
            if module == FUTURE_MODULE {
                // Compiler directive, already validated during import
                // collection. Emitting nothing keeps it out of the module
                // init graph, which has no `__future__` to initialize.
                return Ok(());
            }
            if *star && !ctx.is_entry {
                return Err(err("import * only allowed at module level", *span));
            }
            if is_typing_module(module) {
                // Annotation-only: nothing to initialise and nothing to bind.
                return Ok(());
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
            let msg = match message {
                Some(m) => {
                    let v = lower_expr(m, ctx)?;
                    Some(coerce(v, ir::Ty::Str, m.span, "raise message")?)
                }
                None => None,
            };
            out.push(ir::Stmt::Raise {
                exc: resolve_exc_name(exc)?,
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
            // `assert x` raises `AssertionError()` with no argument;
            // `assert x, ""` raises `AssertionError('')` with one.
            let message = match msg {
                Some(m) => {
                    let m_ir = lower_expr(m, ctx)?;
                    Some(if m_ir.ty == ir::Ty::Str {
                        m_ir
                    } else {
                        lower_cast(ast::TypeName::Str, m_ir, m.span)?
                    })
                }
                None => None,
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
                ctx.handler_depth += 1;
                let body_h = lower_nested_block(&h.body, ctx);
                ctx.handler_depth -= 1;
                let body_h = body_h?;
                let filter = match &h.exc {
                    Some(ts) => Some(
                        ts.iter()
                            .map(resolve_exc_name)
                            .collect::<SResult<Vec<_>>>()?,
                    ),
                    Option::None => Option::None,
                };
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
            // Bare `name := value` as a statement is a SyntaxError in CPython.
            if matches!(e.kind, ast::ExprKind::NamedExpr { .. }) {
                return Err(err(
                    "assignment expression (:=) cannot be used as a statement; \
                     wrap it in parentheses in a larger expression",
                    e.span,
                ));
            }
            // `sys.exit(code)` is a statement: it never returns, so there is
            // no value for an expression position to use.
            if let ast::ExprKind::MethodCall {
                base,
                method,
                args,
                keywords,
                kwargs,
                method_span,
            } = &e.kind
                && method == "exit"
                && matches!(&base.kind, ast::ExprKind::Name(a) if ctx.sys_alias(a))
            {
                if !keywords.is_empty() || kwargs.is_some() {
                    return Err(err("sys.exit() takes no keyword arguments", *method_span));
                }
                let plain = require_plain_args(args, "sys.exit", e.span)?;
                let code = match plain.len() {
                    0 => int_const(0),
                    1 => {
                        let v = lower_expr(plain[0], ctx)?;
                        coerce(v, ir::Ty::Int, plain[0].span, "sys.exit() status")?
                    }
                    n => {
                        return Err(err(
                            format!("sys.exit() takes at most one argument ({n} given)"),
                            e.span,
                        ));
                    }
                };
                out.push(ir::Stmt::SysExit { code });
                return Ok(());
            }
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
                    // Honor class `__str__` / `__repr__` for print (CPython),
                    // including when only a subclass defines a dunder.
                    let a = if let ir::Ty::Class(id) = a.ty {
                        if subclasses_of(id)
                            .iter()
                            .any(|sid| resolve_str_dunder(*sid).is_some())
                        {
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
                let mut sep = const_str_expr(" ");
                let mut end = const_str_expr("\n");
                let mut flush = const_bool_expr(false);
                let mut to_stderr = false;
                let mut to_file: Option<Box<ir::Expr>> = Option::None;
                let mut seen_sep = false;
                let mut seen_end = false;
                let mut seen_flush = false;
                for kw in keywords {
                    match kw.name.as_str() {
                        "sep" => {
                            if seen_sep {
                                return Err(err(
                                    "print() got multiple values for keyword argument 'sep'",
                                    kw.name_span,
                                ));
                            }
                            seen_sep = true;
                            let v = lower_expr(&kw.value, ctx)?;
                            let v = coerce_print_sep_end(v, "sep", kw.value.span)?;
                            // Bind in source order so `end=` before `sep=` still
                            // evaluates left-to-right like CPython.
                            let tmp = ctx.fresh_temp("printsep", ir::Ty::Str);
                            out.push(ir::Stmt::Assign {
                                name: tmp.clone(),
                                value: v,
                            });
                            sep = ir::Expr {
                                ty: ir::Ty::Str,
                                kind: ir::ExprKind::Local(tmp),
                            };
                        }
                        "end" => {
                            if seen_end {
                                return Err(err(
                                    "print() got multiple values for keyword argument 'end'",
                                    kw.name_span,
                                ));
                            }
                            seen_end = true;
                            let v = lower_expr(&kw.value, ctx)?;
                            let v = coerce_print_sep_end(v, "end", kw.value.span)?;
                            let tmp = ctx.fresh_temp("printend", ir::Ty::Str);
                            out.push(ir::Stmt::Assign {
                                name: tmp.clone(),
                                value: v,
                            });
                            end = ir::Expr {
                                ty: ir::Ty::Str,
                                kind: ir::ExprKind::Local(tmp),
                            };
                        }
                        "flush" => {
                            if seen_flush {
                                return Err(err(
                                    "print() got multiple values for keyword argument 'flush'",
                                    kw.name_span,
                                ));
                            }
                            seen_flush = true;
                            let v = lower_expr(&kw.value, ctx)?;
                            let v = to_bool(v, kw.value.span, ctx)?;
                            let tmp = ctx.fresh_temp("printflush", ir::Ty::Bool);
                            out.push(ir::Stmt::Assign {
                                name: tmp.clone(),
                                value: v,
                            });
                            flush = ir::Expr {
                                ty: ir::Ty::Bool,
                                kind: ir::ExprKind::Local(tmp),
                            };
                        }
                        "file" => {
                            // The standard streams stay a flag: they funnel
                            // into the same writer and need no file object.
                            // Anything else must be an open file, and the
                            // print is bracketed by a redirect.
                            match stream_keyword(&kw.value, ctx) {
                                Some(is_err) => to_stderr = is_err,
                                Option::None => {
                                    let f = lower_expr(&kw.value, ctx)?;
                                    if f.ty != ir::Ty::File {
                                        return Err(err(
                                            format!(
                                                "print(file=...) needs a file, found {}; \
                                                 pass one of sys.stdout / sys.stderr or \
                                                 a file from open()",
                                                display_ty(f.ty)
                                            ),
                                            kw.value.span,
                                        ));
                                    }
                                    to_file = Some(Box::new(f));
                                }
                            }
                        }
                        other => {
                            return Err(err(
                                format!("print() got an unexpected keyword argument '{other}'"),
                                kw.name_span,
                            ));
                        }
                    }
                }
                out.push(ir::Stmt::Print {
                    args: lowered_args,
                    sep,
                    end,
                    flush,
                    to_stderr,
                    to_file,
                });
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
                if kwargs.is_some() {
                    return Err(err(
                        "** unpacking is not supported for this method call",
                        *method_span,
                    ));
                }
                // `list.sort(key=…, reverse=…)` — keyword form (statement only).
                if method == "sort" && !keywords.is_empty() {
                    let base_ir = lower_expr(base, ctx)?;
                    match base_ir.ty {
                        ir::Ty::List(elem) => {
                            if !args.is_empty() {
                                return Err(err(
                                    format!(
                                        "sort() takes no positional arguments ({} given)",
                                        args.len()
                                    ),
                                    *method_span,
                                ));
                            }
                            let sk = take_sort_keywords(keywords, "list.sort")?;
                            let list_ty = base_ir.ty;
                            let xs_t = ctx.fresh_temp("lsort", list_ty);
                            let xs = local_expr(xs_t.clone(), list_ty);
                            let (rev_mode, mut rev_bind) = resolve_reverse_flag(sk.reverse, ctx)?;
                            let mut stmts = vec![ir::Stmt::Assign {
                                name: xs_t,
                                value: base_ir,
                            }];
                            stmts.append(&mut rev_bind);
                            // CPython stable reverse: reverse → sort ascending → reverse.
                            push_maybe_reverse(&mut stmts, &rev_mode, &xs, *elem, ctx);
                            if let Some(key_ast) = sk.key {
                                stmts.extend(lower_list_sort_key_stmts(
                                    xs.clone(),
                                    *elem,
                                    key_ast,
                                    ctx,
                                )?);
                            } else {
                                push_plain_list_sort(
                                    &mut stmts,
                                    xs.clone(),
                                    *elem,
                                    *method_span,
                                    ctx,
                                )?;
                            }
                            push_maybe_reverse(&mut stmts, &rev_mode, &xs, *elem, ctx);
                            out.extend(stmts);
                            return Ok(());
                        }
                        _ => {
                            return Err(err(
                                "keyword arguments are not supported for this method call",
                                keywords[0].name_span,
                            ));
                        }
                    }
                }
                let plain = require_plain_args(args, method, *method_span)?;
                let plain_owned: Vec<ast::Expr> = plain.iter().map(|e| (*e).clone()).collect();
                let stmt =
                    lower_method_stmt(base, method, *method_span, &plain_owned, keywords, ctx)?;
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
pub(crate) fn lower_match(
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

pub(crate) fn pattern_is_irrefutable(p: &ast::Pattern) -> bool {
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
pub(crate) fn validate_pattern_no_duplicate_binds(p: &ast::Pattern, span: Span) -> SResult<()> {
    let mut names = HashSet::new();
    check_dup_binds(p, &mut names, span)?;
    Ok(())
}

pub(crate) fn check_dup_binds(
    p: &ast::Pattern,
    seen: &mut HashSet<String>,
    span: Span,
) -> SResult<()> {
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
pub(crate) fn pattern_capture_names(p: &ast::Pattern) -> Vec<String> {
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
pub(crate) fn validate_or_pattern_binds(alts: &[ast::Pattern], span: Span) -> SResult<()> {
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
pub(crate) fn lower_pattern_match(
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
        ast::Pattern::Class {
            name,
            name_span,
            positional,
            keywords,
        } => {
            let class_id = lookup_class(name).ok_or_else(|| {
                err(
                    format!("unknown class '{name}' in match pattern"),
                    *name_span,
                )
            })?;
            // Subject must be class instance (or refine to one).
            let subj = match subject.ty {
                ir::Ty::Class(id) if class_is_subclass(id, class_id) || id == class_id => {
                    subject.clone()
                }
                ir::Ty::Class(id) if class_is_subclass(class_id, id) => {
                    // Static base, pattern is subclass: need isinstance check + retype.
                    subject.clone()
                }
                other => {
                    return Err(err(
                        format!(
                            "class pattern '{name}' requires a class instance subject, found {other}"
                        ),
                        span,
                    ));
                }
            };
            // isinstance(subject, Class)
            let isa = ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::ClassIsInstance {
                    value: Box::new(subj.clone()),
                    class_id,
                },
            };
            let info = class_info(class_id)
                .ok_or_else(|| err("internal: missing class info for pattern", *name_span))?;
            let fields = &info.fields;
            if positional.len() > fields.len() {
                return Err(err(
                    format!(
                        "class pattern '{name}' has {} positional sub-patterns but only {} field(s)",
                        positional.len(),
                        fields.len()
                    ),
                    span,
                ));
            }
            let mut cond = isa;
            // Positional: match fields in layout order.
            for (i, pat) in positional.iter().enumerate() {
                let (fname, fty) = &fields[i];
                let field_e = ir::Expr {
                    ty: *fty,
                    kind: ir::ExprKind::GetField {
                        object: Box::new(ir::Expr {
                            ty: ir::Ty::Class(class_id),
                            kind: subj.kind.clone(),
                        }),
                        class_id,
                        field_index: i as u32,
                    },
                };
                let c = lower_pattern_match(pat, &field_e, span, ctx, binds)?;
                cond = bool_and(cond, c);
                let _ = fname;
            }
            // Keywords: field=pattern
            for (fname, pat) in keywords {
                let (fidx, fty) = field_index(class_id, fname)
                    .ok_or_else(|| err(format!("class '{name}' has no field '{fname}'"), span))?;
                let field_e = ir::Expr {
                    ty: fty,
                    kind: ir::ExprKind::GetField {
                        object: Box::new(ir::Expr {
                            ty: ir::Ty::Class(class_id),
                            kind: subj.kind.clone(),
                        }),
                        class_id,
                        field_index: fidx,
                    },
                };
                let c = lower_pattern_match(pat, &field_e, span, ctx, binds)?;
                cond = bool_and(cond, c);
            }
            Ok(cond)
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

pub(crate) fn lower_sequence_pattern(
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

pub(crate) fn lower_mapping_pattern(
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
pub(crate) fn lower_with(
    item: &ast::Expr,
    target: Option<&(String, Span)>,
    body: &[ast::Stmt],
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    let item_ir = lower_expr(item, ctx)?;

    // User class context manager:
    //   mgr = item; val = mgr.__enter__(); [bind]
    //   try: body
    //   except Exception as e:
    //       if not mgr.__exit__(None, e, None): raise e
    //   else:
    //       mgr.__exit__(None, None, None)
    // Residual vs CPython: type and traceback args are always None; the value
    // arg is the exception object (as Any) when an exception occurred.
    if let ir::Ty::Class(id) = item_ir.ty {
        if resolve_method(id, "__enter__").is_none() || resolve_method(id, "__exit__").is_none() {
            return Err(err(
                format!(
                    "'{}' object does not support the context manager protocol \
                     (need __enter__ and __exit__)",
                    class_info(id)
                        .map(|c| c.name)
                        .unwrap_or_else(|| format!("class#{id}"))
                ),
                item.span,
            ));
        }
        let mgr_t = ctx.fresh_temp("with.mgr", item_ir.ty);
        out.push(ir::Stmt::Assign {
            name: mgr_t.clone(),
            value: item_ir,
        });
        let mgr = || ir::Expr {
            ty: ir::Ty::Class(id),
            kind: ir::ExprKind::Local(mgr_t.clone()),
        };
        let entered = lower_instance_method_call(mgr(), id, "__enter__", item.span, &[], ctx)?;
        if let Some((name, name_span)) = target {
            let bind = bind_name(name, *name_span, None, entered, item.span, ctx)?;
            out.push(bind);
        } else {
            out.push(ir::Stmt::ExprStmt(entered));
        }
        let none_ast = |sp: Span| ast::Expr {
            kind: ast::ExprKind::NoneLit,
            span: sp,
        };
        let map_exit_err = |e: Diagnostic| {
            err(
                format!(
                    "{}; __exit__ must accept three arguments after self \
                     (e.g. a: Any = None, b: Any = None, c: Any = None)",
                    e.message
                ),
                item.span,
            )
        };
        // Success path: __exit__(None, None, None)
        let exit_ok_args = [
            none_ast(item.span),
            none_ast(item.span),
            none_ast(item.span),
        ];
        let exit_ok =
            lower_instance_method_call(mgr(), id, "__exit__", item.span, &exit_ok_args, ctx)
                .map_err(map_exit_err)?;
        // Exception path: bind e, __exit__(None, e, None), suppress or re-raise.
        let exc_t = ctx.fresh_temp("with.exc", ir::Ty::Exception);
        let exc_name_ast = ast::Expr {
            kind: ast::ExprKind::Name(exc_t.clone()),
            span: item.span,
        };
        let exit_exc_args = [none_ast(item.span), exc_name_ast, none_ast(item.span)];
        let exit_exc =
            lower_instance_method_call(mgr(), id, "__exit__", item.span, &exit_exc_args, ctx)
                .map_err(map_exit_err)?;
        let suppress = to_bool(exit_exc, item.span, ctx)?;
        let not_suppress = ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::Unary {
                op: ir::UnOp::Not,
                operand: Box::new(suppress),
            },
        };
        let handler_body = vec![ir::Stmt::If {
            branches: vec![(
                not_suppress,
                vec![ir::Stmt::RaiseExc {
                    value: ir::Expr {
                        ty: ir::Ty::Exception,
                        kind: ir::ExprKind::Local(exc_t.clone()),
                    },
                }],
            )],
            orelse: vec![],
        }];
        let body_ir = lower_nested_block(body, ctx)?;
        out.push(ir::Stmt::Try {
            body: body_ir,
            handlers: vec![(
                Some(vec![ir::ExcType::Exception]),
                Some(exc_t),
                handler_body,
            )],
            orelse: vec![ir::Stmt::ExprStmt(exit_ok)],
            finally: vec![],
        });
        return Ok(());
    }

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
pub(crate) fn lower_method_stmt(
    base: &ast::Expr,
    method: &str,
    method_span: Span,
    args: &[ast::Expr],
    keywords: &[ast::Keyword],
    ctx: &mut FnCtx,
) -> SResult<ir::Stmt> {
    // `super().m(...)` in statement position (e.g. super().__init__(...)).
    if is_zero_arg_super(base) {
        let call = lower_super_method_call(method, method_span, args, ctx)?;
        return Ok(ir::Stmt::ExprStmt(call));
    }
    if is_str_type_name(base, ctx) && method == "maketrans" {
        let call = lower_str_maketrans(args, method_span, ctx)?;
        return Ok(ir::Stmt::ExprStmt(call));
    }
    if is_dict_type_name(base, ctx) && method == "fromkeys" {
        let call = lower_dict_fromkeys(args, method_span, ctx)?;
        return Ok(ir::Stmt::ExprStmt(call));
    }
    let base_ir = lower_expr(base, ctx)?;
    if let ir::Ty::Class(id) = base_ir.ty {
        if resolve_property(id, method).is_some() {
            return Err(err(
                format!(
                    "'{}' object attribute '{method}' is a property and is not callable",
                    class_info(id)
                        .map(|c| c.name)
                        .unwrap_or_else(|| format!("class#{id}"))
                ),
                method_span,
            ));
        }
        let call =
            lower_instance_method_call_kw(base_ir, id, method, method_span, args, keywords, ctx)?;
        return Ok(ir::Stmt::ExprStmt(call));
    }
    // Past this point the base is a builtin type, whose method table has no
    // keyword surface beyond the `sort` case handled by the caller.
    if !keywords.is_empty() {
        return Err(err(
            "keyword arguments are not supported for this method call",
            keywords[0].name_span,
        ));
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
                // `lower_arg_expr`, not `lower_expr` + `coerce`: it steers a
                // container literal with the element type, so
                // `rows.append(["a", 1])` into a `list[list[Any]]` boxes at
                // construction instead of demanding one element type.
                let value = lower_arg_expr(&args[0], *elem, "append() argument", ctx)?;
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
                let value = lower_arg_expr(&args[1], *elem, "insert() argument", ctx)?;
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
                if ty_uses_class_eq(*elem) {
                    return lower_list_remove_protocol(base_ir, value, *elem, method_span, ctx);
                }
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
            "reverse" => {
                if !args.is_empty() {
                    return Err(err(
                        format!("reverse() takes no arguments ({} given)", args.len()),
                        method_span,
                    ));
                }
                Ok(ir::Stmt::ListReverse { list: base_ir })
            }
            "sort" => {
                if !args.is_empty() {
                    return Err(err(
                        format!(
                            "sort() takes no positional arguments ({} given); \
                             use sort(key=..., reverse=...)",
                            args.len()
                        ),
                        method_span,
                    ));
                }
                if class_supports_lt(*elem) {
                    let list_ty = base_ir.ty;
                    let xs_t = ctx.fresh_temp("lsort", list_ty);
                    let xs = local_expr(xs_t.clone(), list_ty);
                    let mut stmts = vec![ir::Stmt::Assign {
                        name: xs_t,
                        value: base_ir,
                    }];
                    stmts.extend(lower_list_sort_class_stmts(xs, *elem, method_span, ctx)?);
                    return Ok(ir::Stmt::ExprStmt(ir::Expr {
                        ty: ir::Ty::None,
                        kind: ir::ExprKind::Block {
                            stmts,
                            result: Box::new(const_none()),
                        },
                    }));
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
            "count" => {
                let n = lower_list_count(base_ir, *elem, args, method_span, ctx)?;
                Ok(ir::Stmt::ExprStmt(n))
            }
            _ => Err(err(
                format!(
                    "list method '{method}' is not supported yet (supported: \
                     append, pop, insert, remove, index, count, clear, reverse, sort, extend, copy)"
                ),
                method_span,
            )),
        },
        ir::Ty::Tuple(elems) => match method {
            "index" => {
                let idx = lower_tuple_index_of(base_ir, elems, args, method_span, ctx)?;
                Ok(ir::Stmt::ExprStmt(idx))
            }
            "count" => {
                let n = lower_tuple_count(base_ir, elems, args, method_span, ctx)?;
                Ok(ir::Stmt::ExprStmt(n))
            }
            _ => Err(err(
                format!("tuple method '{method}' is not supported yet (supported: index, count)"),
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

pub(crate) fn lower_generator_method_stmt(
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
pub(crate) fn lower_gen_send_arg(
    arg: &ast::Expr,
    yield_ty: ir::Ty,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let v = lower_expr(arg, ctx)?;
    if matches!(v.kind, ir::ExprKind::ConstNone) || v.ty == ir::Ty::None {
        return Ok(const_none());
    }
    coerce(v, yield_ty, arg.span, "generator.send value")
}

/// Parse `throw(ExcType)`, `throw(ExcType("msg"))`, or `throw(ExcType, "msg")`.
pub(crate) fn lower_gen_throw_args(
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

pub(crate) fn parse_exc_type_name(e: &ast::Expr) -> SResult<ir::ExcType> {
    match &e.kind {
        ast::ExprKind::Name(n) => name_to_exc_type(n, e.span),
        _ => Err(err(
            "throw() first argument must be an exception type name",
            e.span,
        )),
    }
}

pub(crate) fn name_to_exc_type(name: &str, span: Span) -> SResult<ir::ExcType> {
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
        "AttributeError" => Ok(ir::ExcType::AttributeError),
        "NotImplementedError" => Ok(ir::ExcType::NotImplementedError),
        "ImportError" => Ok(ir::ExcType::ImportError),
        "ModuleNotFoundError" => Ok(ir::ExcType::ModuleNotFoundError),
        "LookupError" => Ok(ir::ExcType::LookupError),
        "ArithmeticError" => Ok(ir::ExcType::ArithmeticError),
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

pub(crate) fn empty_str_const() -> ir::Expr {
    ir::Expr {
        ty: ir::Ty::Str,
        kind: ir::ExprKind::ConstStr(String::new()),
    }
}

pub(crate) fn lower_dict_method_stmt(
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
        "get" | "pop" | "keys" | "values" | "items" | "setdefault" | "popitem" | "fromkeys"
        | "copy" => {
            let call = lower_dict_method(base_ir, key_ty, val_ty, method, method_span, args, ctx)?;
            Ok(ir::Stmt::ExprStmt(call))
        }
        _ => Err(err(
            format!(
                "dict method '{method}' is not supported yet (supported: get, pop, \
                 keys, values, items, clear, update, setdefault, popitem, copy, fromkeys)"
            ),
            method_span,
        )),
    }
}

pub(crate) fn lower_set_method_stmt(
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
        "issubset" | "issuperset" | "isdisjoint" => {
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
            let rel = lower_set_relation(base_ir, other, method_span, method)?;
            Ok(ir::Stmt::ExprStmt(rel))
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
                op: ir::SetUpdateOp::Union,
            })
        }
        "intersection_update" | "difference_update" | "symmetric_difference_update" => {
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
            let expect = ir::set_of(elem_ty);
            if other.ty != expect {
                return Err(err(
                    format!(
                        "set.{method}() expects {expect}, found {} (same element type required)",
                        other.ty
                    ),
                    args[0].span,
                ));
            }
            let op = match method {
                "intersection_update" => ir::SetUpdateOp::Intersect,
                "difference_update" => ir::SetUpdateOp::Diff,
                _ => ir::SetUpdateOp::SymDiff,
            };
            Ok(ir::Stmt::SetUpdate {
                set: base_ir,
                other,
                op,
            })
        }
        "copy" => {
            if !args.is_empty() {
                return Err(err(
                    format!("copy() takes no arguments ({} given)", args.len()),
                    method_span,
                ));
            }
            Ok(ir::Stmt::ExprStmt(ir::Expr {
                ty: ir::set_of(elem_ty),
                kind: ir::ExprKind::SetCopy(Box::new(base_ir)),
            }))
        }
        "pop" => {
            let p = lower_set_pop(base_ir, elem_ty, args, method_span)?;
            Ok(ir::Stmt::ExprStmt(p))
        }
        _ => Err(err(
            format!(
                "set method '{method}' is not supported yet (supported: add, remove, \
                 discard, clear, union, intersection, difference, symmetric_difference, \
                 issubset, issuperset, isdisjoint, update, intersection_update, \
                 difference_update, symmetric_difference_update, copy, pop)"
            ),
            method_span,
        )),
    }
}

pub(crate) fn lower_set_pop(
    set: ir::Expr,
    elem_ty: ir::Ty,
    args: &[ast::Expr],
    method_span: Span,
) -> SResult<ir::Expr> {
    if !args.is_empty() {
        return Err(err(
            format!("pop() takes no arguments ({} given)", args.len()),
            method_span,
        ));
    }
    Ok(ir::Expr {
        ty: elem_ty,
        kind: ir::ExprKind::SetPop(Box::new(set)),
    })
}

pub(crate) fn lower_set_relation(
    l: ir::Expr,
    r: ir::Expr,
    span: Span,
    name: &str,
) -> SResult<ir::Expr> {
    match (l.ty, r.ty) {
        (ir::Ty::Set(a), ir::Ty::Set(b)) if a == b => {
            let kind = match name {
                "issubset" => ir::ExprKind::SetIsSubset {
                    left: Box::new(l),
                    right: Box::new(r),
                    proper: false,
                },
                "issuperset" => ir::ExprKind::SetIsSubset {
                    left: Box::new(r),
                    right: Box::new(l),
                    proper: false,
                },
                "isdisjoint" => ir::ExprKind::SetIsDisjoint {
                    left: Box::new(l),
                    right: Box::new(r),
                },
                "lt" => ir::ExprKind::SetIsSubset {
                    left: Box::new(l),
                    right: Box::new(r),
                    proper: true,
                },
                "le" => ir::ExprKind::SetIsSubset {
                    left: Box::new(l),
                    right: Box::new(r),
                    proper: false,
                },
                "gt" => ir::ExprKind::SetIsSubset {
                    left: Box::new(r),
                    right: Box::new(l),
                    proper: true,
                },
                "ge" => ir::ExprKind::SetIsSubset {
                    left: Box::new(r),
                    right: Box::new(l),
                    proper: false,
                },
                _ => unreachable!("unknown set relation {name}"),
            };
            Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind,
            })
        }
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

pub(crate) fn lower_set_union(l: ir::Expr, r: ir::Expr, span: Span) -> SResult<ir::Expr> {
    lower_set_binary_op(l, r, span, "union", |left, right| ir::ExprKind::SetUnion {
        left,
        right,
    })
}

pub(crate) fn lower_set_binary_op(
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
pub(crate) fn lower_list_ctor(arg: ir::Expr, span: Span, ctx: &mut FnCtx) -> SResult<ir::Expr> {
    // A generator is drained into a list first; `list(gen)` then *is* that list.
    if let ir::Ty::Generator { yield_ty } = arg.ty {
        return drain_generator_to_list(arg, *yield_ty, ctx);
    }
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
            // Fixed-arity: bind tuple once, then index the temp (no re-eval).
            let n = elems.len();
            let tmp = ctx.fresh_temp("list.tup", arg.ty);
            let mut items = Vec::new();
            let base_local = ir::Expr {
                ty: arg.ty,
                kind: ir::ExprKind::Local(tmp.clone()),
            };
            for i in 0..n {
                items.push(ir::Expr {
                    ty: first,
                    kind: ir::ExprKind::Index {
                        base: Box::new(base_local.clone()),
                        index: Box::new(int_const(i as i64)),
                    },
                });
            }
            Ok(ir::Expr {
                ty: ir::list_of(first),
                kind: ir::ExprKind::Let {
                    name: tmp,
                    value: Box::new(arg),
                    body: Box::new(ir::Expr {
                        ty: ir::list_of(first),
                        kind: ir::ExprKind::ListLit(items),
                    }),
                },
            })
        }
        other => Err(err(format!("list() cannot convert {other} yet"), span)),
    }
}

pub(crate) fn lower_set_ctor(arg: ir::Expr, span: Span) -> SResult<ir::Expr> {
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

pub(crate) fn lower_dict_ctor(arg: ir::Expr, span: Span) -> SResult<ir::Expr> {
    match arg.ty {
        ir::Ty::Dict { key, value } => Ok(ir::Expr {
            ty: ir::dict_of(*key, *value),
            kind: ir::ExprKind::DictCopy(Box::new(arg)),
        }),
        ir::Ty::List(elem) => match *elem {
            ir::Ty::Tuple(ts) if ts.len() == 2 => {
                let k = ts[0];
                let v = ts[1];
                if !is_hashable_key_ty(k) {
                    return Err(err(
                        format!("dict() keys must be int, str, or a tuple of those, found {k}"),
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

pub(crate) fn lower_tuple_ctor(arg: ir::Expr, span: Span) -> SResult<ir::Expr> {
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

pub(crate) fn lower_dict_method(
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
        "setdefault" => {
            if args.is_empty() || args.len() > 2 {
                return Err(err(
                    if args.is_empty() {
                        "setdefault expected at least 1 argument, got 0".into()
                    } else {
                        format!(
                            "setdefault expected at most 2 arguments, got {}",
                            args.len()
                        )
                    },
                    method_span,
                ));
            }
            let key = lower_expr(&args[0], ctx)?;
            let key = coerce(key, key_ty, args[0].span, "dict.setdefault() key")?;
            let default = if args.len() == 2 {
                lower_arg_expr(&args[1], val_ty, "dict.setdefault() default", ctx)?
            } else {
                if !ir::is_optional(val_ty) {
                    return Err(err(
                        format!(
                            "dict.setdefault() without a default requires a value type \
                             that includes None (found {val_ty})"
                        ),
                        method_span,
                    ));
                }
                coerce(
                    const_none(),
                    val_ty,
                    method_span,
                    "dict.setdefault() default",
                )?
            };
            Ok(ir::Expr {
                ty: val_ty,
                kind: ir::ExprKind::DictSetDefault {
                    dict: Box::new(base_ir),
                    key: Box::new(key),
                    default: Box::new(default),
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
        "popitem" => {
            if !args.is_empty() {
                return Err(err(
                    format!("dict.popitem() takes no arguments ({} given)", args.len()),
                    method_span,
                ));
            }
            Ok(ir::Expr {
                ty: ir::tuple_of(&[key_ty, val_ty]),
                kind: ir::ExprKind::DictPopItem(Box::new(base_ir)),
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
        "fromkeys" => lower_dict_fromkeys(args, method_span, ctx),
        _ => Err(err(
            format!(
                "dict method '{method}' is not supported yet (supported: get, pop, \
                 keys, values, items, clear, copy, setdefault, popitem, fromkeys)"
            ),
            method_span,
        )),
    }
}

/// The supported file methods.
pub(crate) fn lower_file_method(
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
        "flush" => (Flush, ir::Ty::None, false),
        _ => {
            return Err(err(
                format!(
                    "file method '{method}' is not supported yet (supported: \
                     read, readline, readlines, write, close, flush)"
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

/// `str` as a type name (not a shadowed local/global) for `str.maketrans`.
pub(crate) fn is_str_type_name(base: &ast::Expr, ctx: &FnCtx) -> bool {
    is_builtin_type_name(base, ctx, "str")
}

/// `dict` as a type name for `dict.fromkeys`.
pub(crate) fn is_dict_type_name(base: &ast::Expr, ctx: &FnCtx) -> bool {
    is_builtin_type_name(base, ctx, "dict")
}

pub(crate) fn is_builtin_type_name(base: &ast::Expr, ctx: &FnCtx, name: &str) -> bool {
    match &base.kind {
        ast::ExprKind::Name(n) if n == name => {
            !ctx.locals.contains_key(n)
                && !ctx.globals.contains_key(n)
                && !ctx.cell_locals.contains_key(n)
        }
        _ => false,
    }
}

/// `dict.fromkeys(iterable[, value])` — classmethod; `self` is ignored.
pub(crate) fn lower_dict_fromkeys(
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.is_empty() {
        return Err(err(
            "fromkeys expected at least 1 argument, got 0",
            method_span,
        ));
    }
    if args.len() > 2 {
        return Err(err(
            format!("fromkeys expected at most 2 arguments, got {}", args.len()),
            method_span,
        ));
    }
    let keys_ir = lower_expr(&args[0], ctx)?;
    let keys_list = match keys_ir.ty {
        ir::Ty::List(elem) => {
            check_hashable_key(*elem, args[0].span, "fromkeys")?;
            keys_ir
        }
        ir::Ty::Set(elem) => {
            check_hashable_key(*elem, args[0].span, "fromkeys")?;
            ir::Expr {
                ty: ir::list_of(*elem),
                kind: ir::ExprKind::SetToList(Box::new(keys_ir)),
            }
        }
        ir::Ty::Str => ir::Expr {
            ty: ir::list_of(ir::Ty::Str),
            kind: ir::ExprKind::ListFromStr(Box::new(keys_ir)),
        },
        other => {
            return Err(err(
                format!(
                    "fromkeys() iterable must be list/set of int or str, or a str, found {other}"
                ),
                args[0].span,
            ));
        }
    };
    let ir::Ty::List(key) = keys_list.ty else {
        unreachable!("fromkeys keys lowered to list");
    };
    let value = if args.len() == 2 {
        let v = lower_expr(&args[1], ctx)?;
        if v.ty == ir::Ty::File || v.ty == ir::Ty::Exception {
            return Err(err(
                format!("fromkeys() value type {} is not supported", v.ty),
                args[1].span,
            ));
        }
        v
    } else {
        const_none()
    };
    let value_ty = value.ty;
    Ok(ir::Expr {
        ty: ir::dict_of(*key, value_ty),
        kind: ir::ExprKind::DictFromKeys {
            keys: Box::new(keys_list),
            value: Box::new(value),
            key: Box::new(*key),
            value_ty: Box::new(value_ty),
        },
    })
}

pub(crate) fn expect_str_arg(e: ir::Expr, span: Span, what: &str) -> SResult<ir::Expr> {
    if e.ty != ir::Ty::Str {
        return Err(err(format!("{what} must be a str, found {}", e.ty), span));
    }
    Ok(e)
}

/// `str.maketrans(x, y[, z])` or `str.maketrans(dict)` pass-through.
pub(crate) fn lower_str_maketrans(
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    match args.len() {
        1 => {
            let table = lower_expr(&args[0], ctx)?;
            match table.ty {
                ir::Ty::Dict { key, value }
                    if *key == ir::Ty::Int
                        && (*value == ir::Ty::Int || *value == ir::optional_of(ir::Ty::Int)) =>
                {
                    Ok(table)
                }
                other => Err(err(
                    format!(
                        "if you give only one argument to maketrans it must be a dict \
                         [int, int] or dict[int, int | None], found {other}"
                    ),
                    args[0].span,
                )),
            }
        }
        2 => {
            let x = expect_str_arg(
                lower_expr(&args[0], ctx)?,
                args[0].span,
                "maketrans() arg 1",
            )?;
            let y = expect_str_arg(
                lower_expr(&args[1], ctx)?,
                args[1].span,
                "maketrans() arg 2",
            )?;
            // "equal length" is CPython's character count, not a byte count
            if let (ir::ExprKind::ConstStr(a), ir::ExprKind::ConstStr(b)) = (&x.kind, &y.kind)
                && a.chars().count() != b.chars().count()
            {
                return Err(err(
                    "the first two maketrans arguments must have equal length",
                    args[1].span,
                ));
            }
            Ok(ir::Expr {
                ty: ir::dict_of(ir::Ty::Int, ir::Ty::Int),
                kind: ir::ExprKind::StrCall {
                    func: ir::StrFn::MakeTrans,
                    args: vec![x, y],
                },
            })
        }
        3 => {
            let x = expect_str_arg(
                lower_expr(&args[0], ctx)?,
                args[0].span,
                "maketrans() arg 1",
            )?;
            let y = expect_str_arg(
                lower_expr(&args[1], ctx)?,
                args[1].span,
                "maketrans() arg 2",
            )?;
            let z = expect_str_arg(
                lower_expr(&args[2], ctx)?,
                args[2].span,
                "maketrans() arg 3",
            )?;
            // "equal length" is CPython's character count, not a byte count
            if let (ir::ExprKind::ConstStr(a), ir::ExprKind::ConstStr(b)) = (&x.kind, &y.kind)
                && a.chars().count() != b.chars().count()
            {
                return Err(err(
                    "the first two maketrans arguments must have equal length",
                    args[1].span,
                ));
            }
            Ok(ir::Expr {
                ty: ir::dict_of(ir::Ty::Int, ir::optional_of(ir::Ty::Int)),
                kind: ir::ExprKind::StrCall {
                    func: ir::StrFn::MakeTransDelete,
                    args: vec![x, y, z],
                },
            })
        }
        n => Err(err(
            format!("maketrans expected 1 to 3 arguments, got {n}"),
            method_span,
        )),
    }
}

pub(crate) fn lower_str_translate(
    base_ir: ir::Expr,
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.len() != 1 {
        return Err(err(
            format!(
                "translate() takes exactly one argument ({} given)",
                args.len()
            ),
            method_span,
        ));
    }
    let table = lower_expr(&args[0], ctx)?;
    match table.ty {
        ir::Ty::Dict { key, value }
            if *key == ir::Ty::Int
                && (*value == ir::Ty::Int || *value == ir::optional_of(ir::Ty::Int)) =>
        {
            Ok(ir::Expr {
                ty: ir::Ty::Str,
                kind: ir::ExprKind::StrCall {
                    func: ir::StrFn::Translate,
                    args: vec![base_ir, table],
                },
            })
        }
        other => Err(err(
            format!(
                "translate() table must be dict[int, int] or dict[int, int | None], found {other}"
            ),
            args[0].span,
        )),
    }
}

/// The supported `str` methods (ASCII case/whitespace rules).
pub(crate) fn lower_str_method(
    base_ir: ir::Expr,
    method: &str,
    method_span: Span,
    args: &[ast::Expr],
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    use ir::StrFn::*;

    // `.format()` on a literal is handled before lowering; reaching here means
    // the format string is a runtime value.
    if method == "format" {
        return Err(err(
            "format() needs a literal format string, because the fields are \
             resolved at compile time; use an f-string, or inline the format \
             string",
            method_span,
        ));
    }

    // (runtime function, result type, extra str args expected)
    let (func, ret, str_args): (ir::StrFn, ir::Ty, usize) = match method {
        "upper" => (Upper, ir::Ty::Str, 0),
        "lower" => (Lower, ir::Ty::Str, 0),
        "casefold" => (CaseFold, ir::Ty::Str, 0),
        "translate" => {
            return lower_str_translate(base_ir, args, method_span, ctx);
        }
        "maketrans" => {
            return lower_str_maketrans(args, method_span, ctx);
        }
        "capitalize" => (Capitalize, ir::Ty::Str, 0),
        "title" => (Title, ir::Ty::Str, 0),
        "swapcase" => (SwapCase, ir::Ty::Str, 0),
        "zfill" => {
            return lower_str_zfill(base_ir, args, method_span, ctx);
        }
        "center" => {
            return lower_str_pad("center", ir::StrFn::Center, base_ir, args, method_span, ctx);
        }
        "ljust" => {
            return lower_str_pad("ljust", ir::StrFn::LJust, base_ir, args, method_span, ctx);
        }
        "rjust" => {
            return lower_str_pad("rjust", ir::StrFn::RJust, base_ir, args, method_span, ctx);
        }
        "expandtabs" => {
            return lower_str_expandtabs(base_ir, args, method_span, ctx);
        }
        "strip" => {
            return lower_str_strip_family(
                "strip",
                Strip,
                StripChars,
                base_ir,
                args,
                method_span,
                ctx,
            );
        }
        "lstrip" => {
            return lower_str_strip_family(
                "lstrip",
                Lstrip,
                LstripChars,
                base_ir,
                args,
                method_span,
                ctx,
            );
        }
        "rstrip" => {
            return lower_str_strip_family(
                "rstrip",
                Rstrip,
                RstripChars,
                base_ir,
                args,
                method_span,
                ctx,
            );
        }
        "startswith" => {
            return lower_str_affix_family("startswith", false, base_ir, args, method_span, ctx);
        }
        "endswith" => {
            return lower_str_affix_family("endswith", true, base_ir, args, method_span, ctx);
        }
        "find" => {
            return lower_str_find_family("find", Find, base_ir, args, method_span, ctx);
        }
        "index" => {
            return lower_str_find_family("index", Index, base_ir, args, method_span, ctx);
        }
        "rfind" => {
            return lower_str_find_family("rfind", RFind, base_ir, args, method_span, ctx);
        }
        "rindex" => {
            return lower_str_find_family("rindex", RIndex, base_ir, args, method_span, ctx);
        }
        "count" => {
            return lower_str_find_family("count", Count, base_ir, args, method_span, ctx);
        }
        "replace" => {
            return lower_str_replace(base_ir, args, method_span, ctx);
        }
        "split" => {
            return lower_str_split_family("split", false, base_ir, args, method_span, ctx);
        }
        "rsplit" => {
            return lower_str_split_family("rsplit", true, base_ir, args, method_span, ctx);
        }
        "splitlines" => {
            return lower_str_splitlines(base_ir, args, method_span, ctx);
        }
        "join" => {
            if args.len() != 1 {
                return Err(err(
                    format!("join() takes exactly one argument ({} given)", args.len()),
                    method_span,
                ));
            }
            let parts = materialize_iterable_arg(&args[0], ctx)?;
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
        "isalnum" => (IsAlnum, ir::Ty::Bool, 0),
        "istitle" => (IsTitle, ir::Ty::Bool, 0),
        "isascii" => (IsAscii, ir::Ty::Bool, 0),
        "isdecimal" => (IsDecimal, ir::Ty::Bool, 0),
        "isnumeric" => (IsNumeric, ir::Ty::Bool, 0),
        "isidentifier" => (IsIdentifier, ir::Ty::Bool, 0),
        "isprintable" => (IsPrintable, ir::Ty::Bool, 0),
        "removeprefix" => (RemovePrefix, ir::Ty::Str, 1),
        "removesuffix" => (RemoveSuffix, ir::Ty::Str, 1),
        "partition" => (
            Partition,
            ir::tuple_of(&[ir::Ty::Str, ir::Ty::Str, ir::Ty::Str]),
            1,
        ),
        "rpartition" => (
            RPartition,
            ir::tuple_of(&[ir::Ty::Str, ir::Ty::Str, ir::Ty::Str]),
            1,
        ),
        _ => {
            return Err(err(
                format!(
                    "str method '{method}' is not supported yet (supported: \
                     upper, lower, casefold, translate, maketrans, capitalize, title, swapcase, zfill, center, ljust, rjust, \
                     strip, lstrip, rstrip (optional chars/None), startswith, \
                     endswith, find, index, rfind, rindex, count, replace, split, \
                     join, isdigit, isalpha, isspace, isupper, islower, \
                     isalnum, istitle, isascii, isdecimal, isnumeric, \
                     isidentifier, isprintable, \
                     removeprefix, removesuffix, partition, rpartition, rsplit, \
                     splitlines, expandtabs)"
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
    if matches!(func, Partition | RPartition)
        && matches!(&call_args[1].kind, ir::ExprKind::ConstStr(c) if c.is_empty())
    {
        return Err(err("empty separator", args[0].span));
    }
    Ok(ir::Expr {
        ty: ret,
        kind: ir::ExprKind::StrCall {
            func,
            args: call_args,
        },
    })
}

/// `s.splitlines([keepends])` — CPython line boundaries; `keepends` is truthy.
pub(crate) fn lower_str_splitlines(
    base_ir: ir::Expr,
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.len() > 1 {
        return Err(err(
            format!(
                "splitlines() takes at most 1 argument ({} given)",
                args.len()
            ),
            method_span,
        ));
    }
    let keepends = if args.is_empty() {
        const_bool_expr(false)
    } else {
        let v = lower_expr(&args[0], ctx)?;
        to_bool(v, args[0].span, ctx)?
    };
    Ok(ir::Expr {
        ty: ir::list_of(ir::Ty::Str),
        kind: ir::ExprKind::StrCall {
            func: ir::StrFn::SplitLines,
            args: vec![base_ir, keepends],
        },
    })
}

pub(crate) fn as_str_width(arg: &ast::Expr, ctx: &mut FnCtx) -> SResult<ir::Expr> {
    let w = lower_expr(arg, ctx)?;
    match w.ty {
        ir::Ty::Bool => Ok(ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::BoolToInt(Box::new(w)),
        }),
        ir::Ty::Int => Ok(w),
        other => Err(err(
            format!("'{other}' object cannot be interpreted as an integer"),
            arg.span,
        )),
    }
}

pub(crate) fn as_fillchar(arg: &ast::Expr, ctx: &mut FnCtx) -> SResult<ir::Expr> {
    let f = lower_expr(arg, ctx)?;
    if f.ty != ir::Ty::Str {
        return Err(err(
            format!(
                "The fill character must be a unicode character, not {}",
                f.ty
            ),
            arg.span,
        ));
    }
    // "one character" is one code point, which may be several UTF-8 bytes
    if let ir::ExprKind::ConstStr(c) = &f.kind
        && c.chars().count() != 1
    {
        return Err(err(
            "The fill character must be exactly one character long",
            arg.span,
        ));
    }
    Ok(f)
}

/// `s.strip([chars])` / `lstrip` / `rstrip` — omitted or `None` is whitespace.
pub(crate) fn lower_str_strip_family(
    name: &str,
    ws: ir::StrFn,
    chars_fn: ir::StrFn,
    base_ir: ir::Expr,
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.len() > 1 {
        return Err(err(
            format!("{name}() takes at most 1 argument ({} given)", args.len()),
            method_span,
        ));
    }
    if args.is_empty() {
        return Ok(ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::StrCall {
                func: ws,
                args: vec![base_ir],
            },
        });
    }
    let chars = lower_expr(&args[0], ctx)?;
    match chars.ty {
        ir::Ty::None => Ok(ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::StrCall {
                func: ws,
                args: vec![base_ir],
            },
        }),
        ir::Ty::Str => Ok(ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::StrCall {
                func: chars_fn,
                args: vec![base_ir, chars],
            },
        }),
        other => Err(err(
            format!("{name} arg must be None or str, found {other}"),
            args[0].span,
        )),
    }
}

pub(crate) fn lower_str_expandtabs(
    base_ir: ir::Expr,
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.len() > 1 {
        return Err(err(
            format!(
                "expandtabs() takes at most 1 argument ({} given)",
                args.len()
            ),
            method_span,
        ));
    }
    let tabsize = if args.is_empty() {
        int_const(8)
    } else {
        as_str_width(&args[0], ctx)?
    };
    Ok(ir::Expr {
        ty: ir::Ty::Str,
        kind: ir::ExprKind::StrCall {
            func: ir::StrFn::ExpandTabs,
            args: vec![base_ir, tabsize],
        },
    })
}

pub(crate) fn lower_str_zfill(
    base_ir: ir::Expr,
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.len() != 1 {
        return Err(err(
            format!("zfill() takes exactly one argument ({} given)", args.len()),
            method_span,
        ));
    }
    let width = as_str_width(&args[0], ctx)?;
    Ok(ir::Expr {
        ty: ir::Ty::Str,
        kind: ir::ExprKind::StrCall {
            func: ir::StrFn::ZFill,
            args: vec![base_ir, width],
        },
    })
}

pub(crate) fn lower_str_pad(
    name: &str,
    func: ir::StrFn,
    base_ir: ir::Expr,
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.is_empty() || args.len() > 2 {
        return Err(err(
            format!(
                "{name}() takes from 1 to 2 positional arguments but {} were given",
                args.len()
            ),
            method_span,
        ));
    }
    let width = as_str_width(&args[0], ctx)?;
    let fill = if args.len() == 2 {
        as_fillchar(&args[1], ctx)?
    } else {
        const_str_expr(" ")
    };
    Ok(ir::Expr {
        ty: ir::Ty::Str,
        kind: ir::ExprKind::StrCall {
            func,
            args: vec![base_ir, width, fill],
        },
    })
}

/// `s.replace(old, new[, count])` — `count < 0` is unlimited (CPython).
pub(crate) fn lower_str_replace(
    base_ir: ir::Expr,
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.len() < 2 {
        return Err(err(
            format!("replace expected at least 2 arguments, got {}", args.len()),
            method_span,
        ));
    }
    if args.len() > 3 {
        return Err(err(
            format!("replace expected at most 3 arguments, got {}", args.len()),
            method_span,
        ));
    }
    let old = lower_expr(&args[0], ctx)?;
    let new = lower_expr(&args[1], ctx)?;
    if old.ty != ir::Ty::Str {
        return Err(err(
            format!("replace() argument 1 must be str, not {}", old.ty),
            args[0].span,
        ));
    }
    if new.ty != ir::Ty::Str {
        return Err(err(
            format!("replace() argument 2 must be str, not {}", new.ty),
            args[1].span,
        ));
    }
    let count = if args.len() == 3 {
        let c = lower_expr(&args[2], ctx)?;
        match c.ty {
            ir::Ty::Bool => ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::BoolToInt(Box::new(c)),
            },
            ir::Ty::Int => c,
            other => {
                return Err(err(
                    format!("'{other}' object cannot be interpreted as an integer"),
                    args[2].span,
                ));
            }
        }
    } else {
        int_const(-1)
    };
    Ok(ir::Expr {
        ty: ir::Ty::Str,
        kind: ir::ExprKind::StrCall {
            func: ir::StrFn::Replace,
            args: vec![base_ir, old, new, count],
        },
    })
}

/// `s.startswith/endswith(affix[, start[, end]])` — affix is str or tuple of str.
pub(crate) fn lower_str_affix_family(
    name: &str,
    from_end: bool,
    base_ir: ir::Expr,
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.is_empty() || args.len() > 3 {
        return Err(err(
            format!(
                "{name}() takes from 1 to 3 positional arguments but {} were given",
                args.len()
            ),
            method_span,
        ));
    }
    let affix = lower_expr(&args[0], ctx)?;
    let func = match affix.ty {
        ir::Ty::Str => {
            if from_end {
                ir::StrFn::EndsWith
            } else {
                ir::StrFn::StartsWith
            }
        }
        ir::Ty::Tuple(elems) if elems.iter().all(|e| *e == ir::Ty::Str) => {
            if from_end {
                ir::StrFn::EndsWithTuple
            } else {
                ir::StrFn::StartsWithTuple
            }
        }
        other => {
            return Err(err(
                format!("{name} first arg must be str or a tuple of str, not {other}"),
                args[0].span,
            ));
        }
    };
    let start = if args.len() >= 2 {
        as_str_slice_bound(&args[1], ctx, 0)?
    } else {
        int_const(0)
    };
    let end = if args.len() >= 3 {
        as_str_slice_bound(&args[2], ctx, i64::MIN)?
    } else {
        int_const(i64::MIN)
    };
    Ok(ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::StrCall {
            func,
            args: vec![base_ir, affix, start, end],
        },
    })
}

/// `s.find/index/rfind/rindex/count(sub[, start[, end]])` — CPython slice bounds.
/// Missing `end` is `i64::MIN` (codegen/runtime treat it as `len(s)`).
pub(crate) fn lower_str_find_family(
    name: &str,
    func: ir::StrFn,
    base_ir: ir::Expr,
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.is_empty() || args.len() > 3 {
        return Err(err(
            format!(
                "{name}() takes from 1 to 3 positional arguments but {} were given",
                args.len()
            ),
            method_span,
        ));
    }
    let needle = lower_expr(&args[0], ctx)?;
    if needle.ty != ir::Ty::Str {
        return Err(err(
            format!("{name}() argument 1 must be str, not {}", needle.ty),
            args[0].span,
        ));
    }
    let start = if args.len() >= 2 {
        as_str_slice_bound(&args[1], ctx, 0)?
    } else {
        int_const(0)
    };
    let end = if args.len() >= 3 {
        as_str_slice_bound(&args[2], ctx, i64::MIN)?
    } else {
        int_const(i64::MIN)
    };
    Ok(ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::StrCall {
            func,
            args: vec![base_ir, needle, start, end],
        },
    })
}

pub(crate) fn as_str_slice_bound(
    arg: &ast::Expr,
    ctx: &mut FnCtx,
    none_as: i64,
) -> SResult<ir::Expr> {
    let v = lower_expr(arg, ctx)?;
    match v.ty {
        ir::Ty::None => Ok(int_const(none_as)),
        ir::Ty::Bool => Ok(ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::BoolToInt(Box::new(v)),
        }),
        ir::Ty::Int => Ok(v),
        other => Err(err(
            format!("'{other}' object cannot be interpreted as an integer"),
            arg.span,
        )),
    }
}

pub(crate) fn lower_str_split_family(
    name: &str,
    from_right: bool,
    base_ir: ir::Expr,
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.len() > 2 {
        return Err(err(
            format!("{name}() takes at most 2 arguments ({} given)", args.len()),
            method_span,
        ));
    }
    let mut sep: Option<ir::Expr> = None;
    if !args.is_empty() {
        let s = lower_expr(&args[0], ctx)?;
        match s.ty {
            ir::Ty::None => {}
            ir::Ty::Str => {
                if matches!(&s.kind, ir::ExprKind::ConstStr(c) if c.is_empty()) {
                    return Err(err("empty separator", args[0].span));
                }
                sep = Some(s);
            }
            other => {
                return Err(err(
                    format!("{name}() separator must be a str or None, found {other}"),
                    args[0].span,
                ));
            }
        }
    }
    let maxsplit = if args.len() == 2 {
        let m = lower_expr(&args[1], ctx)?;
        match m.ty {
            ir::Ty::Bool => ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::BoolToInt(Box::new(m)),
            },
            ir::Ty::Int => m,
            other => {
                return Err(err(
                    format!("'{other}' object cannot be interpreted as an integer"),
                    args[1].span,
                ));
            }
        }
    } else {
        int_const(-1)
    };
    let func = match (from_right, sep.is_some()) {
        (false, false) => ir::StrFn::SplitWs,
        (false, true) => ir::StrFn::Split,
        (true, false) => ir::StrFn::RSplitWs,
        (true, true) => ir::StrFn::RSplit,
    };
    let mut call_args = vec![base_ir];
    if let Some(s) = sep {
        call_args.push(s);
    }
    call_args.push(maxsplit);
    Ok(ir::Expr {
        ty: ir::list_of(ir::Ty::Str),
        kind: ir::ExprKind::StrCall {
            func,
            args: call_args,
        },
    })
}

pub(crate) fn lower_list_pop(
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

/// `a < b` is lowerable when the class defines `__lt__` or the reflected `__gt__`.
pub(crate) fn class_supports_lt(ty: ir::Ty) -> bool {
    class_has_method(ty, "__lt__") || class_has_method(ty, "__gt__")
}

pub(crate) fn class_has_method(ty: ir::Ty, method: &str) -> bool {
    match ty {
        ir::Ty::Class(id) => resolve_method(id, method).is_some(),
        _ => false,
    }
}

/// Reflected rich-compare slot (`a < b` → `b.__gt__(a)`, `a != b` → `b.__ne__(a)`).
pub(crate) fn class_reflected_method(op: ast::BinOp) -> Option<&'static str> {
    match op {
        ast::BinOp::Eq => Some("__eq__"),
        ast::BinOp::NotEq => Some("__ne__"),
        ast::BinOp::Lt => Some("__gt__"),
        ast::BinOp::LtEq => Some("__ge__"),
        ast::BinOp::Gt => Some("__lt__"),
        ast::BinOp::GtEq => Some("__le__"),
        _ => None,
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ClassComparisonMethod {
    pub(crate) name: &'static str,
    pub(crate) invert: bool,
}

/// `object.__ne__` delegates to equality only when no explicit (possibly
/// inherited) inequality method exists on this receiver.
pub(crate) fn resolve_class_comparison(
    class_id: ir::ClassId,
    method: &'static str,
) -> Option<ClassComparisonMethod> {
    if resolve_method(class_id, method).is_some() {
        Some(ClassComparisonMethod {
            name: method,
            invert: false,
        })
    } else if method == "__ne__" && resolve_method(class_id, "__eq__").is_some() {
        Some(ClassComparisonMethod {
            name: "__eq__",
            invert: true,
        })
    } else {
        None
    }
}

pub(crate) fn lookup_method_sig(
    class_id: ir::ClassId,
    method: &str,
    ctx: &FnCtx,
) -> Option<FuncSig> {
    let name = resolve_method(class_id, method)?;
    ctx.mctx
        .funcs
        .get(&name)
        .cloned()
        .or_else(|| method_sig_lookup(&name))
        .or_else(|| {
            for data in ctx.mctx.mods.values() {
                if let Some(s) = data.funcs.get(&name) {
                    return Some(s.clone());
                }
            }
            None
        })
}

/// Whether `src` can be passed as a method argument of type `dst` (same rules
/// as `coerce`, without building IR).
pub(crate) fn ty_can_pass_as(src: ir::Ty, dst: ir::Ty) -> bool {
    if src == dst {
        return true;
    }
    if dst == ir::Ty::Any {
        return can_box_as_any(src);
    }
    if src == ir::Ty::Any {
        return can_box_as_any(dst);
    }
    match (src, dst) {
        (ir::Ty::Bool, ir::Ty::Int | ir::Ty::Float) => true,
        (ir::Ty::Int, ir::Ty::Float) => true,
        (ir::Ty::Class(a), ir::Ty::Class(b)) if class_is_subclass(a, b) => true,
        _ => {
            matches!(dst, ir::Ty::Union(_))
                && ir::flatten_union_members(dst)
                    .iter()
                    .any(|m| ty_can_pass_as(src, *m))
        }
    }
}

/// A type as a *user* should see it in a diagnostic.
///
/// `ir::Ty`'s `Display` prints a class as `class#0`, an internal id that means
/// nothing to the reader of an error message. Everything else already prints
/// the way Python spells it.
pub(crate) fn display_ty(ty: ir::Ty) -> String {
    match ty {
        ir::Ty::Class(id) => class_info(id).map_or_else(|| ty.to_string(), |c| c.name),
        _ => ty.to_string(),
    }
}

/// The dunder an arithmetic/bitwise operator dispatches to on a class.
///
/// Separate from [`class_reflected_method`] because the two families reflect
/// differently: a comparison swaps the *operator* (`a < b` → `b.__gt__(a)`),
/// while arithmetic swaps the *name* (`a + b` → `b.__radd__(a)`).
pub(crate) fn class_arith_method(op: ast::BinOp) -> Option<&'static str> {
    Some(match op {
        ast::BinOp::Add => "__add__",
        ast::BinOp::Sub => "__sub__",
        ast::BinOp::Mul => "__mul__",
        ast::BinOp::MatMul => "__matmul__",
        ast::BinOp::Div => "__truediv__",
        ast::BinOp::FloorDiv => "__floordiv__",
        ast::BinOp::Mod => "__mod__",
        ast::BinOp::Pow => "__pow__",
        ast::BinOp::BitAnd => "__and__",
        ast::BinOp::BitOr => "__or__",
        ast::BinOp::BitXor => "__xor__",
        ast::BinOp::LShift => "__lshift__",
        ast::BinOp::RShift => "__rshift__",
        _ => return Option::None,
    })
}

/// The reflected form, tried on the right operand: `a + b` → `b.__radd__(a)`.
pub(crate) fn class_reflected_arith_method(op: ast::BinOp) -> Option<&'static str> {
    Some(match op {
        ast::BinOp::Add => "__radd__",
        ast::BinOp::Sub => "__rsub__",
        ast::BinOp::Mul => "__rmul__",
        ast::BinOp::MatMul => "__rmatmul__",
        ast::BinOp::Div => "__rtruediv__",
        ast::BinOp::FloorDiv => "__rfloordiv__",
        ast::BinOp::Mod => "__rmod__",
        ast::BinOp::Pow => "__rpow__",
        ast::BinOp::BitAnd => "__rand__",
        ast::BinOp::BitOr => "__ror__",
        ast::BinOp::BitXor => "__rxor__",
        ast::BinOp::LShift => "__rlshift__",
        ast::BinOp::RShift => "__rrshift__",
        _ => return Option::None,
    })
}

/// The in-place form. CPython falls back to the plain form when it is absent,
/// which is why `v += w` works on a class defining only `__add__`.
pub(crate) fn class_inplace_arith_method(op: ast::BinOp) -> Option<&'static str> {
    Some(match op {
        ast::BinOp::Add => "__iadd__",
        ast::BinOp::Sub => "__isub__",
        ast::BinOp::Mul => "__imul__",
        ast::BinOp::MatMul => "__imatmul__",
        ast::BinOp::Div => "__itruediv__",
        ast::BinOp::FloorDiv => "__ifloordiv__",
        ast::BinOp::Mod => "__imod__",
        ast::BinOp::Pow => "__ipow__",
        ast::BinOp::BitAnd => "__iand__",
        ast::BinOp::BitOr => "__ior__",
        ast::BinOp::BitXor => "__ixor__",
        ast::BinOp::LShift => "__ilshift__",
        ast::BinOp::RShift => "__irshift__",
        _ => return Option::None,
    })
}

/// The dunder a unary operator dispatches to on a class.
pub(crate) fn class_unary_method(op: ast::UnaryOp) -> Option<&'static str> {
    Some(match op {
        ast::UnaryOp::Neg => "__neg__",
        ast::UnaryOp::Pos => "__pos__",
        ast::UnaryOp::Invert => "__invert__",
        ast::UnaryOp::Not => return Option::None,
    })
}

/// Whether a dunder on `class_id` accepts `arg_ty` as its argument.
///
/// Used to decide whether a slot is *usable* before committing to it, so that
/// `2.0 * vec` can fall through `float`'s absent `__mul__` to `vec.__rmul__`
/// rather than reporting a mismatch inside the call.
pub(crate) fn class_equality_accepts(
    class_id: ir::ClassId,
    method: &str,
    arg_ty: ir::Ty,
    ctx: &FnCtx,
) -> bool {
    match lookup_method_sig(class_id, method, ctx) {
        Some(sig) => match sig.params.get(1) {
            Some(p) => ty_can_pass_as(arg_ty, p.ty),
            None => sig.vararg.is_some(),
        },
        None => true,
    }
}

pub(crate) fn class_reflected_usable(
    op: ast::BinOp,
    class_id: ir::ClassId,
    arg_ty: ir::Ty,
    ctx: &FnCtx,
) -> bool {
    let Some(refl) = class_reflected_method(op) else {
        return false;
    };
    let Some(method) = resolve_class_comparison(class_id, refl) else {
        return false;
    };
    if matches!(op, ast::BinOp::Eq | ast::BinOp::NotEq) {
        class_equality_accepts(class_id, method.name, arg_ty, ctx)
    } else {
        true
    }
}

pub(crate) fn ensure_sortable_list_elem(elem: ir::Ty, span: Span) -> SResult<()> {
    match elem {
        ir::Ty::Int | ir::Ty::Float | ir::Ty::Bool | ir::Ty::Str => Ok(()),
        ir::Ty::Tuple(_) | ir::Ty::List(_) if is_orderable_ty(elem) => Ok(()),
        ir::Ty::Class(_) if class_supports_lt(elem) => Ok(()),
        other => Err(err(
            format!(
                "sort is only supported for list[int], list[float], list[bool], \
                 list[str], lists of orderable tuples/lists, and lists of classes \
                 that define __lt__ or __gt__, found list[{other}]"
            ),
            span,
        )),
    }
}

/// In-place `list.sort` / `sorted` without `key=`: primitive `ListSort`, or a
/// stable insertion sort that calls virtual `__lt__` on class elements.
pub(crate) fn push_plain_list_sort(
    stmts: &mut Vec<ir::Stmt>,
    list: ir::Expr,
    elem: ir::Ty,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<()> {
    ensure_sortable_list_elem(elem, span)?;
    if class_supports_lt(elem) {
        stmts.extend(lower_list_sort_class_stmts(list, elem, span, ctx)?);
    } else {
        stmts.push(ir::Stmt::ListSort { list });
    }
    Ok(())
}

/// Stable insertion sort of class instances using only `__lt__` (`cur < prev`).
pub(crate) fn lower_list_sort_class_stmts(
    list: ir::Expr,
    elem: ir::Ty,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<Vec<ir::Stmt>> {
    let n_t = ctx.fresh_temp("csort.n", ir::Ty::Int);
    let i_t = ctx.fresh_temp("csort.i", ir::Ty::Int);
    let j_t = ctx.fresh_temp("csort.j", ir::Ty::Int);
    let cur_t = ctx.fresh_temp("csort.cur", elem);

    let n = local_expr(n_t.clone(), ir::Ty::Int);
    let i = local_expr(i_t.clone(), ir::Ty::Int);
    let j = local_expr(j_t.clone(), ir::Ty::Int);
    let cur = local_expr(cur_t.clone(), elem);

    let mut stmts = vec![
        ir::Stmt::Assign {
            name: n_t,
            value: ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Len(Box::new(list.clone())),
            },
        },
        ir::Stmt::Assign {
            name: i_t.clone(),
            value: int_const(1),
        },
    ];

    let sort_cond = key_cmp(ir::BinOp::Lt, i.clone(), n);
    let cur_load = ir::Expr {
        ty: elem,
        kind: ir::ExprKind::Index {
            base: Box::new(list.clone()),
            index: Box::new(i.clone()),
        },
    };
    let j_gt0 = key_cmp(ir::BinOp::Gt, j.clone(), int_const(0));
    let j_m1 = ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::Binary {
            op: ir::BinOp::Sub,
            left: Box::new(j.clone()),
            right: Box::new(int_const(1)),
        },
    };
    let prev = ir::Expr {
        ty: elem,
        kind: ir::ExprKind::Index {
            base: Box::new(list.clone()),
            index: Box::new(j_m1.clone()),
        },
    };
    let lt = lower_class_compare(ast::BinOp::Lt, cur.clone(), prev.clone(), span, ctx)?;
    let shift_cond = bool_and(j_gt0, lt);
    let shift_body = vec![
        ir::Stmt::IndexAssign {
            base: list.clone(),
            index: j.clone(),
            value: prev,
        },
        ir::Stmt::Assign {
            name: j_t.clone(),
            value: j_m1,
        },
    ];
    let outer_body = vec![
        ir::Stmt::Assign {
            name: cur_t,
            value: cur_load,
        },
        ir::Stmt::Assign {
            name: j_t,
            value: i.clone(),
        },
        ir::Stmt::While {
            cond: shift_cond,
            body: shift_body,
            step: vec![],
        },
        ir::Stmt::IndexAssign {
            base: list,
            index: j,
            value: cur,
        },
        ir::Stmt::Assign {
            name: i_t,
            value: ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Add,
                    left: Box::new(i),
                    right: Box::new(int_const(1)),
                },
            },
        },
    ];
    stmts.push(ir::Stmt::While {
        cond: sort_cond,
        body: outer_body,
        step: vec![],
    });
    Ok(stmts)
}

pub(crate) fn as_seq_index_bound(arg: &ast::Expr, ctx: &mut FnCtx) -> SResult<ir::Expr> {
    let v = lower_expr(arg, ctx)?;
    match v.ty {
        ir::Ty::Bool => Ok(ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::BoolToInt(Box::new(v)),
        }),
        ir::Ty::Int => Ok(v),
        _ => Err(err(
            "slice indices must be integers or have an __index__ method",
            arg.span,
        )),
    }
}

pub(crate) fn lower_seq_index_bounds(
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<(ir::Expr, ir::Expr)> {
    if args.is_empty() {
        return Err(err(
            "index expected at least 1 argument, got 0",
            method_span,
        ));
    }
    if args.len() > 3 {
        return Err(err(
            format!("index expected at most 3 arguments, got {}", args.len()),
            method_span,
        ));
    }
    let start = if args.len() >= 2 {
        as_seq_index_bound(&args[1], ctx)?
    } else {
        int_const(0)
    };
    let end = if args.len() >= 3 {
        as_seq_index_bound(&args[2], ctx)?
    } else {
        int_const(i64::MIN)
    };
    Ok((start, end))
}

pub(crate) fn lower_list_index_of(
    list: ir::Expr,
    elem: ir::Ty,
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.is_empty() {
        return Err(err(
            "index expected at least 1 argument, got 0",
            method_span,
        ));
    }
    let value = lower_expr(&args[0], ctx)?;
    let value = coerce(value, elem, args[0].span, "index() argument")?;
    let (start, end) = lower_seq_index_bounds(args, method_span, ctx)?;
    if ty_uses_class_eq(elem) {
        return lower_list_index_protocol(list, value, elem, start, end, method_span, ctx);
    }
    Ok(ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::ListIndexOf {
            list: Box::new(list),
            value: Box::new(value),
            start: Box::new(start),
            end: Box::new(end),
        },
    })
}

pub(crate) fn lower_list_count(
    list: ir::Expr,
    elem: ir::Ty,
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.len() != 1 {
        return Err(err(
            format!("count() takes exactly one argument ({} given)", args.len()),
            method_span,
        ));
    }
    let value = lower_expr(&args[0], ctx)?;
    let value = coerce(value, elem, args[0].span, "count() argument")?;
    if ty_uses_class_eq(elem) {
        return lower_list_count_protocol(list, value, elem, method_span, ctx);
    }
    Ok(ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::ListCount {
            list: Box::new(list),
            value: Box::new(value),
        },
    })
}

/// Needle for tuple `in` / `index` / `count`: coerce when homogeneous;
/// otherwise keep as-is if comparable to any element (runtime matches tags).
pub(crate) fn lower_tuple_search_needle(
    needle: ir::Expr,
    elems: &[ir::Ty],
    span: Span,
    what: &str,
) -> SResult<ir::Expr> {
    let mut uniq: Vec<ir::Ty> = Vec::new();
    for e in elems {
        if !uniq.iter().any(|u| u == e) {
            uniq.push(*e);
        }
    }
    if uniq.len() == 1 {
        return coerce(needle, uniq[0], span, what);
    }
    let ok = uniq.iter().any(|e| {
        needle.ty == *e
            || matches!(
                (needle.ty, *e),
                (ir::Ty::Bool, ir::Ty::Int)
                    | (ir::Ty::Int, ir::Ty::Float)
                    | (ir::Ty::Bool, ir::Ty::Float)
            )
    });
    if !ok && !uniq.is_empty() {
        return Err(err(
            format!(
                "{what} type {} is not compatible with any element type",
                needle.ty
            ),
            span,
        ));
    }
    Ok(needle)
}

pub(crate) fn lower_tuple_index_of(
    tuple: ir::Expr,
    elems: &[ir::Ty],
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.is_empty() {
        return Err(err(
            "index expected at least 1 argument, got 0",
            method_span,
        ));
    }
    let value = lower_expr(&args[0], ctx)?;
    let value = lower_tuple_search_needle(value, elems, args[0].span, "index() argument")?;
    let (start, end) = lower_seq_index_bounds(args, method_span, ctx)?;
    if let Some(elem) = homogeneous_class_tuple_elem(elems) {
        return lower_list_index_protocol(tuple, value, elem, start, end, method_span, ctx);
    }
    Ok(ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::TupleIndexOf {
            tuple: Box::new(tuple),
            value: Box::new(value),
            start: Box::new(start),
            end: Box::new(end),
        },
    })
}

pub(crate) fn lower_tuple_count(
    tuple: ir::Expr,
    elems: &[ir::Ty],
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.len() != 1 {
        return Err(err(
            format!("count() takes exactly one argument ({} given)", args.len()),
            method_span,
        ));
    }
    let value = lower_expr(&args[0], ctx)?;
    let value = lower_tuple_search_needle(value, elems, args[0].span, "count() argument")?;
    if let Some(elem) = homogeneous_class_tuple_elem(elems) {
        return lower_list_count_protocol(tuple, value, elem, method_span, ctx);
    }
    Ok(ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::TupleCount {
            tuple: Box::new(tuple),
            value: Box::new(value),
        },
    })
}

/// The static type an expression has, for the shapes that need no lowering:
/// a name, and attribute/index chains over one.
///
/// Deliberately partial. It exists to supply an *expected type* to a
/// container literal on the right of an assignment, and returning `None` only
/// costs the hint — so anything with a call, an operator or a comprehension in
/// it declines rather than guessing. Lowering the base twice to learn its type
/// would duplicate side effects.
pub(crate) fn probe_expr_ty(e: &ast::Expr, ctx: &FnCtx) -> Option<ir::Ty> {
    match &e.kind {
        ast::ExprKind::Name(n) => ctx
            .cell_locals
            .get(n)
            .copied()
            .or_else(|| ctx.locals.get(n).copied())
            .or_else(|| ctx.globals.get(n).copied()),
        ast::ExprKind::Attribute { base, attr, .. } => {
            let ir::Ty::Class(id) = probe_expr_ty(base, ctx)? else {
                return Option::None;
            };
            class_field_ty(id, attr)
        }
        ast::ExprKind::Index { base, .. } => match probe_expr_ty(base, ctx)? {
            ir::Ty::List(elem) => Some(*elem),
            ir::Ty::Dict { value, .. } => Some(*value),
            _ => Option::None,
        },
        _ => Option::None,
    }
}

pub(crate) fn class_field_ty(id: ir::ClassId, attr: &str) -> Option<ir::Ty> {
    class_info(id)?
        .fields
        .iter()
        .find(|(n, _)| n == attr)
        .map(|(_, t)| *t)
}

/// The declared type of the slot a non-`Name` assignment target writes into.
///
/// Unlike the multi-assign storage hint this is *exact* — a container's
/// element type or a class field's type, both declared — so it can steer a
/// non-empty literal too. `f.cols["name"] = ["a", "b"]` into a
/// `dict[str, list[Any]]` boxes each element at construction, which is what
/// `xs: list[Any] = ["a", "b"]` has always done at a `Name` target.
pub(crate) fn probe_target_slot_ty(target: &ast::AssignTarget, ctx: &FnCtx) -> Option<ir::Ty> {
    match target {
        ast::AssignTarget::Index { base, .. } => match probe_expr_ty(base, ctx)? {
            ir::Ty::List(elem) => Some(*elem),
            ir::Ty::Dict { value, .. } => Some(*value),
            _ => Option::None,
        },
        ast::AssignTarget::Attr { base, attr, .. } => {
            let ir::Ty::Class(id) = probe_expr_ty(base, ctx)? else {
                return Option::None;
            };
            class_field_ty(id, attr)
        }
        _ => Option::None,
    }
}

pub(crate) fn lower_assign(
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
    // The declared slot type of a non-`Name` target. Exact rather than a
    // join guess, so it steers non-empty literals as an annotation does.
    let slot_hint = if ann_ty.is_none() {
        probe_target_slot_ty(target, ctx)
    } else {
        None
    };
    let exact = ann_ty.is_some() || slot_hint.is_some();
    let expected = ann_ty.or(slot_hint).or(storage_hint);
    // Propagate expected types into empty / annotated container literals
    let lowered = match (&value.kind, expected) {
        (ast::ExprKind::ListLit(items), Some(ir::Ty::List(elem))) if exact || items.is_empty() => {
            lower_list_lit(items, Some(*elem), value.span, ctx)?
        }
        (ast::ExprKind::DictLit(items), Some(ir::Ty::Dict { key, value: val }))
            if exact || items.is_empty() =>
        {
            lower_dict_lit(items, Some((*key, *val)), value.span, ctx)?
        }
        (ast::ExprKind::SetLit(items), Some(ir::Ty::Set(elem))) if exact || items.is_empty() => {
            lower_set_lit(items, Some(*elem), value.span, ctx)?
        }
        (ast::ExprKind::TupleLit(items), Some(ir::Ty::Tuple(elems))) if exact => {
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
pub(crate) fn lower_assign_ir(
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
            let base_ir = lower_expr(base, ctx)?;
            if let ir::Ty::Class(id) = base_ir.ty {
                return lower_class_setitem(base_ir, id, index, value_ir, value_span, ctx, out);
            }
            let (base_ir, elem, index_ir) = lower_index_target_on(base_ir, index, ctx)?;
            let value_ir = coerce(value_ir, elem, value_span, "item assignment")?;
            out.push(ir::Stmt::IndexAssign {
                base: base_ir,
                index: index_ir,
                value: value_ir,
            });
            Ok(())
        }
        ast::AssignTarget::Slice {
            base, lo, hi, step, ..
        } => {
            if ann_ty.is_some() {
                return Err(err(
                    "type annotations are only allowed on plain variable names",
                    value_span,
                ));
            }
            let base_ir = lower_expr(base, ctx)?;
            let ir::Ty::List(elem) = base_ir.ty else {
                return Err(err(
                    format!(
                        "slice assignment is only supported on lists, found {}",
                        base_ir.ty
                    ),
                    base.span,
                ));
            };
            let (lo_ir, hi_ir, step_ir) =
                lower_slice_bounds(lo.as_deref(), hi.as_deref(), step.as_deref(), ctx)?;
            let value_ir = match value_ir.ty {
                ir::Ty::List(other) if *other == *elem || *other == ir::Ty::Any => value_ir,
                ir::Ty::List(other) => {
                    return Err(err(
                        format!(
                            "slice assignment element type mismatch: expected list[{elem}], found list[{other}]"
                        ),
                        value_span,
                    ));
                }
                other => {
                    return Err(err(
                        format!("slice assignment currently requires a list, found {other}"),
                        value_span,
                    ));
                }
            };
            out.push(ir::Stmt::ListSliceAssign {
                list: base_ir,
                lo: Box::new(lo_ir),
                hi: Box::new(hi_ir),
                step: Box::new(step_ir),
                value: Box::new(value_ir),
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
            // A class constant is substituted where it is read, so there is
            // no storage to assign to. Checked before lowering the base,
            // which for a bare class name would fail as an undefined name.
            let const_owner = match &base.kind {
                ast::ExprKind::Name(cls) if !ctx.locals.contains_key(cls) => lookup_class(cls),
                _ => ctx
                    .locals
                    .get(match &base.kind {
                        ast::ExprKind::Name(n) => n.as_str(),
                        _ => "",
                    })
                    .and_then(|t| match t {
                        ir::Ty::Class(id) => Some(*id),
                        _ => Option::None,
                    }),
            };
            if let Some(id) = const_owner
                && class_const(id, attr).is_some()
                // An instance field of the same name shadows the constant, as
                // in CPython, and assigning to *that* is fine. Only a name
                // with no field behind it has nothing to assign to.
                && field_index(id, attr).is_none()
            {
                return Err(err(
                    format!(
                        "'{attr}' is a class constant and cannot be assigned: it is \
                         substituted where it is used, not stored. Use an instance \
                         field assigned in __init__ if it needs to change"
                    ),
                    *attr_span,
                ));
            }
            // An annotation here declared the field's type during class
            // collection; the value is coerced to that type below, so a
            // disagreeing annotation is reported as a value mismatch.
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
            if resolve_property(class_id, attr).is_some() {
                return Err(err(
                    format!("property '{attr}' is read-only and cannot be assigned"),
                    *attr_span,
                ));
            }
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

pub(crate) fn lower_delete(
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
                ir::Ty::List(_) => {
                    let index_ir = lower_expr(index, ctx)?;
                    let index_ir = coerce(index_ir, ir::Ty::Int, index.span, "list index")?;
                    out.push(ir::Stmt::IndexDelete {
                        base: base_ir,
                        index: index_ir,
                    });
                    Ok(())
                }
                ir::Ty::Class(id) => lower_class_delitem(base_ir, id, index, span, ctx, out),
                other => Err(err(
                    format!(
                        "'del' on '{other}' is not supported yet (only list indices, slices, dict keys, and classes with __delitem__)"
                    ),
                    span,
                )),
            }
        }
        ast::AssignTarget::Slice {
            base, lo, hi, step, ..
        } => {
            let base_ir = lower_expr(base, ctx)?;
            let ir::Ty::List(elem) = base_ir.ty else {
                return Err(err(
                    format!(
                        "'del' slice is only supported on lists, found {}",
                        base_ir.ty
                    ),
                    span,
                ));
            };
            let (lo_ir, hi_ir, step_ir) =
                lower_slice_bounds(lo.as_deref(), hi.as_deref(), step.as_deref(), ctx)?;
            let empty = ir::Expr {
                ty: ir::list_of(*elem),
                kind: ir::ExprKind::ListNew {
                    cap: Box::new(int_const(0)),
                },
            };
            out.push(ir::Stmt::ListSliceAssign {
                list: base_ir,
                lo: Box::new(lo_ir),
                hi: Box::new(hi_ir),
                step: Box::new(step_ir),
                value: Box::new(empty),
            });
            Ok(())
        }
        _ => Err(err(
            "'del' only supports list index/slice and dict item deletion \
             (del xs[i] / del xs[i:j] / del d[key]) for now",
            span,
        )),
    }
}

pub(crate) fn lower_slice_bounds(
    lo: Option<&ast::Expr>,
    hi: Option<&ast::Expr>,
    step: Option<&ast::Expr>,
    ctx: &mut FnCtx,
) -> SResult<(ir::Expr, ir::Expr, ir::Expr)> {
    let lo_ir = match lo {
        Some(e) => {
            let v = lower_expr(e, ctx)?;
            coerce(v, ir::Ty::Int, e.span, "slice bound")?
        }
        None => int_const(i64::MIN),
    };
    let hi_ir = match hi {
        Some(e) => {
            let v = lower_expr(e, ctx)?;
            coerce(v, ir::Ty::Int, e.span, "slice bound")?
        }
        None => int_const(i64::MIN),
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
        None => int_const(1),
    };
    Ok((lo_ir, hi_ir, step_ir))
}

/// Unpack `value` into `targets` (tuple/list RHS). Supports a single `*rest`.
pub(crate) fn lower_unpack(
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

/// `obj[key]` → virtual `__getitem__(key)` when the class defines it.
pub(crate) fn lower_class_getitem(
    base_ir: ir::Expr,
    class_id: ir::ClassId,
    index: &ast::Expr,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if resolve_method(class_id, "__getitem__").is_none() {
        return Err(err(
            format!(
                "'{}' object is not subscriptable (need __getitem__)",
                class_display_name(class_id)
            ),
            span,
        ));
    }
    lower_instance_method_call(
        base_ir,
        class_id,
        "__getitem__",
        span,
        std::slice::from_ref(index),
        ctx,
    )
}

/// `obj[key] = value` → virtual `__setitem__(key, value)`.
pub(crate) fn lower_class_setitem(
    base_ir: ir::Expr,
    class_id: ir::ClassId,
    index: &ast::Expr,
    value_ir: ir::Expr,
    span: Span,
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    if resolve_method(class_id, "__setitem__").is_none() {
        return Err(err(
            format!(
                "'{}' object does not support item assignment (need __setitem__)",
                class_display_name(class_id)
            ),
            span,
        ));
    }
    let val_t = ctx.fresh_temp("setitem.v", value_ir.ty);
    let val_name = ast::Expr {
        kind: ast::ExprKind::Name(val_t.clone()),
        span,
    };
    let call = lower_instance_method_call(
        base_ir,
        class_id,
        "__setitem__",
        span,
        &[index.clone(), val_name],
        ctx,
    )?;
    out.push(ir::Stmt::Assign {
        name: val_t,
        value: value_ir,
    });
    out.push(ir::Stmt::ExprStmt(call));
    Ok(())
}

/// `del obj[key]` → virtual `__delitem__(key)`.
pub(crate) fn lower_class_delitem(
    base_ir: ir::Expr,
    class_id: ir::ClassId,
    index: &ast::Expr,
    span: Span,
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    if resolve_method(class_id, "__delitem__").is_none() {
        return Err(err(
            format!(
                "'{}' object does not support item deletion (need __delitem__)",
                class_display_name(class_id)
            ),
            span,
        ));
    }
    let call = lower_instance_method_call(
        base_ir,
        class_id,
        "__delitem__",
        span,
        std::slice::from_ref(index),
        ctx,
    )?;
    out.push(ir::Stmt::ExprStmt(call));
    Ok(())
}

/// `obj[key] op= rhs` → `__getitem__` then binary then `__setitem__`.
/// Base and index are evaluated once.
pub(crate) fn lower_class_aug_index(
    base_ir: ir::Expr,
    index: &ast::Expr,
    op: ast::BinOp,
    value: &ast::Expr,
    span: Span,
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    let ir::Ty::Class(class_id) = base_ir.ty else {
        unreachable!("lower_class_aug_index requires a class instance");
    };
    if resolve_method(class_id, "__getitem__").is_none() {
        return Err(err(
            format!(
                "'{}' object is not subscriptable (need __getitem__)",
                class_display_name(class_id)
            ),
            span,
        ));
    }
    if resolve_method(class_id, "__setitem__").is_none() {
        return Err(err(
            format!(
                "'{}' object does not support item assignment (need __setitem__)",
                class_display_name(class_id)
            ),
            span,
        ));
    }
    let base_ty = base_ir.ty;
    let base_t = ctx.fresh_temp("aug.base", base_ty);
    out.push(ir::Stmt::Assign {
        name: base_t.clone(),
        value: base_ir,
    });
    let base_local = ir::Expr {
        ty: base_ty,
        kind: ir::ExprKind::Local(base_t),
    };
    let index_ir = lower_expr(index, ctx)?;
    let idx_t = ctx.fresh_temp("aug.idx", index_ir.ty);
    out.push(ir::Stmt::Assign {
        name: idx_t.clone(),
        value: index_ir,
    });
    let idx_name = ast::Expr {
        kind: ast::ExprKind::Name(idx_t),
        span,
    };
    let current = lower_instance_method_call(
        base_local.clone(),
        class_id,
        "__getitem__",
        span,
        std::slice::from_ref(&idx_name),
        ctx,
    )?;
    let right = lower_expr(value, ctx)?;
    let combined = lower_aug_binary(op, current, right, span, ctx)?;
    lower_class_setitem(base_local, class_id, &idx_name, combined, span, ctx, out)
}

/// Check and lower the target of `base[index] = ...` (list or dict).
pub(crate) fn lower_index_target_on(
    base_ir: ir::Expr,
    index: &ast::Expr,
    ctx: &mut FnCtx,
) -> SResult<(ir::Expr, ir::Ty, ir::Expr)> {
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
            index.span,
        )),
        ir::Ty::Tuple(_) => Err(err(
            "'tuple' object does not support item assignment",
            index.span,
        )),
        other => Err(err(
            format!("'{other}' object does not support item assignment"),
            index.span,
        )),
    }
}

/// Bind `name = value_ir`, inferring or checking the variable's type.
/// At the top level (or after a `global` declaration) the binding targets
/// a module global; otherwise it creates/updates a function local.
pub(crate) fn bind_name(
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
pub(crate) fn refined_member_after_assign(rhs: ir::Ty, target: ir::Ty) -> Option<ir::Ty> {
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

pub(crate) fn lower_aug_assign(
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
            // Storage type. `cell_locals` has to be probed the same way
            // `bind_name` probes it: a `nonlocal` name keeps its type there
            // under the user name, and `locals` holds only `.cell.<name>`.
            let (current_ty, _is_global) = if let Some(&t) = ctx.locals.get(name) {
                (t, false)
            } else if let Some(&t) = ctx.cell_locals.get(name) {
                (t, false)
            } else if ctx.binds_global(name) {
                match ctx.globals.get(name) {
                    Some(&t) => (t, true),
                    Option::None => {
                        return Err(err(
                            unsupported_dunder(name)
                                .or_else(|| unsupported_feature(name))
                                .map(str::to_string)
                                .unwrap_or_else(|| format!("name '{name}' is not defined")),
                            *name_span,
                        ));
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
                return Err(err(
                    unsupported_dunder(name)
                        .or_else(|| unsupported_feature(name))
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("name '{name}' is not defined")),
                    *name_span,
                ));
            };
            // `x op= v` is `x = x op v`, so the load must be *the same load*
            // the expression path performs. Only that one knows about cell
            // bindings, narrowing refinements and comprehension renames; a
            // hand-rolled `Local(name)` here silently missed all three.
            let left = lower_expr(
                &ast::Expr {
                    kind: ast::ExprKind::Name(name.clone()),
                    span: *name_span,
                },
                ctx,
            )?;
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
                    op: ir::SetUpdateOp::Union,
                });
                return Ok(());
            }
            if matches!(current_ty, ir::Ty::Set(_))
                && matches!(
                    op,
                    ast::BinOp::BitAnd | ast::BinOp::Sub | ast::BinOp::BitXor
                )
            {
                if right.ty != current_ty {
                    let op_s = match op {
                        ast::BinOp::BitAnd => "&=",
                        ast::BinOp::Sub => "-=",
                        _ => "^=",
                    };
                    return Err(err(
                        format!(
                            "set {op_s} requires the same set type on the right, found {}",
                            right.ty
                        ),
                        span,
                    ));
                }
                let set_op = match op {
                    ast::BinOp::BitAnd => ir::SetUpdateOp::Intersect,
                    ast::BinOp::Sub => ir::SetUpdateOp::Diff,
                    _ => ir::SetUpdateOp::SymDiff,
                };
                out.push(ir::Stmt::SetUpdate {
                    set: left,
                    other: right,
                    op: set_op,
                });
                return Ok(());
            }
            let combined = lower_aug_binary(op, left, right, span, ctx)?;
            // And the store is the same store a plain assignment performs —
            // `bind_name` is the only place that writes through a cell.
            out.push(bind_name(
                name,
                *name_span,
                Option::None,
                combined,
                span,
                ctx,
            )?);
            Ok(())
        }
        // `xs[i] op= v`: evaluate base and index once via temps
        ast::AssignTarget::Index { base, index } => {
            let base_ir = lower_expr(base, ctx)?;
            if matches!(base_ir.ty, ir::Ty::Class(_)) {
                return lower_class_aug_index(base_ir, index, op, value, span, ctx, out);
            }
            let (list_ir, elem, index_ir) = lower_index_target_on(base_ir, index, ctx)?;
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
            let combined = lower_aug_binary(op, current, right, span, ctx)?;
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
            let combined = lower_aug_binary(op, current, right, span, ctx)?;
            let combined = coerce(combined, field_ty, span, "attribute assignment")?;
            out.push(ir::Stmt::SetField {
                object: base_local,
                class_id,
                field_index,
                value: combined,
            });
            Ok(())
        }
        ast::AssignTarget::Slice { .. } => Err(err(
            "augmented assignment to a slice is not supported yet",
            span,
        )),
        ast::AssignTarget::Tuple(_) | ast::AssignTarget::Starred { .. } => Err(err(
            "augmented assignment to a tuple is not supported",
            span,
        )),
    }
}

// ---- for loops ----

pub(crate) fn assign_target_span(target: &ast::AssignTarget) -> Span {
    match target {
        ast::AssignTarget::Name { span, .. } => *span,
        ast::AssignTarget::Index { base, index } => base.span.to(index.span),
        ast::AssignTarget::Slice { base, hi, step, .. } => {
            let end = step
                .as_ref()
                .or(hi.as_ref())
                .map(|e| e.span)
                .unwrap_or(base.span);
            base.span.to(end)
        }
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
/// Bind a cursor element into a `for` target, without building the tuple when
/// the target immediately takes it apart again.
///
/// `for a, b in zip(xs, ys)` produced a heap tuple per element and destructured
/// it on the next line. Measured on a 1M-element zip, that was the whole cost:
/// building the two lists took 19 ms, and the loop that paired them took a
/// further 332 ms against CPython's 66 ms. The allocation is pure overhead
/// whenever the arity is known and matches, which is exactly the `zip` and
/// `enumerate` cases.
pub(crate) fn bind_cursor_element(
    target: &ast::AssignTarget,
    element: ir::Expr,
    ctx: &mut FnCtx,
) -> SResult<Vec<ir::Stmt>> {
    if let ast::AssignTarget::Tuple(parts) = target
        && let ir::ExprKind::TupleLit(items) = &element.kind
        && parts.len() == items.len()
        // A starred target consumes an unknown number of elements, so the
        // arity match above does not describe it.
        && !parts
            .iter()
            .any(|t| matches!(t, ast::AssignTarget::Starred { .. }))
    {
        let mut stmts = Vec::new();
        for (part, item) in parts.iter().zip(items.iter()) {
            stmts.extend(bind_for_target(part, item.clone(), ctx)?);
        }
        return Ok(stmts);
    }
    bind_for_target(target, element, ctx)
}

pub(crate) fn bind_for_target(
    target: &ast::AssignTarget,
    value: ir::Expr,
    ctx: &mut FnCtx,
) -> SResult<Vec<ir::Stmt>> {
    let mut stmts = Vec::new();
    let span = assign_target_span(target);
    lower_assign_ir(target, None, value, span, ctx, &mut stmts)?;
    Ok(stmts)
}

pub(crate) fn lower_for(
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

    // `for ... in zip(...)` / `enumerate(...)` — advanced in lockstep, with
    // no list and no per-element tuple beyond the one the target binds.
    if is_lazy_combinator(iter, ctx) {
        let mut setup = Vec::new();
        let parts = lower_comp_iter(iter, false, ctx, &mut setup)?;
        return lower_for_parts(target, parts, setup, body, orelse, ctx, out);
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
        // A dynamic value is iterated by index; the runtime decides what the
        // i-th element *is* from the value's own tag.
        ir::Ty::Any => lower_for_indexed(target, seq, body, orelse, ctx, out),
        other => Err(err(
            format!("'{}' object is not iterable", display_ty(other)),
            iter.span,
        )),
    }
}

/// `next(it)` / `next(it, default)` for user iterators and generators.
pub(crate) fn lower_builtin_next(
    args: &[&ast::Expr],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.is_empty() || args.len() > 2 {
        return Err(err(
            format!("next() takes 1 or 2 arguments ({} given)", args.len()),
            span,
        ));
    }
    let it = lower_expr(args[0], ctx)?;
    let default = if args.len() == 2 {
        Some(lower_expr(args[1], ctx)?)
    } else {
        None
    };

    match it.ty {
        ir::Ty::Class(id) if resolve_method(id, "__next__").is_some() => {
            // Evaluate iterator once into a temp, then call __next__.
            let it_t = ctx.fresh_temp("next.it", ir::Ty::Class(id));
            let it_local = ir::Expr {
                ty: ir::Ty::Class(id),
                kind: ir::ExprKind::Local(it_t.clone()),
            };
            let next_call = lower_instance_method_call(it_local, id, "__next__", span, &[], ctx)?;
            let yield_ty = next_call.ty;
            if yield_ty == ir::Ty::None {
                return Err(err("__next__ must return a non-None value type", span));
            }
            match default {
                None => Ok(ir::Expr {
                    ty: yield_ty,
                    kind: ir::ExprKind::Block {
                        stmts: vec![ir::Stmt::Assign {
                            name: it_t,
                            value: it,
                        }],
                        result: Box::new(next_call),
                    },
                }),
                Some(def) => {
                    let def = coerce(def, yield_ty, args[1].span, "next() default")?;
                    let out_t = ctx.fresh_temp("next.out", yield_ty);
                    let stmts = vec![
                        ir::Stmt::Assign {
                            name: it_t,
                            value: it,
                        },
                        ir::Stmt::Try {
                            body: vec![ir::Stmt::Assign {
                                name: out_t.clone(),
                                value: next_call,
                            }],
                            handlers: vec![(
                                Some(vec![ir::ExcType::StopIteration]),
                                None,
                                vec![ir::Stmt::Assign {
                                    name: out_t.clone(),
                                    value: def,
                                }],
                            )],
                            orelse: vec![],
                            finally: vec![],
                        },
                    ];
                    Ok(ir::Expr {
                        ty: yield_ty,
                        kind: ir::ExprKind::Block {
                            stmts,
                            result: Box::new(ir::Expr {
                                ty: yield_ty,
                                kind: ir::ExprKind::Local(out_t),
                            }),
                        },
                    })
                }
            }
        }
        ir::Ty::Generator { yield_ty } => {
            let yield_ty = *yield_ty;
            let opt_ty = ir::optional_of(yield_ty);
            let gen_t = ctx.fresh_temp("next.gen", it.ty);
            let nxt_t = ctx.fresh_temp("next.gnxt", opt_ty);
            let out_t = ctx.fresh_temp("next.gout", yield_ty);
            let gen_local = ir::Expr {
                ty: it.ty,
                kind: ir::ExprKind::Local(gen_t.clone()),
            };
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
            match default {
                None => {
                    // if next is None: raise StopIteration("") else: yield value
                    let stmts = vec![
                        ir::Stmt::Assign {
                            name: gen_t,
                            value: it,
                        },
                        ir::Stmt::Assign {
                            name: nxt_t,
                            value: next_e,
                        },
                        ir::Stmt::If {
                            branches: vec![(
                                is_none,
                                vec![ir::Stmt::Raise {
                                    exc: ir::ExcType::StopIteration,
                                    // Generator exhaustion is `StopIteration()`
                                    // with no argument, as CPython raises it.
                                    message: None,
                                }],
                            )],
                            orelse: vec![ir::Stmt::Assign {
                                name: out_t.clone(),
                                value: extracted,
                            }],
                        },
                    ];
                    Ok(ir::Expr {
                        ty: yield_ty,
                        kind: ir::ExprKind::Block {
                            stmts,
                            result: Box::new(ir::Expr {
                                ty: yield_ty,
                                kind: ir::ExprKind::Local(out_t),
                            }),
                        },
                    })
                }
                Some(def) => {
                    let def = coerce(def, yield_ty, args[1].span, "next() default")?;
                    let stmts = vec![
                        ir::Stmt::Assign {
                            name: gen_t,
                            value: it,
                        },
                        ir::Stmt::Assign {
                            name: nxt_t,
                            value: next_e,
                        },
                        ir::Stmt::If {
                            branches: vec![(
                                is_none,
                                vec![ir::Stmt::Assign {
                                    name: out_t.clone(),
                                    value: def,
                                }],
                            )],
                            orelse: vec![ir::Stmt::Assign {
                                name: out_t.clone(),
                                value: extracted,
                            }],
                        },
                    ];
                    Ok(ir::Expr {
                        ty: yield_ty,
                        kind: ir::ExprKind::Block {
                            stmts,
                            result: Box::new(ir::Expr {
                                ty: yield_ty,
                                kind: ir::ExprKind::Local(out_t),
                            }),
                        },
                    })
                }
            }
        }
        other => Err(err(
            format!("'{other}' object is not an iterator"),
            args[0].span,
        )),
    }
}

/// `for x in obj:` when `obj` is a class with `__iter__` / `__next__`.
/// Desugars to: `it = obj.__iter__(); while more: try: x = it.__next__()
/// except StopIteration: more = False else: bind; body`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lower_for_user_iter(
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

    // try: nxt = it.__next__()
    // except StopIteration: more = False
    // else: bind; body
    // StopIteration from bind/body must propagate (CPython).
    let next_call = lower_instance_method_call(it_local, it_id, "__next__", span, &[], ctx)?;
    let try_body = vec![ir::Stmt::Assign {
        name: nxt_t.clone(),
        value: next_call,
    }];
    let mut try_orelse = bind;
    try_orelse.extend(user_body);
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
        orelse: try_orelse,
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
pub(crate) fn lower_for_generator(
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
pub(crate) fn restore_refinements_after_for(
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

pub(crate) fn clear_orelse_assigns(ctx: &mut FnCtx, orelse: &[ast::Stmt]) {
    for name in assigned_names_in_stmts(orelse) {
        ctx.type_refinements.remove(&name);
    }
}

/// Emit while + optional else (else runs only if no break).
pub(crate) fn push_loop_with_else(
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

pub(crate) fn rewrite_breaks_set_flag(stmts: Vec<ir::Stmt>, broke: &str) -> Vec<ir::Stmt> {
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
pub(crate) fn lower_for_file(
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
/// Emit a `for` loop over an already-built cursor.
///
/// The cursor protocol was only used by comprehensions and drains; `for` had
/// a parallel family of `lower_for_*` functions. Routing `for` through it too
/// is what lets one iterable form — a composed `zip`, say — serve every
/// consumer, and it keeps `break`/`continue`/`else` and the type-refinement
/// lifecycle in exactly one place.
pub(crate) fn lower_for_parts(
    target: &ast::AssignTarget,
    parts: CompIterParts,
    setup: Vec<ir::Stmt>,
    body: &[ast::Stmt],
    orelse: &[ast::Stmt],
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    out.extend(setup);
    let entry_ref = ctx.type_refinements.clone();
    let bind = bind_cursor_element(target, parts.element, ctx)?;
    ctx.loop_depth += 1;
    let user_body = lower_nested_block(body, ctx);
    ctx.loop_depth -= 1;
    restore_refinements_after_for(ctx, entry_ref, target, body, orelse);
    let mut payload = bind;
    payload.extend(user_body?);
    let loop_body = comp_kind_body(parts.kind, payload);
    push_loop_with_else(parts.cond, loop_body, parts.step, orelse, ctx, out)?;
    clear_orelse_assigns(ctx, orelse);
    Ok(())
}

pub(crate) fn lower_for_indexed(
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
        // A dynamic value yields dynamic elements.
        ir::Ty::Any => ir::Ty::Any,
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
    // A dynamic sequence uses the iteration accessor rather than the
    // subscript: iterating a dict yields its keys, while `d[0]` looks one up.
    let element = ir::Expr {
        ty: elem_ty,
        kind: if seq_ty == ir::Ty::Any {
            ir::ExprKind::AnyIterGet {
                base: Box::new(seq_local),
                index: Box::new(idx_local.clone()),
            }
        } else {
            ir::ExprKind::Index {
                base: Box::new(seq_local),
                index: Box::new(idx_local.clone()),
            }
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

pub(crate) fn lower_for_range(
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
    // Bind start, then stop, then step (below): CPython evaluates a call's
    // arguments left to right, and `lower_expr` leaves side effects inside
    // the expression, so the order these temps are assigned in *is* the
    // order the operands run in. Binding stop first made
    // `range(a(), b())` call `b()` before `a()`.
    let it_t = ctx.fresh_temp("range.it", ir::Ty::Int);
    out.push(ir::Stmt::Assign {
        name: it_t.clone(),
        value: start,
    });
    let it_local = ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::Local(it_t.clone()),
    };
    let stop_t = ctx.fresh_temp("range.stop", ir::Ty::Int);
    out.push(ir::Stmt::Assign {
        name: stop_t.clone(),
        value: stop,
    });
    let stop_local = ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::Local(stop_t),
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

pub(crate) fn int_const(v: i64) -> ir::Expr {
    ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::ConstInt(v),
    }
}

pub(crate) fn int_cmp(op: ir::BinOp, l: ir::Expr, r: ir::Expr) -> ir::Expr {
    ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Binary {
            op,
            left: Box::new(l),
            right: Box::new(r),
        },
    }
}

pub(crate) fn bool_and(l: ir::Expr, r: ir::Expr) -> ir::Expr {
    ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Binary {
            op: ir::BinOp::And,
            left: Box::new(l),
            right: Box::new(r),
        },
    }
}

pub(crate) fn bool_or(l: ir::Expr, r: ir::Expr) -> ir::Expr {
    ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Binary {
            op: ir::BinOp::Or,
            left: Box::new(l),
            right: Box::new(r),
        },
    }
}

pub(crate) fn is_same(left: ir::Expr, right: ir::Expr) -> ir::Expr {
    ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::IsIdentity {
            left: Box::new(left),
            right: Box::new(right),
            not: false,
        },
    }
}

/// Coerce `value` for assignment into a variable of type `target`.
pub(crate) fn coerce_assign(
    value: ir::Expr,
    target: ir::Ty,
    name: &str,
    span: Span,
) -> SResult<ir::Expr> {
    coerce(value, target, span, &format!("assignment to '{name}'")).map_err(|e| {
        Diagnostic::new(
            Phase::Semantic,
            format!(
                "{}; variable '{name}' has storage type {} (join of its assignments \
                 and annotation)",
                e.message,
                display_ty(target)
            ),
            e.span,
        )
    })
}

/// Wrap `value` into a union type (identity if already that union).
pub(crate) fn to_union(value: ir::Expr, union: ir::Ty) -> ir::Expr {
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
pub(crate) fn union_is_subset(src: ir::Ty, dst: ir::Ty) -> bool {
    let src_ms = ir::flatten_union_members(src);
    let dst_ms = ir::flatten_union_members(dst);
    src_ms.iter().all(|m| dst_ms.iter().any(|d| d == m))
}

/// Whether `ty` can be boxed into [`ir::Ty::Any`] (has a print-tag encoding).
pub(crate) fn can_box_as_any(ty: ir::Ty) -> bool {
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
pub(crate) fn coerce(value: ir::Expr, target: ir::Ty, span: Span, what: &str) -> SResult<ir::Expr> {
    if value.ty == target {
        return Ok(value);
    }
    // An empty `[]` has no element type to infer, so it is typed provisionally
    // as `list[Any]`. It is compatible with any list, and the runtime value --
    // a length-zero list -- is identical, so it just takes the target's type.
    // Without this, an empty literal nested in a container is rejected:
    // `[["a"], []]` and `{"a": ["b"], "d": []}` are ordinary Python.
    if matches!(target, ir::Ty::List(_))
        && value.ty == ir::list_of(ir::Ty::Any)
        && matches!(
            value.kind,
            ir::ExprKind::ListLit(ref items) if items.is_empty()
        )
    {
        return Ok(ir::Expr {
            ty: target,
            kind: value.kind,
        });
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
                "type mismatch in {what}: expected {}, found {}",
                display_ty(target),
                display_ty(value.ty)
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
            "type mismatch in {what}: expected {}, found {}",
            display_ty(target),
            display_ty(value.ty)
        ),
        span,
    ))
}

/// Drop the leading `self` parameter from a method signature for call matching
/// (self is supplied via `extra_leading` in `lower_call_with_sig`).
pub(crate) fn method_user_sig(sig: &FuncSig) -> FuncSig {
    let mut s = sig.clone();
    if !s.params.is_empty() {
        s.params = s.params[1..].to_vec();
        // The `/` and `*` boundaries are indices into `params`, so dropping
        // `self` shifts them by one.
        s.posonly_end = s.posonly_end.saturating_sub(1);
        s.kwonly_start = s.kwonly_start.map(|k| k.saturating_sub(1));
    }
    s
}

/// True when `e` is a zero-arg `super()` call expression.
pub(crate) fn is_zero_arg_super(e: &ast::Expr) -> bool {
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
pub(crate) fn lower_super_method_call(
    method: &str,
    method_span: Span,
    args: &[ast::Expr],
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let class_id = ctx
        .current_class
        .ok_or_else(|| err("super() outside of a method is not supported", method_span))?;
    // Reject super() in staticmethod / classmethod (zero-arg needs instance self).
    if ctx.self_param.is_none() {
        return Err(err(
            "super() is only supported in instance methods (not staticmethod/classmethod)",
            method_span,
        ));
    }
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
/// Classmethod body `cls(...)`: allocate using `cls`'s runtime type_id.
pub(crate) fn lower_classmethod_construct(
    cls_name: String,
    defining_id: ir::ClassId,
    args: &[ast::PosArg],
    keywords: &[ast::Keyword],
    kwargs: Option<&ast::Expr>,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    // Match user args against defining class's __init__ (shared by inheritance).
    let init_func = resolve_method(defining_id, "__init__");
    let mut user_args: Vec<ir::Expr> = Vec::new();
    if let Some(init_name) = &init_func {
        let sig = ctx
            .mctx
            .funcs
            .get(init_name)
            .cloned()
            .or_else(|| method_sig_lookup(init_name))
            .ok_or_else(|| {
                err(
                    "cannot construct via cls(...): __init__ signature not available",
                    span,
                )
            })?;
        let user_sig = method_user_sig(&sig);
        // Reuse call matching by building a dummy call then extracting args.
        // Simpler: manually coerce positionals (no keywords for minimal surface).
        if !keywords.is_empty() || kwargs.is_some() {
            return Err(err(
                "keyword arguments in cls(...) classmethod construct are not supported yet",
                span,
            ));
        }
        let plain = require_plain_args(args, "cls", span)?;
        let required = user_sig
            .params
            .iter()
            .filter(|p| p.default.is_none())
            .count();
        if plain.len() < required || plain.len() > user_sig.params.len() {
            return Err(err(
                format!(
                    "cls() takes {} to {} arguments ({} given)",
                    required,
                    user_sig.params.len(),
                    plain.len()
                ),
                span,
            ));
        }
        for (i, a) in plain.iter().enumerate() {
            let v = lower_expr(a, ctx)?;
            let want = user_sig.params[i].ty;
            user_args.push(coerce(v, want, a.span, "cls(...) argument")?);
        }
        // Fill defaults for remaining params.
        for p in user_sig.params.iter().skip(plain.len()) {
            if let Some(d) = &p.default {
                let v = lower_expr(d, ctx)?;
                user_args.push(coerce(v, p.ty, d.span, "cls(...) default")?);
            }
        }
    } else if !args.is_empty() || !keywords.is_empty() || kwargs.is_some() {
        return Err(err(
            "cls() takes no arguments (no __init__ on this class)",
            span,
        ));
    }

    // Per closed-world subclass: only call `__init__` when arity matches the
    // static arg list. Mismatches become runtime TypeError (CPython), not UB.
    let n_args = user_args.len();
    let mut candidates: Vec<(ir::ClassId, Option<String>)> = Vec::new();
    let mut arity_errors: Vec<(ir::ClassId, String)> = Vec::new();
    let mut sids = subclasses_of(defining_id);
    if sids.is_empty() {
        sids.push(defining_id);
    }
    for sid in sids {
        let cname = class_info(sid)
            .map(|c| c.name)
            .unwrap_or_else(|| format!("class#{sid}"));
        let sid_init = resolve_method(sid, "__init__");
        match sid_init {
            None => {
                if n_args == 0 {
                    candidates.push((sid, None));
                } else {
                    arity_errors.push((sid, format!("TypeError: {cname}() takes no arguments")));
                }
            }
            Some(init_name) => {
                let Some(sig) = ctx
                    .mctx
                    .funcs
                    .get(&init_name)
                    .cloned()
                    .or_else(|| method_sig_lookup(&init_name))
                else {
                    arity_errors.push((
                        sid,
                        format!("TypeError: {cname}.__init__() signature not available"),
                    ));
                    continue;
                };
                let user_sig = method_user_sig(&sig);
                let required = user_sig
                    .params
                    .iter()
                    .filter(|p| p.default.is_none())
                    .count();
                let max_p = user_sig.params.len();
                if n_args < required {
                    let missing = required - n_args;
                    let names: Vec<&str> = user_sig
                        .params
                        .iter()
                        .skip(n_args)
                        .filter(|p| p.default.is_none())
                        .map(|p| p.name.as_str())
                        .collect();
                    let arg_list = if names.len() == 1 {
                        format!("'{}'", names[0])
                    } else if names.is_empty() {
                        String::new()
                    } else {
                        let mut parts: Vec<String> =
                            names.iter().map(|n| format!("'{n}'")).collect();
                        let last = parts.pop().unwrap();
                        format!("{} and {last}", parts.join(", "))
                    };
                    let noun = if missing == 1 {
                        "argument"
                    } else {
                        "arguments"
                    };
                    let msg = if arg_list.is_empty() {
                        format!(
                            "TypeError: {cname}.__init__() missing {missing} required positional {noun}"
                        )
                    } else {
                        format!(
                            "TypeError: {cname}.__init__() missing {missing} required positional {noun}: {arg_list}"
                        )
                    };
                    arity_errors.push((sid, msg));
                } else if n_args > max_p {
                    // CPython counts `self` in "takes N positional arguments".
                    arity_errors.push((
                        sid,
                        format!(
                            "TypeError: {cname}.__init__() takes {} positional arguments but {} were given",
                            max_p + 1,
                            n_args + 1
                        ),
                    ));
                } else {
                    candidates.push((sid, Some(init_name)));
                }
            }
        }
    }
    if candidates.is_empty() && arity_errors.is_empty() {
        candidates.push((defining_id, init_func));
    }
    let cls_obj = ir::Expr {
        ty: ir::Ty::Class(defining_id),
        kind: ir::ExprKind::Local(cls_name),
    };
    // Return type: defining class (subclass is assignable via coerce at use).
    Ok(ir::Expr {
        ty: ir::Ty::Class(defining_id),
        kind: ir::ExprKind::ClassConstructDynamic {
            cls_obj: Box::new(cls_obj),
            candidates,
            arity_errors,
            args: user_args,
        },
    })
}

pub(crate) fn lower_class_construct(
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
/// `obj.method(args)` with no keyword arguments — every internal dunder
/// dispatch, and the shape most call sites want.
pub(crate) fn lower_instance_method_call(
    base_ir: ir::Expr,
    class_id: ir::ClassId,
    method: &str,
    method_span: Span,
    args: &[ast::Expr],
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    lower_instance_method_call_kw(base_ir, class_id, method, method_span, args, &[], ctx)
}

/// `obj.method(a, b=1)`.
///
/// Keywords were rejected outright for any instance method — not just for a
/// keyword-only parameter — while a free function accepted them. The binding
/// logic already lived in `lower_call_with_sig`; this path simply never
/// handed it the keywords.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lower_instance_method_call_kw(
    base_ir: ir::Expr,
    class_id: ir::ClassId,
    method: &str,
    method_span: Span,
    args: &[ast::Expr],
    keywords: &[ast::Keyword],
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if method == "__init__" {
        return Err(err(
            "calling __init__ directly is not supported yet; construct with Class(...)",
            method_span,
        ));
    }
    // `self.handler(x)` where `handler` is a *field* holding a function value,
    // not a method. Python looks the attribute up and calls whatever it finds;
    // here the two live in different namespaces, so a field has to be tried
    // before reporting a missing method.
    if resolve_method(class_id, method).is_none()
        && let Some((fidx, fty)) = field_index(class_id, method)
        && let ir::Ty::Closure {
            params,
            ret,
            capture_tys,
            func,
        } = fty
    {
        if !keywords.is_empty() {
            return Err(err(
                format!(
                    "'{method}' is a function-valued field, and a closure value \
                     carries no parameter names, so it cannot take keyword arguments"
                ),
                method_span,
            ));
        }
        if args.len() != params.len() {
            return Err(err(
                format!(
                    "'{method}' takes {} argument(s) but {} were given",
                    params.len(),
                    args.len()
                ),
                method_span,
            ));
        }
        let field = ir::Expr {
            ty: fty,
            kind: ir::ExprKind::GetField {
                object: Box::new(base_ir),
                class_id,
                field_index: fidx,
            },
        };
        let mut lowered = Vec::with_capacity(args.len());
        for (a, want) in args.iter().zip(params.iter()) {
            let v = lower_expr(a, ctx)?;
            lowered.push(coerce(v, *want, a.span, "argument")?);
        }
        return Ok(ir::Expr {
            ty: *ret,
            kind: ir::ExprKind::CallClosure {
                closure: Box::new(field),
                args: lowered,
                capture_tys: capture_tys.to_vec(),
                func: func.to_string(),
            },
        });
    }
    let direct = resolve_method(class_id, method).ok_or_else(|| {
        let hint = match class_field_ty(class_id, method) {
            Some(t) => format!(
                " (there is a field '{method}' of type {}, but only a function-valued \
                 field can be called)",
                display_ty(t)
            ),
            Option::None => String::new(),
        };
        err(
            format!(
                "'{}' object has no method '{method}'{hint}",
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
        return lower_call_with_sig(
            method,
            direct,
            &sig,
            &pos,
            keywords,
            None,
            method_span,
            ctx,
            &[],
        );
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
            keywords,
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
        keywords,
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
pub(crate) fn lower_class_name_method_call(
    class_id: ir::ClassId,
    method: &str,
    method_span: Span,
    args: &[ast::Expr],
    keywords: &[ast::Keyword],
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
        MethodKind::Static => lower_call_with_sig(
            method,
            direct,
            &sig,
            &pos,
            keywords,
            None,
            method_span,
            ctx,
            &[],
        ),
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
                keywords,
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
pub(crate) fn coerce_numeric_into(value: ir::Expr, target: ir::Ty) -> SResult<ir::Expr> {
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
/// `body if test else orelse`.
///
/// Lowered to a temp assigned in the two arms of an `If`, wrapped in the
/// `Block` node comprehensions already use to put statements inside an
/// expression. That keeps Python's laziness for free: the branch not taken
/// is never emitted into the same basic block, so its side effects do not
/// run and neither do its traps.
///
/// The result type uses `join_elem_types`, not the scalar-assignment
/// `join_types`. CPython evaluates to one branch's *value*, so
/// `1 if c else 2.5` is `1`, not `1.0` -- collapsing mixed numerics here
/// would reintroduce exactly the defect 0.89 fixed for list literals.
pub(crate) fn lower_if_exp(
    test: &ast::Expr,
    body: &ast::Expr,
    orelse: &ast::Expr,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let cond = lower_condition(test, ctx)?;

    // `test` narrows its arms exactly as an `if` statement narrows its bodies:
    // in `x if x is not None else 0` the then-arm sees the peeled type. Without
    // this the arms join back to the union and `coerce` rejects the result with
    // "use 'is None' check" — advice the author had already taken. Same
    // save / splice / restore the `and` / `or` arm of `lower_expr` performs.
    let saved = ctx.type_refinements.clone();
    let (then_ref, else_ref) = narrowing_from_condition(test, ctx);

    for (k, v) in then_ref {
        ctx.type_refinements.insert(k, v);
    }
    let then_val = lower_expr(body, ctx)?;

    ctx.type_refinements = saved.clone();
    for (k, v) in else_ref {
        ctx.type_refinements.insert(k, v);
    }
    let else_val = lower_expr(orelse, ctx)?;
    ctx.type_refinements = saved;

    // A conditional expression yields one value, so a union of the two branch
    // types is exactly its type. `join_elem_types` gives the numeric pairs the
    // same treatment list literals get (a union, so `1 if c else 2.5` stays
    // `1`); anything else it declines becomes a plain union. That is more than
    // a list literal infers, and deliberately so: there is no container
    // storage here whose representation would have to be chosen.
    let ty = match join_elem_types(then_val.ty, else_val.ty) {
        Some(ty) => ty,
        Option::None => ir::union_of(&[then_val.ty, else_val.ty]),
    };

    // A mixed-numeric join is a union, so each branch is boxed into it and
    // keeps its own runtime type.
    let (then_val, else_val) = if matches!(ty, ir::Ty::Union(_)) {
        (to_union(then_val, ty), to_union(else_val, ty))
    } else {
        (then_val, else_val)
    };

    let name = ctx.fresh_temp("ifexp", ty);
    let branch = ir::Stmt::If {
        branches: vec![(
            cond,
            vec![ir::Stmt::Assign {
                name: name.clone(),
                value: then_val,
            }],
        )],
        orelse: vec![ir::Stmt::Assign {
            name: name.clone(),
            value: else_val,
        }],
    };
    Ok(ir::Expr {
        ty,
        kind: ir::ExprKind::Block {
            stmts: vec![branch],
            result: Box::new(local_expr(name, ty)),
        },
    })
}

pub(crate) fn lower_condition(cond: &ast::Expr, ctx: &mut FnCtx) -> SResult<ir::Expr> {
    let lowered = lower_expr(cond, ctx)?;
    to_bool(lowered, cond.span, ctx)
}

/// Truthiness without class dunders (match patterns / any/all).
pub(crate) fn to_bool_default(value: ir::Expr, span: Span) -> SResult<ir::Expr> {
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

/// `lower_cast`, plus the one conversion that has to call a user method.
///
/// `bool(x)` on a class instance must consult `__bool__` (then `__len__`)
/// exactly as `if x:` and `not x` do. `lower_cast` is deliberately ctx-free —
/// it lowers representations, and only `str()` gets a class arm because
/// `lower_class_to_str` needs no context — so its `Bool` arm folded every
/// instance to a constant `true`, silently, with no diagnostic.
pub(crate) fn lower_cast_ctx(
    ty: ast::TypeName,
    value: ir::Expr,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    // *Every* bool cast, not only a class one. `lower_cast` kept its own list
    // of convertible types and it had drifted from `to_bool_default`'s —
    // `bool(x)` on a union or a `None` was refused while `if x:` accepted
    // both. Routing the whole cast through `to_bool` means the two spellings
    // are one code path and cannot drift again.
    if matches!(ty, ast::TypeName::Bool) {
        return to_bool(value, span, ctx);
    }
    lower_cast(ty, value, span)
}

pub(crate) fn to_bool(value: ir::Expr, span: Span, ctx: &mut FnCtx) -> SResult<ir::Expr> {
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

pub(crate) fn const_none() -> ir::Expr {
    ir::Expr {
        ty: ir::Ty::None,
        kind: ir::ExprKind::ConstNone,
    }
}
