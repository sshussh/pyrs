//! Flow-sensitive type refinements and condition narrowing.

use std::collections::{HashMap, HashSet};

use parser::ast;

use crate::prelude::*;

/// Apply a flow refinement to a loaded name.
///
/// - Union storage → concrete member: `FromUnion` peel.
/// - Union storage → subclass of a class member: extract the base class
///   member, then retype to the subclass (same pointer; layout prefix).
/// - Monomorphic class storage → subclass: retype the load (isinstance peel).
/// - Multi-member peels keep storage (tags unsafe to rematerialize).
pub(crate) fn apply_type_refinement(base: ir::Expr, storage: ir::Ty, nty: ir::Ty) -> ir::Expr {
    if nty == storage {
        return base;
    }
    // `Any` narrowed by isinstance: unwrap the box. `FromAny` re-checks the
    // tag, which is redundant under the guard that produced the refinement and
    // cheap — and it means a refinement that is ever wrong traps rather than
    // reinterpreting the payload as the wrong type.
    if storage == ir::Ty::Any && nty != ir::Ty::Any && can_box_as_any(nty) {
        return ir::Expr {
            ty: nty,
            kind: ir::ExprKind::FromAny {
                value: Box::new(base),
            },
        };
    }
    // Class base → more specific subclass (isinstance).
    if let (ir::Ty::Class(src), ir::Ty::Class(dst)) = (storage, nty)
        && class_is_subclass(dst, src)
    {
        return ir::Expr {
            ty: nty,
            kind: base.kind,
        };
    }
    // Class base → union of subclasses (isinstance(x, (B, C))): keep the
    // Class ABI (ptr). Attribute lowering consults the refinement map for
    // common fields; retyping Local to Union would make codegen load
    // `{i32,i64}` from a `ptr` alloca.
    if matches!(storage, ir::Ty::Union(_)) && !matches!(nty, ir::Ty::Union(_)) {
        let members = ir::flatten_union_members(storage);
        // Exact member peel.
        if members.contains(&nty) {
            return ir::Expr {
                ty: nty,
                kind: ir::ExprKind::FromUnion {
                    value: Box::new(base),
                },
            };
        }
        // isinstance subclass peel: storage has Class(base), refine to Class(sub).
        if let ir::Ty::Class(want) = nty {
            for m in members {
                if let ir::Ty::Class(got) = m
                    && class_is_subclass(want, got)
                {
                    let extracted = ir::Expr {
                        ty: m,
                        kind: ir::ExprKind::FromUnion {
                            value: Box::new(base),
                        },
                    };
                    return ir::Expr {
                        ty: nty,
                        kind: extracted.kind,
                    };
                }
            }
        }
        // Multi-member / unknown peel: keep storage type.
        return base;
    }
    // Multi-member peels (class or scalar): keep storage. Member-index tags
    // must not be rematerialized — retyping a subset (A|B|C → B|C) renumbers
    // indices and blanks/mis-prints or segfaults. Exclusive class fields and
    // common fields consult `type_refinements` / `exclusive_class_field`, not
    // a retyped load ABI.
    let _ = nty;
    base
}

/// If `ty` is a union of classes that all share `attr` at the same index/type,
/// return `(representative_class_id, field_index, field_ty)`.
pub(crate) fn common_class_field(ty: ir::Ty, attr: &str) -> Option<(ir::ClassId, u32, ir::Ty)> {
    let members = ir::flatten_union_members(ty);
    if members.is_empty() || !members.iter().all(|m| matches!(m, ir::Ty::Class(_))) {
        return None;
    }
    let mut found: Option<(ir::ClassId, u32, ir::Ty)> = None;
    for m in members {
        let ir::Ty::Class(id) = m else {
            return None;
        };
        let (idx, fty) = field_index(id, attr)?;
        match found {
            None => found = Some((id, idx, fty)),
            Some((_, pi, pt)) if pi == idx && pt == fty => {}
            _ => return None,
        }
    }
    found
}

/// Field access that exists on a *subset* of a class union (or on classes with
/// differing layout indices). Returns closed-world `(class_id, field_index)`
/// candidates that have `attr` at a uniform field type, for a runtime type_id
/// switch. `None` when no refined class has the field or types disagree.
pub(crate) fn exclusive_class_field(
    ty: ir::Ty,
    attr: &str,
) -> Option<(Vec<(ir::ClassId, u32)>, ir::Ty)> {
    let members = ir::flatten_union_members(ty);
    if members.is_empty() || !members.iter().all(|m| matches!(m, ir::Ty::Class(_))) {
        return None;
    }
    let mut candidates: Vec<(ir::ClassId, u32)> = Vec::new();
    let mut field_ty: Option<ir::Ty> = None;
    // Expand each refined class to every closed-world subclass that may appear
    // at runtime after isinstance(x, (B, C)) (type_id is the most specific).
    let mut seen: HashSet<ir::ClassId> = HashSet::new();
    for m in members {
        let ir::Ty::Class(id) = m else {
            return None;
        };
        for sid in subclasses_of(id) {
            if !seen.insert(sid) {
                continue;
            }
            if let Some((idx, fty)) = field_index(sid, attr) {
                match field_ty {
                    None => field_ty = Some(fty),
                    Some(prev) if prev == fty => {}
                    Some(_) => return None, // incompatible field types
                }
                candidates.push((sid, idx));
            }
        }
    }
    if candidates.is_empty() {
        return None;
    }
    Some((candidates, field_ty.expect("candidates non-empty")))
}

/// Per-member `isinstance` peel: `(then_ty, else_ty)` — either side may be
/// absent when that arm is impossible for this storage member.
///
/// Class members are special: `isinstance(x, Sub)` when `x` is statically a
/// base class peels then-arm to `Sub` and keeps the base in the else-arm.
pub(crate) fn isinstance_peel_member(
    m: ir::Ty,
    pats: &[IsInstancePat],
) -> (Option<ir::Ty>, Option<ir::Ty>) {
    let class_wants: Vec<ir::ClassId> = pats
        .iter()
        .filter_map(|p| match p {
            IsInstancePat::Class(id) => Some(*id),
            _ => None,
        })
        .collect();

    if let ir::Ty::Class(got) = m {
        if class_wants.is_empty() {
            // isinstance(obj, int) etc. — never true for user instances.
            let hit = pats.iter().any(|p| isinstance_pat_matches(m, *p));
            return if hit {
                (Some(m), None)
            } else {
                (None, Some(m))
            };
        }
        // got <: want → always True; keep the more-specific static type.
        if class_wants.iter().any(|&w| class_is_subclass(got, w)) {
            return (Some(m), None);
        }
        // want <: got → runtime check; then peels to want(s), else keeps base.
        let then_ids: Vec<ir::ClassId> = class_wants
            .iter()
            .copied()
            .filter(|&w| class_is_subclass(w, got))
            .collect();
        if !then_ids.is_empty() {
            let then_tys: Vec<ir::Ty> = then_ids.into_iter().map(ir::Ty::Class).collect();
            let then_ty = match then_tys.len() {
                1 => then_tys[0],
                _ => ir::union_of(&then_tys),
            };
            return (Some(then_ty), Some(m));
        }
        // Unrelated class patterns.
        return (None, Some(m));
    }

    // `Any` is a tagged box, so an isinstance test genuinely narrows it: the
    // then-arm takes the tested type, and the else-arm stays `Any` because
    // ruling out one tag says nothing about the rest.
    //
    // One pattern only, and no containers. `isinstance(x, list)` cannot peel
    // to a concrete `list[T]` — the element type is not recoverable from the
    // tag — and a multi-pattern peel would need a union whose member indices
    // do not exist in the box's global tag space. Both decline rather than
    // guess, leaving `Any` on both arms as before.
    if m == ir::Ty::Any {
        if let [pat] = pats
            && let Some(want) = isinstance_pat_to_ty(*pat)
            && can_box_as_any(want)
        {
            return (Some(want), Some(m));
        }
        return (Some(m), Some(m));
    }

    // All exception instances share Ty::Exception — cannot peel subtypes.
    if m == ir::Ty::Exception {
        let has_exc = pats.iter().any(|p| matches!(p, IsInstancePat::Exc(_)));
        return if has_exc {
            (Some(m), Some(m))
        } else {
            (None, Some(m))
        };
    }

    let hit = pats.iter().any(|p| isinstance_pat_matches(m, *p));
    if hit {
        (Some(m), None)
    } else {
        (None, Some(m))
    }
}

/// Look up storage type of a local/cell/module name (not refinements).
pub(crate) fn name_storage_ty(name: &str, ctx: &FnCtx) -> Option<ir::Ty> {
    ctx.locals
        .get(name)
        .copied()
        .or_else(|| ctx.cell_locals.get(name).copied())
        .or_else(|| {
            // Module Optionals: free reads (no `global` needed) and explicit
            // `global` / entry use GlobalLoad.
            if !ctx.locals.contains_key(name) && !ctx.cell_locals.contains_key(name) {
                ctx.globals.get(name).copied()
            } else {
                None
            }
        })
}

/// Best-effort type of an AST expr for flow peels (walrus RHS, etc.).
pub(crate) fn expr_ty_hint(
    e: &ast::Expr,
    ctx: &FnCtx,
    active: &HashMap<String, ir::Ty>,
) -> Option<ir::Ty> {
    match &e.kind {
        ast::ExprKind::Name(n) => {
            name_refined_ty(n, ctx, active).or_else(|| name_storage_ty(n, ctx))
        }
        ast::ExprKind::NoneLit => Some(ir::Ty::None),
        ast::ExprKind::Int(_) | ast::ExprKind::IntDigits(_) => Some(ir::Ty::Int),
        ast::ExprKind::Float(_) => Some(ir::Ty::Float),
        ast::ExprKind::Bool(_) => Some(ir::Ty::Bool),
        ast::ExprKind::Str(_) | ast::ExprKind::JoinedStr(_) => Some(ir::Ty::Str),
        ast::ExprKind::NamedExpr { value, .. } => expr_ty_hint(value, ctx, active),
        _ => None,
    }
}

/// Effective type of `name` under an active refinement overlay (and-chain mid
/// peels), falling back to `ctx.type_refinements` then storage.
pub(crate) fn name_refined_ty(
    name: &str,
    ctx: &FnCtx,
    active: &HashMap<String, ir::Ty>,
) -> Option<ir::Ty> {
    let storage = name_storage_ty(name, ctx)?;
    Some(
        active
            .get(name)
            .copied()
            .or_else(|| ctx.type_refinements.get(name).copied())
            .unwrap_or(storage),
    )
}

/// Flow-sensitive narrowing for `x is None` / `x is not None` / `not (x is None)`.
/// Also peels `A and B` / `A or B` so `x is not None and flag` narrows `x` in
/// the then-arm (and complementary or-arms). Returns (then, else) maps.
pub(crate) fn narrowing_from_condition(
    cond: &ast::Expr,
    ctx: &FnCtx,
) -> (HashMap<String, ir::Ty>, HashMap<String, ir::Ty>) {
    // Seed active peels with current refinements so nested and/or compose.
    narrowing_from_condition_with(cond, ctx, &ctx.type_refinements)
}

/// Like [`narrowing_from_condition`], but peels are computed relative to
/// `active` (left-arm peels of an `and` chain, etc.). Right-arm peels thus see
/// more-specific left refinements instead of wiping them with storage.
pub(crate) fn narrowing_from_condition_with(
    cond: &ast::Expr,
    ctx: &FnCtx,
    active: &HashMap<String, ir::Ty>,
) -> (HashMap<String, ir::Ty>, HashMap<String, ir::Ty>) {
    let mut then_m = HashMap::new();
    let mut else_m = HashMap::new();
    match &cond.kind {
        ast::ExprKind::Unary {
            op: ast::UnaryOp::Not,
            operand,
        } => {
            let (t, e) = narrowing_from_condition_with(operand, ctx, active);
            return (e, t);
        }
        // Walrus in conditions: peels apply to the bound name after the value
        // expression's peels (e.g. `(y := x) is not None` peels `y`).
        ast::ExprKind::NamedExpr { target, value, .. } => {
            let (vt, ve) = narrowing_from_condition_with(value, ctx, active);
            // Also treat `name := value` where value peels are empty: no then peel
            // on the name from the assignment alone.
            let _ = target;
            return (vt, ve);
        }
        // `A and B`: then sees left peels, then right peels computed *under*
        // left (so `isinstance(x, B) and x is not None` keeps B; chained
        // isinstance keeps the more specific class). Else is not a simple
        // complement (A may be true and B false).
        ast::ExprKind::Binary {
            op: ast::BinOp::And,
            left,
            right,
        } => {
            let (lt, _) = narrowing_from_condition_with(left, ctx, active);
            let mut active_right = active.clone();
            for (k, v) in &lt {
                active_right.insert(k.clone(), *v);
            }
            let (rt, _) = narrowing_from_condition_with(right, ctx, &active_right);
            // Start with left peels; right overwrites only when it mentions a
            // name (and was computed under left, so stays at least as specific).
            then_m = lt;
            for (k, v) in rt {
                then_m.insert(k, v);
            }
            return (then_m, else_m);
        }
        // `A or B`: else sees both else-refs (both failed). Compose right's
        // else peels under left's else peels for the same sequential reason.
        ast::ExprKind::Binary {
            op: ast::BinOp::Or,
            left,
            right,
        } => {
            let (_, le) = narrowing_from_condition_with(left, ctx, active);
            let mut active_else = active.clone();
            for (k, v) in &le {
                active_else.insert(k.clone(), *v);
            }
            let (_, re) = narrowing_from_condition_with(right, ctx, &active_else);
            else_m = le;
            for (k, v) in re {
                else_m.insert(k, v);
            }
            return (then_m, else_m);
        }
        ast::ExprKind::Binary {
            op: op @ (ast::BinOp::Is | ast::BinOp::IsNot),
            left,
            right,
        } => {
            let not = matches!(op, ast::BinOp::IsNot);
            // `(y := x) is not None` — peel the walrus target using value's type.
            let (name, name_ty) = match (&left.kind, &right.kind) {
                (
                    ast::ExprKind::NamedExpr {
                        target: n, value, ..
                    },
                    ast::ExprKind::NoneLit,
                ) => (
                    n.as_str(),
                    name_storage_ty(n, ctx).or_else(|| expr_ty_hint(value, ctx, active)),
                ),
                (
                    ast::ExprKind::NoneLit,
                    ast::ExprKind::NamedExpr {
                        target: n, value, ..
                    },
                ) => (
                    n.as_str(),
                    name_storage_ty(n, ctx).or_else(|| expr_ty_hint(value, ctx, active)),
                ),
                (ast::ExprKind::Name(n), ast::ExprKind::NoneLit) => {
                    (n.as_str(), name_storage_ty(n, ctx))
                }
                (ast::ExprKind::NoneLit, ast::ExprKind::Name(n)) => {
                    (n.as_str(), name_storage_ty(n, ctx))
                }
                _ => return (then_m, else_m),
            };
            let Some(storage_ty) = name_ty else {
                return (then_m, else_m);
            };
            // Prefer active overlay (and-chain left peels), then outer refinements.
            let ty = name_refined_ty(name, ctx, active).unwrap_or(storage_ty);
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
        // `isinstance(x, T)` / `isinstance(x, (T1, T2))` peels unions and class
        // bases (subclass → then-arm; complementary miss → else-arm).
        // Subject may be a Name or walrus target: `isinstance((y := x), T)`.
        ast::ExprKind::Call {
            func,
            args,
            keywords,
            kwargs,
            ..
        } if func == "isinstance" && keywords.is_empty() && kwargs.is_none() => {
            if let Ok(plain) = require_plain_args(args, "isinstance", cond.span)
                && plain.len() == 2
                && let Ok(pats) = parse_isinstance_type_arg(plain[1])
            {
                let subject = match &plain[0].kind {
                    ast::ExprKind::Name(n) => name_storage_ty(n, ctx).map(|st| (n.clone(), st)),
                    ast::ExprKind::NamedExpr { target, value, .. } => name_storage_ty(target, ctx)
                        .or_else(|| expr_ty_hint(value, ctx, active))
                        .map(|st| (target.clone(), st)),
                    _ => None,
                };
                if let Some((n, storage_ty)) = subject {
                    // Prefer active overlay (and-chain left peels), then outer.
                    let ty = name_refined_ty(&n, ctx, active).unwrap_or(storage_ty);
                    let members = ir::flatten_union_members(ty);
                    let mut hit: Vec<ir::Ty> = Vec::new();
                    let mut miss: Vec<ir::Ty> = Vec::new();
                    for m in members {
                        let (t, e) = isinstance_peel_member(m, &pats);
                        if let Some(t) = t {
                            hit.push(t);
                        }
                        if let Some(e) = e {
                            miss.push(e);
                        }
                    }
                    if !hit.is_empty() {
                        let then_ty = match hit.len() {
                            1 => hit[0],
                            _ => ir::union_of(&hit),
                        };
                        then_m.insert(n.clone(), then_ty);
                    }
                    if !miss.is_empty() {
                        let else_ty = match miss.len() {
                            1 => miss[0],
                            _ => ir::union_of(&miss),
                        };
                        else_m.insert(n, else_ty);
                    }
                }
            }
        }
        _ => {}
    }
    (then_m, else_m)
}
