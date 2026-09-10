//! Expression lowering (calls, comprehensions, literals, yields).

use std::collections::HashSet;

use common::Span;
use parser::ast;

use crate::prelude::*;

pub(crate) fn lower_expr(expr: &ast::Expr, ctx: &mut FnCtx) -> SResult<ir::Expr> {
    match &expr.kind {
        ast::ExprKind::IfExp { test, body, orelse } => lower_if_exp(test, body, orelse, ctx),
        ast::ExprKind::GenExp { elem, generators } => {
            lower_gen_exp(elem, generators, expr.span, ctx)
        }
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
            } else if let Some(sig) = ctx.funcs().get(name).cloned() {
                // A module-level function in value position is a closure with
                // an empty environment — the same shape the free-function
                // decorator desugar already builds. Nested `def`s and lambdas
                // have been first-class since closures existed; this arm was
                // simply never written.
                if sig.vararg.is_some() || sig.kwarg.is_some() {
                    return Err(err(
                        format!(
                            "'{name}' cannot be used as a value because it takes \
                             *args or **kwargs; a closure value has a fixed \
                             parameter list"
                        ),
                        expr.span,
                    ));
                }
                if sig.params.iter().any(|p| p.default.is_some()) {
                    return Err(err(
                        format!(
                            "'{name}' cannot be used as a value because it has \
                             default arguments; a closure value carries no defaults"
                        ),
                        expr.span,
                    ));
                }
                let params: Vec<ir::Ty> = sig.params.iter().map(|p| p.ty).collect();
                let ir_name = ctx.own_func(name);
                let ty = ir::closure_of_full(&params, sig.ret, &[], &ir_name);
                Ok(ir::Expr {
                    ty,
                    kind: ir::ExprKind::MakeClosure {
                        func: ir_name,
                        captures: vec![],
                        capture_is_cell: vec![],
                    },
                })
            } else if name == "__name__" {
                // The one module attribute with a compile-time answer: the
                // entry module is `__main__`, an imported one is its import
                // name. Reached only after locals and globals, so a user
                // binding of the same name still shadows it.
                Ok(const_str(if ctx.mctx.is_root {
                    ENTRY_NAME
                } else {
                    ctx.mctx.module
                }))
            } else {
                Err(err(
                    unsupported_dunder(name)
                        .or_else(|| unsupported_feature(name))
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("name '{name}' is not defined")),
                    expr.span,
                ))
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
        ast::ExprKind::NamedExpr {
            target,
            target_span,
            value,
        } => {
            // `name := value` → assign then yield the value.
            let v = lower_expr(value, ctx)?;
            let ty = v.ty;
            let bind = bind_name(target, *target_span, None, v, value.span, ctx)?;
            let load = if ctx.binds_global(target) {
                ir::Expr {
                    ty,
                    kind: ir::ExprKind::GlobalLoad(ctx.own_global(target)),
                }
            } else {
                ir::Expr {
                    ty,
                    kind: ir::ExprKind::Local(target.clone()),
                }
            };
            Ok(ir::Expr {
                ty,
                kind: ir::ExprKind::Block {
                    stmts: vec![bind],
                    result: Box::new(load),
                },
            })
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
                ir::Ty::Class(id) => lower_class_getitem(base_ir, id, index, expr.span, ctx),
                // A dynamic value knows at run time whether it is a list or a
                // dict; the index type says which one is being asked for, and
                // the result is dynamic in turn.
                ir::Ty::Any => {
                    let key_ir = lower_expr(index, ctx)?;
                    match key_ir.ty {
                        // A dynamic key carries its own tag, so it hashes and
                        // compares as whatever it holds.
                        ir::Ty::Int | ir::Ty::Bool | ir::Ty::Str | ir::Ty::Any => {}
                        other => {
                            return Err(err(
                                format!(
                                    "a dynamic value can be indexed by int (list or \
                                     tuple), str (dict), or another dynamic value \
                                     (dict), found {}",
                                    display_ty(other)
                                ),
                                index.span,
                            ));
                        }
                    }
                    Ok(ir::Expr {
                        ty: ir::Ty::Any,
                        kind: ir::ExprKind::Index {
                            base: Box::new(base_ir),
                            index: Box::new(key_ir),
                        },
                    })
                }
                other => Err(err(
                    format!("'{}' object is not subscriptable", display_ty(other)),
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
                    // The three standard streams are file objects, so the
                    // whole file surface -- read, readline, readlines, write,
                    // iteration, `with` -- applies to them. They are
                    // singletons: `sys.stdout is sys.stdout`.
                    if let Some(which) = match attr.as_str() {
                        "stdin" => Some(0u8),
                        "stdout" => Some(1),
                        "stderr" => Some(2),
                        _ => Option::None,
                    } {
                        return Ok(ir::Expr {
                            ty: ir::Ty::File,
                            kind: ir::ExprKind::StdStream(which),
                        });
                    }
                    let note = match attr.as_str() {
                        "exit" => "sys.exit is a call, not a value: write sys.exit(code)",
                        _ => "",
                    };
                    return Err(err(
                        if note.is_empty() {
                            format!(
                                "'sys.{attr}' is not supported yet (sys.argv, \
                                 sys.exit(), and sys.stdin / sys.stdout / sys.stderr \
                                 as file objects are)"
                            )
                        } else {
                            note.to_string()
                        },
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
            // `ClassName.CONST` — a class-body constant, substituted here.
            // Checked before the module path, since a bare name that is a
            // class is not a module.
            if let ast::ExprKind::Name(cls) = &base.kind
                && !ctx.locals.contains_key(cls)
                && let Some(id) = lookup_class(cls)
            {
                if let Some(v) = class_const(id, attr) {
                    return Ok(v);
                }
                // A class name with no such constant: say that, rather than
                // falling through to "name 'C' is not defined", which sends
                // the reader looking for a missing binding.
                return Err(err(
                    format!(
                        "class '{cls}' has no attribute '{attr}'; class-body \
                         constants must be literals, and methods are accessed \
                         on an instance"
                    ),
                    *attr_span,
                ));
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
            // Exception objects: `e.args` (message tuple as list[str] for empty/non-empty).
            if base_ir.ty == ir::Ty::Exception {
                if attr == "args" {
                    // Materialize as list[str] so empty and 1-element both typecheck.
                    return Ok(ir::Expr {
                        ty: ir::list_of(ir::Ty::Str),
                        kind: ir::ExprKind::ExcArgs(Box::new(base_ir)),
                    });
                }
                return Err(err(
                    format!("'exception' object has no attribute '{attr}' (supported: args)"),
                    *attr_span,
                ));
            }
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
                // Not an instance field or method: a class constant, which
                // an instance reads through its class, as in CPython.
                if let Some(v) = class_const(id, attr) {
                    return Ok(v);
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
            if kwargs.is_some() {
                return Err(err(
                    "** unpacking is not supported for this method call",
                    *method_span,
                ));
            }
            // `"...".format(...)` on a literal, before the keyword guard:
            // `.format(name=x)` is one of its two normal spellings.
            if method == "format"
                && let ast::ExprKind::Str(fmt) = &base.kind
            {
                return lower_str_format(fmt, args, keywords, *method_span, ctx);
            }
            // `list.sort(key=…, reverse=…)` is statement-only (returns None).
            if method == "sort" && !keywords.is_empty() {
                let base_ir = lower_expr(base, ctx)?;
                match base_ir.ty {
                    ir::Ty::List(_) => {
                        // Validate kwargs the same way as the statement path.
                        let _ = take_sort_keywords(keywords, "list.sort")?;
                        return Err(err(
                            "list.sort(...) returns None and cannot be used \
                             in an expression",
                            *method_span,
                        ));
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
            let args: Vec<ast::Expr> = plain.iter().map(|e| (*e).clone()).collect();
            // `super().m(...)` — static parent method call (before lowering base).
            if is_zero_arg_super(base) {
                return lower_super_method_call(method, *method_span, &args, ctx);
            }
            // `ClassName.static_or_class_method(...)` before lowering base as value.
            if let ast::ExprKind::Name(cls_name) = &base.kind
                && let Some(class_id) = lookup_class(cls_name)
            {
                return lower_class_name_method_call(
                    class_id,
                    method,
                    *method_span,
                    &args,
                    keywords,
                    ctx,
                );
            }
            if is_str_type_name(base, ctx) && method == "maketrans" {
                return lower_str_maketrans(&args, *method_span, ctx);
            }
            if is_dict_type_name(base, ctx) && method == "fromkeys" {
                return lower_dict_fromkeys(&args, *method_span, ctx);
            }
            let base_ir = lower_expr(base, ctx)?;
            // User class instance method (not property: obj.prop() is TypeError).
            if let ir::Ty::Class(id) = base_ir.ty {
                if resolve_property(id, method).is_some() {
                    return Err(err(
                        format!(
                            "'{}' object attribute '{method}' is a property and is not callable",
                            class_info(id)
                                .map(|c| c.name)
                                .unwrap_or_else(|| format!("class#{id}"))
                        ),
                        *method_span,
                    ));
                }
                return lower_instance_method_call_kw(
                    base_ir,
                    id,
                    method,
                    *method_span,
                    &args,
                    keywords,
                    ctx,
                );
            }
            // Past this point the base is a builtin type, whose method table
            // has no keyword surface.
            if !keywords.is_empty() {
                return Err(err(
                    "keyword arguments are not supported for this method call",
                    keywords[0].name_span,
                ));
            }
            match base_ir.ty {
                ir::Ty::List(elem) => match method.as_str() {
                    // pop returns the removed element
                    "pop" => lower_list_pop(base_ir, *elem, &args, *method_span, ctx),
                    "index" => lower_list_index_of(base_ir, *elem, &args, *method_span, ctx),
                    "count" => lower_list_count(base_ir, *elem, &args, *method_span, ctx),
                    "append" | "insert" | "remove" | "clear" | "reverse" | "sort" | "extend" => {
                        Err(err(
                            format!(
                                "list.{method}(...) returns None and cannot be used \
                                 in an expression"
                            ),
                            *method_span,
                        ))
                    }
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
                ir::Ty::Tuple(elems) => match method.as_str() {
                    "index" => lower_tuple_index_of(base_ir, elems, &args, *method_span, ctx),
                    "count" => lower_tuple_count(base_ir, elems, &args, *method_span, ctx),
                    _ => Err(err(
                        format!(
                            "tuple method '{method}' is not supported yet (supported: index, count)"
                        ),
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
                ir::Ty::Set(elem) => match method.as_str() {
                    "add"
                    | "remove"
                    | "discard"
                    | "clear"
                    | "update"
                    | "intersection_update"
                    | "difference_update"
                    | "symmetric_difference_update" => Err(err(
                        format!(
                            "set.{method}(...) returns None and cannot be used in an \
                             expression"
                        ),
                        *method_span,
                    )),
                    "union"
                    | "intersection"
                    | "difference"
                    | "symmetric_difference"
                    | "issubset"
                    | "issuperset"
                    | "isdisjoint" => {
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
                            "symmetric_difference" => {
                                lower_set_binary_op(base_ir, other, *method_span, method, |l, r| {
                                    ir::ExprKind::SetSymDiff { left: l, right: r }
                                })
                            }
                            rel => lower_set_relation(base_ir, other, *method_span, rel),
                        }
                    }
                    "copy" => {
                        if !args.is_empty() {
                            return Err(err(
                                format!("copy() takes no arguments ({} given)", args.len()),
                                *method_span,
                            ));
                        }
                        Ok(ir::Expr {
                            ty: ir::set_of(*elem),
                            kind: ir::ExprKind::SetCopy(Box::new(base_ir)),
                        })
                    }
                    "pop" => lower_set_pop(base_ir, *elem, &args, *method_span),
                    _ => Err(err(
                        format!(
                            "set method '{method}' is not supported yet (supported: add, \
                             remove, discard, clear, union, intersection, difference, \
                             symmetric_difference, issubset, issuperset, isdisjoint, update, \
                             intersection_update, difference_update, \
                             symmetric_difference_update, copy, pop)"
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
                // A dynamic dict can list its keys; the runtime reads the
                // insertion order the value already carries.
                ir::Ty::Any if method == "keys" && args.is_empty() => Ok(ir::Expr {
                    ty: ir::list_of(ir::Ty::Str),
                    kind: ir::ExprKind::AnyDictKeys(Box::new(base_ir)),
                }),
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
            let (lo_ir, hi_ir, step_ir) =
                lower_slice_bounds(lo.as_deref(), hi.as_deref(), step.as_deref(), ctx)?;
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
            lower_cast_ctx(*ty, value, arg.span, ctx)
        }
        ast::ExprKind::Unary { op, operand } => {
            let value = lower_expr(operand, ctx)?;
            // A class instance dispatches to its dunder; `not` never does,
            // because Python's `not` is truthiness and has no `__not__`.
            if matches!(value.ty, ir::Ty::Class(_)) && class_unary_method(*op).is_some() {
                return lower_class_unary(*op, value, expr.span, ctx);
            }
            match op {
                // `+x` is identity on a number. It is not a no-op in general,
                // which is why the parser records it: CPython rejects `+"a"`.
                ast::UnaryOp::Pos => unary_numeric(value, "+", operand.span),
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
                    let value = unary_numeric(value, "-", operand.span)?;
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
            // `"%d items" % n` on a literal format string, before either side
            // is lowered: the arguments are needed individually.
            if matches!(op, ast::BinOp::Mod)
                && let ast::ExprKind::Str(fmt) = &left.kind
            {
                return lower_percent_format(fmt, right, expr.span, ctx);
            }
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
            lower_binary(*op, l, r, expr.span, ctx)
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
pub(crate) fn lower_yield_from(
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
pub(crate) fn lower_compare_chain(
    prev: ir::Expr,
    rest: &[(ast::BinOp, ast::Expr)],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let (op, operand) = &rest[0];
    let cur = lower_expr(operand, ctx)?;

    if rest.len() == 1 {
        return lower_binary(*op, prev, cur, span, ctx);
    }

    // Bind a() before b() so a() < b() < c() evaluates operands in source order.
    let prev_temp = ctx.fresh_temp("cmp.prev", prev.ty);
    let prev_local = local_expr(prev_temp.clone(), prev.ty);
    let cur_ty = cur.ty;
    let temp = ctx.fresh_temp("cmp", cur_ty);
    let temp_local = ir::Expr {
        ty: cur_ty,
        kind: ir::ExprKind::Local(temp.clone()),
    };

    let head = lower_binary(*op, prev_local, temp_local.clone(), span, ctx)?;
    let tail = lower_compare_chain(temp_local, &rest[1..], span, ctx)?;

    Ok(ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Let {
            name: prev_temp,
            value: Box::new(prev),
            body: Box::new(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Let {
                    name: temp,
                    value: Box::new(cur),
                    body: Box::new(bool_and(head, tail)),
                },
            }),
        },
    })
}

/// How one comprehension generator advances each iteration.
pub(crate) enum CompIterKind {
    /// Index or range: bind runs directly in the while body.
    Indexed,
    /// User iterator: try `__next__`; StopIteration clears `more`.
    StopTry {
        next_assign: Box<ir::Stmt>,
        more: String,
    },
    /// Generator / file: prelude, then if exhausted clear `more`, else bind.
    ExhaustIf {
        prelude: Vec<ir::Stmt>,
        exhausted: ir::Expr,
        more: String,
    },
}

pub(crate) struct CompIterParts {
    pub(crate) cond: ir::Expr,
    pub(crate) step: Vec<ir::Stmt>,
    pub(crate) element: ir::Expr,
    pub(crate) cap: Option<ir::Expr>,
    pub(crate) kind: CompIterKind,
}

/// One prepared `for` level inside a list comprehension.
pub(crate) struct CompLevel {
    /// Stmts that run before this level's while (at the appropriate nesting).
    pub(crate) setup: Vec<ir::Stmt>,
    pub(crate) cond: ir::Expr,
    pub(crate) step: Vec<ir::Stmt>,
    /// Bind the iteration element into the target.
    pub(crate) bind: Vec<ir::Stmt>,
    /// Filters for this generator (`if` clauses), already lowered.
    pub(crate) ifs: Vec<ir::Expr>,
    /// Exact capacity when knowable (only used for a single unfiltered gen).
    pub(crate) cap: Option<ir::Expr>,
    pub(crate) kind: CompIterKind,
}

pub(crate) fn assign_const_bool(name: String, value: bool) -> ir::Stmt {
    ir::Stmt::Assign {
        name,
        value: ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ConstBool(value),
        },
    }
}

pub(crate) fn push_comp_more(ctx: &mut FnCtx, setup: &mut Vec<ir::Stmt>) -> (String, ir::Expr) {
    let more_t = ctx.fresh_temp("comp.more", ir::Ty::Bool);
    setup.push(assign_const_bool(more_t.clone(), true));
    (
        more_t.clone(),
        ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::Local(more_t),
        },
    )
}

pub(crate) fn homogeneous_tuple_elem(elems: &[ir::Ty], span: Span) -> SResult<ir::Ty> {
    if elems.is_empty() {
        return Ok(ir::Ty::Int);
    }
    let t0 = elems[0];
    if elems.iter().all(|e| *e == t0) {
        Ok(t0)
    } else {
        Err(err(
            "iterating a heterogeneous tuple is not supported yet; \
             unpack or index with constants",
            span,
        ))
    }
}

/// The loop body for one cursor: try to produce an element, and run `payload`
/// if one appeared.
///
/// Shared by comprehensions, drains and `for` loops, so all three agree on
/// what a cursor means.
pub(crate) fn comp_kind_body(kind: CompIterKind, payload: Vec<ir::Stmt>) -> Vec<ir::Stmt> {
    match kind {
        CompIterKind::Indexed => payload,
        CompIterKind::StopTry { next_assign, more } => vec![ir::Stmt::Try {
            body: vec![*next_assign],
            handlers: vec![(
                Some(vec![ir::ExcType::StopIteration]),
                None,
                vec![assign_const_bool(more, false)],
            )],
            orelse: payload,
            finally: vec![],
        }],
        CompIterKind::ExhaustIf {
            prelude,
            exhausted,
            more,
        } => {
            let mut b = prelude;
            b.push(ir::Stmt::If {
                branches: vec![(exhausted, vec![assign_const_bool(more, false)])],
                orelse: payload,
            });
            b
        }
    }
}

/// A cursor's advance, split so it can be nested inside another cursor's.
///
/// Returns the statements that attempt to produce the next element, and the
/// test that says whether one appeared. Composition needs this shape and the
/// three kinds do not share it: `Indexed` tests before producing, while the
/// other two produce and then discover exhaustion.
pub(crate) fn parts_to_advance(parts: &CompIterParts) -> (Vec<ir::Stmt>, ir::Expr) {
    match &parts.kind {
        CompIterKind::Indexed => (Vec::new(), parts.cond.clone()),
        CompIterKind::StopTry { next_assign, more } => (
            vec![ir::Stmt::Try {
                body: vec![(**next_assign).clone()],
                handlers: vec![(
                    Some(vec![ir::ExcType::StopIteration]),
                    None,
                    vec![assign_const_bool(more.clone(), false)],
                )],
                orelse: Vec::new(),
                finally: Vec::new(),
            }],
            parts.cond.clone(),
        ),
        CompIterKind::ExhaustIf {
            prelude,
            exhausted,
            more,
        } => {
            let mut advance = prelude.clone();
            advance.push(ir::Stmt::If {
                branches: vec![(
                    exhausted.clone(),
                    vec![assign_const_bool(more.clone(), false)],
                )],
                orelse: Vec::new(),
            });
            (advance, parts.cond.clone())
        }
    }
}

pub(crate) fn wrap_comp_level(level: CompLevel, inner: Vec<ir::Stmt>) -> Vec<ir::Stmt> {
    let mut payload = level.bind;
    payload.extend(wrap_comp_ifs(&level.ifs, inner));
    let while_stmt = ir::Stmt::While {
        cond: level.cond,
        body: comp_kind_body(level.kind, payload),
        step: level.step,
    };
    let mut wrapped = level.setup;
    wrapped.push(while_stmt);
    wrapped
}

/// Whether `e` is a call to a builtin that this module can advance lazily,
/// rather than by materializing a list first.
pub(crate) fn is_lazy_combinator(e: &ast::Expr, ctx: &FnCtx) -> bool {
    matches!(&e.kind, ast::ExprKind::Call { func, .. }
        if matches!(func.as_str(), "zip" | "enumerate" | "map" | "filter")
            && !ctx.funcs().contains_key(func.as_str()))
}

/// `zip(a, b, ...)` as one cursor.
///
/// The components are advanced **inside each other**, left to right:
/// component *k+1* is only advanced when component *k* produced an element.
/// That is CPython's order, and it is the whole point — the previous
/// lowering drained every argument into a list before pairing them, so
/// `list(zip(infinite(), [1]))` never reached the shortest input and did not
/// terminate.
///
/// The generated shape, for two components:
///
/// ```text
/// setup:  <a's setup>; <b's setup>; more = True; done = False
/// body:   <a's advance>
///         if <a produced>:
///             e0 = <a's element>; <a's step>
///             <b's advance>
///             if <b produced>:
///                 e1 = <b's element>; <b's step>
///             else: done = True
///         else: done = True
///         if done: more = False else: <payload with (e0, e1)>
/// ```
pub(crate) fn zip_parts(
    args: &[&ast::Expr],
    ctx: &mut FnCtx,
    setup: &mut Vec<ir::Stmt>,
) -> SResult<CompIterParts> {
    if args.is_empty() {
        // CPython: `list(zip())` is `[]`. A cursor that never produces is the
        // composition-friendly way to say that.
        let (more_t, more_local) = push_comp_more(ctx, setup);
        return Ok(CompIterParts {
            cond: more_local,
            step: Vec::new(),
            element: ir::Expr {
                ty: ir::tuple_of(&[]),
                kind: ir::ExprKind::TupleLit(Vec::new()),
            },
            cap: Some(int_const(0)),
            kind: CompIterKind::ExhaustIf {
                prelude: Vec::new(),
                exhausted: ir::Expr {
                    ty: ir::Ty::Bool,
                    kind: ir::ExprKind::ConstBool(true),
                },
                more: more_t,
            },
        });
    }
    // Component setups run first and in source order, so `zip(a(), b())`
    // evaluates `a()` before `b()` exactly as a call would.
    let mut components: Vec<(CompIterParts, String)> = Vec::new();
    for arg in args {
        let parts = lower_comp_iter(arg, false, ctx, setup)?;
        let slot = ctx.fresh_temp("zip.elem", parts.element.ty);
        components.push((parts, slot));
    }
    let (more_t, more_local) = push_comp_more(ctx, setup);
    // A separate `done` flag rather than negating `more`: the IR has no
    // boolean not, and testing a flag set by the advance is clearer than
    // threading an inverted condition through the nesting.
    let done_t = ctx.fresh_temp("zip.done", ir::Ty::Bool);
    setup.push(assign_const_bool(done_t.clone(), false));

    let elem_tys: Vec<ir::Ty> = components.iter().map(|(p, _)| p.element.ty).collect();
    let element = ir::Expr {
        ty: ir::tuple_of(&elem_tys),
        kind: ir::ExprKind::TupleLit(
            components
                .iter()
                .map(|(p, slot)| ir::Expr {
                    ty: p.element.ty,
                    kind: ir::ExprKind::Local(slot.clone()),
                })
                .collect(),
        ),
    };

    // Build the nesting from the inside out, so the first component ends up
    // outermost and the last one is only reached when all before it produced.
    let mut prelude: Vec<ir::Stmt> = Vec::new();
    for (parts, slot) in components.into_iter().rev() {
        let (advance, produced) = parts_to_advance(&parts);
        let mut then = vec![ir::Stmt::Assign {
            name: slot,
            value: parts.element,
        }];
        then.extend(parts.step);
        then.extend(prelude);
        let mut block = advance;
        block.push(ir::Stmt::If {
            branches: vec![(produced, then)],
            orelse: vec![assign_const_bool(done_t.clone(), true)],
        });
        prelude = block;
    }

    Ok(CompIterParts {
        cond: more_local,
        step: Vec::new(),
        element,
        // The components' lengths are not all knowable, and the shortest
        // decides; presizing is given up rather than guessed.
        cap: None,
        kind: CompIterKind::ExhaustIf {
            prelude,
            exhausted: ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Local(done_t),
            },
            more: more_t,
        },
    })
}

/// `enumerate(it)` / `enumerate(it, start)` as one cursor.
///
/// The counter rides along on the inner cursor: same exhaustion shape, one
/// extra step. No list and no intermediate `list[tuple[int, T]]`.
pub(crate) fn enumerate_parts(
    args: &[&ast::Expr],
    keywords: &[ast::Keyword],
    span: Span,
    ctx: &mut FnCtx,
    setup: &mut Vec<ir::Stmt>,
) -> SResult<CompIterParts> {
    // The argument contract is unchanged from the eager lowering; only how
    // the sequence is consumed differs.
    if let Some(kw) = keywords.iter().find(|k| k.name != "start") {
        return Err(err(
            "enumerate() only supports the optional start= keyword",
            kw.name_span,
        ));
    }
    if args.is_empty() || args.len() > 2 {
        return Err(err(
            format!(
                "enumerate() takes 1 or 2 positional arguments ({} given)",
                args.len()
            ),
            span,
        ));
    }
    if args.len() == 2 && keywords.iter().any(|k| k.name == "start") {
        return Err(err(
            "enumerate() got multiple values for argument 'start'",
            span,
        ));
    }
    // Iterable first, then `start`: CPython evaluates arguments left to right.
    let inner = lower_comp_iter(args[0], false, ctx, setup)?;
    let start = if let Some(kw) = keywords.iter().find(|k| k.name == "start") {
        let v = lower_expr(&kw.value, ctx)?;
        coerce(v, ir::Ty::Int, kw.value.span, "enumerate start")?
    } else if let Some(a) = args.get(1) {
        let v = lower_expr(a, ctx)?;
        coerce(v, ir::Ty::Int, a.span, "enumerate start")?
    } else {
        int_const(0)
    };
    let idx_t = ctx.fresh_temp("enum.i", ir::Ty::Int);
    setup.push(ir::Stmt::Assign {
        name: idx_t.clone(),
        value: start,
    });
    let idx_local = ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::Local(idx_t.clone()),
    };

    let elem_ty = inner.element.ty;
    let element = ir::Expr {
        ty: ir::tuple_of(&[ir::Ty::Int, elem_ty]),
        kind: ir::ExprKind::TupleLit(vec![idx_local.clone(), inner.element]),
    };
    let mut step = inner.step;
    step.push(ir::Stmt::Assign {
        name: idx_t,
        value: ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Binary {
                op: ir::BinOp::Add,
                left: Box::new(idx_local),
                right: Box::new(int_const(1)),
            },
        },
    });
    Ok(CompIterParts {
        cond: inner.cond,
        step,
        element,
        cap: inner.cap,
        kind: inner.kind,
    })
}

/// `map(f, it)` as one cursor.
///
/// The inner cursor's element, with `f` applied. Nothing else changes — the
/// exhaustion shape, the step and the capacity all pass through — so `map`
/// over a list stays `Indexed` and allocation-free, and `map` over a
/// generator stays lazy.
pub(crate) fn map_parts(
    args: &[&ast::Expr],
    span: Span,
    ctx: &mut FnCtx,
    setup: &mut Vec<ir::Stmt>,
) -> SResult<CompIterParts> {
    if args.len() != 2 {
        return Err(err(
            format!(
                "map() takes a function and one iterable ({} arguments given); \
                 mapping over several iterables at once is not supported yet",
                args.len()
            ),
            span,
        ));
    }
    // The iterable is lowered first because resolving the callable needs the
    // element type, but its setup is emitted *second*, so a side-effecting
    // callable expression still runs before the iterable — CPython's
    // left-to-right argument order.
    let mut iter_setup = Vec::new();
    let inner = lower_comp_iter(args[1], false, ctx, &mut iter_setup)?;
    let (key, _) = resolve_sort_key(args[0], inner.element.ty, ctx)?;
    let (key, key_setup) = bind_sort_key(key, ctx);
    setup.extend(key_setup);
    setup.extend(iter_setup);

    let element = call_sort_key(&key, inner.element, args[1].span, ctx)?;
    Ok(CompIterParts {
        cond: inner.cond,
        step: inner.step,
        element,
        cap: inner.cap,
        kind: inner.kind,
    })
}

/// `filter(f, it)` as one cursor, and `filter(None, it)` for truthiness.
///
/// Filtering cannot be a guard around the loop body: a cursor advances once
/// per iteration, and a skipped element must not be *paired* by an enclosing
/// `zip`. So the advance itself loops until it either finds a passing element
/// or exhausts the input, which keeps `ok` meaning "an element the consumer
/// should see" and lets a filtered cursor compose like any other.
pub(crate) fn filter_parts(
    args: &[&ast::Expr],
    span: Span,
    ctx: &mut FnCtx,
    setup: &mut Vec<ir::Stmt>,
) -> SResult<CompIterParts> {
    if args.len() != 2 {
        return Err(err(
            format!(
                "filter() takes a predicate (or None) and one iterable \
                 ({} arguments given)",
                args.len()
            ),
            span,
        ));
    }
    let mut iter_setup = Vec::new();
    let inner = lower_comp_iter(args[1], false, ctx, &mut iter_setup)?;
    let elem_ty = inner.element.ty;
    let pred = match &args[0].kind {
        // `filter(None, xs)` keeps the truthy elements.
        ast::ExprKind::NoneLit => Option::None,
        _ => {
            let (key, key_setup) = {
                let (key, _) = resolve_sort_key(args[0], elem_ty, ctx)?;
                bind_sort_key(key, ctx)
            };
            setup.extend(key_setup);
            Some(key)
        }
    };
    setup.extend(iter_setup);

    let (more_t, more_local) = push_comp_more(ctx, setup);
    let found_t = ctx.fresh_temp("filter.found", ir::Ty::Bool);
    let done_t = ctx.fresh_temp("filter.done", ir::Ty::Bool);
    setup.push(assign_const_bool(done_t.clone(), false));
    let slot = ctx.fresh_temp("filter.elem", elem_ty);
    let found_local = local_expr(found_t.clone(), ir::Ty::Bool);
    let done_local = local_expr(done_t.clone(), ir::Ty::Bool);

    // if <predicate holds>: found = True
    let kept = match &pred {
        Some(key) => {
            let call = call_sort_key(key, local_expr(slot.clone(), elem_ty), args[0].span, ctx)?;
            to_bool(call, args[0].span, ctx)?
        }
        Option::None => to_bool(local_expr(slot.clone(), elem_ty), args[1].span, ctx)?,
    };

    let (advance, produced) = parts_to_advance(&inner);
    let mut attempt = advance;
    let mut keep = vec![ir::Stmt::Assign {
        name: slot.clone(),
        value: inner.element,
    }];
    keep.extend(inner.step);
    keep.push(ir::Stmt::If {
        branches: vec![(kept, vec![assign_const_bool(found_t.clone(), true)])],
        orelse: Vec::new(),
    });
    attempt.push(ir::Stmt::If {
        branches: vec![(produced, keep)],
        orelse: vec![assign_const_bool(done_t.clone(), true)],
    });

    // found = False; while not found and not done: <attempt>
    let prelude = vec![
        assign_const_bool(found_t.clone(), false),
        ir::Stmt::While {
            cond: bool_and(bool_not(found_local), bool_not(done_local.clone())),
            body: attempt,
            step: Vec::new(),
        },
    ];

    Ok(CompIterParts {
        cond: more_local,
        step: Vec::new(),
        element: local_expr(slot, elem_ty),
        // How many survive the predicate is not knowable, so no presize.
        cap: Option::None,
        kind: CompIterKind::ExhaustIf {
            prelude,
            exhausted: done_local,
            more: more_t,
        },
    })
}

/// Build loop setup for one comprehension generator.
/// Appends setup into `setup`.
pub(crate) fn lower_comp_iter(
    iter: &ast::Expr,
    want_cap: bool,
    ctx: &mut FnCtx,
    setup: &mut Vec<ir::Stmt>,
) -> SResult<CompIterParts> {
    if let ast::ExprKind::Call {
        func,
        args,
        keywords,
        ..
    } = &iter.kind
        && matches!(func.as_str(), "zip" | "enumerate" | "map" | "filter")
        && !ctx.funcs().contains_key(func.as_str())
    {
        let plain = require_plain_args(args, func, iter.span)?;
        return match func.as_str() {
            "zip" => zip_parts(&plain, ctx, setup),
            "map" => map_parts(&plain, iter.span, ctx, setup),
            "filter" => filter_parts(&plain, iter.span, ctx, setup),
            _ => enumerate_parts(&plain, keywords, iter.span, ctx, setup),
        };
    }
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
        // start, then stop, then step -- see `lower_for_range` for why the
        // assignment order is the evaluation order.
        let it_t = ctx.fresh_temp("comp.it", ir::Ty::Int);
        setup.push(ir::Stmt::Assign {
            name: it_t.clone(),
            value: start,
        });
        let it_local = ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Local(it_t.clone()),
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
        return Ok(CompIterParts {
            cond: loop_cond,
            step: vec![step_stmt],
            element: it_local,
            cap,
            kind: CompIterKind::Indexed,
        });
    }

    let seq = lower_expr(iter, ctx)?;
    match seq.ty {
        ir::Ty::List(_) | ir::Ty::Str | ir::Ty::Tuple(_) | ir::Ty::Dict { .. } | ir::Ty::Set(_) => {
            lower_comp_indexed(seq, want_cap, iter.span, ctx, setup)
        }
        ir::Ty::File => lower_comp_file(seq, iter.span, ctx, setup),
        ir::Ty::Generator { yield_ty } => lower_comp_generator(seq, *yield_ty, ctx, setup),
        ir::Ty::Class(id) if resolve_method(id, "__iter__").is_some() => {
            lower_comp_user_iter(seq, id, iter.span, ctx, setup)
        }
        // A dynamic value is iterated by index, like the other indexed
        // sequences; the runtime decides what the i-th element is.
        ir::Ty::Any => lower_comp_indexed(seq, want_cap, iter.span, ctx, setup),
        other => Err(err(
            format!("'{}' object is not iterable", display_ty(other)),
            iter.span,
        )),
    }
}

pub(crate) fn lower_comp_indexed(
    seq: ir::Expr,
    want_cap: bool,
    span: Span,
    ctx: &mut FnCtx,
    setup: &mut Vec<ir::Stmt>,
) -> SResult<CompIterParts> {
    let seq = match seq.ty {
        ir::Ty::Dict { key, .. } => ir::Expr {
            ty: ir::list_of(*key),
            kind: ir::ExprKind::DictKeys(Box::new(seq)),
        },
        ir::Ty::Set(elem) => ir::Expr {
            ty: ir::list_of(*elem),
            kind: ir::ExprKind::SetToList(Box::new(seq)),
        },
        _ => seq,
    };
    let src_elem_ty = match &seq.ty {
        ir::Ty::List(e) => **e,
        ir::Ty::Str => ir::Ty::Str,
        ir::Ty::Tuple(elems) => homogeneous_tuple_elem(elems, span)?,
        // A dynamic value yields dynamic elements.
        ir::Ty::Any => ir::Ty::Any,
        other => {
            return Err(err(
                format!("internal error: lower_comp_indexed on {other}"),
                span,
            ));
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
    let seq_local = local_expr(seq_t, seq_ty);
    let idx_local = local_expr(idx_t.clone(), ir::Ty::Int);
    let cond = int_cmp(
        ir::BinOp::Lt,
        idx_local.clone(),
        ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Len(Box::new(seq_local.clone())),
        },
    );
    // A dynamic sequence uses the iteration accessor: iterating a dict
    // yields its keys, while `d[0]` would look one up.
    let element = if seq_ty == ir::Ty::Any {
        ir::Expr {
            ty: src_elem_ty,
            kind: ir::ExprKind::AnyIterGet {
                base: Box::new(seq_local.clone()),
                index: Box::new(idx_local.clone()),
            },
        }
    } else {
        ir::Expr {
            ty: src_elem_ty,
            kind: ir::ExprKind::Index {
                base: Box::new(seq_local.clone()),
                index: Box::new(idx_local.clone()),
            },
        }
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
    Ok(CompIterParts {
        cond,
        step: vec![step_stmt],
        element,
        cap,
        kind: CompIterKind::Indexed,
    })
}

pub(crate) fn lower_comp_file(
    file: ir::Expr,
    span: Span,
    ctx: &mut FnCtx,
    setup: &mut Vec<ir::Stmt>,
) -> SResult<CompIterParts> {
    let file_t = ctx.fresh_temp("comp.file", ir::Ty::File);
    setup.push(ir::Stmt::Assign {
        name: file_t.clone(),
        value: file,
    });
    let (more, cond) = push_comp_more(ctx, setup);
    let line_t = ctx.fresh_temp("comp.line", ir::Ty::Str);
    let line_local = local_expr(line_t.clone(), ir::Ty::Str);
    let prelude = vec![ir::Stmt::Assign {
        name: line_t,
        value: ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::FileCall {
                func: ir::FileFn::ReadLine,
                args: vec![local_expr(file_t, ir::Ty::File)],
            },
        },
    }];
    let truthy = to_bool(line_local.clone(), span, ctx)?;
    let exhausted = ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Unary {
            op: ir::UnOp::Not,
            operand: Box::new(truthy),
        },
    };
    Ok(CompIterParts {
        cond,
        step: vec![],
        element: line_local,
        cap: None,
        kind: CompIterKind::ExhaustIf {
            prelude,
            exhausted,
            more,
        },
    })
}

pub(crate) fn lower_comp_generator(
    gen_expr: ir::Expr,
    yield_ty: ir::Ty,
    ctx: &mut FnCtx,
    setup: &mut Vec<ir::Stmt>,
) -> SResult<CompIterParts> {
    let gen_ty = gen_expr.ty;
    let gen_t = ctx.fresh_temp("comp.gen", gen_ty);
    setup.push(ir::Stmt::Assign {
        name: gen_t.clone(),
        value: gen_expr,
    });
    let (more, cond) = push_comp_more(ctx, setup);
    let opt_ty = ir::optional_of(yield_ty);
    let nxt_t = ctx.fresh_temp("comp.gnext", opt_ty);
    let nxt_local = local_expr(nxt_t.clone(), opt_ty);
    let prelude = vec![ir::Stmt::Assign {
        name: nxt_t,
        value: ir::Expr {
            ty: opt_ty,
            kind: ir::ExprKind::GeneratorNext {
                generator: Box::new(local_expr(gen_t, gen_ty)),
                send: Box::new(const_none()),
            },
        },
    }];
    let exhausted = ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::IsNone {
            value: Box::new(nxt_local.clone()),
            not: false,
        },
    };
    let element = ir::Expr {
        ty: yield_ty,
        kind: ir::ExprKind::FromUnion {
            value: Box::new(nxt_local),
        },
    };
    Ok(CompIterParts {
        cond,
        step: vec![],
        element,
        cap: None,
        kind: CompIterKind::ExhaustIf {
            prelude,
            exhausted,
            more,
        },
    })
}

pub(crate) fn lower_comp_user_iter(
    obj: ir::Expr,
    class_id: ir::ClassId,
    span: Span,
    ctx: &mut FnCtx,
    setup: &mut Vec<ir::Stmt>,
) -> SResult<CompIterParts> {
    let it_call = lower_instance_method_call(obj, class_id, "__iter__", span, &[], ctx)?;
    let it_ty = it_call.ty;
    let ir::Ty::Class(it_id) = it_ty else {
        return Err(err(
            format!("__iter__ must return a class instance, found {it_ty}"),
            span,
        ));
    };
    let Some(next_func) = resolve_method(it_id, "__next__") else {
        return Err(err("iterator from __iter__ must define __next__", span));
    };
    let next_sig = method_sig_lookup(&next_func)
        .ok_or_else(|| err("internal error: missing signature for __next__", span))?;
    let yield_ty = next_sig.ret;
    if yield_ty == ir::Ty::None {
        return Err(err("__next__ must return a non-None value type", span));
    }
    let it_t = ctx.fresh_temp("comp.it", it_ty);
    setup.push(ir::Stmt::Assign {
        name: it_t.clone(),
        value: it_call,
    });
    let (more, cond) = push_comp_more(ctx, setup);
    let nxt_t = ctx.fresh_temp("comp.inext", yield_ty);
    let it_local = local_expr(it_t, it_ty);
    let next_call = lower_instance_method_call(it_local, it_id, "__next__", span, &[], ctx)?;
    let next_assign = ir::Stmt::Assign {
        name: nxt_t.clone(),
        value: next_call,
    };
    Ok(CompIterParts {
        cond,
        step: vec![],
        element: local_expr(nxt_t, yield_ty),
        cap: None,
        kind: CompIterKind::StopTry {
            next_assign: Box::new(next_assign),
            more,
        },
    })
}

/// Bind a comprehension target: simple names use hidden storage (no leak);
/// unpack / index targets bind real locals via `lower_assign_ir`.
pub(crate) fn bind_comp_target(
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
pub(crate) fn wrap_comp_ifs(ifs: &[ir::Expr], inner: Vec<ir::Stmt>) -> Vec<ir::Stmt> {
    let mut body = inner;
    for c in ifs.iter().rev() {
        body = vec![ir::Stmt::If {
            branches: vec![(c.clone(), body)],
            orelse: vec![],
        }];
    }
    body
}

/// Build a `list[E]` from prepared comprehension parts — the loop
/// `[x for x in it]` runs, minus the user-written target binding.
pub(crate) fn drain_parts_to_list(
    parts: CompIterParts,
    setup: Vec<ir::Stmt>,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let list_ty = ir::list_of(parts.element.ty);
    let res_t = ctx.fresh_temp("drain", list_ty);
    let res_local = || ir::Expr {
        ty: list_ty,
        kind: ir::ExprKind::Local(res_t.clone()),
    };
    let mut stmts = setup;
    stmts.push(ir::Stmt::Assign {
        name: res_t.clone(),
        value: ir::Expr {
            ty: list_ty,
            kind: ir::ExprKind::ListNew {
                cap: Box::new(int_const(4)),
            },
        },
    });
    let append = ir::Stmt::ListAppend {
        list: res_local(),
        value: parts.element,
    };
    let level = CompLevel {
        setup: Vec::new(),
        cond: parts.cond,
        step: parts.step,
        // No target to bind: this drain has no user-written loop variable.
        bind: Vec::new(),
        ifs: Vec::new(),
        cap: parts.cap,
        kind: parts.kind,
    };
    stmts.extend(wrap_comp_level(level, vec![append]));
    Ok(ir::Expr {
        ty: list_ty,
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(res_local()),
        },
    })
}

/// Materialize a generator into a `list[yield_ty]`, exactly as
/// `[x for x in gen]` does.
pub(crate) fn drain_generator_to_list(
    gen_expr: ir::Expr,
    yield_ty: ir::Ty,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let mut setup = Vec::new();
    let parts = lower_comp_generator(gen_expr, yield_ty, ctx, &mut setup)?;
    drain_parts_to_list(parts, setup, ctx)
}

/// Materialize any already-lowered iterable into a `list`.
///
/// A list passes through untouched — it is already the shape callers want,
/// and copying it would change identity for no reason. Everything else goes
/// through the same comprehension machinery `[x for x in it]` uses, so the
/// element type and iteration order are whatever a `for` loop would produce.
///
/// Sound for the builtins that consume their whole argument. `any` and `all`
/// short-circuit and must not come through here.
pub(crate) fn materialize_iterable_value(value: ir::Expr, ctx: &mut FnCtx) -> SResult<ir::Expr> {
    match value.ty {
        ir::Ty::List(_) => Ok(value),
        ir::Ty::Generator { yield_ty } => drain_generator_to_list(value, *yield_ty, ctx),
        ir::Ty::Str | ir::Ty::Tuple(_) | ir::Ty::Dict { .. } | ir::Ty::Set(_) => {
            let mut setup = Vec::new();
            let span = Span::default();
            let parts = lower_comp_indexed(value, false, span, ctx, &mut setup)?;
            drain_parts_to_list(parts, setup, ctx)
        }
        // Not an iterable this handles: leave it for the caller's own
        // diagnostic, which can name the builtin.
        _ => Ok(value),
    }
}

/// Lower an argument that a builtin will iterate, as a `list`.
///
/// `range(...)` is not a first-class value here, so it cannot be lowered and
/// then converted; when lowering fails, the expression is materialized
/// through a synthesized `[x for x in it]` instead, which is the path the
/// comprehension machinery already handles it on. Probing rather than
/// enumerating keeps this correct as more iterables become values.
pub(crate) fn materialize_iterable_arg(e: &ast::Expr, ctx: &mut FnCtx) -> SResult<ir::Expr> {
    // A combinator *can* be lowered as a value -- into a materialized list --
    // so probing `lower_expr` first would find the eager path and never
    // reach the lazy one. These are taken before the probe, not after it.
    if is_lazy_combinator(e, ctx) {
        let mut setup = Vec::new();
        let parts = lower_comp_iter(e, false, ctx, &mut setup)?;
        return drain_parts_to_list(parts, setup, ctx);
    }
    match lower_expr(e, ctx) {
        Ok(v) => materialize_iterable_value(v, ctx),
        Err(direct) => {
            let mut setup = Vec::new();
            let parts = lower_comp_iter(e, false, ctx, &mut setup).map_err(|_| direct)?;
            drain_parts_to_list(parts, setup, ctx)
        }
    }
}

/// `[elem for target in iter if cond ... for ...]` desugars to nested loops
/// building a list inside an expression-level Block. Simple name targets live
/// in hidden storage (Python 3: shadow, do not leak). Unpack targets bind real
/// locals. Fast path: single generator, no filters, knowable length → presize
/// + unchecked append.
pub(crate) fn lower_list_comp(
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
        let parts = lower_comp_iter(&clause.iter, want_cap, ctx, &mut setup)?;
        let (bind, n_renames) = bind_comp_target(&clause.target, parts.element, ctx)?;
        renames_pushed += n_renames;
        let mut ifs = Vec::with_capacity(clause.ifs.len());
        for c in &clause.ifs {
            ifs.push(lower_condition(c, ctx)?);
        }
        levels.push(CompLevel {
            setup,
            cond: parts.cond,
            step: parts.step,
            bind,
            ifs,
            cap: parts.cap,
            kind: parts.kind,
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
        inner_body = wrap_comp_level(level, inner_body);
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
pub(crate) fn lower_comp_levels(
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
        let parts = lower_comp_iter(&clause.iter, false, ctx, &mut setup)?;
        let (bind, n_renames) = bind_comp_target(&clause.target, parts.element, ctx)?;
        renames_pushed += n_renames;
        let mut ifs = Vec::with_capacity(clause.ifs.len());
        for c in &clause.ifs {
            ifs.push(lower_condition(c, ctx)?);
        }
        levels.push(CompLevel {
            setup,
            cond: parts.cond,
            step: parts.step,
            bind,
            ifs,
            cap: parts.cap,
            kind: parts.kind,
        });
    }
    Ok((levels, renames_pushed))
}

pub(crate) fn nest_comp_body(levels: Vec<CompLevel>, inner_body: Vec<ir::Stmt>) -> Vec<ir::Stmt> {
    let mut body = inner_body;
    for level in levels.into_iter().rev() {
        body = wrap_comp_level(level, body);
    }
    body
}

pub(crate) fn lower_set_comp(
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
    if !is_hashable_key_ty(elem_ty) {
        return Err(err(
            format!(
                "set comprehension elements must be int, str, or a tuple of \
                 those, found {elem_ty}"
            ),
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

pub(crate) fn lower_dict_comp(
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
    if !is_hashable_key_ty(key_ty) {
        return Err(err(
            format!(
                "dict comprehension keys must be int, str, or a tuple of those, \
                 found {key_ty}"
            ),
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

pub(crate) fn lower_list_lit(
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
                            // `seed_join`, not `join_elem_types`: an empty `[]`
                            // element is a provisional `list[Any]`, so
                            // `[["a"], []]` is a `list[list[str]]` rather than
                            // a type error.
                            Some(prev) => seed_join(prev, item.ty).ok_or_else(|| {
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

pub(crate) fn lower_tuple_lit(
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

pub(crate) fn lower_dict_lit(
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
                // `seed_join`, not `join_elem_types`: an empty `[]` value is a
                // provisional `list[Any]`, so `{"a": ["b"], "d": []}` is a
                // `dict[str, list[str]]` rather than a type error.
                vt = seed_join(vt, vr.ty).unwrap_or(vt);
                if vt != vr.ty {
                    vt = seed_join(vt, vr.ty).ok_or_else(|| {
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

pub(crate) fn lower_set_lit(
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

pub(crate) fn join_elem_types(a: ir::Ty, b: ir::Ty) -> Option<ir::Ty> {
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
        // Python keeps each element's own type: `[1, 2.5, 1]` is
        // `[1, 2.5, 1]`, not `[1.0, 2.5, 1.0]`. Collapsing to one numeric type
        // changed printed values and `is`/`==` results, so mixed numerics
        // become a union and keep per-element identity. Homogeneous lists are
        // unaffected and keep their optimized storage.
        (ir::Ty::Float, ir::Ty::Int)
        | (ir::Ty::Int, ir::Ty::Float)
        | (ir::Ty::Float, ir::Ty::Bool)
        | (ir::Ty::Bool, ir::Ty::Float)
        | (ir::Ty::Int, ir::Ty::Bool)
        | (ir::Ty::Bool, ir::Ty::Int) => {
            // Built here rather than through `join_types`, which collapses
            // numerics to one type. That rule is right for a scalar variable's
            // storage (the documented "join of all assignments") but wrong for
            // container elements, which Python keeps individually typed.
            let mut members = ir::flatten_union_members(a);
            members.extend(ir::flatten_union_members(b));
            Some(ir::union_of(&members))
        }
        // Grow optionals/unions in homogeneous containers (list/dict values).
        _ if a == ir::Ty::None
            || b == ir::Ty::None
            || matches!(a, ir::Ty::Union(_))
            || matches!(b, ir::Ty::Union(_)) =>
        {
            Some(join_types(a, b))
        }
        // An empty `[]` is a *provisional* `list[Any]` and must yield to a
        // concrete `list[T]` rather than union with it: `{"x": [1], "y": []}`
        // is a `dict[str, list[int]]`, not a dict of two different list
        // types. `join_types` already owns that rule.
        (ir::Ty::List(e), ir::Ty::List(f)) if *e == ir::Ty::Any || *f == ir::Ty::Any => {
            Some(join_types(a, b))
        }
        // Two unrelated concrete types: the same union the numeric pairs
        // above produce, for the same reason — a container keeps each
        // element's own type, and `[1, "a"]` is a list in CPython. 0.89 built
        // this for numerics and scoped the rest out; nothing about the
        // representation was in the way, and `xs: list[int | str] = [1, "a"]`
        // has worked since unions existed.
        //
        // `File` and `Cell` are the exceptions, and they are not a policy
        // choice: a union member is stored as a tagged slot, and `elem_tag`
        // has no tag to give those two.
        _ if can_box_as_any(a) && can_box_as_any(b) => {
            let mut members = ir::flatten_union_members(a);
            members.extend(ir::flatten_union_members(b));
            Some(ir::union_of(&members))
        }
        _ => Option::None,
    }
}

/// Require plain (non-`*`) positional args — used by builtins and methods.
pub(crate) fn const_str_expr(s: &str) -> ir::Expr {
    ir::Expr {
        ty: ir::Ty::Str,
        kind: ir::ExprKind::ConstStr(s.to_string()),
    }
}

pub(crate) fn const_bool_expr(v: bool) -> ir::Expr {
    ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::ConstBool(v),
    }
}

/// `hex` / `bin` / `oct`: CPython `format(n, "#x")` / `"#b"` / `"#o"`.
pub(crate) fn lower_int_prefix_expr(
    name: &str,
    spec: &str,
    args: &[&ast::Expr],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.len() != 1 {
        return Err(err(
            format!("{name}() takes exactly one argument ({} given)", args.len()),
            span,
        ));
    }
    let arg = lower_expr(args[0], ctx)?;
    let arg = match arg.ty {
        ir::Ty::Bool => ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::BoolToInt(Box::new(arg)),
        },
        ir::Ty::Int => arg,
        other => {
            return Err(err(
                format!("'{other}' object cannot be interpreted as an integer"),
                args[0].span,
            ));
        }
    };
    Ok(ir::Expr {
        ty: ir::Ty::Str,
        kind: ir::ExprKind::FormatValue {
            value: Box::new(arg),
            spec: Box::new(const_str_expr(spec)),
        },
    })
}

pub(crate) fn is_numeric_ty(ty: ir::Ty) -> bool {
    matches!(ty, ir::Ty::Int | ir::Ty::Float | ir::Ty::Bool)
}

/// `divmod(a, b)` → `(a // b, a % b)` with operands evaluated once.
pub(crate) fn lower_divmod_expr(
    args: &[&ast::Expr],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.len() != 2 {
        return Err(err(
            format!("divmod expected 2 arguments, got {}", args.len()),
            span,
        ));
    }
    let l = lower_expr(args[0], ctx)?;
    let r = lower_expr(args[1], ctx)?;
    if !is_numeric_ty(l.ty) || !is_numeric_ty(r.ty) {
        return Err(err(
            format!(
                "unsupported operand type(s) for divmod(): '{}' and '{}'",
                l.ty, r.ty
            ),
            span,
        ));
    }
    let (l, r, ty) = unify_numeric(l, r, span, "divmod()")?;
    let a = ctx.fresh_temp("dm", ty);
    let b = ctx.fresh_temp("dm", ty);
    let loc = |name: &str| ir::Expr {
        ty,
        kind: ir::ExprKind::Local(name.to_string()),
    };
    let bin = |op: ir::BinOp, left: ir::Expr, right: ir::Expr| ir::Expr {
        ty,
        kind: ir::ExprKind::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
    };
    let pair = ir::Expr {
        ty: ir::tuple_of(&[ty, ty]),
        kind: ir::ExprKind::TupleLit(vec![
            bin(ir::BinOp::FloorDiv, loc(&a), loc(&b)),
            bin(ir::BinOp::Mod, loc(&a), loc(&b)),
        ]),
    };
    let inner = ir::Expr {
        ty: pair.ty,
        kind: ir::ExprKind::Let {
            name: b,
            value: Box::new(r),
            body: Box::new(pair),
        },
    };
    Ok(ir::Expr {
        ty: inner.ty,
        kind: ir::ExprKind::Let {
            name: a,
            value: Box::new(l),
            body: Box::new(inner),
        },
    })
}

pub(crate) fn as_pow_int(value: ir::Expr, span: Span, which: &str) -> SResult<ir::Expr> {
    match value.ty {
        ir::Ty::Bool => Ok(ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::BoolToInt(Box::new(value)),
        }),
        ir::Ty::Int => Ok(value),
        other => Err(err(
            format!("pow() {which} must be an integer, not {other}"),
            span,
        )),
    }
}

/// `pow(base, exp)` is `base ** exp`. `pow(base, exp, mod)` is modular.
pub(crate) fn lower_pow_expr(
    args: &[&ast::Expr],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.len() < 2 || args.len() > 3 {
        return Err(err(
            if args.len() < 2 {
                format!("pow expected at least 2 arguments, got {}", args.len())
            } else {
                format!("pow expected at most 3 arguments, got {}", args.len())
            },
            span,
        ));
    }
    let base = lower_expr(args[0], ctx)?;
    let exp = lower_expr(args[1], ctx)?;
    if args.len() == 2 {
        if !is_numeric_ty(base.ty) || !is_numeric_ty(exp.ty) {
            return Err(err(
                format!(
                    "unsupported operand type(s) for ** or pow(): '{}' and '{}'",
                    base.ty, exp.ty
                ),
                span,
            ));
        }
        let (l, r, ty) = unify_numeric(base, exp, span, "pow()")?;
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
        return Ok(ir::Expr {
            ty,
            kind: ir::ExprKind::Binary {
                op: ir::BinOp::Pow,
                left: Box::new(l),
                right: Box::new(r),
            },
        });
    }
    let modulus = lower_expr(args[2], ctx)?;
    if !matches!(base.ty, ir::Ty::Int | ir::Ty::Bool)
        || !matches!(exp.ty, ir::Ty::Int | ir::Ty::Bool)
        || !matches!(modulus.ty, ir::Ty::Int | ir::Ty::Bool)
    {
        return Err(err(
            "pow() 3rd argument not allowed unless all arguments are integers",
            span,
        ));
    }
    let base = as_pow_int(base, args[0].span, "base")?;
    let exp = as_pow_int(exp, args[1].span, "exp")?;
    let modulus = as_pow_int(modulus, args[2].span, "modulus")?;
    Ok(ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::PowMod {
            base: Box::new(base),
            exp: Box::new(exp),
            modulus: Box::new(modulus),
        },
    })
}

/// `print` `sep=` / `end=`: CPython accepts `str` or `None` (None → default).
pub(crate) fn coerce_print_sep_end(value: ir::Expr, which: &str, span: Span) -> SResult<ir::Expr> {
    match value.ty {
        ir::Ty::Str => Ok(value),
        ir::Ty::None => Ok(ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::Block {
                stmts: vec![ir::Stmt::ExprStmt(value)],
                result: Box::new(const_str_expr(if which == "sep" { " " } else { "\n" })),
            },
        }),
        other => Err(err(
            format!("print() {which} must be None or a string, not {other}"),
            span,
        )),
    }
}

pub(crate) fn require_plain_args<'a>(
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

pub(crate) fn lower_arg_expr(
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
pub(crate) fn lower_call_with_sig(
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
    // Positional arguments stop at the `*` marker; `params[kwonly_start..]`
    // can only be supplied by name.
    let positional_limit = sig.kwonly_start.unwrap_or(n);
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
                if positional_count < positional_limit {
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
                        if positional_limit < n {
                            format!(
                                "function '{display}' takes {positional_limit} positional \
                                 argument(s) but more were given ({} of its parameters are \
                                 keyword-only)",
                                n - positional_limit
                            )
                        } else {
                            format!(
                                "function '{display}' takes {n} argument(s) but more were given"
                            )
                        },
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
                let remaining_fixed = positional_limit.saturating_sub(positional_count);
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
        // A keyword may not name a positional-only parameter. When the
        // function has `**kwargs` the name lands there instead, which is
        // exactly what CPython does — that is the point of `/`.
        let named = sig
            .params
            .iter()
            .position(|p| p.name == kw.name)
            .filter(|idx| *idx >= sig.posonly_end);
        if let Some(idx) = named {
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
        } else if sig.params[..sig.posonly_end]
            .iter()
            .any(|p| p.name == kw.name)
        {
            return Err(err(
                format!(
                    "function '{display}' got some positional-only arguments \
                     passed as keyword arguments: '{name}'",
                    name = kw.name
                ),
                kw.name_span,
            ));
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
        } else if i >= positional_limit {
            return Err(err(
                format!(
                    "function '{display}' missing required keyword-only argument '{name}'",
                    name = p.name
                ),
                span,
            ));
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
pub(crate) fn resolve_parent_value(
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
pub(crate) fn resolve_reexport_value(
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
pub(crate) fn resolve_parent_func(
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

pub(crate) fn resolve_reexport_func(
    ctx: &FnCtx,
    module: &str,
    name: &str,
) -> Option<(String, FuncSig)> {
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
pub(crate) fn lower_module_call(
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

/// Drop flow-sensitive refinements for cell-backed names that a nested call
/// might overwrite via `nonlocal` (caller must not keep a stale concrete peel).
pub(crate) fn invalidate_cell_refinements_for_call(ctx: &mut FnCtx, info: &NestedFnInfo) {
    for (i, (name, _)) in info.captures.iter().enumerate() {
        if info.capture_is_cell.get(i).copied().unwrap_or(false) {
            ctx.type_refinements.remove(name);
        }
    }
}

/// `fs[0](x)` / general callable expression: first arg of synthetic `.call`.
pub(crate) fn lower_value_call(
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
pub(crate) fn lower_call_closure_value(
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
pub(crate) fn invalidate_all_cell_refinements(ctx: &mut FnCtx) {
    let names: Vec<String> = ctx.cell_locals.keys().cloned().collect();
    for n in names {
        ctx.type_refinements.remove(&n);
    }
}

pub(crate) fn lower_call(
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
    // Inside @classmethod: `cls(...)` constructs the *invoker* class (runtime
    // type_id of the cls token), not always the defining class.
    if let Some((cls_name, defining_id)) = ctx.classmethod_cls.clone()
        && func == cls_name
    {
        return lower_classmethod_construct(
            cls_name,
            defining_id,
            args,
            keywords,
            kwargs,
            span,
            ctx,
        );
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
    // A comprehension target is stored under a renamed local, so `[g(1) for g
    // in fs]` has to resolve `g` the same way the expression path's `Name` arm
    // does. Without this the call reports `function 'g' is not defined` while
    // the identical plain `for` loop works.
    if let Some((_, storage, ty)) = ctx
        .comp_renames
        .iter()
        .rev()
        .find(|(user, _, _)| user == func)
        .map(|(u, s, t)| (u.clone(), s.clone(), *t))
        && let ir::Ty::Closure { .. } = ty
    {
        let clos_expr = ir::Expr {
            ty,
            kind: ir::ExprKind::Local(storage),
        };
        return lower_call_closure_value(&clos_expr, args, keywords, kwargs, span, ctx);
    }
    // Call through a local/global of closure type first (local rebind shadows nested def).
    // Decorated free functions rebind the name to a Closure global — prefer that
    // over the original free-function Call target.
    // Cell-captured outer params (e.g. nested `f(x)` where f is outer) load via cell.
    if let Some(ty) = ctx.cell_locals.get(func).copied()
        && let ir::Ty::Closure { .. } = ty
        && ctx.locals.contains_key(&format!(".cell.{func}"))
    {
        let cell = ir::Expr {
            ty: ir::cell_of(ty),
            kind: ir::ExprKind::Local(format!(".cell.{func}")),
        };
        let clos_expr = ir::Expr {
            ty,
            kind: ir::ExprKind::CellLoad(Box::new(cell)),
        };
        return lower_call_closure_value(&clos_expr, args, keywords, kwargs, span, ctx);
    }
    let closure_ty = ctx
        .locals
        .get(func)
        .copied()
        .or_else(|| ctx.globals.get(func).copied());
    if let Some(ty) = closure_ty
        && let ir::Ty::Closure { .. } = ty
        && !ctx.locals.contains_key(func)
        && ctx.globals.contains_key(func)
        && ctx.funcs().contains_key(func)
    {
        // Free function rebinding via decorator: call as closure value.
        let clos_expr = ir::Expr {
            ty,
            kind: ir::ExprKind::GlobalLoad(ctx.own_global(func)),
        };
        return lower_call_closure_value(&clos_expr, args, keywords, kwargs, span, ctx);
    }
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
    if func == "int" {
        return lower_int_call(args, keywords, span, ctx);
    }
    if func == "float" {
        return lower_float_call(args, keywords, span, ctx);
    }
    // Builtin keywords we accept: enumerate/sum(start=), sorted/min/max(key=),
    // round(ndigits=).
    if !matches!(
        func,
        "enumerate" | "sorted" | "min" | "max" | "sum" | "round"
    ) && let Some(kw) = keywords.first()
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
                let arg = materialize_iterable_arg(args[0], ctx)?;
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
                // `list(range(n))` and friends: range is not a value, so the
                // argument is materialized through the comprehension path.
                // `list(zip(...))` takes it too, and must take it *before*
                // the probe below, which would otherwise find the eager
                // lowering and drain an infinite input.
                if is_lazy_combinator(args[0], ctx) {
                    let mut setup = Vec::new();
                    let parts = lower_comp_iter(args[0], false, ctx, &mut setup)?;
                    return drain_parts_to_list(parts, setup, ctx);
                }
                match lower_expr(args[0], ctx) {
                    Ok(arg) => lower_list_ctor(arg, args[0].span, ctx),
                    Err(direct) => {
                        let mut setup = Vec::new();
                        let parts =
                            lower_comp_iter(args[0], false, ctx, &mut setup).map_err(|_| direct)?;
                        drain_parts_to_list(parts, setup, ctx)
                    }
                }
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
                // Not materialized: tuples here are fixed-arity, so
                // lower_tuple_ctor needs the tuple itself, and a list (which
                // is what materializing produces) is what it cannot accept.
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
                // `Any` is sized when it holds a sized thing, which only the
                // runtime tag can say — so the check moves there.
                if !matches!(
                    arg.ty,
                    ir::Ty::Str
                        | ir::Ty::List(_)
                        | ir::Ty::Tuple(_)
                        | ir::Ty::Dict { .. }
                        | ir::Ty::Set(_)
                        | ir::Ty::Any
                ) {
                    return Err(err(
                        format!("object of type '{}' has no len()", display_ty(arg.ty)),
                        args[0].span,
                    ));
                }
                Ok(ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Len(Box::new(arg)),
                })
            }
            "round" => lower_round_expr(&args, keywords, span, ctx),
            "repr" | "ascii" => {
                if args.len() != 1 {
                    return Err(err(
                        format!("{func}() takes exactly one argument ({} given)", args.len()),
                        span,
                    ));
                }
                let arg = lower_expr(args[0], ctx)?;
                lower_repr_like(arg, func == "ascii", args[0].span)
            }
            "ord" => {
                if args.len() != 1 {
                    return Err(err(
                        format!("ord() takes exactly one argument ({} given)", args.len()),
                        span,
                    ));
                }
                let arg = lower_expr(args[0], ctx)?;
                if arg.ty != ir::Ty::Str {
                    return Err(err(
                        format!("ord() expected string of length 1, but {} found", arg.ty),
                        args[0].span,
                    ));
                }
                Ok(ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Ord(Box::new(arg)),
                })
            }
            "chr" => {
                if args.len() != 1 {
                    return Err(err(
                        format!("chr() takes exactly one argument ({} given)", args.len()),
                        span,
                    ));
                }
                let arg = lower_expr(args[0], ctx)?;
                let arg = match arg.ty {
                    ir::Ty::Bool => ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::BoolToInt(Box::new(arg)),
                    },
                    ir::Ty::Int => arg,
                    other => {
                        return Err(err(
                            format!("'{other}' object cannot be interpreted as an integer"),
                            args[0].span,
                        ));
                    }
                };
                Ok(ir::Expr {
                    ty: ir::Ty::Str,
                    kind: ir::ExprKind::Chr(Box::new(arg)),
                })
            }
            "hex" => lower_int_prefix_expr("hex", "#x", &args, span, ctx),
            "bin" => lower_int_prefix_expr("bin", "#b", &args, span, ctx),
            "oct" => lower_int_prefix_expr("oct", "#o", &args, span, ctx),
            "divmod" => lower_divmod_expr(&args, span, ctx),
            "pow" => lower_pow_expr(&args, span, ctx),
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
            "next" => lower_builtin_next(&args, span, ctx),
            "min" | "max" => lower_min_max_expr(func, &args, keywords, span, ctx),
            "sum" => lower_sum_expr(&args, keywords, span, ctx),
            "sorted" => lower_sorted_expr(&args, keywords, span, ctx),
            "range" => Err(err(
                "range(...) is not a value here: it works as the iterable of a \
                 'for' loop or a comprehension, and as an argument to list, \
                 set, sorted, sum, min and max, but it cannot be stored in a \
                 variable or passed anywhere else",
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
            _ if is_exc_class_name(&with_class_env(|e| e.current_module.clone()), func) => {
                Err(err(
                    format!(
                        "'{func}' is an exception class; it can only be used in \
                         `raise {func}(...)` and `except {func}`, not constructed \
                         as a value"
                    ),
                    func_span,
                ))
            }
            _ => Err(err(
                unsupported_feature(func)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("function '{func}' is not defined")),
                func_span,
            )),
        }
    }
}
