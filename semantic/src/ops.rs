//! Binary/unary operators, equality protocols, and containment.

use common::Span;
use parser::ast;

use crate::prelude::*;

/// bool → int; int/float pass through; anything else is an error.
pub(crate) fn promote_numeric(value: ir::Expr, span: Span, what: &str) -> SResult<ir::Expr> {
    match value.ty {
        ir::Ty::Int | ir::Ty::Float => Ok(value),
        ir::Ty::Bool => Ok(ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::BoolToInt(Box::new(value)),
        }),
        // A mixed-numeric union comes from a value that keeps each element's
        // or branch's own type -- `[1, 2.5]`, or `1 if c else 2.5`. That is
        // what makes it print like CPython, and it is also why there is no
        // single machine type to compute in. Both suggestions below are
        // checked to work; `float(x)` on the union itself does not, and
        // neither does annotating the target.
        ir::Ty::Union(ms)
            if ms
                .iter()
                .all(|m| matches!(m, ir::Ty::Int | ir::Ty::Float | ir::Ty::Bool)) =>
        {
            Err(err(
                format!(
                    "{what} is not supported for values of type {}: a mixed \
                     numeric value keeps each part's own type, so it has no \
                     single numeric representation to compute in. Give the \
                     parts one type (e.g. `1.0` instead of `1`), or narrow \
                     with `isinstance` first",
                    ir::Ty::Union(ms)
                ),
                span,
            ))
        }
        other => Err(err(
            format!(
                "{what} is not supported for values of type {}",
                display_ty(other)
            ),
            span,
        )),
    }
}

/// Promote both operands to a common numeric type (int unless either side
/// is float).
pub(crate) fn unify_numeric(
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
pub(crate) fn join_types(a: ir::Ty, b: ir::Ty) -> ir::Ty {
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
pub(crate) fn unify_and_or(
    l: ir::Expr,
    r: ir::Expr,
    span: Span,
) -> SResult<(ir::Expr, ir::Expr, ir::Ty)> {
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
pub(crate) fn math_intrinsic(name: &str) -> Option<ir::MathOp> {
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
pub(crate) fn unwrap_from_union_peel(e: ir::Expr) -> ir::Expr {
    match e.kind {
        ir::ExprKind::FromUnion { value } => *value,
        _ => e,
    }
}

/// `expr is None` / `expr is not None`, or pointer identity for heap objects
/// (lists, dicts, sets, tuples, str, closures, generators, files).
pub(crate) fn lower_is_none(
    op: ast::BinOp,
    l: ir::Expr,
    r: ir::Expr,
    span: Span,
) -> SResult<ir::Expr> {
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
            // A None-typed call/local still needs evaluation (side effects,
            // exceptions and unbound reads), even when the result is known.
            Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Block {
                    stmts: vec![ir::Stmt::ExprStmt(l), ir::Stmt::ExprStmt(r)],
                    result: Box::new(ir::Expr {
                        ty: ir::Ty::Bool,
                        kind: ir::ExprKind::ConstBool(!not),
                    }),
                },
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

pub(crate) fn lower_binary(
    op: ast::BinOp,
    l: ir::Expr,
    r: ir::Expr,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let describe = format!("operator '{op}'");

    // membership tests work on str and list; check before type dispatch
    if matches!(op, ast::BinOp::In | ast::BinOp::NotIn) {
        return lower_contains(op, l, r, span, ctx);
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
            ast::BinOp::Eq | ast::BinOp::NotEq => {
                let (ir::Ty::Set(a), ir::Ty::Set(b)) = (l.ty, r.ty) else {
                    unreachable!("set compare");
                };
                if a != b {
                    return Err(err(format!("cannot compare set[{a}] and set[{b}]"), span));
                }
                return Ok(ir::Expr {
                    ty: ir::Ty::Bool,
                    kind: ir::ExprKind::Binary {
                        op: comparison_ir_op(op),
                        left: Box::new(l),
                        right: Box::new(r),
                    },
                });
            }
            ast::BinOp::Lt => return lower_set_relation(l, r, span, "lt"),
            ast::BinOp::LtEq => return lower_set_relation(l, r, span, "le"),
            ast::BinOp::Gt => return lower_set_relation(l, r, span, "gt"),
            ast::BinOp::GtEq => return lower_set_relation(l, r, span, "ge"),
            _ => {}
        }
    }

    // ---- string operations ----
    if l.ty == ir::Ty::Str || r.ty == ir::Ty::Str {
        return lower_str_binary(op, l, r, span);
    }
    // ---- list + / * ----
    if matches!(l.ty, ir::Ty::List(_)) || matches!(r.ty, ir::Ty::List(_)) {
        return lower_list_binary(op, l, r, span, ctx);
    }
    // ---- tuple equality ----
    if matches!(l.ty, ir::Ty::Tuple(_)) || matches!(r.ty, ir::Ty::Tuple(_)) {
        return lower_tuple_binary(op, l, r, span, ctx);
    }
    // ---- class operators (identity or matching dunder) ----
    if matches!(l.ty, ir::Ty::Class(_)) || matches!(r.ty, ir::Ty::Class(_)) {
        match op {
            ast::BinOp::Eq
            | ast::BinOp::NotEq
            | ast::BinOp::Lt
            | ast::BinOp::LtEq
            | ast::BinOp::Gt
            | ast::BinOp::GtEq => {
                return lower_class_compare(op, l, r, span, ctx);
            }
            // Arithmetic and bitwise, when either side offers a slot. When
            // neither does, fall through so a mixed operand still gets the
            // numeric path's wording rather than a dunder hint.
            _ if class_arith_method(op).is_some() && class_arith_slot_exists(op, l.ty, r.ty) => {
                return lower_class_arith(op, l, r, span, ctx);
            }
            _ => {}
        }
    }

    match op {
        // Reached only when neither operand offered `__matmul__`: no builtin
        // type implements `@`, so there is no numeric path to fall back to.
        ast::BinOp::MatMul => Err(err(
            format!(
                "operator '@' is not supported between {} and {}: no builtin type \
                 implements matrix multiplication, so an operand must be a class \
                 defining __matmul__",
                display_ty(l.ty),
                display_ty(r.ty)
            ),
            span,
        )),
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
            // Comparison must preserve an integer's exact value. Converting
            // it to f64 can collapse distinct integers above 2**53, or turn
            // a finite bigint into infinity. The IR permits mixed numeric
            // comparison operands; only bool needs promotion here.
            let l = promote_numeric(l, span, &describe)?;
            let r = promote_numeric(r, span, &describe)?;
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
pub(crate) fn lower_bitwise(
    op: ast::BinOp,
    l: ir::Expr,
    r: ir::Expr,
    span: Span,
) -> SResult<ir::Expr> {
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

/// Class instance `==` / `!=` / `<` / `<=` / `>` / `>=`.
///
/// If the left class (or a parent) defines the matching dunder
/// (`__eq__` / `__ne__` / `__lt__` / `__le__` / `__gt__` / `__ge__`), call it
/// (virtual). Otherwise the right operand's reflected slot is tried
/// (`b.__eq__(a)` for `a == b`, `b.__gt__(a)` for `a < b`). Reflected
/// equality/inequality is used only when the left type is assignable to `other`;
/// otherwise `==` / `!=` fall back to pointer identity when both sides
/// are class instances (CPython default when neither side has a usable
/// `__eq__` / `__ne__` protocol).
/// Ordering has no identity fallback. If the right type is a proper
/// subclass of the left and defines the reflected method, it is tried
/// first (CPython subclass-first). There is no `NotImplemented`
/// fallthrough. Missing `__ne__` delegates to negated `__eq__` on that
/// receiver. Operands are always evaluated once in source order, regardless
/// of which receiver is selected. `sorted` / `list.sort` / `min` / `max`
/// desugar to `<`.
pub(crate) fn lower_class_compare(
    op: ast::BinOp,
    l: ir::Expr,
    r: ir::Expr,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let method = match op {
        ast::BinOp::Eq => "__eq__",
        ast::BinOp::NotEq => "__ne__",
        ast::BinOp::Lt => "__lt__",
        ast::BinOp::LtEq => "__le__",
        ast::BinOp::Gt => "__gt__",
        ast::BinOp::GtEq => "__ge__",
        _ => {
            return Err(err(
                format!("operator '{op}' is not supported for class instances"),
                span,
            ));
        }
    };
    let reflected = class_reflected_method(op);

    // CPython: if the right type is a proper subtype of the left and has the
    // reflected slot, try `right.reflected(left)` first.
    if let (ir::Ty::Class(lid), ir::Ty::Class(rid), Some(refl)) = (l.ty, r.ty, reflected)
        && lid != rid
        && class_is_subclass(rid, lid)
        && class_reflected_usable(op, rid, l.ty, ctx)
        && let Some(method) = resolve_class_comparison(rid, refl)
    {
        return lower_class_cmp_call(l, r, method, true, span, ctx);
    }

    if let ir::Ty::Class(id) = l.ty
        && let Some(method) = resolve_class_comparison(id, method)
    {
        return lower_class_cmp_call(l, r, method, false, span, ctx);
    }

    if let (ir::Ty::Class(id), Some(refl)) = (r.ty, reflected)
        && class_reflected_usable(op, id, l.ty, ctx)
        && let Some(method) = resolve_class_comparison(id, refl)
    {
        return lower_class_cmp_call(l, r, method, true, span, ctx);
    }

    if matches!(op, ast::BinOp::Eq | ast::BinOp::NotEq) {
        if matches!((l.ty, r.ty), (ir::Ty::Class(_), ir::Ty::Class(_))) {
            return Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::IsIdentity {
                    left: Box::new(l),
                    right: Box::new(r),
                    not: matches!(op, ast::BinOp::NotEq),
                },
            });
        }
        return Err(err(
            format!(
                "cannot compare {} and {} with '{op}' (define {method}{} or use 'is')",
                l.ty,
                r.ty,
                if op == ast::BinOp::NotEq {
                    " or __eq__"
                } else {
                    ""
                }
            ),
            span,
        ));
    }
    let hint = match reflected {
        Some(refl) => format!("define {method} or reflected {refl}"),
        None => format!("define {method}"),
    };
    Err(err(
        format!("operator '{op}' is not supported for class instances ({hint})"),
        span,
    ))
}

/// A dunder call with both operands already spilled to temps.
///
/// The spill is the whole point: source order is left-then-right whichever
/// operand ends up the receiver, and a reflected dispatch would otherwise
/// evaluate the right one first. `cli/tests/protocol_order.rs` pins that.
pub(crate) struct SpilledBinopCall {
    pub(crate) stmts: Vec<ir::Stmt>,
    pub(crate) call: ir::Expr,
}

pub(crate) fn lower_class_binop_call(
    left: ir::Expr,
    right: ir::Expr,
    method: &'static str,
    reflected: bool,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<SpilledBinopCall> {
    let left_t = ctx.fresh_temp("cmp.l", left.ty);
    let right_t = ctx.fresh_temp("cmp.r", right.ty);
    let (receiver, argument) = if reflected {
        (local_expr(right_t.clone(), right.ty), left_t.clone())
    } else {
        (local_expr(left_t.clone(), left.ty), right_t.clone())
    };
    let ir::Ty::Class(class_id) = receiver.ty else {
        unreachable!("class operator receiver must be a class instance");
    };
    let argument_name = ast::Expr {
        kind: ast::ExprKind::Name(argument),
        span,
    };
    let call = lower_instance_method_call(receiver, class_id, method, span, &[argument_name], ctx)?;
    Ok(SpilledBinopCall {
        // Source order: evaluate left, then right, then dispatch.
        stmts: vec![
            ir::Stmt::Assign {
                name: left_t,
                value: left,
            },
            ir::Stmt::Assign {
                name: right_t,
                value: right,
            },
        ],
        call,
    })
}

pub(crate) fn lower_class_cmp_call(
    left: ir::Expr,
    right: ir::Expr,
    method: ClassComparisonMethod,
    reflected: bool,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let spilled = lower_class_binop_call(left, right, method.name, reflected, span, ctx)?;
    // A comparison answers yes or no whatever the dunder returned.
    let call = if spilled.call.ty == ir::Ty::Bool {
        spilled.call
    } else {
        to_bool(spilled.call, span, ctx)?
    };
    let result = if method.invert {
        ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::Unary {
                op: ir::UnOp::Not,
                operand: Box::new(call),
            },
        }
    } else {
        call
    };
    Ok(ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Block {
            stmts: spilled.stmts,
            result: Box::new(result),
        },
    })
}

/// Whether either operand offers a slot for `op`, so the guard can decline
/// and leave a mixed `Point + 1` to the numeric path's own wording.
pub(crate) fn class_arith_slot_exists(op: ast::BinOp, l: ir::Ty, r: ir::Ty) -> bool {
    let direct = class_arith_method(op)
        .is_some_and(|m| matches!(l, ir::Ty::Class(id) if resolve_method(id, m).is_some()));
    let reflected = class_reflected_arith_method(op)
        .is_some_and(|m| matches!(r, ir::Ty::Class(id) if resolve_method(id, m).is_some()));
    direct || reflected
}

/// `a <op> b` where at least one side is a class instance.
///
/// Resolution follows CPython's order, and unlike a comparison the result
/// keeps the dunder's own return type — `V.__add__ -> V` yields a `V`.
pub(crate) fn lower_class_arith(
    op: ast::BinOp,
    l: ir::Expr,
    r: ir::Expr,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let method = class_arith_method(op).expect("caller filtered to arithmetic operators");
    let reflected = class_reflected_arith_method(op);

    let finish = |spilled: SpilledBinopCall| {
        Ok(ir::Expr {
            ty: spilled.call.ty,
            kind: ir::ExprKind::Block {
                stmts: spilled.stmts,
                result: Box::new(spilled.call),
            },
        })
    };

    // CPython: a proper subclass on the right gets the first attempt, so a
    // subclass can override its base's arithmetic from either side.
    if let (ir::Ty::Class(lid), ir::Ty::Class(rid), Some(refl)) = (l.ty, r.ty, reflected)
        && lid != rid
        && class_is_subclass(rid, lid)
        && resolve_method(rid, refl).is_some()
        && class_equality_accepts(rid, refl, l.ty, ctx)
    {
        return finish(lower_class_binop_call(l, r, refl, true, span, ctx)?);
    }

    if let ir::Ty::Class(id) = l.ty
        && resolve_method(id, method).is_some()
        && class_equality_accepts(id, method, r.ty, ctx)
    {
        return finish(lower_class_binop_call(l, r, method, false, span, ctx)?);
    }

    // `2.0 * vec`: the left operand has no slot to offer, so the right one's
    // reflected form is the answer.
    if let (ir::Ty::Class(id), Some(refl)) = (r.ty, reflected)
        && resolve_method(id, refl).is_some()
        && class_equality_accepts(id, refl, l.ty, ctx)
    {
        return finish(lower_class_binop_call(l, r, refl, true, span, ctx)?);
    }

    let hint = match reflected {
        Some(refl) => format!("define {method} on the left operand or {refl} on the right"),
        Option::None => format!("define {method}"),
    };
    Err(err(
        format!(
            "operator '{op}' is not supported between {} and {} ({hint})",
            display_ty(l.ty),
            display_ty(r.ty)
        ),
        span,
    ))
}

/// `x <op>= y`, which prefers the in-place dunder and otherwise is `x <op> y`.
///
/// CPython rebinds the result either way, so `__iadd__` returning `self` is
/// what makes the mutation visible; a class defining only `__add__` still
/// works, and gets a new object.
pub(crate) fn lower_aug_binary(
    op: ast::BinOp,
    left: ir::Expr,
    right: ir::Expr,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if let ir::Ty::Class(id) = left.ty
        && let Some(inplace) = class_inplace_arith_method(op)
        && resolve_method(id, inplace).is_some()
        && class_equality_accepts(id, inplace, right.ty, ctx)
    {
        let spilled = lower_class_binop_call(left, right, inplace, false, span, ctx)?;
        return Ok(ir::Expr {
            ty: spilled.call.ty,
            kind: ir::ExprKind::Block {
                stmts: spilled.stmts,
                result: Box::new(spilled.call),
            },
        });
    }
    lower_binary(op, left, right, span, ctx)
}

/// `-x` / `+x` on a number, with CPython's wording when it is not one.
pub(crate) fn unary_numeric(value: ir::Expr, sym: &str, span: Span) -> SResult<ir::Expr> {
    if matches!(value.ty, ir::Ty::Int | ir::Ty::Float | ir::Ty::Bool) {
        return promote_numeric(value, span, &format!("unary '{sym}'"));
    }
    Err(err(
        format!(
            "bad operand type for unary {sym}: '{}'",
            display_ty(value.ty)
        ),
        span,
    ))
}

/// `-x` / `+x` / `~x` where `x` is a class instance.
pub(crate) fn lower_class_unary(
    op: ast::UnaryOp,
    value: ir::Expr,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let method = class_unary_method(op).expect("caller filtered to unary operators");
    let ir::Ty::Class(id) = value.ty else {
        unreachable!("class unary receiver must be a class instance");
    };
    if resolve_method(id, method).is_none() {
        let sym = match op {
            ast::UnaryOp::Neg => "-",
            ast::UnaryOp::Pos => "+",
            _ => "~",
        };
        return Err(err(
            format!(
                "bad operand type for unary {sym}: '{}' (define {method})",
                display_ty(value.ty)
            ),
            span,
        ));
    }
    lower_instance_method_call(value, id, method, span, &[], ctx)
}

pub(crate) fn comparison_ir_op(op: ast::BinOp) -> ir::BinOp {
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

/// Element types whose container `==` must go through class `__eq__`
/// (including nested lists/tuples of those types).
pub(crate) fn ty_uses_class_eq(ty: ir::Ty) -> bool {
    match ty {
        ir::Ty::Class(_) => true,
        ir::Ty::List(inner) => ty_uses_class_eq(*inner),
        ir::Ty::Tuple(elems) => elems.iter().copied().any(ty_uses_class_eq),
        _ => false,
    }
}

/// Homogeneous tuple whose elements need class `==` (same static type).
pub(crate) fn homogeneous_class_tuple_elem(elems: &[ir::Ty]) -> Option<ir::Ty> {
    let first = *elems.first()?;
    if ty_uses_class_eq(first) && elems.iter().all(|e| *e == first) {
        Some(first)
    } else {
        None
    }
}

/// Types whose IR `BinOp::Eq` codegen already implements.
pub(crate) fn ty_has_binop_eq(ty: ir::Ty) -> bool {
    matches!(
        ty,
        ir::Ty::Int | ir::Ty::Float | ir::Ty::Bool | ir::Ty::Str | ir::Ty::Set(_)
    )
}

pub(crate) fn bool_not(operand: ir::Expr) -> ir::Expr {
    ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Unary {
            op: ir::UnOp::Not,
            operand: Box::new(operand),
        },
    }
}

pub(crate) fn assign_incr(name: String) -> ir::Stmt {
    ir::Stmt::Assign {
        name: name.clone(),
        value: ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Binary {
                op: ir::BinOp::Add,
                left: Box::new(local_expr(name, ir::Ty::Int)),
                right: Box::new(int_const(1)),
            },
        },
    }
}

pub(crate) fn index_at(base: ir::Expr, index: ir::Expr, elem: ir::Ty) -> ir::Expr {
    ir::Expr {
        ty: elem,
        kind: ir::ExprKind::Index {
            base: Box::new(base),
            index: Box::new(index),
        },
    }
}

pub(crate) fn len_of(value: ir::Expr) -> ir::Expr {
    ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::Len(Box::new(value)),
    }
}

/// Container-element `==`. Class pairs use identity then `__eq__` (CPython
/// `PyObject_RichCompareBool`); scalar `a == a` still goes through
/// `lower_class_compare` and always calls `__eq__`. List/tuple recurse.
/// Other pairs must be types whose IR `Eq` codegen implements, or they
/// are compared via a 1-tuple so `pyrs_tuple_eq` / `slot_eq` runs
/// (dict/set/union/pointer slots in mixed tuples).
pub(crate) fn lower_eq(l: ir::Expr, r: ir::Expr, span: Span, ctx: &mut FnCtx) -> SResult<ir::Expr> {
    match (l.ty, r.ty) {
        (ir::Ty::Class(_), _) | (_, ir::Ty::Class(_)) => {
            lower_identity_or_class_eq(l, r, span, ctx)
        }
        (ir::Ty::List(_), ir::Ty::List(_)) => lower_list_binary(ast::BinOp::Eq, l, r, span, ctx),
        (ir::Ty::Tuple(_), ir::Ty::Tuple(_)) => lower_tuple_binary(ast::BinOp::Eq, l, r, span, ctx),
        _ if l.ty == r.ty && ty_has_binop_eq(l.ty) => Ok(ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::Binary {
                op: ir::BinOp::Eq,
                left: Box::new(l),
                right: Box::new(r),
            },
        }),
        _ => Ok(unit_tuple_slot_eq(l, r)),
    }
}

pub(crate) fn unit_tuple_slot_eq(l: ir::Expr, r: ir::Expr) -> ir::Expr {
    let ty = ir::tuple_of(&[l.ty]);
    ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Binary {
            op: ir::BinOp::Eq,
            left: Box::new(ir::Expr {
                ty,
                kind: ir::ExprKind::TupleLit(vec![l]),
            }),
            right: Box::new(ir::Expr {
                ty,
                kind: ir::ExprKind::TupleLit(vec![r]),
            }),
        },
    }
}

pub(crate) fn lower_identity_or_class_eq(
    left: ir::Expr,
    right: ir::Expr,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let l_t = ctx.fresh_temp("ceq.l", left.ty);
    let r_t = ctx.fresh_temp("ceq.r", right.ty);
    let l = local_expr(l_t.clone(), left.ty);
    let r = local_expr(r_t.clone(), right.ty);
    let eq = lower_class_compare(ast::BinOp::Eq, l.clone(), r.clone(), span, ctx)?;
    Ok(ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Block {
            stmts: vec![
                ir::Stmt::Assign {
                    name: l_t,
                    value: left,
                },
                ir::Stmt::Assign {
                    name: r_t,
                    value: right,
                },
            ],
            result: Box::new(bool_or(is_same(l, r), eq)),
        },
    })
}

pub(crate) fn maybe_invert_eq(eq: ir::Expr, op: ast::BinOp) -> ir::Expr {
    if op == ast::BinOp::NotEq {
        bool_not(eq)
    } else {
        eq
    }
}

/// CPython list/tuple equality: compare lengths, then `left[i] == right[i]`.
/// `!=` is the negation of that result, not per-element `__ne__`.
pub(crate) fn lower_list_eq_protocol(
    left: ir::Expr,
    right: ir::Expr,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let ir::Ty::List(elem) = left.ty else {
        unreachable!("list eq protocol");
    };
    let l_t = ctx.fresh_temp("leq.l", left.ty);
    let r_t = ctx.fresh_temp("leq.r", right.ty);
    let n_t = ctx.fresh_temp("leq.n", ir::Ty::Int);
    let m_t = ctx.fresh_temp("leq.m", ir::Ty::Int);
    let i_t = ctx.fresh_temp("leq.i", ir::Ty::Int);
    let out_t = ctx.fresh_temp("leq.out", ir::Ty::Bool);
    let l = local_expr(l_t.clone(), left.ty);
    let r = local_expr(r_t.clone(), right.ty);
    let n = local_expr(n_t.clone(), ir::Ty::Int);
    let m = local_expr(m_t.clone(), ir::Ty::Int);
    let i = local_expr(i_t.clone(), ir::Ty::Int);
    let item_eq = lower_eq(
        index_at(l.clone(), i.clone(), *elem),
        index_at(r.clone(), i.clone(), *elem),
        span,
        ctx,
    )?;
    Ok(ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Block {
            stmts: vec![
                ir::Stmt::Assign {
                    name: l_t,
                    value: left,
                },
                ir::Stmt::Assign {
                    name: r_t,
                    value: right,
                },
                ir::Stmt::If {
                    branches: vec![(
                        is_same(l.clone(), r.clone()),
                        vec![ir::Stmt::Assign {
                            name: out_t.clone(),
                            value: const_bool_expr(true),
                        }],
                    )],
                    orelse: vec![
                        ir::Stmt::Assign {
                            name: n_t,
                            value: len_of(l),
                        },
                        ir::Stmt::Assign {
                            name: m_t,
                            value: len_of(r),
                        },
                        ir::Stmt::Assign {
                            name: out_t.clone(),
                            value: const_bool_expr(true),
                        },
                        ir::Stmt::If {
                            branches: vec![(
                                key_cmp(ir::BinOp::Ne, n.clone(), m),
                                vec![ir::Stmt::Assign {
                                    name: out_t.clone(),
                                    value: const_bool_expr(false),
                                }],
                            )],
                            orelse: vec![
                                ir::Stmt::Assign {
                                    name: i_t.clone(),
                                    value: int_const(0),
                                },
                                ir::Stmt::While {
                                    cond: key_cmp(ir::BinOp::Lt, i, n),
                                    body: vec![
                                        ir::Stmt::If {
                                            branches: vec![(
                                                bool_not(item_eq),
                                                vec![
                                                    ir::Stmt::Assign {
                                                        name: out_t.clone(),
                                                        value: const_bool_expr(false),
                                                    },
                                                    ir::Stmt::Break,
                                                ],
                                            )],
                                            orelse: vec![],
                                        },
                                        assign_incr(i_t),
                                    ],
                                    step: vec![],
                                },
                            ],
                        },
                    ],
                },
            ],
            result: Box::new(local_expr(out_t, ir::Ty::Bool)),
        },
    })
}

/// CPython `list_contains`: `item == needle`. Bind needle first so
/// `needle in haystack` still evaluates operands in source order.
pub(crate) fn lower_list_contains_protocol(
    needle: ir::Expr,
    haystack: ir::Expr,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let elem = match haystack.ty {
        ir::Ty::List(elem) => *elem,
        ir::Ty::Tuple(elems) => match homogeneous_class_tuple_elem(elems) {
            Some(elem) => elem,
            None => unreachable!("seq contains protocol"),
        },
        other => unreachable!("seq contains protocol on {other}"),
    };
    let ndl_t = ctx.fresh_temp("lin.ndl", needle.ty);
    let hay_t = ctx.fresh_temp("lin.hay", haystack.ty);
    let n_t = ctx.fresh_temp("lin.n", ir::Ty::Int);
    let i_t = ctx.fresh_temp("lin.i", ir::Ty::Int);
    let out_t = ctx.fresh_temp("lin.out", ir::Ty::Bool);
    let ndl = local_expr(ndl_t.clone(), needle.ty);
    let hay = local_expr(hay_t.clone(), haystack.ty);
    let n = local_expr(n_t.clone(), ir::Ty::Int);
    let i = local_expr(i_t.clone(), ir::Ty::Int);
    let item_eq = lower_eq(index_at(hay.clone(), i.clone(), elem), ndl, span, ctx)?;
    Ok(ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Block {
            stmts: vec![
                ir::Stmt::Assign {
                    name: ndl_t,
                    value: needle,
                },
                ir::Stmt::Assign {
                    name: hay_t,
                    value: haystack,
                },
                ir::Stmt::Assign {
                    name: n_t,
                    value: len_of(hay),
                },
                ir::Stmt::Assign {
                    name: out_t.clone(),
                    value: const_bool_expr(false),
                },
                ir::Stmt::Assign {
                    name: i_t.clone(),
                    value: int_const(0),
                },
                ir::Stmt::While {
                    cond: key_cmp(ir::BinOp::Lt, i, n),
                    body: vec![
                        ir::Stmt::If {
                            branches: vec![(
                                item_eq,
                                vec![
                                    ir::Stmt::Assign {
                                        name: out_t.clone(),
                                        value: const_bool_expr(true),
                                    },
                                    ir::Stmt::Break,
                                ],
                            )],
                            orelse: vec![],
                        },
                        assign_incr(i_t),
                    ],
                    step: vec![],
                },
            ],
            result: Box::new(local_expr(out_t, ir::Ty::Bool)),
        },
    })
}

/// CPython `listindex` / `list_count` / `list_remove`: `item == needle`.
pub(crate) fn lower_list_find_eq(
    list: ir::Expr,
    needle: ir::Expr,
    elem: ir::Ty,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<(Vec<ir::Stmt>, ir::Expr, ir::Expr, String)> {
    let xs_t = ctx.fresh_temp("lfind.xs", list.ty);
    let ndl_t = ctx.fresh_temp("lfind.ndl", needle.ty);
    let xs = local_expr(xs_t.clone(), list.ty);
    let ndl = local_expr(ndl_t.clone(), needle.ty);
    let i_t = ctx.fresh_temp("lfind.i", ir::Ty::Int);
    let i = local_expr(i_t.clone(), ir::Ty::Int);
    let item_eq = lower_eq(index_at(xs.clone(), i, elem), ndl, span, ctx)?;
    let stmts = vec![
        ir::Stmt::Assign {
            name: xs_t,
            value: list,
        },
        ir::Stmt::Assign {
            name: ndl_t,
            value: needle,
        },
    ];
    Ok((stmts, xs, item_eq, i_t))
}

pub(crate) fn push_adjust_seq_bounds(
    stmts: &mut Vec<ir::Stmt>,
    n: &ir::Expr,
    start_t: String,
    end_t: String,
) -> (ir::Expr, ir::Expr) {
    let start_e = local_expr(start_t.clone(), ir::Ty::Int);
    let end_e = local_expr(end_t.clone(), ir::Ty::Int);
    let add_n = |value: ir::Expr| ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::Binary {
            op: ir::BinOp::Add,
            left: Box::new(value),
            right: Box::new(n.clone()),
        },
    };
    stmts.push(ir::Stmt::If {
        branches: vec![(
            key_cmp(ir::BinOp::Lt, start_e.clone(), int_const(0)),
            vec![
                ir::Stmt::Assign {
                    name: start_t.clone(),
                    value: add_n(start_e.clone()),
                },
                ir::Stmt::If {
                    branches: vec![(
                        key_cmp(ir::BinOp::Lt, start_e.clone(), int_const(0)),
                        vec![ir::Stmt::Assign {
                            name: start_t.clone(),
                            value: int_const(0),
                        }],
                    )],
                    orelse: vec![],
                },
            ],
        )],
        orelse: vec![],
    });
    stmts.push(ir::Stmt::If {
        branches: vec![
            (
                key_cmp(ir::BinOp::Eq, end_e.clone(), int_const(i64::MIN)),
                vec![ir::Stmt::Assign {
                    name: end_t.clone(),
                    value: n.clone(),
                }],
            ),
            (
                key_cmp(ir::BinOp::Lt, end_e.clone(), int_const(0)),
                vec![
                    ir::Stmt::Assign {
                        name: end_t.clone(),
                        value: add_n(end_e.clone()),
                    },
                    ir::Stmt::If {
                        branches: vec![(
                            key_cmp(ir::BinOp::Lt, end_e.clone(), int_const(0)),
                            vec![ir::Stmt::Assign {
                                name: end_t.clone(),
                                value: int_const(0),
                            }],
                        )],
                        orelse: vec![],
                    },
                ],
            ),
            (
                key_cmp(ir::BinOp::Gt, end_e.clone(), n.clone()),
                vec![ir::Stmt::Assign {
                    name: end_t,
                    value: n.clone(),
                }],
            ),
        ],
        orelse: vec![],
    });
    (start_e, end_e)
}

pub(crate) fn lower_list_index_protocol(
    list: ir::Expr,
    needle: ir::Expr,
    elem: ir::Ty,
    start: ir::Expr,
    end: ir::Expr,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let miss = if matches!(list.ty, ir::Ty::Tuple(_)) {
        "tuple.index(x): x not in tuple"
    } else {
        "list.index(x): x not in list"
    };
    let (mut stmts, xs, item_eq, i_t) = lower_list_find_eq(list, needle, elem, span, ctx)?;
    let start_t = ctx.fresh_temp("lidx.s", ir::Ty::Int);
    let end_t = ctx.fresh_temp("lidx.e", ir::Ty::Int);
    let n_t = ctx.fresh_temp("lidx.n", ir::Ty::Int);
    let found_t = ctx.fresh_temp("lidx.found", ir::Ty::Bool);
    let out_t = ctx.fresh_temp("lidx.out", ir::Ty::Int);
    let n = local_expr(n_t.clone(), ir::Ty::Int);
    let i = local_expr(i_t.clone(), ir::Ty::Int);
    stmts.extend([
        ir::Stmt::Assign {
            name: start_t.clone(),
            value: start,
        },
        ir::Stmt::Assign {
            name: end_t.clone(),
            value: end,
        },
        ir::Stmt::Assign {
            name: n_t,
            value: len_of(xs),
        },
    ]);
    let (start_e, end_e) = push_adjust_seq_bounds(&mut stmts, &n, start_t, end_t);
    stmts.extend([
        ir::Stmt::Assign {
            name: found_t.clone(),
            value: const_bool_expr(false),
        },
        ir::Stmt::Assign {
            name: out_t.clone(),
            value: int_const(0),
        },
        ir::Stmt::Assign {
            name: i_t.clone(),
            value: start_e,
        },
        ir::Stmt::While {
            cond: key_cmp(ir::BinOp::Lt, i.clone(), end_e),
            body: vec![
                ir::Stmt::If {
                    branches: vec![(
                        item_eq,
                        vec![
                            ir::Stmt::Assign {
                                name: found_t.clone(),
                                value: const_bool_expr(true),
                            },
                            ir::Stmt::Assign {
                                name: out_t.clone(),
                                value: i,
                            },
                            ir::Stmt::Break,
                        ],
                    )],
                    orelse: vec![],
                },
                assign_incr(i_t),
            ],
            step: vec![],
        },
        ir::Stmt::If {
            branches: vec![(
                bool_not(local_expr(found_t, ir::Ty::Bool)),
                vec![ir::Stmt::Raise {
                    exc: ir::ExcType::ValueError,
                    message: Some(const_str(miss)),
                }],
            )],
            orelse: vec![],
        },
    ]);
    Ok(ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(local_expr(out_t, ir::Ty::Int)),
        },
    })
}

pub(crate) fn lower_list_count_protocol(
    list: ir::Expr,
    needle: ir::Expr,
    elem: ir::Ty,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let (mut stmts, xs, item_eq, i_t) = lower_list_find_eq(list, needle, elem, span, ctx)?;
    let n_t = ctx.fresh_temp("lcnt.n", ir::Ty::Int);
    let out_t = ctx.fresh_temp("lcnt.out", ir::Ty::Int);
    let n = local_expr(n_t.clone(), ir::Ty::Int);
    let i = local_expr(i_t.clone(), ir::Ty::Int);
    stmts.extend([
        ir::Stmt::Assign {
            name: n_t,
            value: len_of(xs),
        },
        ir::Stmt::Assign {
            name: out_t.clone(),
            value: int_const(0),
        },
        ir::Stmt::Assign {
            name: i_t.clone(),
            value: int_const(0),
        },
        ir::Stmt::While {
            cond: key_cmp(ir::BinOp::Lt, i, n),
            body: vec![
                ir::Stmt::If {
                    branches: vec![(item_eq, vec![assign_incr(out_t.clone())])],
                    orelse: vec![],
                },
                assign_incr(i_t),
            ],
            step: vec![],
        },
    ]);
    Ok(ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(local_expr(out_t, ir::Ty::Int)),
        },
    })
}

pub(crate) fn lower_list_remove_protocol(
    list: ir::Expr,
    needle: ir::Expr,
    elem: ir::Ty,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Stmt> {
    let (mut stmts, xs, item_eq, i_t) = lower_list_find_eq(list, needle, elem, span, ctx)?;
    let n_t = ctx.fresh_temp("lrm.n", ir::Ty::Int);
    let found_t = ctx.fresh_temp("lrm.found", ir::Ty::Bool);
    let n = local_expr(n_t.clone(), ir::Ty::Int);
    let i = local_expr(i_t.clone(), ir::Ty::Int);
    stmts.extend([
        ir::Stmt::Assign {
            name: n_t,
            value: len_of(xs.clone()),
        },
        ir::Stmt::Assign {
            name: found_t.clone(),
            value: const_bool_expr(false),
        },
        ir::Stmt::Assign {
            name: i_t.clone(),
            value: int_const(0),
        },
        ir::Stmt::While {
            cond: key_cmp(ir::BinOp::Lt, i.clone(), n),
            body: vec![
                ir::Stmt::If {
                    branches: vec![(
                        item_eq,
                        vec![
                            ir::Stmt::IndexDelete { base: xs, index: i },
                            ir::Stmt::Assign {
                                name: found_t.clone(),
                                value: const_bool_expr(true),
                            },
                            ir::Stmt::Break,
                        ],
                    )],
                    orelse: vec![],
                },
                assign_incr(i_t),
            ],
            step: vec![],
        },
        ir::Stmt::If {
            branches: vec![(
                bool_not(local_expr(found_t, ir::Ty::Bool)),
                vec![ir::Stmt::Raise {
                    exc: ir::ExcType::ValueError,
                    message: Some(const_str("list.remove(x): x not in list")),
                }],
            )],
            orelse: vec![],
        },
    ]);
    Ok(ir::Stmt::ExprStmt(ir::Expr {
        ty: ir::Ty::None,
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(const_none()),
        },
    }))
}

pub(crate) fn lower_tuple_eq_protocol(
    left: ir::Expr,
    right: ir::Expr,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let ir::Ty::Tuple(elems) = left.ty else {
        unreachable!("tuple eq protocol");
    };
    let l_t = ctx.fresh_temp("teq.l", left.ty);
    let r_t = ctx.fresh_temp("teq.r", right.ty);
    let l = local_expr(l_t.clone(), left.ty);
    let r = local_expr(r_t.clone(), right.ty);
    let stmts = vec![
        ir::Stmt::Assign {
            name: l_t,
            value: left,
        },
        ir::Stmt::Assign {
            name: r_t,
            value: right,
        },
    ];
    let mut eq = const_bool_expr(true);
    for (i, elem) in elems.iter().enumerate() {
        let pair = lower_eq(
            index_at(l.clone(), int_const(i as i64), *elem),
            index_at(r.clone(), int_const(i as i64), *elem),
            span,
            ctx,
        )?;
        eq = if i == 0 { pair } else { bool_and(eq, pair) };
    }
    Ok(ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(bool_or(is_same(l, r), eq)),
        },
    })
}

/// `needle in haystack` / `not in`: substring, list/tuple/set membership, dict keys,
/// or user-class `__contains__`.
pub(crate) fn lower_contains(
    op: ast::BinOp,
    l: ir::Expr,
    r: ir::Expr,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    // Class with __contains__(self, item) -> bool (desugar; do not use Contains IR).
    if let ir::Ty::Class(id) = r.ty {
        if resolve_method(id, "__contains__").is_none() {
            return Err(err(
                format!(
                    "'{}' object does not support 'in' (need __contains__)",
                    class_info(id)
                        .map(|c| c.name)
                        .unwrap_or_else(|| format!("class#{id}"))
                ),
                span,
            ));
        }
        // Stash needle/haystack in source order before dispatching on haystack,
        // so effects and exceptions in the needle happen first.
        let hay_t = ctx.fresh_temp("in.hay", r.ty);
        let ndl_t = ctx.fresh_temp("in.ndl", l.ty);
        // Build call via IR temps: we cannot easily pass IR needle through the
        // AST-based method helper, so materialize locals then Name them.
        // Emit as a Block: assign temps, call, result.
        let hay_local = ir::Expr {
            ty: r.ty,
            kind: ir::ExprKind::Local(hay_t.clone()),
        };
        let ndl_name = ast::Expr {
            kind: ast::ExprKind::Name(ndl_t.clone()),
            span,
        };
        // Register needle type before Name lookup in method call.
        // (fresh_temp already registered both.)
        let call =
            lower_instance_method_call(hay_local, id, "__contains__", span, &[ndl_name], ctx)?;
        let call = if call.ty == ir::Ty::Bool {
            call
        } else {
            // CPython prefers bool; accept any truthy return.
            to_bool(call, span, ctx)?
        };
        let result = if op == ast::BinOp::NotIn {
            ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Unary {
                    op: ir::UnOp::Not,
                    operand: Box::new(call),
                },
            }
        } else {
            call
        };
        return Ok(ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::Block {
                stmts: vec![
                    ir::Stmt::Assign {
                        name: ndl_t,
                        value: l,
                    },
                    ir::Stmt::Assign {
                        name: hay_t,
                        value: r,
                    },
                ],
                result: Box::new(result),
            },
        });
    }

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
        ir::Ty::List(elem) => {
            let needle = coerce(l, *elem, span, "'in' operand")?;
            if ty_uses_class_eq(*elem) {
                let contains = lower_list_contains_protocol(needle, r, span, ctx)?;
                return Ok(if op == ast::BinOp::NotIn {
                    bool_not(contains)
                } else {
                    contains
                });
            }
            needle
        }
        ir::Ty::Dict { key, .. } => coerce(l, *key, span, "'in' dict key")?,
        ir::Ty::Set(elem) => coerce(l, *elem, span, "'in' set element")?,
        ir::Ty::Tuple(elems) => {
            if let Some(elem) = homogeneous_class_tuple_elem(elems) {
                let needle = coerce(l, elem, span, "'in' tuple operand")?;
                let contains = lower_list_contains_protocol(needle, r, span, ctx)?;
                return Ok(if op == ast::BinOp::NotIn {
                    bool_not(contains)
                } else {
                    contains
                });
            }
            lower_tuple_search_needle(l, elems, span, "'in' tuple operand")?
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

pub(crate) fn lower_tuple_binary(
    op: ast::BinOp,
    l: ir::Expr,
    r: ir::Expr,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    match op {
        ast::BinOp::Eq
        | ast::BinOp::NotEq
        | ast::BinOp::Lt
        | ast::BinOp::LtEq
        | ast::BinOp::Gt
        | ast::BinOp::GtEq => match (l.ty, r.ty) {
            (ir::Ty::Tuple(a), ir::Ty::Tuple(b)) if a == b => {
                if !matches!(op, ast::BinOp::Eq | ast::BinOp::NotEq)
                    && !a.iter().all(|e| is_orderable_ty(*e))
                {
                    return Err(err(
                        format!(
                            "operator '{op}' is not supported for {0} (element types must be \
                             orderable: int|float|bool|str|tuple of those)",
                            l.ty
                        ),
                        span,
                    ));
                }
                if matches!(op, ast::BinOp::Eq | ast::BinOp::NotEq) && ty_uses_class_eq(l.ty) {
                    let eq = lower_tuple_eq_protocol(l, r, span, ctx)?;
                    return Ok(maybe_invert_eq(eq, op));
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

/// Element types that support lexicographic / numeric ordering (`<` / min / sort).
pub(crate) fn is_orderable_ty(ty: ir::Ty) -> bool {
    match ty {
        ir::Ty::Int | ir::Ty::Float | ir::Ty::Bool | ir::Ty::Str => true,
        ir::Ty::Tuple(elems) => elems.iter().all(|e| is_orderable_ty(*e)),
        ir::Ty::List(elem) => is_orderable_ty(*elem),
        _ => false,
    }
}

pub(crate) fn lower_list_binary(
    op: ast::BinOp,
    l: ir::Expr,
    r: ir::Expr,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
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
        // element-wise equality / lexicographic order (same list element type)
        ast::BinOp::Eq
        | ast::BinOp::NotEq
        | ast::BinOp::Lt
        | ast::BinOp::LtEq
        | ast::BinOp::Gt
        | ast::BinOp::GtEq => match (l.ty, r.ty) {
            (ir::Ty::List(a), ir::Ty::List(b)) if a == b => {
                if !matches!(op, ast::BinOp::Eq | ast::BinOp::NotEq) && !is_orderable_ty(*a) {
                    return Err(err(
                        format!(
                            "operator '{op}' is not supported for list[{a}] (element type must be \
                             orderable: int|float|bool|str|tuple|list of those)"
                        ),
                        span,
                    ));
                }
                if matches!(op, ast::BinOp::Eq | ast::BinOp::NotEq) && ty_uses_class_eq(*a) {
                    let eq = lower_list_eq_protocol(l, r, span, ctx)?;
                    return Ok(maybe_invert_eq(eq, op));
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

pub(crate) fn lower_str_binary(
    op: ast::BinOp,
    l: ir::Expr,
    r: ir::Expr,
    span: Span,
) -> SResult<ir::Expr> {
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
        // `%` on a literal is handled before lowering; reaching here means the
        // format string is a runtime value.
        ast::BinOp::Mod if l.ty == ir::Ty::Str => Err(err(
            "% formatting needs a literal format string, because the \
             conversions are resolved at compile time; use an f-string, or \
             inline the format string",
            span,
        )),
        other => Err(err(
            format!("operator '{other}' is not supported for str"),
            span,
        )),
    }
}

// ---- return-path analysis ----

pub(crate) fn block_returns(stmts: &[ir::Stmt]) -> bool {
    stmts.iter().any(stmt_returns)
}

pub(crate) fn stmt_returns(stmt: &ir::Stmt) -> bool {
    match stmt {
        ir::Stmt::Return(_) => true,
        // A call to a function that always raises transfers control just as
        // a `raise` here would.
        ir::Stmt::ExprStmt(e) => match &e.kind {
            ir::ExprKind::Call { func, .. } => is_no_return(func),
            ir::ExprKind::CallMethod { direct_func, .. } => is_no_return(direct_func),
            _ => false,
        },
        // Die / Raise exit the process or transfer; cannot fall through
        ir::Stmt::Die(_)
        | ir::Stmt::Raise { .. }
        | ir::Stmt::RaiseExc { .. }
        | ir::Stmt::Reraise => true,
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
pub(crate) fn block_may_raise(stmts: &[ir::Stmt]) -> bool {
    stmts.iter().any(|s| match s {
        ir::Stmt::Raise { .. }
        | ir::Stmt::RaiseExc { .. }
        | ir::Stmt::Reraise
        | ir::Stmt::Die(_) => true,
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
pub(crate) fn loop_breaks(stmts: &[ir::Stmt]) -> bool {
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
