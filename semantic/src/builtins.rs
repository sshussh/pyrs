//! Builtin call lowering, casts, formatting, and isinstance.

use common::Span;
use parser::ast;

use crate::prelude::*;

/// Type-name patterns accepted by `isinstance(x, T)` / `isinstance(x, (T1, T2))`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]

pub(crate) enum IsInstancePat {
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

pub(crate) fn parse_isinstance_type_arg(e: &ast::Expr) -> SResult<Vec<IsInstancePat>> {
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
        // `isinstance(x, A or B)` is accepted by CPython and does not mean what
        // it looks like: `or` yields its first truthy operand and a type object
        // is always truthy, so `A or B` is just `A` and `B` is never tested.
        // Name the trap rather than letting the general message send someone
        // looking for a different mistake.
        ast::ExprKind::Binary {
            op: ast::BinOp::Or | ast::BinOp::And,
            left,
            right,
        } => {
            let spelled = |x: &ast::Expr| match &x.kind {
                ast::ExprKind::Name(n) => n.clone(),
                ast::ExprKind::NoneLit => "None".to_string(),
                _ => "...".to_string(),
            };
            Err(err(
                format!(
                    "isinstance() second argument cannot use 'or'/'and': write a \
                     tuple, isinstance(x, ({}, {})). CPython accepts this spelling \
                     but it tests only the first type — 'A or B' evaluates to 'A', \
                     because a type object is always truthy",
                    spelled(left),
                    spelled(right)
                ),
                e.span,
            ))
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

pub(crate) fn name_to_isinstance_pat(name: &str, span: Span) -> SResult<IsInstancePat> {
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

pub(crate) fn isinstance_pat_matches(ty: ir::Ty, pat: IsInstancePat) -> bool {
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
pub(crate) fn isinstance_pat_to_ty(pat: IsInstancePat) -> Option<ir::Ty> {
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

pub(crate) fn isinstance_pat_tag(pat: IsInstancePat) -> Option<i32> {
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

pub(crate) fn lower_isinstance(
    args: &[&ast::Expr],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
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
/// `any(gen)` / `all(gen)` — a short-circuiting walk over a generator.
///
/// Unlike the list path this cannot drain first: `any` must stop at the first
/// truthy element, so a side-effecting or infinite generator behaves as it
/// does in CPython. (The list path does not short-circuit either, but over a
/// list that is unobservable.)
/// `any` / `all` over a cursor, short-circuiting.
///
/// These cannot go through the drain-to-a-list path the other eager builtins
/// use: they must stop at the deciding element, so `any(map(f, infinite()))`
/// terminates when `f` first returns something truthy.
///
/// The cursor is normalised to its advance form first, because stopping early
/// means clearing a flag and an `Indexed` cursor has none — its condition is
/// an index test. `parts_to_advance` is the same reconciliation `zip` needs.
pub(crate) fn lower_any_all_cursor(
    is_any: bool,
    parts: CompIterParts,
    mut stmts: Vec<ir::Stmt>,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let name = if is_any { "any" } else { "all" };
    let more_t = ctx.fresh_temp(&format!("{name}.more"), ir::Ty::Bool);
    stmts.push(assign_const_bool(more_t.clone(), true));
    let acc_t = ctx.fresh_temp(&format!("{name}.acc"), ir::Ty::Bool);
    stmts.push(assign_const_bool(acc_t.clone(), !is_any));

    let (advance, produced) = parts_to_advance(&parts);
    // The element decides when its truthiness equals `is_any`: a truthy
    // element settles `any`, a falsy one settles `all`.
    let truth = to_bool(parts.element, span, ctx)?;
    let decides = if is_any { truth } else { bool_not(truth) };
    let mut kept = parts.step;
    kept.push(ir::Stmt::If {
        branches: vec![(
            decides,
            vec![
                assign_const_bool(acc_t.clone(), is_any),
                assign_const_bool(more_t.clone(), false),
            ],
        )],
        orelse: Vec::new(),
    });

    let mut body = advance;
    body.push(ir::Stmt::If {
        branches: vec![(produced, kept)],
        orelse: vec![assign_const_bool(more_t.clone(), false)],
    });
    stmts.push(ir::Stmt::While {
        cond: local_expr(more_t, ir::Ty::Bool),
        body,
        step: Vec::new(),
    });
    Ok(ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(local_expr(acc_t, ir::Ty::Bool)),
        },
    })
}

pub(crate) fn lower_any_all_generator(
    is_any: bool,
    gen_expr: ir::Expr,
    yield_ty: ir::Ty,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let name = if is_any { "any" } else { "all" };
    let gen_ty = gen_expr.ty;
    let gen_t = ctx.fresh_temp(&format!("{name}.gen"), gen_ty);
    let mut stmts = vec![ir::Stmt::Assign {
        name: gen_t.clone(),
        value: gen_expr,
    }];
    let (more_t, more_cond) = push_comp_more(ctx, &mut stmts);
    let acc_t = ctx.fresh_temp(&format!("{name}.acc"), ir::Ty::Bool);
    stmts.push(assign_const_bool(acc_t.clone(), !is_any));

    let opt_ty = ir::optional_of(yield_ty);
    let nxt_t = ctx.fresh_temp(&format!("{name}.next"), opt_ty);
    let nxt_local = local_expr(nxt_t.clone(), opt_ty);
    let advance = ir::Stmt::Assign {
        name: nxt_t,
        value: ir::Expr {
            ty: opt_ty,
            kind: ir::ExprKind::GeneratorNext {
                generator: Box::new(local_expr(gen_t, gen_ty)),
                send: Box::new(const_none()),
            },
        },
    };
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
    // `any` decides on a truthy element, `all` on a falsy one; either way the
    // answer is `is_any` and the walk stops.
    let truth = to_bool_default(element, span)?;
    let decisive = if is_any {
        truth
    } else {
        ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::Unary {
                op: ir::UnOp::Not,
                operand: Box::new(truth),
            },
        }
    };
    let decide = ir::Stmt::If {
        branches: vec![(
            decisive,
            vec![
                assign_const_bool(acc_t.clone(), is_any),
                assign_const_bool(more_t.clone(), false),
            ],
        )],
        orelse: vec![],
    };
    let step_body = ir::Stmt::If {
        branches: vec![(exhausted, vec![assign_const_bool(more_t, false)])],
        orelse: vec![decide],
    };
    stmts.push(ir::Stmt::While {
        cond: more_cond,
        body: vec![advance, step_body],
        step: vec![],
    });
    Ok(ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(local_expr(acc_t, ir::Ty::Bool)),
        },
    })
}

pub(crate) fn lower_any_all(
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
    // A lazy combinator is not a value, and must short-circuit rather than
    // be drained, so it takes the cursor path.
    if is_lazy_combinator(args[0], ctx) {
        let mut setup = Vec::new();
        let parts = lower_comp_iter(args[0], false, ctx, &mut setup)?;
        return lower_any_all_cursor(is_any, parts, setup, span, ctx);
    }
    let seq = lower_expr(args[0], ctx)?;
    if let ir::Ty::Generator { yield_ty } = seq.ty {
        return lower_any_all_generator(is_any, seq, *yield_ty, span, ctx);
    }
    match seq.ty {
        ir::Ty::List(_) | ir::Ty::Str | ir::Ty::Tuple(_) | ir::Ty::Set(_) | ir::Ty::Dict { .. } => {
        }
        other => {
            return Err(err(
                format!(
                    "{name}() expects a list, str, tuple, set, dict, or generator, \
                     found {other}"
                ),
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
    // Sets and dicts have no index, so their elements (for a dict, its keys,
    // as in CPython) are materialized first. Neither has side effects on
    // iteration, so this is invisible -- unlike a generator, which gets its
    // own short-circuiting walk above.
    let set_or_dict = match seq_ty {
        ir::Ty::Set(elem) => Some((*elem, true)),
        ir::Ty::Dict { key, .. } => Some((*key, false)),
        _ => Option::None,
    };
    let (iter_ty, iter_expr) = if let Some((elem, is_set)) = set_or_dict {
        let list_ty = ir::list_of(elem);
        let lt = ctx.fresh_temp(&format!("{name}.els"), list_ty);
        let src = Box::new(ir::Expr {
            ty: seq_ty,
            kind: ir::ExprKind::Local(seq_t.clone()),
        });
        stmts.push(ir::Stmt::Assign {
            name: lt.clone(),
            value: ir::Expr {
                ty: list_ty,
                kind: if is_set {
                    ir::ExprKind::SetToList(src)
                } else {
                    ir::ExprKind::DictKeys(src)
                },
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

/// Builtin / cast used as a monomorphic `key=` (not first-class values).
#[derive(Clone, Copy, Debug)]
pub(crate) enum BuiltinKey {
    Len,
    Abs,
    CastInt,
    CastFloat,
    CastBool,
    CastStr,
}

/// Callable used by `sorted` / `min` / `max` / `list.sort` `key=` desugaring.
pub(crate) enum SortKey {
    /// First-class closure / lambda (CallClosure).
    Closure(ir::Expr),
    /// Module-level free function (direct Call by IR name).
    Direct {
        ir_name: String,
        param_ty: ir::Ty,
        ret: ir::Ty,
    },
    /// Builtin or cast applied as IR ops (`key=len`, `key=abs`, `key=str`, …).
    Builtin(BuiltinKey),
}

pub(crate) fn local_expr(name: String, ty: ir::Ty) -> ir::Expr {
    ir::Expr {
        ty,
        kind: ir::ExprKind::Local(name),
    }
}

pub(crate) fn const_str(s: &str) -> ir::Expr {
    ir::Expr {
        ty: ir::Ty::Str,
        kind: ir::ExprKind::ConstStr(s.to_string()),
    }
}

/// Parsed `key=` / `reverse=` / `default=` kwargs for sorted / list.sort / min / max.
pub(crate) struct SortKeywords<'a> {
    pub(crate) key: Option<&'a ast::Expr>,
    pub(crate) reverse: Option<&'a ast::Expr>,
    pub(crate) default: Option<&'a ast::Expr>,
}

/// Parse `key=` / `reverse=` (sorted & list.sort) / `default=` (min/max) /
/// reject unexpected kwargs.
pub(crate) fn take_sort_keywords<'a>(
    keywords: &'a [ast::Keyword],
    builtin: &str,
) -> SResult<SortKeywords<'a>> {
    let mut key = None;
    let mut reverse = None;
    let mut default = None;
    for kw in keywords {
        match kw.name.as_str() {
            "key" => {
                if key.is_some() {
                    return Err(err(
                        format!("{builtin}() got multiple values for keyword argument 'key'"),
                        kw.name_span,
                    ));
                }
                key = Some(&kw.value);
            }
            "reverse" => {
                // CPython: sorted and list.sort accept reverse=; min/max do not.
                if matches!(builtin, "min" | "max") {
                    return Err(err(
                        format!("{builtin}() got an unexpected keyword argument 'reverse'"),
                        kw.name_span,
                    ));
                }
                if reverse.is_some() {
                    return Err(err(
                        format!("{builtin}() got multiple values for keyword argument 'reverse'"),
                        kw.name_span,
                    ));
                }
                reverse = Some(&kw.value);
            }
            "default" => {
                // CPython: only min/max iterable form; sorted/list.sort reject.
                if !matches!(builtin, "min" | "max") {
                    return Err(err(
                        format!("{builtin}() got an unexpected keyword argument 'default'"),
                        kw.name_span,
                    ));
                }
                if default.is_some() {
                    return Err(err(
                        format!("{builtin}() got multiple values for keyword argument 'default'"),
                        kw.name_span,
                    ));
                }
                default = Some(&kw.value);
            }
            other => {
                return Err(err(
                    format!("{builtin}() got an unexpected keyword argument '{other}'"),
                    kw.name_span,
                ));
            }
        }
    }
    Ok(SortKeywords {
        key,
        reverse,
        default,
    })
}

/// min/max path: `key=` and optional `default=` (reverse already unexpected).
pub(crate) fn take_min_max_keywords<'a>(
    keywords: &'a [ast::Keyword],
    builtin: &str,
) -> SResult<(Option<&'a ast::Expr>, Option<&'a ast::Expr>)> {
    let kw = take_sort_keywords(keywords, builtin)?;
    Ok((kw.key, kw.default))
}

/// How to apply CPython's stable reverse-sort-reverse.
pub(crate) enum ReverseMode {
    Never,
    Always,
    /// Runtime `bool` (prefer a Local so the cond is cheap).
    Cond(ir::Expr),
}

/// Lower `reverse=` to a mode. CPython truthiness (bool, int, str, …);
/// const-folds common literals so True/1 always reverse and False/0 never do.
pub(crate) fn resolve_reverse_flag(
    rev_ast: Option<&ast::Expr>,
    ctx: &mut FnCtx,
) -> SResult<(ReverseMode, Vec<ir::Stmt>)> {
    let Some(rev_ast) = rev_ast else {
        return Ok((ReverseMode::Never, vec![]));
    };
    let rev = lower_expr(rev_ast, ctx)?;
    // Const-fold known truthy/falsy literals (CPython reverse= uses truthiness).
    match &rev.kind {
        ir::ExprKind::ConstBool(true) => return Ok((ReverseMode::Always, vec![])),
        ir::ExprKind::ConstBool(false) => return Ok((ReverseMode::Never, vec![])),
        ir::ExprKind::ConstNone => return Ok((ReverseMode::Never, vec![])),
        ir::ExprKind::ConstInt(0) => return Ok((ReverseMode::Never, vec![])),
        ir::ExprKind::ConstInt(_) => return Ok((ReverseMode::Always, vec![])),
        ir::ExprKind::ConstFloat(f) if *f == 0.0 => return Ok((ReverseMode::Never, vec![])),
        ir::ExprKind::ConstFloat(_) => return Ok((ReverseMode::Always, vec![])),
        ir::ExprKind::ConstStr(s) if s.is_empty() => return Ok((ReverseMode::Never, vec![])),
        ir::ExprKind::ConstStr(_) => return Ok((ReverseMode::Always, vec![])),
        _ => {}
    }
    // Runtime truthiness via ToBool (same rules as if/while conditions).
    let as_bool = to_bool(rev, rev_ast.span, ctx)?;
    match as_bool.kind {
        ir::ExprKind::ConstBool(true) => Ok((ReverseMode::Always, vec![])),
        ir::ExprKind::ConstBool(false) => Ok((ReverseMode::Never, vec![])),
        _ => {
            let name = ctx.fresh_temp("sort.rev", ir::Ty::Bool);
            let stmts = vec![ir::Stmt::Assign {
                name: name.clone(),
                value: as_bool,
            }];
            Ok((ReverseMode::Cond(local_expr(name, ir::Ty::Bool)), stmts))
        }
    }
}

/// In-place reverse of a list (swap from both ends). `list` should be a Local.
pub(crate) fn lower_list_reverse_in_place(
    list: ir::Expr,
    elem: ir::Ty,
    ctx: &mut FnCtx,
) -> Vec<ir::Stmt> {
    let n_t = ctx.fresh_temp("lrev.n", ir::Ty::Int);
    let i_t = ctx.fresh_temp("lrev.i", ir::Ty::Int);
    let j_t = ctx.fresh_temp("lrev.j", ir::Ty::Int);
    let tmp_t = ctx.fresh_temp("lrev.tmp", elem);
    let n = local_expr(n_t.clone(), ir::Ty::Int);
    let i = local_expr(i_t.clone(), ir::Ty::Int);
    let j = local_expr(j_t.clone(), ir::Ty::Int);
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
            value: int_const(0),
        },
        ir::Stmt::Assign {
            name: j_t.clone(),
            value: ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Sub,
                    left: Box::new(n),
                    right: Box::new(int_const(1)),
                },
            },
        },
    ];
    let cond = key_cmp(ir::BinOp::Lt, i.clone(), j.clone());
    let body = vec![
        ir::Stmt::Assign {
            name: tmp_t.clone(),
            value: ir::Expr {
                ty: elem,
                kind: ir::ExprKind::Index {
                    base: Box::new(list.clone()),
                    index: Box::new(i.clone()),
                },
            },
        },
        ir::Stmt::IndexAssign {
            base: list.clone(),
            index: i.clone(),
            value: ir::Expr {
                ty: elem,
                kind: ir::ExprKind::Index {
                    base: Box::new(list.clone()),
                    index: Box::new(j.clone()),
                },
            },
        },
        ir::Stmt::IndexAssign {
            base: list.clone(),
            index: j.clone(),
            value: local_expr(tmp_t, elem),
        },
        ir::Stmt::Assign {
            name: i_t.clone(),
            value: ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Add,
                    left: Box::new(i),
                    right: Box::new(int_const(1)),
                },
            },
        },
        ir::Stmt::Assign {
            name: j_t,
            value: ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Sub,
                    left: Box::new(j),
                    right: Box::new(int_const(1)),
                },
            },
        },
    ];
    stmts.push(ir::Stmt::While {
        cond,
        body,
        step: vec![],
    });
    stmts
}

/// Append reverse-before/after steps for stable reverse sort (CPython reverse-sort-reverse).
pub(crate) fn push_maybe_reverse(
    stmts: &mut Vec<ir::Stmt>,
    mode: &ReverseMode,
    list: &ir::Expr,
    elem: ir::Ty,
    ctx: &mut FnCtx,
) {
    match mode {
        ReverseMode::Never => {}
        ReverseMode::Always => {
            stmts.extend(lower_list_reverse_in_place(list.clone(), elem, ctx));
        }
        ReverseMode::Cond(cond) => {
            let body = lower_list_reverse_in_place(list.clone(), elem, ctx);
            stmts.push(ir::Stmt::If {
                branches: vec![(cond.clone(), body)],
                orelse: vec![],
            });
        }
    }
}

/// Type-level coerce check without allocating IR temps (discards the result).
pub(crate) fn ensure_key_arg_types(elem_ty: ir::Ty, param_ty: ir::Ty, span: Span) -> SResult<()> {
    let dummy = ir::Expr {
        ty: elem_ty,
        kind: ir::ExprKind::Local(".key.probe".into()),
    };
    let _ = coerce(dummy, param_ty, span, "key= argument")?;
    Ok(())
}

/// Resolve `key=` to a monomorphic `T → K` callable; `K` must be sortable.
pub(crate) fn resolve_sort_key(
    key_ast: &ast::Expr,
    elem_ty: ir::Ty,
    ctx: &mut FnCtx,
) -> SResult<(SortKey, ir::Ty)> {
    // Bare name of a free / nested / imported function: free functions are not
    // first-class values in this subset, so special-case them before lower_expr.
    if let ast::ExprKind::Name(name) = &key_ast.kind
        && !ctx.locals.contains_key(name)
        && !ctx.cell_locals.contains_key(name)
    {
        if let Some(info) = ctx.nested_funcs.get(name).cloned() {
            let clos = make_closure_expr(&info, key_ast.span, ctx)?;
            return validate_sort_key_closure(clos, elem_ty, key_ast.span);
        }
        if let Some(sig) = ctx.funcs().get(name).cloned() {
            return validate_sort_key_direct(ctx.own_func(name), &sig, elem_ty, key_ast.span);
        }
        // Imported free function: use Direct Call with foreign IR name.
        if let Some(ImportBinding::Symbol { module, name: real }) = ctx
            .local_imports
            .get(name)
            .or_else(|| ctx.mctx.imports.get(name))
            .cloned()
        {
            if let Some(data) = ctx.mctx.mods.get(&module) {
                let (om, on) = data
                    .reexports
                    .get(&real)
                    .cloned()
                    .unwrap_or_else(|| (module.clone(), real.clone()));
                if let Some(sig) = data.funcs.get(&real).cloned().or_else(|| {
                    ctx.mctx
                        .mods
                        .get(&om)
                        .and_then(|d| d.funcs.get(&on).cloned())
                }) {
                    return validate_sort_key_direct(qual(&om, &on), &sig, elem_ty, key_ast.span);
                }
            }
            return Err(err(
                format!(
                    "key= cannot use imported name '{name}' as a free-function key \
                     (module '{module}' has no function '{real}'); use a same-module \
                     function, nested def, or lambda"
                ),
                key_ast.span,
            ));
        }
        // Builtins / casts as monomorphic key= (not first-class values).
        if let Some(result) = resolve_builtin_sort_key(name, elem_ty, key_ast.span)? {
            return Ok(result);
        }
        if BUILTINS.contains(&name.as_str())
            || matches!(
                name.as_str(),
                "int" | "float" | "bool" | "str" | "list" | "dict" | "tuple"
            )
        {
            return Err(err(
                format!(
                    "key= cannot use builtin '{name}' as a value; wrap it in a free \
                     function or lambda (e.g. lambda x=0: {name}(x))"
                ),
                key_ast.span,
            ));
        }
    }
    // A `key=` lambda is passed one element, so its parameter type is known
    // here even when the body cannot reveal it (`lambda s: len(s)`).
    let key_val = match &key_ast.kind {
        ast::ExprKind::Lambda { params, body } => {
            lower_lambda_typed(params, body, &[elem_ty], key_ast.span, ctx)?
        }
        _ => lower_expr(key_ast, ctx)?,
    };
    match key_val.ty {
        ir::Ty::Closure { .. } => validate_sort_key_closure(key_val, elem_ty, key_ast.span),
        other => Err(err(
            format!(
                "key= must be a monomorphic callable (lambda / nested function / \
                 free function / builtin len|abs|int|float|bool|str), found {other}"
            ),
            key_ast.span,
        )),
    }
}

/// Resolve bare `key=len` / `key=abs` / cast names to a monomorphic Builtin key.
/// Returns `Ok(None)` when the name is not a supported key builtin (caller
/// may still reject other builtins).
pub(crate) fn resolve_builtin_sort_key(
    name: &str,
    elem_ty: ir::Ty,
    span: Span,
) -> SResult<Option<(SortKey, ir::Ty)>> {
    match name {
        "len" => {
            let ok = match elem_ty {
                ir::Ty::Str
                | ir::Ty::List(_)
                | ir::Ty::Tuple(_)
                | ir::Ty::Dict { .. }
                | ir::Ty::Set(_) => true,
                ir::Ty::Class(id) => resolve_method(id, "__len__").is_some(),
                _ => false,
            };
            if !ok {
                return Err(err(
                    format!(
                        "key=len is not supported for element type {elem_ty}; \
                         use a free function or lambda (e.g. lambda x=\"\": len(x))"
                    ),
                    span,
                ));
            }
            Ok(Some((SortKey::Builtin(BuiltinKey::Len), ir::Ty::Int)))
        }
        "abs" => {
            let ret = match elem_ty {
                ir::Ty::Bool | ir::Ty::Int => ir::Ty::Int,
                ir::Ty::Float => ir::Ty::Float,
                other => {
                    return Err(err(
                        format!("bad operand type for key=abs: '{other}'"),
                        span,
                    ));
                }
            };
            Ok(Some((SortKey::Builtin(BuiltinKey::Abs), ret)))
        }
        "int" => {
            ensure_key_cast_ok(elem_ty, ast::TypeName::Int, span, "int")?;
            Ok(Some((SortKey::Builtin(BuiltinKey::CastInt), ir::Ty::Int)))
        }
        "float" => {
            ensure_key_cast_ok(elem_ty, ast::TypeName::Float, span, "float")?;
            Ok(Some((
                SortKey::Builtin(BuiltinKey::CastFloat),
                ir::Ty::Float,
            )))
        }
        "bool" => {
            ensure_key_cast_ok(elem_ty, ast::TypeName::Bool, span, "bool")?;
            Ok(Some((SortKey::Builtin(BuiltinKey::CastBool), ir::Ty::Bool)))
        }
        "str" => {
            ensure_key_cast_ok(elem_ty, ast::TypeName::Str, span, "str")?;
            Ok(Some((SortKey::Builtin(BuiltinKey::CastStr), ir::Ty::Str)))
        }
        _ => Ok(None),
    }
}

pub(crate) fn ensure_key_cast_ok(
    elem_ty: ir::Ty,
    cast: ast::TypeName,
    span: Span,
    name: &str,
) -> SResult<()> {
    let dummy = ir::Expr {
        ty: elem_ty,
        kind: ir::ExprKind::Local(".key.cast.probe".into()),
    };
    match lower_cast(cast, dummy, span) {
        Ok(_) => Ok(()),
        Err(e) => Err(err(
            format!(
                "key={name} cannot convert element type {elem_ty}: {}",
                e.message
            ),
            span,
        )),
    }
}

pub(crate) fn validate_sort_key_direct(
    ir_name: String,
    sig: &FuncSig,
    elem_ty: ir::Ty,
    span: Span,
) -> SResult<(SortKey, ir::Ty)> {
    if sig.vararg.is_some() || sig.kwarg.is_some() {
        return Err(err(
            "key= function must take exactly one positional parameter \
             (no *args/**kwargs)",
            span,
        ));
    }
    if sig.params.len() != 1 {
        return Err(err(
            format!(
                "key= function must take exactly one argument (takes {})",
                sig.params.len()
            ),
            span,
        ));
    }
    let param_ty = sig.params[0].ty;
    ensure_key_arg_types(elem_ty, param_ty, span)?;
    let ret = sig.ret;
    // Tuples of orderables sort lexicographically, which is what makes the
    // multi-key idiom `key=lambda p: (-p[1], p[0])` work.
    if !is_orderable_ty(ret) {
        return Err(err(
            format!(
                "key= return type must be sortable (int|float|bool|str, or a \
                 tuple or list of those), found {ret}"
            ),
            span,
        ));
    }
    Ok((
        SortKey::Direct {
            ir_name,
            param_ty,
            ret,
        },
        ret,
    ))
}

pub(crate) fn validate_sort_key_closure(
    clos: ir::Expr,
    elem_ty: ir::Ty,
    span: Span,
) -> SResult<(SortKey, ir::Ty)> {
    let ir::Ty::Closure { params, ret, .. } = clos.ty else {
        return Err(err("internal: expected closure key", span));
    };
    if params.len() != 1 {
        return Err(err(
            format!(
                "key= callable must take exactly one argument (takes {})",
                params.len()
            ),
            span,
        ));
    }
    ensure_key_arg_types(elem_ty, params[0], span)?;
    let key_ty = *ret;
    if !is_orderable_ty(key_ty) {
        return Err(err(
            format!(
                "key= return type must be sortable (int|float|bool|str, or a \
                 tuple or list of those), found {key_ty}"
            ),
            span,
        ));
    }
    Ok((SortKey::Closure(clos), key_ty))
}

pub(crate) fn call_sort_key(
    key: &SortKey,
    arg: ir::Expr,
    arg_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    match key {
        SortKey::Direct {
            ir_name,
            param_ty,
            ret,
        } => {
            let arg = coerce(arg, *param_ty, arg_span, "key= argument")?;
            Ok(ir::Expr {
                ty: *ret,
                kind: ir::ExprKind::Call {
                    func: ir_name.clone(),
                    args: vec![arg],
                },
            })
        }
        SortKey::Closure(clos) => {
            let ir::Ty::Closure {
                params,
                ret,
                capture_tys,
                func,
            } = clos.ty
            else {
                return Err(err("internal: expected closure key", arg_span));
            };
            let arg = coerce(arg, params[0], arg_span, "key= argument")?;
            Ok(ir::Expr {
                ty: *ret,
                kind: ir::ExprKind::CallClosure {
                    closure: Box::new(clos.clone()),
                    args: vec![arg],
                    capture_tys: capture_tys.to_vec(),
                    func: func.to_string(),
                },
            })
        }
        SortKey::Builtin(bk) => call_builtin_sort_key(*bk, arg, arg_span, ctx),
    }
}

pub(crate) fn call_builtin_sort_key(
    bk: BuiltinKey,
    arg: ir::Expr,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    match bk {
        BuiltinKey::Len => {
            if let ir::Ty::Class(id) = arg.ty {
                if resolve_method(id, "__len__").is_none() {
                    return Err(err(
                        format!("object of type '{}' has no len()", arg.ty),
                        span,
                    ));
                }
                let call = lower_instance_method_call(arg, id, "__len__", span, &[], ctx)?;
                if call.ty != ir::Ty::Int {
                    return Err(err("__len__ must return int", span));
                }
                return Ok(call);
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
                    span,
                ));
            }
            Ok(ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Len(Box::new(arg)),
            })
        }
        BuiltinKey::Abs => {
            let arg = match arg.ty {
                ir::Ty::Bool => ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::BoolToInt(Box::new(arg)),
                },
                ir::Ty::Int | ir::Ty::Float => arg,
                other => {
                    return Err(err(format!("bad operand type for abs(): '{other}'"), span));
                }
            };
            Ok(ir::Expr {
                ty: arg.ty,
                kind: ir::ExprKind::Abs(Box::new(arg)),
            })
        }
        BuiltinKey::CastInt => lower_cast(ast::TypeName::Int, arg, span),
        BuiltinKey::CastFloat => lower_cast(ast::TypeName::Float, arg, span),
        BuiltinKey::CastBool => lower_cast_ctx(ast::TypeName::Bool, arg, span, ctx),
        BuiltinKey::CastStr => lower_cast(ast::TypeName::Str, arg, span),
    }
}

/// Compare two keys of the same sortable type with `<` (for min) or `>` (for max/sort).
/// Compare two `key=` results. Scalars compare with a plain binary op; a
/// tuple key needs the lexicographic lowering `(1, 2) < (1, 3)` already uses,
/// because a raw `Binary` node on a tuple is not something codegen handles.
pub(crate) fn key_value_cmp(
    op: ast::BinOp,
    left: ir::Expr,
    right: ir::Expr,
    key_ty: ir::Ty,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if matches!(key_ty, ir::Ty::Tuple(_)) {
        return lower_tuple_binary(op, left, right, span, ctx);
    }
    Ok(key_cmp(comparison_ir_op(op), left, right))
}

pub(crate) fn key_cmp(op: ir::BinOp, left: ir::Expr, right: ir::Expr) -> ir::Expr {
    ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        },
    }
}

pub(crate) fn lower_sorted_expr(
    args: &[&ast::Expr],
    keywords: &[ast::Keyword],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let sk = take_sort_keywords(keywords, "sorted")?;
    if args.len() != 1 {
        return Err(err(
            format!(
                "sorted() takes exactly 1 positional argument ({} given)",
                args.len()
            ),
            span,
        ));
    }
    let arg = materialize_iterable_arg(args[0], ctx)?;
    let elem = match arg.ty {
        ir::Ty::List(e) => *e,
        other => {
            return Err(err(
                format!("sorted() expects an iterable, found {other}"),
                args[0].span,
            ));
        }
    };
    let list_ty = arg.ty;
    let (rev_mode, mut rev_bind) = resolve_reverse_flag(sk.reverse, ctx)?;

    // Copy input, then reverse-sort-reverse (stable) optionally with key=.
    let out_t = ctx.fresh_temp("sorted", list_ty);
    let out = local_expr(out_t.clone(), list_ty);
    let mut stmts = Vec::new();
    stmts.append(&mut rev_bind);
    stmts.push(ir::Stmt::Assign {
        name: out_t,
        value: ir::Expr {
            ty: list_ty,
            kind: ir::ExprKind::Binary {
                op: ir::BinOp::Mul,
                left: Box::new(arg),
                right: Box::new(int_const(1)),
            },
        },
    });
    push_maybe_reverse(&mut stmts, &rev_mode, &out, elem, ctx);
    if let Some(key_ast) = sk.key {
        stmts.extend(lower_list_sort_key_stmts(out.clone(), elem, key_ast, ctx)?);
    } else {
        push_plain_list_sort(&mut stmts, out.clone(), elem, args[0].span, ctx)?;
    }
    push_maybe_reverse(&mut stmts, &rev_mode, &out, elem, ctx);
    Ok(ir::Expr {
        ty: list_ty,
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(out),
        },
    })
}

/// In-place stable insertion sort by monomorphic `key=` (shared by `sorted` and
/// `list.sort`). `list` should be a cheap Local — callers bind side-effecting
/// bases once. Keys are evaluated once into a GC-managed auxiliary list.
pub(crate) fn lower_list_sort_key_stmts(
    list: ir::Expr,
    elem: ir::Ty,
    key_ast: &ast::Expr,
    ctx: &mut FnCtx,
) -> SResult<Vec<ir::Stmt>> {
    let (key, key_ty) = resolve_sort_key(key_ast, elem, ctx)?;
    // Bind key callable once (CPython evaluates `key` once).
    let (key, mut key_bind) = bind_sort_key(key, ctx);

    let keys_ty = ir::list_of(key_ty);
    let keys_t = ctx.fresh_temp("ksort.keys", keys_ty);
    let n_t = ctx.fresh_temp("ksort.n", ir::Ty::Int);
    let i_t = ctx.fresh_temp("ksort.i", ir::Ty::Int);
    let j_t = ctx.fresh_temp("ksort.j", ir::Ty::Int);
    let cur_t = ctx.fresh_temp("ksort.cur", elem);
    let cur_k_t = ctx.fresh_temp("ksort.curk", key_ty);

    let keys = local_expr(keys_t.clone(), keys_ty);
    let n = local_expr(n_t.clone(), ir::Ty::Int);
    let i = local_expr(i_t.clone(), ir::Ty::Int);
    let j = local_expr(j_t.clone(), ir::Ty::Int);

    let mut stmts = Vec::new();
    stmts.append(&mut key_bind);
    // Param/ret compatibility already checked in resolve_sort_key.
    stmts.extend([
        // n = len(list)
        ir::Stmt::Assign {
            name: n_t.clone(),
            value: ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Len(Box::new(list.clone())),
            },
        },
        // keys = ListNew(n)
        ir::Stmt::Assign {
            name: keys_t.clone(),
            value: ir::Expr {
                ty: keys_ty,
                kind: ir::ExprKind::ListNew {
                    cap: Box::new(n.clone()),
                },
            },
        },
        // i = 0
        ir::Stmt::Assign {
            name: i_t.clone(),
            value: int_const(0),
        },
    ]);

    // while i < n: keys.append(key(list[i])); i += 1
    // Capacity is n from ListNew — use unchecked appends (comprehension style).
    let fill_cond = key_cmp(ir::BinOp::Lt, i.clone(), n.clone());
    let elem_i = ir::Expr {
        ty: elem,
        kind: ir::ExprKind::Index {
            base: Box::new(list.clone()),
            index: Box::new(i.clone()),
        },
    };
    let keyed = call_sort_key(&key, elem_i, key_ast.span, ctx)?;
    let fill_body = vec![
        ir::Stmt::ListAppendUnchecked {
            list: keys.clone(),
            value: keyed,
        },
        ir::Stmt::Assign {
            name: i_t.clone(),
            value: ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Add,
                    left: Box::new(i.clone()),
                    right: Box::new(int_const(1)),
                },
            },
        },
    ];
    stmts.push(ir::Stmt::While {
        cond: fill_cond,
        body: fill_body,
        step: vec![],
    });

    // Insertion sort (stable): i from 1..n
    stmts.push(ir::Stmt::Assign {
        name: i_t.clone(),
        value: int_const(1),
    });
    let sort_cond = key_cmp(ir::BinOp::Lt, i.clone(), n.clone());
    let cur_load = ir::Expr {
        ty: elem,
        kind: ir::ExprKind::Index {
            base: Box::new(list.clone()),
            index: Box::new(i.clone()),
        },
    };
    let cur_key_load = ir::Expr {
        ty: key_ty,
        kind: ir::ExprKind::Index {
            base: Box::new(keys.clone()),
            index: Box::new(i.clone()),
        },
    };
    // inner: while j > 0 and keys[j-1] > cur_key
    let j_gt0 = key_cmp(ir::BinOp::Gt, j.clone(), int_const(0));
    let j_m1 = ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::Binary {
            op: ir::BinOp::Sub,
            left: Box::new(j.clone()),
            right: Box::new(int_const(1)),
        },
    };
    let keys_jm1 = ir::Expr {
        ty: key_ty,
        kind: ir::ExprKind::Index {
            base: Box::new(keys.clone()),
            index: Box::new(j_m1.clone()),
        },
    };
    let key_gt = key_value_cmp(
        ast::BinOp::Gt,
        keys_jm1.clone(),
        local_expr(cur_k_t.clone(), key_ty),
        key_ty,
        key_ast.span,
        ctx,
    )?;
    let shift_cond = bool_and(j_gt0, key_gt);
    let list_jm1 = ir::Expr {
        ty: elem,
        kind: ir::ExprKind::Index {
            base: Box::new(list.clone()),
            index: Box::new(j_m1.clone()),
        },
    };
    let shift_body = vec![
        ir::Stmt::IndexAssign {
            base: list.clone(),
            index: j.clone(),
            value: list_jm1,
        },
        ir::Stmt::IndexAssign {
            base: keys.clone(),
            index: j.clone(),
            value: keys_jm1,
        },
        ir::Stmt::Assign {
            name: j_t.clone(),
            value: j_m1,
        },
    ];
    let outer_body = vec![
        ir::Stmt::Assign {
            name: cur_t.clone(),
            value: cur_load,
        },
        ir::Stmt::Assign {
            name: cur_k_t.clone(),
            value: cur_key_load,
        },
        ir::Stmt::Assign {
            name: j_t.clone(),
            value: i.clone(),
        },
        ir::Stmt::While {
            cond: shift_cond,
            body: shift_body,
            step: vec![],
        },
        ir::Stmt::IndexAssign {
            base: list.clone(),
            index: j.clone(),
            value: local_expr(cur_t, elem),
        },
        ir::Stmt::IndexAssign {
            base: keys.clone(),
            index: j,
            value: local_expr(cur_k_t, key_ty),
        },
        ir::Stmt::Assign {
            name: i_t.clone(),
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

pub(crate) fn lower_min_max_expr(
    func: &str,
    args: &[&ast::Expr],
    keywords: &[ast::Keyword],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let (key_ast, default_ast) = take_min_max_keywords(keywords, func)?;
    if args.is_empty() {
        return Err(err(
            format!("{func}() expected at least 1 argument, got 0"),
            span,
        ));
    }
    // CPython: default= only with a single iterable positional.
    if args.len() != 1 {
        if default_ast.is_some() {
            return Err(err(
                format!("Cannot specify a default for {func}() with multiple positional arguments"),
                span,
            ));
        }
        if let Some(key_ast) = key_ast {
            // Multi-arg form with key=: min(a, b[, c…], key=f) — linear scan.
            return lower_min_max_multi_key(func, args, key_ast, span, ctx);
        }
        // Multi-arg without key=: homogeneous str (lexicographic) or numeric fold.
        return lower_min_max_multi_plain(func, args, span, ctx);
    }

    // Iterable form: any iterable, materialized into a list.
    let arg = materialize_iterable_arg(args[0], ctx)?;
    let elem = match arg.ty {
        ir::Ty::List(e) => *e,
        other => {
            return Err(err(
                format!("{func}() iterable form expects an iterable, found {other}"),
                args[0].span,
            ));
        }
    };
    let default_ir = match default_ast {
        Some(d) => Some(lower_expr(d, ctx)?),
        None => None,
    };
    if let Some(key_ast) = key_ast {
        return lower_min_max_list_key(func, arg, elem, key_ast, default_ir, args[0].span, ctx);
    }
    // No key=: numbers, str, orderable tuples/lists (MinList/MaxList),
    // classes with __lt__ (desugared scan), optional default=.
    match elem {
        ir::Ty::Int | ir::Ty::Float | ir::Ty::Bool | ir::Ty::Str => {
            lower_min_max_list_plain(func, arg, elem, default_ir, span, ctx)
        }
        ir::Ty::Tuple(_) | ir::Ty::List(_) if is_orderable_ty(elem) => {
            lower_min_max_list_plain(func, arg, elem, default_ir, span, ctx)
        }
        ir::Ty::Class(_) if class_supports_lt(elem) => {
            lower_min_max_list_class(func, arg, elem, default_ir, span, ctx)
        }
        other => Err(err(
            format!(
                "{func}() is only supported for list[int], list[float], \
                 list[bool], list[str], lists of orderable tuples/lists, and lists of \
                 classes that define __lt__ or __gt__ without key=; found list[{other}]"
            ),
            args[0].span,
        )),
    }
}

/// Multi-arg `min(a, b[, c…])` / `max` without key=: str fold or numeric unify.
pub(crate) fn lower_min_max_multi_plain(
    func: &str,
    args: &[&ast::Expr],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    debug_assert!(args.len() >= 2);
    let mut lowered = Vec::with_capacity(args.len());
    for a in args {
        lowered.push(lower_expr(a, ctx)?);
    }
    // Homogeneous str / orderable tuple / orderable list: lexicographic Min/Max fold.
    let first_ty = lowered[0].ty;
    if (first_ty == ir::Ty::Str
        || matches!(first_ty, ir::Ty::Tuple(_) | ir::Ty::List(_)) && is_orderable_ty(first_ty))
        && lowered.iter().all(|e| e.ty == first_ty)
    {
        let mut acc = lowered.remove(0);
        for right in lowered {
            let kind = if func == "min" {
                ir::ExprKind::Min {
                    left: Box::new(acc),
                    right: Box::new(right),
                }
            } else {
                ir::ExprKind::Max {
                    left: Box::new(acc),
                    right: Box::new(right),
                }
            };
            acc = ir::Expr { ty: first_ty, kind };
        }
        return Ok(acc);
    }
    // Homogeneous class instances with __lt__: linear scan (CPython uses < only).
    if matches!(first_ty, ir::Ty::Class(_)) {
        for (i, e) in lowered.iter().enumerate().skip(1) {
            if e.ty != first_ty {
                return Err(err(
                    format!(
                        "{func}() multi-arg form requires all arguments to have \
                         the same type (found {first_ty} and {})",
                        e.ty
                    ),
                    args[i].span,
                ));
            }
        }
        if !class_supports_lt(first_ty) {
            return Err(err(
                format!("{func}() is not supported for class instances without __lt__ or __gt__"),
                span,
            ));
        }
        return lower_min_max_multi_class(func, lowered, span, ctx);
    }
    // Numeric form: fold Min/Max with unify_numeric (bool → int → float).
    let mut acc = lowered.remove(0);
    for right in lowered {
        let (left, right, ty) = unify_numeric(acc, right, span, &format!("{func}()"))?;
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
        acc = ir::Expr { ty, kind };
    }
    Ok(acc)
}

/// `min(xs)` / `max(xs)` over numeric or str lists; optional `default=` on empty.
pub(crate) fn lower_min_max_list_plain(
    func: &str,
    arg: ir::Expr,
    elem: ir::Ty,
    default_ir: Option<ir::Expr>,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let kind_of = |list: ir::Expr| -> ir::ExprKind {
        if func == "min" {
            ir::ExprKind::MinList(Box::new(list))
        } else {
            ir::ExprKind::MaxList(Box::new(list))
        }
    };
    let Some(default_ir) = default_ir else {
        return Ok(ir::Expr {
            ty: elem,
            kind: kind_of(arg),
        });
    };
    // result type = join(elem, default); empty → default, else MinList/MaxList.
    let ret_ty = join_types(elem, default_ir.ty);
    let list_ty = arg.ty;
    let xs_t = ctx.fresh_temp("mm.xs", list_ty);
    let n_t = ctx.fresh_temp("mm.n", ir::Ty::Int);
    let out_t = ctx.fresh_temp("mm.out", ret_ty);
    let xs = local_expr(xs_t.clone(), list_ty);
    let n = local_expr(n_t.clone(), ir::Ty::Int);
    let def_c = coerce(default_ir, ret_ty, span, &format!("{func}() default="))?;
    let min_c = coerce(
        ir::Expr {
            ty: elem,
            kind: kind_of(xs.clone()),
        },
        ret_ty,
        span,
        &format!("{func}()"),
    )?;
    let stmts = vec![
        ir::Stmt::Assign {
            name: xs_t,
            value: arg,
        },
        ir::Stmt::Assign {
            name: n_t,
            value: ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Len(Box::new(xs)),
            },
        },
        ir::Stmt::If {
            branches: vec![(
                key_cmp(ir::BinOp::Eq, n, int_const(0)),
                vec![ir::Stmt::Assign {
                    name: out_t.clone(),
                    value: def_c,
                }],
            )],
            orelse: vec![ir::Stmt::Assign {
                name: out_t.clone(),
                value: min_c,
            }],
        },
    ];
    Ok(ir::Expr {
        ty: ret_ty,
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(local_expr(out_t, ret_ty)),
        },
    })
}

/// Multi-arg `min(a, b[, c…])` / `max` over class instances that define `__lt__`.
/// CPython only uses `<`: min updates when `x < best`, max when `best < x`.
pub(crate) fn lower_min_max_multi_class(
    func: &str,
    lowered: Vec<ir::Expr>,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    debug_assert!(lowered.len() >= 2);
    let elem = lowered[0].ty;
    let best_t = ctx.fresh_temp("mm.best", elem);
    let mut stmts = vec![ir::Stmt::Assign {
        name: best_t.clone(),
        value: lowered[0].clone(),
    }];
    for val in lowered.into_iter().skip(1) {
        let x_t = ctx.fresh_temp("mm.x", elem);
        stmts.push(ir::Stmt::Assign {
            name: x_t.clone(),
            value: val,
        });
        let x = local_expr(x_t.clone(), elem);
        let best = local_expr(best_t.clone(), elem);
        let better = if func == "min" {
            lower_class_compare(ast::BinOp::Lt, x, best, span, ctx)?
        } else {
            lower_class_compare(ast::BinOp::Lt, best, x, span, ctx)?
        };
        stmts.push(ir::Stmt::If {
            branches: vec![(
                better,
                vec![ir::Stmt::Assign {
                    name: best_t.clone(),
                    value: local_expr(x_t, elem),
                }],
            )],
            orelse: vec![],
        });
    }
    Ok(ir::Expr {
        ty: elem,
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(local_expr(best_t, elem)),
        },
    })
}

/// `min(xs)` / `max(xs)` over `list[C]` where `C` defines `__lt__`.
pub(crate) fn lower_min_max_list_class(
    func: &str,
    arg: ir::Expr,
    elem: ir::Ty,
    default_ir: Option<ir::Expr>,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let list_ty = arg.ty;
    let ret_ty = match &default_ir {
        Some(d) => join_types(elem, d.ty),
        None => elem,
    };

    let xs_t = ctx.fresh_temp("mm.xs", list_ty);
    let n_t = ctx.fresh_temp("mm.n", ir::Ty::Int);
    let i_t = ctx.fresh_temp("mm.i", ir::Ty::Int);
    let best_t = ctx.fresh_temp("mm.best", elem);
    let x_t = ctx.fresh_temp("mm.x", elem);
    let out_t = ctx.fresh_temp("mm.out", ret_ty);

    let xs = local_expr(xs_t.clone(), list_ty);
    let n = local_expr(n_t.clone(), ir::Ty::Int);
    let i = local_expr(i_t.clone(), ir::Ty::Int);

    let empty_msg = if func == "min" {
        "min() iterable argument is empty"
    } else {
        "max() iterable argument is empty"
    };

    let stmts_head = vec![
        ir::Stmt::Assign {
            name: xs_t,
            value: arg,
        },
        ir::Stmt::Assign {
            name: n_t,
            value: ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Len(Box::new(xs.clone())),
            },
        },
    ];

    let x_load = ir::Expr {
        ty: elem,
        kind: ir::ExprKind::Index {
            base: Box::new(xs.clone()),
            index: Box::new(i.clone()),
        },
    };
    let x = local_expr(x_t.clone(), elem);
    let best = local_expr(best_t.clone(), elem);
    let better = if func == "min" {
        lower_class_compare(ast::BinOp::Lt, x, best, span, ctx)?
    } else {
        lower_class_compare(ast::BinOp::Lt, best, x, span, ctx)?
    };
    let scan_body = vec![
        ir::Stmt::Assign {
            name: x_t.clone(),
            value: x_load,
        },
        ir::Stmt::If {
            branches: vec![(
                better,
                vec![ir::Stmt::Assign {
                    name: best_t.clone(),
                    value: local_expr(x_t, elem),
                }],
            )],
            orelse: vec![],
        },
        ir::Stmt::Assign {
            name: i_t.clone(),
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
    let mut nonempty_body = vec![
        ir::Stmt::Assign {
            name: best_t.clone(),
            value: ir::Expr {
                ty: elem,
                kind: ir::ExprKind::Index {
                    base: Box::new(xs),
                    index: Box::new(int_const(0)),
                },
            },
        },
        ir::Stmt::Assign {
            name: i_t.clone(),
            value: int_const(1),
        },
        ir::Stmt::While {
            cond: key_cmp(ir::BinOp::Lt, local_expr(i_t, ir::Ty::Int), n.clone()),
            body: scan_body,
            step: vec![],
        },
    ];
    nonempty_body.push(ir::Stmt::Assign {
        name: out_t.clone(),
        value: coerce(local_expr(best_t, elem), ret_ty, span, &format!("{func}()"))?,
    });

    let empty_body = if let Some(default_ir) = default_ir {
        vec![ir::Stmt::Assign {
            name: out_t.clone(),
            value: coerce(default_ir, ret_ty, span, &format!("{func}() default="))?,
        }]
    } else {
        vec![ir::Stmt::Raise {
            exc: ir::ExcType::ValueError,
            message: Some(const_str(empty_msg)),
        }]
    };

    let mut stmts = stmts_head;
    stmts.push(ir::Stmt::If {
        branches: vec![(key_cmp(ir::BinOp::Eq, n, int_const(0)), empty_body)],
        orelse: nonempty_body,
    });

    Ok(ir::Expr {
        ty: ret_ty,
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(local_expr(out_t, ret_ty)),
        },
    })
}

/// `min(a, b[, c…], key=f)` / `max(...)` — compare monomorphic `key=` over positionals.
pub(crate) fn lower_min_max_multi_key(
    func: &str,
    args: &[&ast::Expr],
    key_ast: &ast::Expr,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    debug_assert!(args.len() >= 2);
    let mut lowered = Vec::with_capacity(args.len());
    for a in args {
        lowered.push(lower_expr(a, ctx)?);
    }
    // Homogeneous candidates so `best` has a single storage type (monomorphic key).
    let elem = lowered[0].ty;
    for (i, e) in lowered.iter().enumerate().skip(1) {
        if e.ty != elem {
            return Err(err(
                format!(
                    "{func}() multi-arg form with key= requires all arguments to have \
                     the same type (found {elem} and {})",
                    e.ty
                ),
                args[i].span,
            ));
        }
    }
    let (key, key_ty) = resolve_sort_key(key_ast, elem, ctx)?;
    let (key, mut key_bind) = bind_sort_key(key, ctx);

    let best_t = ctx.fresh_temp("mm.best", elem);
    let best_k_t = ctx.fresh_temp("mm.bestk", key_ty);

    let mut stmts = Vec::new();
    stmts.append(&mut key_bind);
    // best = args[0]; best_k = key(best)
    stmts.push(ir::Stmt::Assign {
        name: best_t.clone(),
        value: lowered[0].clone(),
    });
    stmts.push(ir::Stmt::Assign {
        name: best_k_t.clone(),
        value: call_sort_key(&key, local_expr(best_t.clone(), elem), args[0].span, ctx)?,
    });

    let cmp_op = if func == "min" {
        ast::BinOp::Lt
    } else {
        ast::BinOp::Gt
    };
    for (i, val) in lowered.into_iter().enumerate().skip(1) {
        let x_t = ctx.fresh_temp("mm.x", elem);
        let k_t = ctx.fresh_temp("mm.k", key_ty);
        stmts.push(ir::Stmt::Assign {
            name: x_t.clone(),
            value: val,
        });
        stmts.push(ir::Stmt::Assign {
            name: k_t.clone(),
            value: call_sort_key(&key, local_expr(x_t.clone(), elem), args[i].span, ctx)?,
        });
        let better = key_value_cmp(
            cmp_op,
            local_expr(k_t.clone(), key_ty),
            local_expr(best_k_t.clone(), key_ty),
            key_ty,
            span,
            ctx,
        )?;
        stmts.push(ir::Stmt::If {
            branches: vec![(
                better,
                vec![
                    ir::Stmt::Assign {
                        name: best_t.clone(),
                        value: local_expr(x_t, elem),
                    },
                    ir::Stmt::Assign {
                        name: best_k_t.clone(),
                        value: local_expr(k_t, key_ty),
                    },
                ],
            )],
            orelse: vec![],
        });
    }

    Ok(ir::Expr {
        ty: elem,
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(local_expr(best_t, elem)),
        },
    })
}

/// Bind a closure key into a local so MakeClosure runs once.
pub(crate) fn bind_sort_key(key: SortKey, ctx: &mut FnCtx) -> (SortKey, Vec<ir::Stmt>) {
    match key {
        SortKey::Closure(clos) => {
            let ty = clos.ty;
            let name = ctx.fresh_temp("key.fn", ty);
            let stmts = vec![ir::Stmt::Assign {
                name: name.clone(),
                value: clos,
            }];
            (SortKey::Closure(local_expr(name, ty)), stmts)
        }
        // Direct / Builtin need no bind (stateless).
        other => (other, vec![]),
    }
}

/// `min(xs, key=f[, default=d])` / `max(...)` — linear scan comparing `f(x)`.
pub(crate) fn lower_min_max_list_key(
    func: &str,
    arg: ir::Expr,
    elem: ir::Ty,
    key_ast: &ast::Expr,
    default_ir: Option<ir::Expr>,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let list_ty = arg.ty;
    let (key, key_ty) = resolve_sort_key(key_ast, elem, ctx)?;
    let (key, mut key_bind) = bind_sort_key(key, ctx);
    // Param/ret compatibility already checked in resolve_sort_key.

    let ret_ty = match &default_ir {
        Some(d) => join_types(elem, d.ty),
        None => elem,
    };

    let xs_t = ctx.fresh_temp("mm.xs", list_ty);
    let n_t = ctx.fresh_temp("mm.n", ir::Ty::Int);
    let i_t = ctx.fresh_temp("mm.i", ir::Ty::Int);
    let best_t = ctx.fresh_temp("mm.best", elem);
    let best_k_t = ctx.fresh_temp("mm.bestk", key_ty);
    let x_t = ctx.fresh_temp("mm.x", elem);
    let k_t = ctx.fresh_temp("mm.k", key_ty);
    let out_t = ctx.fresh_temp("mm.out", ret_ty);

    let xs = local_expr(xs_t.clone(), list_ty);
    let n = local_expr(n_t.clone(), ir::Ty::Int);
    let i = local_expr(i_t.clone(), ir::Ty::Int);

    let empty_msg = if func == "min" {
        "min() iterable argument is empty"
    } else {
        "max() iterable argument is empty"
    };

    let mut stmts = Vec::new();
    stmts.append(&mut key_bind);
    stmts.extend([
        ir::Stmt::Assign {
            name: xs_t.clone(),
            value: arg,
        },
        ir::Stmt::Assign {
            name: n_t.clone(),
            value: ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Len(Box::new(xs.clone())),
            },
        },
    ]);

    // Non-empty path: scan then coerce best into out.
    let mut nonempty_body = vec![
        // best = xs[0]; best_k = key(best)
        ir::Stmt::Assign {
            name: best_t.clone(),
            value: ir::Expr {
                ty: elem,
                kind: ir::ExprKind::Index {
                    base: Box::new(xs.clone()),
                    index: Box::new(int_const(0)),
                },
            },
        },
        ir::Stmt::Assign {
            name: best_k_t.clone(),
            value: call_sort_key(&key, local_expr(best_t.clone(), elem), key_ast.span, ctx)?,
        },
        ir::Stmt::Assign {
            name: i_t.clone(),
            value: int_const(1),
        },
    ];

    // while i < n: x = xs[i]; k = key(x); if k < best_k (or > for max): update
    let loop_cond = key_cmp(ir::BinOp::Lt, i.clone(), n.clone());
    let x_load = ir::Expr {
        ty: elem,
        kind: ir::ExprKind::Index {
            base: Box::new(xs),
            index: Box::new(i.clone()),
        },
    };
    let cmp_op = if func == "min" {
        ast::BinOp::Lt
    } else {
        ast::BinOp::Gt
    };
    let better = key_value_cmp(
        cmp_op,
        local_expr(k_t.clone(), key_ty),
        local_expr(best_k_t.clone(), key_ty),
        key_ty,
        span,
        ctx,
    )?;
    let update = vec![
        ir::Stmt::Assign {
            name: best_t.clone(),
            value: local_expr(x_t.clone(), elem),
        },
        ir::Stmt::Assign {
            name: best_k_t.clone(),
            value: local_expr(k_t.clone(), key_ty),
        },
    ];
    let body = vec![
        ir::Stmt::Assign {
            name: x_t.clone(),
            value: x_load,
        },
        ir::Stmt::Assign {
            name: k_t,
            value: call_sort_key(&key, local_expr(x_t, elem), key_ast.span, ctx)?,
        },
        ir::Stmt::If {
            branches: vec![(better, update)],
            orelse: vec![],
        },
        ir::Stmt::Assign {
            name: i_t.clone(),
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
    nonempty_body.push(ir::Stmt::While {
        cond: loop_cond,
        body,
        step: vec![],
    });
    nonempty_body.push(ir::Stmt::Assign {
        name: out_t.clone(),
        value: coerce(
            local_expr(best_t, elem),
            ret_ty,
            key_ast.span,
            &format!("{func}()"),
        )?,
    });

    let empty_body = if let Some(default_ir) = default_ir {
        vec![ir::Stmt::Assign {
            name: out_t.clone(),
            value: coerce(
                default_ir,
                ret_ty,
                key_ast.span,
                &format!("{func}() default="),
            )?,
        }]
    } else {
        vec![ir::Stmt::Raise {
            exc: ir::ExcType::ValueError,
            message: Some(const_str(empty_msg)),
        }]
    };

    stmts.push(ir::Stmt::If {
        branches: vec![(key_cmp(ir::BinOp::Eq, n, int_const(0)), empty_body)],
        orelse: nonempty_body,
    });

    Ok(ir::Expr {
        ty: ret_ty,
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(local_expr(out_t, ret_ty)),
        },
    })
}

pub(crate) fn lower_round_expr(
    args: &[&ast::Expr],
    keywords: &[ast::Keyword],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.is_empty() {
        return Err(err(
            "round() missing required argument 'number' (pos 1)",
            span,
        ));
    }
    if args.len() > 2 {
        return Err(err(
            format!(
                "round() takes at most 2 positional arguments ({} given)",
                args.len()
            ),
            span,
        ));
    }
    let mut ndigits_kw = None;
    for kw in keywords {
        if kw.name == "ndigits" {
            if ndigits_kw.is_some() {
                return Err(err(
                    "round() got multiple values for keyword argument 'ndigits'",
                    kw.name_span,
                ));
            }
            ndigits_kw = Some(kw);
        } else {
            return Err(err(
                format!("round() got an unexpected keyword argument '{}'", kw.name),
                kw.name_span,
            ));
        }
    }
    if args.len() == 2
        && let Some(kw) = ndigits_kw
    {
        return Err(err(
            "round() got multiple values for argument 'ndigits'",
            kw.name_span,
        ));
    }
    let value = lower_expr(args[0], ctx)?;
    let value = match value.ty {
        ir::Ty::Bool => ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::BoolToInt(Box::new(value)),
        },
        ir::Ty::Int | ir::Ty::Float => value,
        other => {
            return Err(err(
                format!("type {} doesn't define __round__ method", other),
                args[0].span,
            ));
        }
    };
    let ndigits = if args.len() == 2 {
        Some(args[1])
    } else {
        ndigits_kw.map(|kw| &kw.value)
    };
    let ndigits = if let Some(nd) = ndigits {
        let n = lower_expr(nd, ctx)?;
        let n = match n.ty {
            ir::Ty::Bool => ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::BoolToInt(Box::new(n)),
            },
            ir::Ty::Int => n,
            other => {
                return Err(err(
                    format!("'{other}' object cannot be interpreted as an integer"),
                    nd.span,
                ));
            }
        };
        Some(n)
    } else {
        None
    };
    let ret_ty = match (value.ty, ndigits.is_some()) {
        (ir::Ty::Float, true) => ir::Ty::Float,
        _ => ir::Ty::Int,
    };
    Ok(ir::Expr {
        ty: ret_ty,
        kind: ir::ExprKind::Round {
            value: Box::new(value),
            ndigits: ndigits.map(Box::new),
        },
    })
}

pub(crate) fn lower_sum_expr(
    args: &[&ast::Expr],
    keywords: &[ast::Keyword],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.is_empty() {
        return Err(err(
            "sum() takes at least 1 positional argument (0 given)",
            span,
        ));
    }
    if args.len() > 2 {
        return Err(err(
            format!(
                "sum() takes at most 2 positional arguments ({} given)",
                args.len()
            ),
            span,
        ));
    }
    let mut start_kw = None;
    for kw in keywords {
        if kw.name == "start" {
            if start_kw.is_some() {
                return Err(err(
                    "sum() got multiple values for keyword argument 'start'",
                    kw.name_span,
                ));
            }
            start_kw = Some(kw);
        } else {
            return Err(err(
                format!("sum() got an unexpected keyword argument '{}'", kw.name),
                kw.name_span,
            ));
        }
    }
    if args.len() == 2
        && let Some(kw) = start_kw
    {
        return Err(err(
            "sum() got multiple values for argument 'start'",
            kw.name_span,
        ));
    }
    let list = materialize_iterable_arg(args[0], ctx)?;
    let elem = match list.ty {
        ir::Ty::List(e) => *e,
        other => {
            return Err(err(
                format!("sum() expects an iterable of numbers, found {other}"),
                args[0].span,
            ));
        }
    };
    if !matches!(elem, ir::Ty::Int | ir::Ty::Float) {
        return Err(err(
            format!(
                "sum() is only supported for list[int] and list[float], \
                 found list[{elem}]"
            ),
            args[0].span,
        ));
    }
    let start = if args.len() == 2 {
        let s = lower_expr(args[1], ctx)?;
        promote_numeric(s, args[1].span, "sum() start")?
    } else if let Some(kw) = start_kw {
        let s = lower_expr(&kw.value, ctx)?;
        promote_numeric(s, kw.value.span, "sum() start")?
    } else if elem == ir::Ty::Float {
        ir::Expr {
            ty: ir::Ty::Float,
            kind: ir::ExprKind::ConstFloat(0.0),
        }
    } else {
        int_const(0)
    };
    // Result type is elem ⊔ start (bool already promoted). Empty → start.
    let (start, ret_ty) = match (elem, start.ty) {
        (ir::Ty::Int, ir::Ty::Int) | (ir::Ty::Float, ir::Ty::Float) => (start, elem),
        (ir::Ty::Float, ir::Ty::Int) => (
            ir::Expr {
                ty: ir::Ty::Float,
                kind: ir::ExprKind::IntToFloat(Box::new(start)),
            },
            ir::Ty::Float,
        ),
        (ir::Ty::Int, ir::Ty::Float) => (start, ir::Ty::Float),
        _ => unreachable!("sum start is int or float after promote_numeric"),
    };
    Ok(ir::Expr {
        ty: ret_ty,
        kind: ir::ExprKind::Sum {
            list: Box::new(list),
            start: Box::new(start),
        },
    })
}

pub(crate) fn lower_enumerate_expr(
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
    // CPython takes `start` positionally as well as by keyword.
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
    let seq = materialize_iterable_arg(args[0], ctx)?;
    let start = if let Some(kw) = keywords.iter().find(|k| k.name == "start") {
        let s = lower_expr(&kw.value, ctx)?;
        coerce(s, ir::Ty::Int, kw.value.span, "enumerate start")?
    } else if let Some(a) = args.get(1) {
        let s = lower_expr(a, ctx)?;
        coerce(s, ir::Ty::Int, a.span, "enumerate start")?
    } else {
        int_const(0)
    };
    let elem_ty = match seq.ty {
        ir::Ty::List(e) => *e,
        ir::Ty::Str => ir::Ty::Str,
        // A dynamic value yields dynamic elements.
        ir::Ty::Any => ir::Ty::Any,
        ir::Ty::Tuple(es) if !es.is_empty() && es.iter().all(|e| e == &es[0]) => es[0],
        ir::Ty::Tuple(_) => {
            return Err(err(
                "enumerate() on heterogeneous tuples is not supported yet",
                args[0].span,
            ));
        }
        other => {
            return Err(err(
                format!("enumerate() expects an iterable, found {other}"),
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

/// `zip(a, b, ...)` over any number of iterables, truncating to the shortest.
///
/// Materializes each argument into a list first (`zip(range(3), xs)` and
/// `zip(gen(), xs)` are ordinary Python), then walks them in lockstep. The
/// result is a `list[tuple[...]]` with one component per argument.
pub(crate) fn lower_zip_expr(
    args: &[&ast::Expr],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.is_empty() {
        // CPython: `list(zip())` is `[]`. Nothing to iterate, so the result is
        // an empty list of empty tuples -- the type the n-ary form would give.
        let _ = span;
        return Ok(ir::Expr {
            ty: ir::list_of(ir::tuple_of(&[])),
            kind: ir::ExprKind::ListLit(vec![]),
        });
    }
    // Element type of each argument, with the argument materialized to a list.
    let mut lists: Vec<ir::Expr> = Vec::with_capacity(args.len());
    let mut elems: Vec<ir::Ty> = Vec::with_capacity(args.len());
    for a in args {
        let v = materialize_iterable_arg(a, ctx)?;
        match v.ty {
            ir::Ty::List(e) => elems.push(*e),
            other => {
                return Err(err(
                    format!("zip() expects iterables, found {other}"),
                    a.span,
                ));
            }
        }
        lists.push(v);
    }

    let tup_ty = ir::tuple_of(&elems);
    let out_ty = ir::list_of(tup_ty);
    let out_t = ctx.fresh_temp("zip.out", out_ty);
    let i_t = ctx.fresh_temp("zip.i", ir::Ty::Int);
    let n_t = ctx.fresh_temp("zip.n", ir::Ty::Int);

    let mut stmts = Vec::new();
    let mut list_locals: Vec<ir::Expr> = Vec::with_capacity(lists.len());
    for (k, v) in lists.into_iter().enumerate() {
        let t = ctx.fresh_temp(&format!("zip.s{k}"), v.ty);
        let ty = v.ty;
        stmts.push(ir::Stmt::Assign {
            name: t.clone(),
            value: v,
        });
        list_locals.push(local_expr(t, ty));
    }

    // n = min(len(...)) across every argument -- zip stops at the shortest.
    let mut n_expr: Option<ir::Expr> = Option::None;
    for l in &list_locals {
        let len = ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Len(Box::new(l.clone())),
        };
        n_expr = Some(match n_expr {
            Option::None => len,
            Some(prev) => ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Min {
                    left: Box::new(prev),
                    right: Box::new(len),
                },
            },
        });
    }
    stmts.push(ir::Stmt::Assign {
        name: n_t.clone(),
        value: n_expr.expect("at least one argument"),
    });
    stmts.push(ir::Stmt::Assign {
        name: out_t.clone(),
        value: ir::Expr {
            ty: out_ty,
            kind: ir::ExprKind::ListNew {
                cap: Box::new(local_expr(n_t.clone(), ir::Ty::Int)),
            },
        },
    });
    stmts.push(ir::Stmt::Assign {
        name: i_t.clone(),
        value: int_const(0),
    });

    let cond = key_cmp(
        ir::BinOp::Lt,
        local_expr(i_t.clone(), ir::Ty::Int),
        local_expr(n_t, ir::Ty::Int),
    );
    let components: Vec<ir::Expr> = list_locals
        .iter()
        .zip(elems.iter())
        .map(|(l, e)| ir::Expr {
            ty: *e,
            kind: ir::ExprKind::Index {
                base: Box::new(l.clone()),
                index: Box::new(local_expr(i_t.clone(), ir::Ty::Int)),
            },
        })
        .collect();
    let body = vec![ir::Stmt::ListAppend {
        list: local_expr(out_t.clone(), out_ty),
        value: ir::Expr {
            ty: tup_ty,
            kind: ir::ExprKind::TupleLit(components),
        },
    }];
    let step = ir::Stmt::Assign {
        name: i_t.clone(),
        value: ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Binary {
                op: ir::BinOp::Add,
                left: Box::new(local_expr(i_t, ir::Ty::Int)),
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
            result: Box::new(local_expr(out_t, out_ty)),
        },
    })
}

pub(crate) fn lower_reversed_expr(
    args: &[&ast::Expr],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
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

pub(crate) fn as_int_base(value: ir::Expr, span: Span) -> SResult<ir::Expr> {
    match value.ty {
        ir::Ty::Bool => Ok(ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::BoolToInt(Box::new(value)),
        }),
        ir::Ty::Int => Ok(value),
        other => Err(err(
            format!("'{other}' object cannot be interpreted as an integer"),
            span,
        )),
    }
}

/// `int()` / `int(x, base)` / `int(x, base=n)` — 1-arg form is a Cast.
pub(crate) fn lower_int_call(
    args: &[ast::PosArg],
    keywords: &[ast::Keyword],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let plain = require_plain_args(args, "int", span)?;
    let mut base_kw = None;
    for kw in keywords {
        if kw.name == "base" {
            if base_kw.is_some() {
                return Err(err(
                    "int() got multiple values for keyword argument 'base'",
                    kw.name_span,
                ));
            }
            base_kw = Some(kw);
        } else {
            return Err(err(
                format!("int() got an unexpected keyword argument '{}'", kw.name),
                kw.name_span,
            ));
        }
    }
    if plain.len() > 2 {
        return Err(err(
            format!("int expected at most 2 arguments, got {}", plain.len()),
            span,
        ));
    }
    if plain.len() == 2 && base_kw.is_some() {
        return Err(err("int() takes at most 2 arguments (3 given)", span));
    }
    if plain.is_empty() {
        if base_kw.is_some() {
            return Err(err("int() missing string argument", span));
        }
        return Ok(int_const(0));
    }
    let value = lower_expr(plain[0], ctx)?;
    let base_src = if plain.len() == 2 {
        Some(plain[1])
    } else {
        base_kw.map(|kw| &kw.value)
    };
    if let Some(b) = base_src {
        if value.ty != ir::Ty::Str {
            return Err(err(
                "int() can't convert non-string with explicit base",
                plain[0].span,
            ));
        }
        let base = lower_expr(b, ctx)?;
        let base = as_int_base(base, b.span)?;
        return Ok(ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::StrToInt {
                value: Box::new(value),
                base: Box::new(base),
            },
        });
    }
    lower_cast(ast::TypeName::Int, value, plain[0].span)
}

/// `float()` / extra-arity `float(...)` — 1-arg form is a Cast.
pub(crate) fn lower_float_call(
    args: &[ast::PosArg],
    keywords: &[ast::Keyword],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if let Some(kw) = keywords.first() {
        return Err(err("float() takes no keyword arguments", kw.name_span));
    }
    let plain = require_plain_args(args, "float", span)?;
    if plain.is_empty() {
        return Ok(ir::Expr {
            ty: ir::Ty::Float,
            kind: ir::ExprKind::ConstFloat(0.0),
        });
    }
    if plain.len() > 1 {
        return Err(err(
            format!("float expected at most 1 argument, got {}", plain.len()),
            span,
        ));
    }
    let value = lower_expr(plain[0], ctx)?;
    lower_cast(ast::TypeName::Float, value, plain[0].span)
}

pub(crate) fn lower_cast(ty: ast::TypeName, value: ir::Expr, span: Span) -> SResult<ir::Expr> {
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
            ir::Ty::Str => Ok(ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::StrToInt {
                    value: Box::new(value),
                    base: Box::new(int_const(10)),
                },
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
            ir::Ty::Str => Ok(ir::Expr {
                ty: ir::Ty::Float,
                kind: ir::ExprKind::StrToFloat(Box::new(value)),
            }),
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
            // A container renders as the text `print` writes -- CPython's
            // `str` and `repr` agree there, elements included.
            ir::Ty::List(_) | ir::Ty::Tuple(_) | ir::Ty::Dict { .. } | ir::Ty::Set(_) => {
                Ok(ir::Expr {
                    ty: ir::Ty::Str,
                    kind: ir::ExprKind::ContainerRepr(Box::new(value)),
                })
            }
            // A dynamic value renders as `print` writes it, which is what
            // `str` means -- and it is what a library taking `object` needs to
            // report what it was handed.
            ir::Ty::Any => Ok(ir::Expr {
                ty: ir::Ty::Str,
                kind: ir::ExprKind::AnyToStr {
                    value: Box::new(value),
                    repr: false,
                },
            }),
            other => Err(err(format!("str() cannot convert {other} yet"), span)),
        },
        // Cast form `list[T](x)` is not supported; use call form `list(x)`.
        ast::TypeName::List(_) => Err(err(
            "typed list(...) casts are not supported; use list(iterable) without a type argument",
            span,
        )),
        ast::TypeName::Tuple(_) => Err(err(
            "typed tuple(...) casts are not supported; use tuple(x) or a tuple literal",
            span,
        )),
        ast::TypeName::Dict { .. } => Err(err(
            "typed dict(...) casts are not supported; use dict(pairs) or a dict literal",
            span,
        )),
        ast::TypeName::Set(_) => Err(err(
            "typed set(...) casts are not supported; use set(iterable) or a set literal",
            span,
        )),
        ast::TypeName::File => Err(err(
            "file() is not a conversion; use open(path) to open a file",
            span,
        )),
        ast::TypeName::None => Err(err("None is not a conversion", span)),
        ast::TypeName::Iterator(_) => Err(err(
            "Iterator[...] is an annotation, not a conversion",
            span,
        )),
        ast::TypeName::Callable { .. } => Err(err(
            "Callable[...] is an annotation, not a conversion",
            span,
        )),
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
/// `"...".format(a, b)` on a *literal* format string.
///
/// Desugared into the same `JoinedStr` parts an f-string produces, so the
/// whole format mini-language — `{:.2f}`, `{!r}`, alignment, width — comes
/// from the code that already implements it, and nothing new reaches the
/// runtime. The format string has to be a literal for that: a runtime one
/// would need a runtime parser and a heterogeneous argument list, which this
/// subset does not have.
pub(crate) fn lower_str_format(
    fmt: &str,
    args: &[ast::PosArg],
    keywords: &[ast::Keyword],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let mut positional: Vec<&ast::Expr> = Vec::new();
    for a in args {
        match a {
            ast::PosArg::Pos(e) => positional.push(e),
            ast::PosArg::Star(_) => {
                return Err(err(
                    "format() does not support * unpacking; pass the arguments \
                     individually",
                    span,
                ));
            }
        }
    }

    // Evaluate every argument exactly once, in call order, before any field
    // is rendered. These are call arguments, so CPython evaluates all of them
    // at the call and none of them again: `"{0} {0}".format(side())` runs
    // `side()` once, and an argument no field names still runs. Substituting
    // the argument expression into each field did neither.
    let mut prelude: Vec<ir::Stmt> = Vec::new();
    let mut pos_vals: Vec<ir::Expr> = Vec::with_capacity(positional.len());
    for e in &positional {
        let v = lower_expr(e, ctx)?;
        let ty = v.ty;
        let name = ctx.fresh_temp("fmtarg", ty);
        prelude.push(ir::Stmt::Assign {
            name: name.clone(),
            value: v,
        });
        pos_vals.push(local_expr(name, ty));
    }
    let mut kw_vals: Vec<(String, ir::Expr)> = Vec::with_capacity(keywords.len());
    for k in keywords {
        let v = lower_expr(&k.value, ctx)?;
        let ty = v.ty;
        let name = ctx.fresh_temp("fmtkw", ty);
        prelude.push(ir::Stmt::Assign {
            name: name.clone(),
            value: v,
        });
        kw_vals.push((k.name.clone(), local_expr(name, ty)));
    }

    let mut parts: Vec<FormatPart> = Vec::new();
    let mut lit = String::new();
    let mut auto = 0usize;
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                lit.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                lit.push('}');
            }
            '}' => {
                return Err(err(
                    "single '}' is not allowed in a format string; use '}}'",
                    span,
                ));
            }
            '{' => {
                // Collect the field, tracking nesting so a format spec that
                // contains its own `{}` stays with it.
                let mut field = String::new();
                let mut depth = 1;
                loop {
                    match chars.next() {
                        Some('{') => {
                            depth += 1;
                            field.push('{');
                        }
                        Some('}') => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                            field.push('}');
                        }
                        Some(ch) => field.push(ch),
                        Option::None => {
                            return Err(err("unterminated '{' in a format string", span));
                        }
                    }
                }
                // Split the name off the `!conv` / `:spec` tail and reuse the
                // f-string fragment splitter for the rest.
                let head_len = field.find(['!', ':']).unwrap_or(field.len());
                let (name, tail) = field.split_at(head_len);
                let value = resolve_format_field(name, &mut auto, &pos_vals, &kw_vals, span)?;
                let (conversion, format_spec) = split_format_tail(tail, span)?;
                if !lit.is_empty() {
                    parts.push(FormatPart::Literal(std::mem::take(&mut lit)));
                }
                parts.push(FormatPart::Field {
                    value,
                    conversion,
                    format_spec,
                });
            }
            _ => lit.push(c),
        }
    }
    if !lit.is_empty() {
        parts.push(FormatPart::Literal(lit));
    }

    let mut result: Option<ir::Expr> = Option::None;
    for part in parts {
        let piece = match part {
            FormatPart::Literal(s) => ir::Expr {
                ty: ir::Ty::Str,
                kind: ir::ExprKind::ConstStr(s),
            },
            FormatPart::Field {
                value,
                conversion,
                format_spec,
            } => render_field(value, conversion, format_spec.as_deref(), span, ctx)?,
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
    let joined = result.unwrap_or_else(|| const_str_expr(""));
    if prelude.is_empty() {
        return Ok(joined);
    }
    // The prelude runs even when no field names a given argument.
    Ok(ir::Expr {
        ty: ir::Ty::Str,
        kind: ir::ExprKind::Block {
            stmts: prelude,
            result: Box::new(joined),
        },
    })
}

/// A piece of a `.format()` result: literal text, or a field bound to an
/// argument that has already been evaluated into a temporary.
pub(crate) enum FormatPart {
    Literal(String),
    Field {
        value: ir::Expr,
        conversion: Option<ast::FStringConversion>,
        format_spec: Option<Box<ast::Expr>>,
    },
}

/// `"%d of %s" % (n, name)` on a *literal* format string.
///
/// Translated into the same `JoinedStr` parts `.format()` and f-strings use,
/// by rewriting each printf conversion into the brace mini-language: `%d` is
/// `{:d}`, `%.2f` is `{:.2f}`, `%-5s` is `{:<5}`. Only a literal format
/// string is handled, for the same reason as `.format()`.
pub(crate) fn lower_percent_format(
    fmt: &str,
    rhs: &ast::Expr,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    // `"%s" % x` takes a bare value; a tuple supplies several.
    let values: Vec<&ast::Expr> = match &rhs.kind {
        ast::ExprKind::TupleLit(items) => items.iter().collect(),
        _ => vec![rhs],
    };

    let mut parts: Vec<ast::FStringPart> = Vec::new();
    let mut lit = String::new();
    let mut next = 0usize;
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            lit.push(c);
            continue;
        }
        if chars.peek() == Some(&'%') {
            chars.next();
            lit.push('%');
            continue;
        }
        // flags, width, .precision, then the conversion character
        let mut flags = String::new();
        while matches!(chars.peek(), Some('-' | '+' | '0' | ' ')) {
            flags.push(chars.next().unwrap());
        }
        let mut width = String::new();
        while matches!(chars.peek(), Some(c) if c.is_ascii_digit()) {
            width.push(chars.next().unwrap());
        }
        let mut precision = String::new();
        if chars.peek() == Some(&'.') {
            precision.push(chars.next().unwrap());
            while matches!(chars.peek(), Some(c) if c.is_ascii_digit()) {
                precision.push(chars.next().unwrap());
            }
        }
        let Some(conv) = chars.next() else {
            return Err(err(
                "incomplete format specifier at the end of a % format string",
                span,
            ));
        };
        // Brace spec equivalent. `-` is `<`, and a leading `0` is `0` in both.
        let mut spec = String::new();
        if flags.contains('-') {
            spec.push('<');
        }
        if flags.contains('+') {
            spec.push('+');
        }
        if flags.contains('0') && !flags.contains('-') {
            spec.push('0');
        }
        spec.push_str(&width);
        spec.push_str(&precision);
        let conversion = match conv {
            // `%s` is str(), which is the default conversion.
            's' => Option::None,
            'r' => Some(ast::FStringConversion::Repr),
            'a' => Some(ast::FStringConversion::Ascii),
            'd' | 'i' => {
                spec.push('d');
                Option::None
            }
            'f' | 'F' => {
                if precision.is_empty() {
                    // CPython's %f defaults to 6 places; `{:f}` does too.
                    spec.push('f');
                } else {
                    spec.push('f');
                }
                Option::None
            }
            'e' | 'E' | 'g' | 'G' | 'x' | 'X' | 'o' | 'b' => {
                spec.push(conv);
                Option::None
            }
            other => {
                return Err(err(
                    format!("unsupported format character '%{other}' in a % format string"),
                    span,
                ));
            }
        };
        let Some(value) = values.get(next).copied() else {
            return Err(err("not enough arguments for the % format string", span));
        };
        next += 1;
        if !lit.is_empty() {
            parts.push(ast::FStringPart::Literal(std::mem::take(&mut lit)));
        }
        let format_spec = if spec.is_empty() {
            Option::None
        } else {
            Some(Box::new(ast::Expr {
                kind: ast::ExprKind::JoinedStr(vec![ast::FStringPart::Literal(spec)]),
                span,
            }))
        };
        parts.push(ast::FStringPart::Expr {
            expr: value.clone(),
            conversion,
            format_spec,
        });
    }
    if next < values.len() {
        return Err(err("not all arguments converted during % formatting", span));
    }
    if !lit.is_empty() {
        parts.push(ast::FStringPart::Literal(lit));
    }
    if parts.is_empty() {
        return Ok(const_str_expr(""));
    }
    lower_joined_str(&parts, ctx)
}

/// Split a field's `!conv` and `:spec` tail.
///
/// Parsed here rather than through the f-string splitter because a nested
/// `{}` inside a spec means different things in the two syntaxes: in an
/// f-string it is an expression, in `.format()` it names another argument.
/// Rather than quietly do the wrong one, a nested field is rejected.
pub(crate) fn split_format_tail(
    tail: &str,
    span: Span,
) -> SResult<(Option<ast::FStringConversion>, Option<Box<ast::Expr>>)> {
    let mut rest = tail;
    let mut conversion = Option::None;
    if let Some(after) = rest.strip_prefix('!') {
        let (c, r) = after.split_at(after.len().min(1));
        conversion = Some(match c {
            "s" => ast::FStringConversion::Str,
            "r" => ast::FStringConversion::Repr,
            "a" => ast::FStringConversion::Ascii,
            other => {
                return Err(err(
                    format!("unknown conversion '!{other}' in a format string (use !s, !r or !a)"),
                    span,
                ));
            }
        });
        rest = r;
    }
    let spec = match rest.strip_prefix(':') {
        Some(spec) => spec,
        Option::None => {
            if !rest.is_empty() {
                return Err(err(
                    format!("unexpected '{rest}' in a format string field"),
                    span,
                ));
            }
            return Ok((conversion, Option::None));
        }
    };
    if spec.contains('{') {
        return Err(err(
            "a nested '{...}' inside a format spec is not supported in \
             format(); use an f-string",
            span,
        ));
    }
    Ok((
        conversion,
        Some(Box::new(ast::Expr {
            kind: ast::ExprKind::JoinedStr(vec![ast::FStringPart::Literal(spec.to_string())]),
            span,
        })),
    ))
}

/// Pick the argument a `{...}` field names: empty is the next positional,
/// digits are an explicit index, anything else is a keyword.
pub(crate) fn resolve_format_field(
    name: &str,
    auto: &mut usize,
    positional: &[ir::Expr],
    keywords: &[(String, ir::Expr)],
    span: Span,
) -> SResult<ir::Expr> {
    if name.is_empty() {
        let i = *auto;
        *auto += 1;
        return positional.get(i).cloned().ok_or_else(|| {
            err(
                format!(
                    "format() needs at least {} positional argument{}, got {}",
                    i + 1,
                    if i == 0 { "" } else { "s" },
                    positional.len()
                ),
                span,
            )
        });
    }
    if let Ok(i) = name.parse::<usize>() {
        return positional.get(i).cloned().ok_or_else(|| {
            err(
                format!(
                    "format() index {i} is out of range ({} positional argument{} given)",
                    positional.len(),
                    if positional.len() == 1 { "" } else { "s" }
                ),
                span,
            )
        });
    }
    keywords
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| err(format!("format() has no keyword argument '{name}'"), span))
}

pub(crate) fn lower_joined_str(
    parts: &[ast::FStringPart],
    ctx: &mut FnCtx<'_>,
) -> SResult<ir::Expr> {
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
                render_field(v, *conversion, format_spec.as_deref(), e.span, ctx)?
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

/// Render one replacement field: optional `!s` / `!r` / `!a` conversion, then
/// either `str(value)` or the format mini-language for a `:spec`.
///
/// Takes an already-lowered value rather than the source expression, because
/// `.format()` must evaluate each argument once even when several fields
/// name it.
pub(crate) fn render_field(
    value: ir::Expr,
    conversion: Option<ast::FStringConversion>,
    format_spec: Option<&ast::Expr>,
    span: Span,
    ctx: &mut FnCtx<'_>,
) -> SResult<ir::Expr> {
    let converted = lower_fstring_conversion(value, conversion, span)?;
    match format_spec {
        Option::None => {
            // No `:` → `str(value)` (after optional conversion).
            if converted.ty == ir::Ty::Str {
                Ok(converted)
            } else {
                lower_cast(ast::TypeName::Str, converted, span)
            }
        }
        Some(spec) => {
            let spec_ir = lower_expr(spec, ctx)?;
            if spec_ir.ty != ir::Ty::Str {
                return Err(err(
                    format!("f-string format specifier must be str, got {}", spec_ir.ty),
                    span,
                ));
            }
            // `{x:}` is `{x}`: CPython's empty spec means `str(x)` for every
            // type, containers included -- only a *non-empty* spec reaches
            // list.__format__ and raises.
            if matches!(&spec_ir.kind, ir::ExprKind::ConstStr(t) if t.is_empty()) {
                return if converted.ty == ir::Ty::Str {
                    Ok(converted)
                } else {
                    lower_cast(ast::TypeName::Str, converted, span)
                };
            }
            // Static check: only scalar types we can format.
            match converted.ty {
                ir::Ty::Int | ir::Ty::Float | ir::Ty::Bool | ir::Ty::Str => {}
                other => {
                    return Err(err(format!("format() cannot convert {other} yet"), span));
                }
            }
            Ok(ir::Expr {
                ty: ir::Ty::Str,
                kind: ir::ExprKind::FormatValue {
                    value: Box::new(converted),
                    spec: Box::new(spec_ir),
                },
            })
        }
    }
}

/// Apply f-string `!s` / `!r` / `!a` conversion (or leave value unchanged).
pub(crate) fn lower_fstring_conversion(
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
pub(crate) fn lower_repr_like(value: ir::Expr, ascii: bool, span: Span) -> SResult<ir::Expr> {
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
        ir::Ty::Exception => Ok(ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::ExcRepr(Box::new(value)),
        }),
        // `repr` of a container is its `str`, which is what `print` writes;
        // `ascii` is the same rendering with non-ASCII escaped inside the
        // elements, which the shared printer does under a flag.
        ir::Ty::List(_) | ir::Ty::Tuple(_) | ir::Ty::Dict { .. } | ir::Ty::Set(_) => Ok(ir::Expr {
            ty: ir::Ty::Str,
            kind: if ascii {
                ir::ExprKind::ContainerAscii(Box::new(value))
            } else {
                ir::ExprKind::ContainerRepr(Box::new(value))
            },
        }),
        // `repr` of a dynamic value quotes a top-level str, where `str`
        // leaves it bare; everything else renders the same. `ascii` would
        // need the escape flag pushed through the box, which the shared
        // printer does not carry yet.
        ir::Ty::Any if !ascii => Ok(ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::AnyToStr {
                value: Box::new(value),
                repr: true,
            },
        }),
        other => Err(err(
            format!(
                "{}() cannot convert {other} yet",
                if ascii { "ascii" } else { "repr" }
            ),
            span,
        )),
    }
}
