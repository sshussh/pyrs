//! Class AST collection, layouts, fields, methods, and exception-class registration.

use std::collections::{HashMap, HashSet};

use common::Span;
use parser::ast;

use crate::prelude::*;

/// One method extracted from a class body.
pub(crate) struct ClassMethodAst<'a> {
    pub(crate) def: &'a ast::FuncDef,
}

/// A class definition found at module top level.
pub(crate) struct ClassAst<'a> {
    pub(crate) module: String,
    pub(crate) is_root: bool,
    pub(crate) name: String,
    pub(crate) bases: Vec<(String, Span)>,
    pub(crate) methods: Vec<ClassMethodAst<'a>>,
    /// Class-body annotated attrs: name → type annotation.
    pub(crate) class_attrs: Vec<(String, ast::TypeName, Span)>,
    /// Class-body constants, in declaration order: `NAME = <literal>`.
    pub(crate) class_consts: Vec<(String, &'a ast::Expr)>,
    pub(crate) span: Span,
}

pub(crate) fn collect_class_asts<'a>(modules: &'a [ModuleInput<'a>]) -> SResult<Vec<ClassAst<'a>>> {
    let root_idx = modules.len() - 1;
    let mut out = Vec::new();
    for (i, m) in modules.iter().enumerate() {
        let is_root = i == root_idx;
        let module_key = if is_root {
            ENTRY_NAME.to_string()
        } else {
            m.name.clone()
        };
        for stmt in &m.ast.body {
            if let ast::StmtKind::ClassDef(c) = &stmt.kind {
                // Exception classes were registered separately and have no
                // instance layout, methods or vtable.
                if is_exc_class_name(&module_key, &c.name) {
                    continue;
                }
                if c.bases.len() > 1 {
                    return Err(err(
                        "multiple inheritance is not supported yet (single base only)",
                        c.bases.get(1).map(|(_, s)| *s).unwrap_or(c.span),
                    )
                    .with_file(i));
                }
                let mut methods = Vec::new();
                let class_attrs = Vec::new();
                let mut class_consts: Vec<(String, &ast::Expr)> = Vec::new();
                let mut seen_methods = HashSet::new();
                for b in &c.body {
                    match &b.kind {
                        ast::StmtKind::FuncDef(f) => {
                            if !seen_methods.insert(f.name.clone()) {
                                return Err(err(
                                    format!(
                                        "method '{}' is defined more than once in class '{}'",
                                        f.name, c.name
                                    ),
                                    f.span,
                                )
                                .with_file(i));
                            }
                            if f.name == "__new__" {
                                return Err(
                                    err("__new__ is not supported yet", f.span).with_file(i)
                                );
                            }
                            methods.push(ClassMethodAst { def: f });
                        }
                        ast::StmtKind::Pass => {}
                        ast::StmtKind::Assign {
                            targets,
                            value,
                            annotation,
                        } => {
                            // A class constant. Only a literal: it is
                            // substituted at every use rather than stored, so
                            // there is no initialisation to order and nothing
                            // that could be mutated. Instance fields still
                            // belong in __init__, where they get real storage.
                            let [ast::AssignTarget::Name { name, .. }] = &targets[..] else {
                                return Err(err(
                                    "only a plain name can be assigned in a class body",
                                    b.span,
                                )
                                .with_file(i));
                            };
                            // A dunder in a class body is a language feature
                            // rather than a constant, so name it as one
                            // whatever value it was given.
                            if let Some(note) = unsupported_dunder(name) {
                                return Err(err(note, value.span).with_file(i));
                            }
                            let Some(ty) = class_const_literal_ty(value) else {
                                return Err(err(
                                    format!(
                                        "class attribute '{name}' must be a literal \
                                         (int, float, bool or str): it is substituted \
                                         where it is used, not stored. Assign a computed \
                                         value in __init__ instead"
                                    ),
                                    value.span,
                                )
                                .with_file(i));
                            };
                            if let Some(ann) = annotation {
                                let want = resolve_type_checked(*ann, b.span)
                                    .map_err(|e| e.with_file(i))?;
                                if want != ty {
                                    return Err(err(
                                        format!(
                                            "class attribute '{name}' is annotated {want} \
                                             but its value is {ty}"
                                        ),
                                        b.span,
                                    )
                                    .with_file(i));
                                }
                            }
                            class_consts.push((name.clone(), value));
                        }
                        ast::StmtKind::ExprStmt(e) if matches!(e.kind, ast::ExprKind::Str(_)) => {}
                        _ => {
                            return Err(err(
                                "this statement is not supported in a class body yet",
                                b.span,
                            )
                            .with_file(i));
                        }
                    }
                }
                out.push(ClassAst {
                    module: m.name.clone(),
                    is_root,
                    name: c.name.clone(),
                    bases: c.bases.clone(),
                    methods,
                    class_attrs,
                    class_consts,
                    span: c.span,
                });
            }
        }
    }
    Ok(out)
}

/// Give a class that defines `__eq__` but not `__ne__` a `__ne__` of its own,
/// equivalent to `return not self.__eq__(other)`.
///
/// Without this, `!=` picked its slot from the operand's *static* type: a
/// `Base`-typed variable holding a `Child` that defines `__ne__` compiled to
/// the negation of `__eq__` and could never reach `Child.__ne__`, losing both
/// the result and any side effects. Synthesizing the slot makes every class
/// that participates in equality carry a real `__ne__`, so dispatch goes
/// through the existing vtable and inheritance, overriding and reflection all
/// work with no special cases.
///
/// The body calls `self.__eq__(...)` rather than the declaring class's `__eq__`
/// directly, so a subclass overriding only `__eq__` still changes `!=`.
///
/// Returns whether a `__ne__` was added.
pub(crate) fn synthesize_default_ne(methods: &mut Vec<ClassMethodAst<'_>>) -> bool {
    if methods.iter().any(|m| m.def.name == "__ne__") {
        return false;
    }
    let Some(eq) = methods
        .iter()
        .find(|m| m.def.name == "__eq__")
        .map(|m| m.def)
    else {
        return false;
    };
    // `__eq__(self, other)` exactly; anything else is rejected elsewhere, and
    // guessing at an unusual signature would be worse than leaving it alone.
    if eq.params.len() != 2 || eq.vararg.is_some() || eq.kwarg.is_some() {
        return false;
    }
    let span = eq.span;
    let self_name = eq.params[0].name.clone();
    let other_name = eq.params[1].name.clone();
    let name_expr = |name: String| ast::Expr {
        kind: ast::ExprKind::Name(name),
        span,
    };
    let call = ast::Expr {
        kind: ast::ExprKind::MethodCall {
            base: Box::new(name_expr(self_name)),
            method: "__eq__".to_string(),
            method_span: span,
            args: vec![ast::PosArg::Pos(name_expr(other_name))],
            keywords: Vec::new(),
            kwargs: None,
        },
        span,
    };
    let body = vec![ast::Stmt {
        kind: ast::StmtKind::Return(Some(ast::Expr {
            kind: ast::ExprKind::Unary {
                op: ast::UnaryOp::Not,
                operand: Box::new(call),
            },
            span,
        })),
        span,
    }];
    let synthetic = ast::FuncDef {
        name: "__ne__".to_string(),
        params: eq.params.clone(),
        // Synthesized *from* `__eq__`, so it inherits that signature's shape.
        posonly_end: eq.posonly_end,
        kwonly_start: eq.kwonly_start,
        vararg: None,
        kwarg: None,
        ret: Some(ast::TypeName::Bool),
        body,
        span,
        decorators: Vec::new(),
    };
    // The class table borrows method definitions from the module AST, which
    // has no slot for a node the source never contained. Leaking matches how
    // this file already handles synthesized type names, and the number of
    // classes in a compilation is bounded by the program.
    methods.push(ClassMethodAst {
        def: Box::leak(Box::new(synthetic)),
    });
    true
}

/// Decide which classes get a default `__ne__`, respecting inheritance.
///
/// CPython places the default on `object`, so it sits above every user class.
/// PyRs has no `object`, so the equivalent is: a class gets one only when no
/// ancestor already supplies a `__ne__`. Getting this wrong in the obvious way
/// -- synthesizing wherever a class declares `__eq__` -- shadows an explicit
/// `Base.__ne__` for a `Child` that overrides only `__eq__`, which CPython
/// resolves to the inherited `Base.__ne__`.
///
/// Classes are visited parents-first so `provided` is populated before any
/// descendant consults it, and an inherited entry propagates down the chain.
pub(crate) fn synthesize_default_ne_methods(classes: &mut [ClassAst<'_>]) {
    let class_depth = |c: &ClassAst<'_>| {
        let Some(id) = lookup_class_in_module(&c.module, &c.name) else {
            return u32::MAX;
        };
        let mut depth = 0u32;
        let mut cur = class_info(id).and_then(|i| i.parent);
        while let Some(p) = cur {
            depth += 1;
            if depth > 64 {
                break;
            }
            cur = class_info(p).and_then(|i| i.parent);
        }
        depth
    };
    let mut order: Vec<usize> = (0..classes.len()).collect();
    order.sort_by_key(|&i| class_depth(&classes[i]));

    let mut provided: HashSet<ir::ClassId> = HashSet::new();
    for idx in order {
        let Some(id) = lookup_class_in_module(&classes[idx].module, &classes[idx].name) else {
            continue;
        };
        if classes[idx].methods.iter().any(|m| m.def.name == "__ne__") {
            provided.insert(id);
            continue;
        }
        let mut cur = class_info(id).and_then(|i| i.parent);
        let mut inherits_ne = false;
        let mut hops = 0u32;
        while let Some(p) = cur {
            if provided.contains(&p) {
                inherits_ne = true;
                break;
            }
            hops += 1;
            if hops > 64 {
                break;
            }
            cur = class_info(p).and_then(|i| i.parent);
        }
        if inherits_ne {
            // An inherited `__ne__` already dispatches `self.__eq__` virtually,
            // so overriding only `__eq__` still changes `!=`. Adding one here
            // would shadow it.
            provided.insert(id);
            continue;
        }
        set_class_current_module(&classes[idx].module);
        if synthesize_default_ne(&mut classes[idx].methods) {
            provided.insert(id);
        }
    }
}

/// Infer field types from `self.attr = expr` assignments. Declaration order
/// is preserved (Vec, not HashMap). Multiple passes refine `self.x = self.y + 1`.
pub(crate) fn collect_self_fields(
    body: &[ast::Stmt],
    self_name: &str,
    param_tys: &HashMap<String, ir::Ty>,
    known_rets: &HashMap<String, ir::Ty>,
    fields: &mut Vec<(String, ir::Ty)>,
) {
    fn field_ty(fields: &[(String, ir::Ty)], name: &str) -> Option<ir::Ty> {
        fields.iter().find(|(n, _)| n == name).map(|(_, t)| *t)
    }
    fn set_field(fields: &mut Vec<(String, ir::Ty)>, name: &str, ty: ir::Ty) {
        if let Some(slot) = fields.iter_mut().find(|(n, _)| n == name) {
            // Keep first type; join if both numeric.
            if slot.1 != ty {
                let j = join_types(slot.1, ty);
                if j != slot.1 && matches!(j, ir::Ty::Int | ir::Ty::Float | ir::Ty::Bool) {
                    slot.1 = j;
                }
            }
        } else {
            fields.push((name.to_string(), ty));
        }
    }
    fn type_field_rhs(
        value: &ast::Expr,
        self_name: &str,
        param_tys: &HashMap<String, ir::Ty>,
        known_rets: &HashMap<String, ir::Ty>,
        fields: &[(String, ir::Ty)],
    ) -> Option<ir::Ty> {
        if let Some(t) = try_type_ast_expr(value, param_tys, known_rets) {
            return Some(t);
        }
        match &value.kind {
            // List / tuple / dict / set literals (list already partially in try_type).
            ast::ExprKind::ListLit(items) if !items.is_empty() => {
                let mut elem: Option<ir::Ty> = None;
                for it in items {
                    let e = match it {
                        ast::ListElem::Item(e) => e,
                        ast::ListElem::Star(_) => return None,
                    };
                    let t = type_field_rhs(e, self_name, param_tys, known_rets, fields)?;
                    elem = Some(match elem {
                        None => t,
                        Some(prev) => join_types(prev, t),
                    });
                }
                Some(ir::list_of(elem?))
            }
            ast::ExprKind::TupleLit(items) => {
                let mut ts = Vec::new();
                for it in items {
                    ts.push(type_field_rhs(
                        it, self_name, param_tys, known_rets, fields,
                    )?);
                }
                Some(ir::tuple_of(&ts))
            }
            ast::ExprKind::DictLit(items) if !items.is_empty() => {
                let mut key_ty: Option<ir::Ty> = None;
                let mut val_ty: Option<ir::Ty> = None;
                for (k, v) in items {
                    let kt = type_field_rhs(k, self_name, param_tys, known_rets, fields)?;
                    let vt = type_field_rhs(v, self_name, param_tys, known_rets, fields)?;
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
                    let t = type_field_rhs(it, self_name, param_tys, known_rets, fields)?;
                    elem = Some(match elem {
                        None => t,
                        Some(prev) => join_types(prev, t),
                    });
                }
                Some(ir::set_of(elem?))
            }
            // self.attr already known as a field.
            ast::ExprKind::Attribute { base, attr, .. } => {
                if let ast::ExprKind::Name(n) = &base.kind
                    && n == self_name
                {
                    return field_ty(fields, attr);
                }
                None
            }
            // Binary using known fields / params.
            ast::ExprKind::Binary { op, left, right } => {
                use ast::BinOp::*;
                let l = type_field_rhs(left, self_name, param_tys, known_rets, fields)?;
                let r = type_field_rhs(right, self_name, param_tys, known_rets, fields)?;
                match op {
                    Eq | NotEq | Lt | LtEq | Gt | GtEq | Is | IsNot | In | NotIn => {
                        Some(ir::Ty::Bool)
                    }
                    And | Or => Some(join_types(l, r)),
                    Div => Some(ir::Ty::Float),
                    // `a @ b` is whatever the class's `__matmul__` returns,
                    // which this shallow pass cannot see.
                    MatMul => None,
                    Add | Sub | Mul | FloorDiv | Mod | BitAnd | BitOr | BitXor | LShift
                    | RShift | Pow => {
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
                }
            }
            ast::ExprKind::Call { func, .. } => match known_rets.get(func).copied() {
                Some(ir::Ty::None) => None,
                Some(t) => Some(t),
                None => None,
            },
            // self.method() when method return type is known.
            ast::ExprKind::MethodCall { base, method, .. } => {
                if let ast::ExprKind::Name(n) = &base.kind
                    && n == self_name
                {
                    return match known_rets.get(method).copied() {
                        Some(ir::Ty::None) => None,
                        Some(t) => Some(t),
                        None => None,
                    };
                }
                None
            }
            _ => None,
        }
    }
    fn walk(
        body: &[ast::Stmt],
        self_name: &str,
        param_tys: &HashMap<String, ir::Ty>,
        known_rets: &HashMap<String, ir::Ty>,
        fields: &mut Vec<(String, ir::Ty)>,
    ) {
        for st in body {
            match &st.kind {
                ast::StmtKind::Assign {
                    targets,
                    value,
                    annotation,
                    ..
                } => {
                    for t in targets {
                        if let ast::AssignTarget::Attr { base, attr, .. } = t
                            && let ast::ExprKind::Name(n) = &base.kind
                            && n == self_name
                        {
                            // `self.xs: list[int] = []` -- the annotation is
                            // the field's type. It is often the only way to
                            // state one: an empty literal has no type to infer.
                            let ty = match annotation {
                                Some(ann) => resolve_type_checked(*ann, st.span).ok(),
                                Option::None => {
                                    type_field_rhs(value, self_name, param_tys, known_rets, fields)
                                }
                            };
                            if let Some(ty) = ty {
                                set_field(fields, attr, ty);
                            }
                        }
                    }
                }
                ast::StmtKind::If { branches, orelse } => {
                    for (_, b) in branches {
                        walk(b, self_name, param_tys, known_rets, fields);
                    }
                    walk(orelse, self_name, param_tys, known_rets, fields);
                }
                ast::StmtKind::While { body, orelse, .. }
                | ast::StmtKind::For { body, orelse, .. } => {
                    walk(body, self_name, param_tys, known_rets, fields);
                    walk(orelse, self_name, param_tys, known_rets, fields);
                }
                ast::StmtKind::Try {
                    body,
                    handlers,
                    orelse,
                    finally,
                } => {
                    walk(body, self_name, param_tys, known_rets, fields);
                    for h in handlers {
                        walk(&h.body, self_name, param_tys, known_rets, fields);
                    }
                    walk(orelse, self_name, param_tys, known_rets, fields);
                    walk(finally, self_name, param_tys, known_rets, fields);
                }
                ast::StmtKind::With { body, .. } => {
                    walk(body, self_name, param_tys, known_rets, fields);
                }
                ast::StmtKind::Match { cases, .. } => {
                    for c in cases {
                        walk(&c.body, self_name, param_tys, known_rets, fields);
                    }
                }
                _ => {}
            }
        }
    }
    // Fixed-point: self.x = self.y + 1 needs y first.
    for _ in 0..8 {
        let before = fields.len();
        walk(body, self_name, param_tys, known_rets, fields);
        if fields.len() == before {
            // Also re-walk once more for type refinements on existing fields.
            let snapshot = fields.clone();
            walk(body, self_name, param_tys, known_rets, fields);
            if *fields == snapshot {
                break;
            }
        }
    }
}

/// Register every `class E(Exception)` in the program, before regular class
/// collection so those classes never enter the instance/vtable pipeline.
///
/// Runs to a fixed point rather than in source order, because a chain can be
/// written in any order and across modules: `class B(A)` may precede
/// `class A(Exception)`. Each round registers the classes whose base is now
/// known to be an exception, and stops when a round adds nothing.
pub(crate) fn collect_exception_classes(modules: &[ModuleInput<'_>]) -> SResult<()> {
    clear_exc_env();
    let root_idx = modules.len() - 1;
    // Source order, not a fixed point: a class statement executes where it is
    // written, so its base must already exist. CPython raises NameError for
    // `class B(A)` above `class A(Exception)`, and resolving it anyway would
    // make PyRs accept a program Python rejects. Modules arrive in topological
    // order, so an imported base is already registered.
    for (i, m) in modules.iter().enumerate() {
        let module = if i == root_idx {
            ENTRY_NAME.to_string()
        } else {
            m.name.clone()
        };
        for stmt in &m.ast.body {
            let ast::StmtKind::ClassDef(c) = &stmt.kind else {
                continue;
            };
            if c.bases.len() != 1 {
                continue;
            }
            let (base_name, _) = &c.bases[0];
            let parent_tag = match builtin_exc_base(base_name) {
                Some(builtin) => builtin.tag(),
                Option::None => match lookup_exc_class(&module, base_name) {
                    Some(tag) => tag as i32,
                    // Not an exception class: leave it to the regular class
                    // pipeline, which reports an unknown base itself.
                    Option::None => continue,
                },
            };
            // The body has to be empty: an exception class here is a tag and a
            // name, with no instance layout to hold fields or methods.
            for b in &c.body {
                let ok = matches!(&b.kind, ast::StmtKind::Pass)
                    || matches!(&b.kind, ast::StmtKind::ExprStmt(e)
                        if matches!(e.kind, ast::ExprKind::Str(_)));
                if !ok {
                    return Err(err(
                        format!(
                            "exception class '{}' may only contain 'pass' or a \
                             docstring; methods and fields on exception classes \
                             are not supported yet",
                            c.name
                        ),
                        b.span,
                    )
                    .with_file(i));
                }
            }
            EXC_ENV.with(|e| {
                let mut env = e.borrow_mut();
                let tag = ir::USER_EXC_BASE + env.classes.len() as u32;
                env.classes.push(ir::ExcClass {
                    name: c.name.clone(),
                    tag,
                    parent_tag,
                });
                env.by_key.insert((module.clone(), c.name.clone()), tag);
            });
        }
    }
    Ok(())
}

/// A builtin exception usable as a base for a user class. `GeneratorExit` is
/// excluded: it is BaseException-only in CPython, and subclassing it would
/// make `except Exception` miss the subclass.
pub(crate) fn builtin_exc_base(name: &str) -> Option<ir::ExcType> {
    let ty = name_to_exc_type(name, Span::default()).ok()?;
    if ty == ir::ExcType::GeneratorExit {
        return Option::None;
    }
    Some(ty)
}

/// The type of a class-body constant's value, or `None` if it is not a
/// literal. A leading `-` or `+` on a number counts: `LIMIT = -1` is a
/// literal to a reader, whatever the AST shape.
pub(crate) fn class_const_literal_ty(e: &ast::Expr) -> Option<ir::Ty> {
    match &e.kind {
        ast::ExprKind::Unary {
            op: ast::UnaryOp::Neg,
            operand,
        } => match class_const_literal_ty(operand)? {
            t @ (ir::Ty::Int | ir::Ty::Float) => Some(t),
            _ => Option::None,
        },
        _ => literal_expr_ty(e),
    }
}

/// Record each class's literal constants, so a `C.NAME` read can be replaced
/// by the value. Runs after class ids exist and before any body is lowered.
pub(crate) fn register_class_consts(classes: &[ClassAst<'_>]) -> SResult<()> {
    for c in classes {
        for (name, value) in &c.class_consts {
            let lit = match &value.kind {
                ast::ExprKind::Int(v) => int_const(*v),
                ast::ExprKind::IntDigits(d) => ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::ConstIntDigits(d.clone()),
                },
                ast::ExprKind::Float(v) => ir::Expr {
                    ty: ir::Ty::Float,
                    kind: ir::ExprKind::ConstFloat(*v),
                },
                ast::ExprKind::Bool(v) => ir::Expr {
                    ty: ir::Ty::Bool,
                    kind: ir::ExprKind::ConstBool(*v),
                },
                ast::ExprKind::Str(v) => const_str_expr(v),
                ast::ExprKind::Unary {
                    op: ast::UnaryOp::Neg,
                    operand,
                } => match &operand.kind {
                    ast::ExprKind::Int(v) => int_const(-*v),
                    ast::ExprKind::Float(v) => ir::Expr {
                        ty: ir::Ty::Float,
                        kind: ir::ExprKind::ConstFloat(-*v),
                    },
                    _ => {
                        return Err(err(
                            format!("class attribute '{name}' must be a literal"),
                            value.span,
                        ));
                    }
                },
                _ => {
                    return Err(err(
                        format!("class attribute '{name}' must be a literal"),
                        value.span,
                    ));
                }
            };
            // Keyed by the same module string `module_of` records, so the
            // lookup in `class_const` matches.
            set_class_const(&c.module, &c.name, name, lit);
        }
    }
    Ok(())
}

/// Pass A: assign ClassIds only (no base resolution yet).
pub(crate) fn register_class_ids(classes: &[ClassAst<'_>]) -> SResult<()> {
    clear_no_return();
    clear_class_env();
    with_class_env_mut(|env| {
        for c in classes {
            let id = env.infos.len() as ir::ClassId;
            let display = if c.is_root || c.module == ENTRY_NAME {
                c.name.clone()
            } else {
                format!("{}.{}", c.module, c.name)
            };
            env.infos.push(ir::ClassInfo {
                id,
                name: display,
                parent: None,
                fields: vec![],
                methods: vec![],
            });
            env.module_of.insert(id, c.module.clone());
            env.by_key.insert((c.module.clone(), c.name.clone()), id);
        }
    });
    let mut seen: HashMap<(String, String), Span> = HashMap::new();
    for c in classes {
        let key = (c.module.clone(), c.name.clone());
        if seen.insert(key, c.span).is_some() {
            return Err(err(
                format!("class '{}' is defined more than once", c.name),
                c.span,
            ));
        }
    }
    Ok(())
}

/// Scan module top-level `from m import Name` and inject class aliases into
/// ClassEnv so bare names resolve for bases and annotations before full
/// import lowering. Relative imports are skipped here (resolved later).
pub(crate) fn inject_class_import_aliases(modules: &[ModuleInput<'_>]) {
    for m in modules {
        for stmt in &m.ast.body {
            let ast::StmtKind::FromImport {
                module: src,
                names,
                star,
                level,
                ..
            } = &stmt.kind
            else {
                continue;
            };
            if *star || *level != 0 || src.is_empty() || src == "sys" || src == FUTURE_MODULE {
                continue;
            }
            for (name, alias, _) in names {
                let Some(id) = lookup_class_in_module(src, name) else {
                    continue;
                };
                let local = alias.clone().unwrap_or_else(|| name.clone());
                with_class_env_mut(|e| {
                    e.by_key.entry((m.name.clone(), local)).or_insert(id);
                });
            }
        }
    }
}

/// Pass B: resolve single bases (same module, import aliases, unique global).
pub(crate) fn resolve_class_bases(classes: &[ClassAst<'_>]) -> SResult<()> {
    for c in classes {
        set_class_current_module(&c.module);
        let Some((base_name, base_span)) = c.bases.first() else {
            continue;
        };
        // 1) same module  2) bare alias in current module (imports)  3) unique global
        let parent_id = lookup_class_in_module(&c.module, base_name)
            .or_else(|| lookup_class(base_name))
            .or_else(|| {
                // Unique class of this short name across the program.
                with_class_env(|e| {
                    let hits: Vec<_> = e
                        .by_key
                        .iter()
                        .filter(|((_, n), _)| n == base_name)
                        .map(|(_, id)| *id)
                        .collect();
                    if hits.len() == 1 { Some(hits[0]) } else { None }
                })
            })
            .ok_or_else(|| {
                if base_name == "GeneratorExit" {
                    return err(
                        "GeneratorExit cannot be subclassed: it is BaseException-only \
                         in CPython, so `except Exception` would not catch the \
                         subclass. Inherit from Exception instead",
                        *base_span,
                    );
                }
                err(
                    format!(
                        "unknown base class '{base_name}' \
                         (import it first, e.g. 'from mod import {base_name}')"
                    ),
                    *base_span,
                )
            })?;
        let child_id = lookup_class_in_module(&c.module, &c.name).unwrap();
        if parent_id == child_id {
            return Err(err(
                format!("class '{}' cannot inherit from itself", c.name),
                c.span,
            ));
        }
        if class_is_subclass(parent_id, child_id) {
            return Err(err(
                format!("inheritance cycle involving class '{}'", c.name),
                c.span,
            ));
        }
        with_class_env_mut(|env| {
            if let Some(info) = env.infos.get_mut(child_id as usize) {
                info.parent = Some(parent_id);
            }
        });
    }
    Ok(())
}

/// Pre-infer a method's return type from annotation or simple body returns
/// (used for field discovery before method_func_sig runs).
pub(crate) fn pre_infer_method_ret(f: &ast::FuncDef) -> Option<ir::Ty> {
    if f.name == "__init__" {
        return Some(ir::Ty::None);
    }
    if let Some(t) = f.ret {
        return resolve_type_checked(t, f.span).ok();
    }
    // Lightweight param map for typing `return x` when x is a param.
    let mut params = HashMap::new();
    for (i, p) in f.params.iter().enumerate() {
        if i == 0 {
            continue; // self
        }
        if let Ok(Some(ty)) = resolve_param_ty_opt(p) {
            params.insert(p.name.clone(), ty);
        }
    }
    try_infer_ret_from_ast_body(&f.body, &params, &HashMap::new()).filter(|t| *t != ir::Ty::None)
}

// Functions that never return, by fully-qualified IR name.
//
// A call to one terminates control flow, so it satisfies "every path returns"
// the same way a `raise` does. Without this a helper like
// `def fail(msg): raise ValueError(msg)` forces every caller to write an
// unreachable `return` after calling it, purely to satisfy the check.
thread_local! {
    pub(crate) static NO_RETURN: std::cell::RefCell<HashSet<String>> =
        std::cell::RefCell::new(HashSet::new());
}

pub(crate) fn register_no_return(name: String) {
    NO_RETURN.with(|n| n.borrow_mut().insert(name));
}

pub(crate) fn is_no_return(name: &str) -> bool {
    NO_RETURN.with(|n| n.borrow().contains(name))
}

pub(crate) fn clear_no_return() {
    NO_RETURN.with(|n| n.borrow_mut().clear());
}

/// Whether every path through these statements raises.
///
/// Computed on the AST, before any body is lowered, so a helper may be
/// defined after its callers. Deliberately conservative: it answers "yes"
/// only for shapes where falling through is impossible.
pub(crate) fn ast_block_always_raises(stmts: &[ast::Stmt]) -> bool {
    stmts.iter().any(ast_stmt_always_raises)
}

pub(crate) fn ast_stmt_always_raises(stmt: &ast::Stmt) -> bool {
    match &stmt.kind {
        ast::StmtKind::Raise { .. } | ast::StmtKind::Reraise => true,
        ast::StmtKind::If { branches, orelse } => {
            !orelse.is_empty()
                && branches.iter().all(|(_, b)| ast_block_always_raises(b))
                && ast_block_always_raises(orelse)
        }
        ast::StmtKind::With { body, .. } => ast_block_always_raises(body),
        // `while True:` with no `break` never falls through.
        ast::StmtKind::While { cond, body, .. } => {
            matches!(cond.kind, ast::ExprKind::Bool(true)) && !ast_block_breaks(body)
        }
        _ => false,
    }
}

pub(crate) fn ast_block_breaks(stmts: &[ast::Stmt]) -> bool {
    stmts.iter().any(ast_stmt_breaks)
}

pub(crate) fn ast_stmt_breaks(stmt: &ast::Stmt) -> bool {
    match &stmt.kind {
        ast::StmtKind::Break => true,
        ast::StmtKind::If { branches, orelse } => {
            branches.iter().any(|(_, b)| ast_block_breaks(b)) || ast_block_breaks(orelse)
        }
        ast::StmtKind::With { body, .. } => ast_block_breaks(body),
        ast::StmtKind::Try {
            body,
            handlers,
            orelse,
            finally,
        } => {
            ast_block_breaks(body)
                || handlers.iter().any(|h| ast_block_breaks(&h.body))
                || ast_block_breaks(orelse)
                || ast_block_breaks(finally)
        }
        // A nested loop's own `break` binds to it, not to the outer one.
        _ => false,
    }
}

/// Record every function and method whose body always raises, before any of
/// them is lowered, so definition order does not matter.
pub(crate) fn pre_register_no_return(modules: &[ModuleInput<'_>]) {
    for m in modules {
        let qualify = |name: &str| -> Vec<String> {
            if m.name == ENTRY_NAME {
                vec![name.to_string()]
            } else {
                vec![name.to_string(), format!("{}.{}", m.name, name)]
            }
        };
        for stmt in &m.ast.body {
            match &stmt.kind {
                ast::StmtKind::FuncDef(f) if ast_block_always_raises(&f.body) => {
                    for n in qualify(&f.name) {
                        register_no_return(n);
                    }
                }
                ast::StmtKind::ClassDef(c) => {
                    for member in &c.body {
                        let ast::StmtKind::FuncDef(f) = &member.kind else {
                            continue;
                        };
                        if ast_block_always_raises(&f.body) {
                            for n in qualify(&format!("{}.{}", c.name, f.name)) {
                                register_no_return(n);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Scan top-level free `def` returns for field-RHS `self.x = make()` typing.
pub(crate) fn pre_infer_free_func_rets(modules: &[ModuleInput<'_>]) -> HashMap<String, ir::Ty> {
    let mut out = HashMap::new();
    for m in modules {
        set_class_current_module(&m.name);
        for stmt in &m.ast.body {
            let ast::StmtKind::FuncDef(f) = &stmt.kind else {
                continue;
            };
            let ty = if let Some(t) = f.ret {
                resolve_type_checked(t, f.span).ok()
            } else {
                let mut params = HashMap::new();
                for p in &f.params {
                    if let Ok(Some(ty)) = resolve_param_ty_opt(p) {
                        params.insert(p.name.clone(), ty);
                    }
                }
                try_infer_ret_from_ast_body(&f.body, &params, &HashMap::new())
                    .filter(|t| *t != ir::Ty::None)
            };
            if let Some(ty) = ty {
                out.entry(f.name.clone()).or_insert(ty);
                if m.name != ENTRY_NAME {
                    out.entry(format!("{}.{}", m.name, f.name)).or_insert(ty);
                }
            }
        }
    }
    out
}

/// Pass C: fields + methods (topo: parents first). Call after bases resolved.
pub(crate) fn finalize_class_layouts(
    classes: &[ClassAst<'_>],
    free_func_rets: &HashMap<String, ir::Ty>,
) -> SResult<()> {
    let mut order: Vec<usize> = (0..classes.len()).collect();
    order.sort_by_key(|&i| {
        let id = lookup_class_in_module(&classes[i].module, &classes[i].name).unwrap();
        let mut depth = 0u32;
        let mut cur = class_info(id).and_then(|c| c.parent);
        while let Some(p) = cur {
            depth += 1;
            cur = class_info(p).and_then(|c| c.parent);
            if depth > 64 {
                break;
            }
        }
        depth
    });

    for &idx in &order {
        let c = &classes[idx];
        set_class_current_module(&c.module);
        let id = lookup_class_in_module(&c.module, &c.name).unwrap();
        let parent = class_info(id).and_then(|i| i.parent);

        // Class-body annotated attributes (defaults rejected in collect_class_asts).
        let mut own_fields: Vec<(String, ir::Ty)> = Vec::new();
        let mut own_field_set: HashSet<String> = HashSet::new();
        for (name, ann, span) in &c.class_attrs {
            let ty = resolve_type_checked(*ann, *span)?;
            if own_field_set.insert(name.clone()) {
                own_fields.push((name.clone(), ty));
            }
        }

        // Fields from __init__ self-assignments (declaration order).
        if let Some(init) = c.methods.iter().find(|m| m.def.name == "__init__") {
            let self_name = init
                .def
                .params
                .first()
                .map(|p| p.name.as_str())
                .unwrap_or("self");
            let mut param_tys: HashMap<String, ir::Ty> = HashMap::new();
            param_tys.insert(self_name.to_string(), ir::Ty::Class(id));
            for p in init.def.params.iter().skip(1) {
                if let Ok(Some(ty)) = resolve_param_ty_opt(p) {
                    param_tys.insert(p.name.clone(), ty);
                }
            }
            // Known returns for self.m() / make() field RHS.
            // 1) This class's AST methods first (methods not yet in ClassInfo).
            // 2) Parent methods via ClassInfo / AST.
            // 3) Free module functions (pre-scanned into free_func_rets).
            let mut known_rets: HashMap<String, ir::Ty> = free_func_rets.clone();
            for m in &c.methods {
                if m.def.name == "__init__" {
                    continue;
                }
                if let Some(ty) = pre_infer_method_ret(m.def) {
                    known_rets.entry(m.def.name.clone()).or_insert(ty);
                }
            }
            let mut walk_cls = parent;
            while let Some(cid) = walk_cls {
                if let Some(info) = class_info(cid) {
                    for (mname, ir_name) in &info.methods {
                        if let Some(sig) = method_sig_lookup(ir_name) {
                            known_rets.entry(mname.clone()).or_insert(sig.ret);
                        } else if let Some(cm) = classes
                            .iter()
                            .find(|x| lookup_class_in_module(&x.module, &x.name) == Some(cid))
                            .and_then(|x| x.methods.iter().find(|mm| mm.def.name == *mname))
                            && let Some(ty) = pre_infer_method_ret(cm.def)
                        {
                            known_rets.entry(mname.clone()).or_insert(ty);
                        }
                    }
                    walk_cls = info.parent;
                } else {
                    break;
                }
            }
            // Seed parent + annotated fields for typing RHS (self.y + 1).
            let mut typing_fields: Vec<(String, ir::Ty)> = Vec::new();
            if let Some(pid) = parent
                && let Some(pinfo) = class_info(pid)
            {
                typing_fields.extend(pinfo.fields.iter().cloned());
            }
            typing_fields.extend(own_fields.iter().cloned());
            collect_self_fields(
                &init.def.body,
                self_name,
                &param_tys,
                &known_rets,
                &mut typing_fields,
            );
            let parent_names: HashSet<String> = parent
                .and_then(class_info)
                .map(|p| p.fields.iter().map(|(n, _)| n.clone()).collect())
                .unwrap_or_default();
            for (n, t) in typing_fields {
                if parent_names.contains(&n) {
                    continue;
                }
                if own_field_set.insert(n.clone()) {
                    own_fields.push((n, t));
                }
            }
        }

        // Layout: parent fields then own. Subclass cannot change parent field types.
        let mut fields = Vec::new();
        let mut field_names: HashSet<String> = HashSet::new();
        if let Some(pid) = parent
            && let Some(pinfo) = class_info(pid)
        {
            for (n, t) in pinfo.fields {
                field_names.insert(n.clone());
                fields.push((n, t));
            }
        }
        for (n, t) in own_fields {
            if field_names.contains(&n) {
                let parent_ty = fields.iter().find(|(fnm, _)| fnm == &n).map(|(_, ty)| *ty);
                if parent_ty != Some(t) {
                    return Err(err(
                        format!(
                            "class '{}' cannot change type of inherited field '{n}' \
                             (parent has {}, subclass assigns {t})",
                            c.name,
                            parent_ty
                                .map(|t| t.to_string())
                                .unwrap_or_else(|| "?".into())
                        ),
                        c.span,
                    ));
                }
                // Same type: keep parent slot.
                continue;
            }
            field_names.insert(n.clone());
            fields.push((n, t));
        }

        let mut methods = Vec::new();
        for m in &c.methods {
            let ir_name = method_ir_name(&c.module, c.is_root, &c.name, &m.def.name);
            methods.push((m.def.name.clone(), ir_name));
        }

        with_class_env_mut(|env| {
            if let Some(info) = env.infos.get_mut(id as usize) {
                info.fields = fields;
                info.methods = methods;
            }
        });
    }
    Ok(())
}

/// Build FuncSig for a class method (self typed as the class instance).
pub(crate) fn method_func_sig(
    class_id: ir::ClassId,
    class_short_name: &str,
    f: &ast::FuncDef,
    kind: MethodKind,
) -> SResult<FuncSig> {
    let mut formals = f.params.clone();
    let mut params;
    match kind {
        MethodKind::Static => {
            // No implicit self — all params are user params.
            params = resolve_params_with_body_infer(&formals, &f.body)?;
        }
        MethodKind::Class => {
            if formals.is_empty() {
                return Err(err(
                    format!("classmethod '{}' must have a 'cls' parameter", f.name),
                    f.span,
                ));
            }
            // First param is the class marker (typed as the class for construct).
            formals[0].ty = Some(ast::TypeName::Class(Box::leak(
                class_short_name.to_string().into_boxed_str(),
            )));
            params = resolve_params_with_body_infer(&formals, &f.body)?;
            params[0].ty = ir::Ty::Class(class_id);
        }
        MethodKind::Instance | MethodKind::Property => {
            if formals.is_empty() {
                return Err(err(
                    format!("instance method '{}' must have a 'self' parameter", f.name),
                    f.span,
                ));
            }
            // First param is always the instance (annotation optional / overridden).
            formals[0].ty = Some(ast::TypeName::Class(Box::leak(
                class_short_name.to_string().into_boxed_str(),
            )));
            params = resolve_params_with_body_infer(&formals, &f.body)?;
            params[0].ty = ir::Ty::Class(class_id);
            if kind == MethodKind::Property && params.len() != 1 {
                return Err(err(
                    format!("@property '{}' must take only self", f.name),
                    f.span,
                ));
            }
        }
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
    // __init__ always returns None (CPython TypeError on non-None).
    if f.name == "__init__" {
        if ret != ir::Ty::None {
            return Err(err("__init__ must return None", f.span));
        }
        ret = ir::Ty::None;
        // Reject bare `return <non-None>` in body (annotation path already None).
        if init_body_returns_non_none(&f.body) {
            return Err(err(
                "__init__ should return None, not an explicit value \
                 (returning a non-None value is not supported)",
                f.span,
            ));
        }
    } else if f.name == "__str__" || f.name == "__repr__" {
        // Protocol methods must return str (reject at definition time).
        if f.ret.is_none() {
            // Infer from body when possible; still require Str.
            let param_map: HashMap<String, ir::Ty> =
                params.iter().map(|p| (p.name.clone(), p.ty)).collect();
            if let Some(ty) = try_infer_ret_from_ast_body(&f.body, &param_map, &HashMap::new()) {
                ret = ty;
            }
        }
        if ret != ir::Ty::Str {
            return Err(err(format!("{} must return str", f.name), f.span));
        }
        // Arity is checked here, not only at a print site, because the
        // container printer reaches `__repr__` through a function-pointer
        // table with no chance to diagnose.
        if params.len() != 1 {
            return Err(err(
                format!("{} must take only self (no extra parameters)", f.name),
                f.span,
            ));
        }
    } else if f.ret.is_none() {
        // Pre-infer unannotated returns from body (same as free functions).
        let param_map: HashMap<String, ir::Ty> =
            params.iter().map(|p| (p.name.clone(), p.ty)).collect();
        if let Some(ty) = try_infer_ret_from_ast_body(&f.body, &param_map, &HashMap::new())
            && ty != ir::Ty::None
        {
            ret = ty;
        }
    }
    let is_generator = stmts_have_yield(&f.body);
    if is_generator {
        return Err(err("generator methods are not supported yet", f.span));
    }
    Ok(FuncSig {
        params,
        posonly_end: f.posonly_end,
        kwonly_start: f.kwonly_start,
        vararg,
        kwarg,
        ret,
        span: f.span,
        is_generator: false,
        yield_ty: None,
        gen_frame_slots: 0,
    })
}

/// True if any `return expr` in `body` is clearly non-None (literals / names).
pub(crate) fn init_body_returns_non_none(body: &[ast::Stmt]) -> bool {
    fn walk(stmts: &[ast::Stmt]) -> bool {
        for st in stmts {
            match &st.kind {
                ast::StmtKind::Return(Some(e)) => match &e.kind {
                    ast::ExprKind::NoneLit => {}
                    // Any other explicit return value is non-None for our purposes.
                    _ => return true,
                },
                ast::StmtKind::If { branches, orelse } => {
                    for (_, b) in branches {
                        if walk(b) {
                            return true;
                        }
                    }
                    if walk(orelse) {
                        return true;
                    }
                }
                ast::StmtKind::While { body, orelse, .. }
                | ast::StmtKind::For { body, orelse, .. } => {
                    if walk(body) || walk(orelse) {
                        return true;
                    }
                }
                ast::StmtKind::Try {
                    body,
                    handlers,
                    orelse,
                    finally,
                } => {
                    if walk(body)
                        || handlers.iter().any(|h| walk(&h.body))
                        || walk(orelse)
                        || walk(finally)
                    {
                        return true;
                    }
                }
                ast::StmtKind::With { body, .. } => {
                    if walk(body) {
                        return true;
                    }
                }
                ast::StmtKind::Match { cases, .. } if cases.iter().any(|c| walk(&c.body)) => {
                    return true;
                }
                _ => {}
            }
        }
        false
    }
    walk(body)
}

/// Reject overrides that change arity (user params) or return type vs parent.
pub(crate) fn check_override_compatibility(classes: &[ClassAst<'_>]) -> SResult<()> {
    for c in classes {
        set_class_current_module(&c.module);
        let id = lookup_class_in_module(&c.module, &c.name).unwrap();
        let Some(parent) = class_info(id).and_then(|i| i.parent) else {
            continue;
        };
        for m in &c.methods {
            if m.def.name == "__init__" {
                continue; // __init__ override is free (different construction args)
            }
            let child_ir = method_ir_name(&c.module, c.is_root, &c.name, &m.def.name);
            let Some(child_sig) = method_sig_lookup(&child_ir) else {
                continue;
            };
            // Find parent method IR name.
            let Some(parent_ir) = resolve_method(parent, &m.def.name) else {
                continue;
            };
            // If resolve_method found the child's own method, skip (no parent def).
            if parent_ir == child_ir {
                continue;
            }
            let Some(parent_sig) = method_sig_lookup(&parent_ir) else {
                continue;
            };
            // Compare user params: skip leading self/cls for instance/class/property;
            // staticmethods have no implicit first param.
            let child_kind = method_kind_lookup(&child_ir);
            let parent_kind = method_kind_lookup(&parent_ir);
            if child_kind != parent_kind {
                return Err(err(
                    format!(
                        "method '{}.{}' overrides parent with a different method kind \
                         (staticmethod/classmethod/instance/property must match)",
                        c.name, m.def.name
                    ),
                    m.def.span,
                ));
            }
            let skip = match child_kind {
                MethodKind::Static => 0,
                MethodKind::Instance | MethodKind::Class | MethodKind::Property => 1,
            };
            let c_user = &child_sig.params[skip.min(child_sig.params.len())..];
            let p_user = &parent_sig.params[skip.min(parent_sig.params.len())..];
            if c_user.len() != p_user.len() {
                return Err(err(
                    format!(
                        "method '{}.{}' overrides parent with incompatible arity \
                         (expected {} parameter(s) after self, found {})",
                        c.name,
                        m.def.name,
                        p_user.len(),
                        c_user.len()
                    ),
                    m.def.span,
                ));
            }
            for (a, b) in c_user.iter().zip(p_user.iter()) {
                if a.ty != b.ty {
                    return Err(err(
                        format!(
                            "method '{}.{}' overrides parent with incompatible parameter \
                             type (expected {}, found {})",
                            c.name, m.def.name, b.ty, a.ty
                        ),
                        m.def.span,
                    ));
                }
            }
            if child_sig.ret != parent_sig.ret {
                return Err(err(
                    format!(
                        "method '{}.{}' overrides parent with incompatible return type \
                         (expected {}, found {})",
                        c.name, m.def.name, parent_sig.ret, child_sig.ret
                    ),
                    m.def.span,
                ));
            }
            if child_sig.vararg.is_some() != parent_sig.vararg.is_some()
                || child_sig.kwarg.is_some() != parent_sig.kwarg.is_some()
            {
                return Err(err(
                    format!(
                        "method '{}.{}' overrides parent with incompatible *args/**kwargs",
                        c.name, m.def.name
                    ),
                    m.def.span,
                ));
            }
        }
    }
    Ok(())
}
