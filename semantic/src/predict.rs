//! Learned type prediction for parameters no call site types.
//!
//! Whole-program inference types a parameter from what its call sites pass.
//! When there are none -- a library's exported function, a function nothing
//! calls yet -- the only evidence is the function itself: its names, its
//! defaults, and how the body uses the parameter. Two learned predictors read
//! that evidence: a linear model (`ml`) and a small transformer (`dl`), both
//! trained offline (`scripts/infer_eval/`) and shipped as weights, the way
//! LLVM ships MLGO policies: no framework in the compiler, a deterministic
//! forward pass on the CPU.
//!
//! A prediction is never trusted as a proof. It stands in only where the
//! static engine has no evidence of its own, and a program whose lowering a
//! prediction breaks is compiled without it.
//!
//! The features mirror `scripts/infer_eval/features.py` exactly;
//! `scripts/infer_eval/parity.py` checks that the two agree.

use parser::ast;

/// The label vocabulary, in the order the models score it.
pub(crate) const LABELS: [&str; 25] = [
    "int",
    "float",
    "str",
    "bool",
    "list[int]",
    "list[float]",
    "list[str]",
    "list[other]",
    "dict[str,int]",
    "dict[str,str]",
    "dict[str,float]",
    "dict[other]",
    "set",
    "tuple",
    "callable",
    "iterator",
    "class",
    "int?",
    "float?",
    "str?",
    "bool?",
    "class?",
    "other?",
    "any",
    "other",
];

const MAX_EVENTS: usize = 48;

/// `snake_case` and `CamelCase` parts, lowercased: `maxValue_2` is
/// `max value 2`. The same split as the Python extractor's regular expression.
pub(crate) fn subtokens(name: &str) -> Vec<String> {
    let trimmed = name.trim_matches('_');
    let mut parts = Vec::new();
    for chunk in trimmed.split('_') {
        let chars: Vec<char> = chunk.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c.is_ascii_digit() {
                let start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                parts.push(chars[start..i].iter().collect::<String>());
            } else if c.is_ascii_uppercase() {
                let start = i;
                let mut j = i;
                while j < chars.len() && chars[j].is_ascii_uppercase() {
                    j += 1;
                }
                if j - start >= 2 && (j == chars.len() || !chars[j].is_ascii_lowercase()) {
                    // an acronym run not followed by lowercase: `HTTP`
                    parts.push(chars[start..j].iter().collect::<String>().to_lowercase());
                    i = j;
                } else if j - start >= 2 {
                    // `HTTPServer`: the acronym, then `Server`
                    parts.push(
                        chars[start..j - 1]
                            .iter()
                            .collect::<String>()
                            .to_lowercase(),
                    );
                    i = j - 1;
                } else {
                    let mut k = i + 1;
                    while k < chars.len() && chars[k].is_ascii_lowercase() {
                        k += 1;
                    }
                    if k == i + 1 {
                        // a lone capital: part of `[A-Z]+(?![a-z])`
                        parts.push(c.to_ascii_lowercase().to_string());
                        i += 1;
                    } else {
                        parts.push(chars[i..k].iter().collect::<String>().to_lowercase());
                        i = k;
                    }
                }
            } else if c.is_ascii_lowercase() {
                let start = i;
                while i < chars.len() && chars[i].is_ascii_lowercase() {
                    i += 1;
                }
                parts.push(chars[start..i].iter().collect::<String>());
            } else {
                i += 1;
            }
        }
    }
    if parts.is_empty() && !trimmed.is_empty() {
        parts.push(trimmed.to_lowercase());
    }
    parts
}

fn expr_kind(e: &ast::Expr) -> &'static str {
    match &e.kind {
        ast::ExprKind::Bool(_) => "bool",
        ast::ExprKind::Int(_) | ast::ExprKind::IntDigits(_) => "int",
        ast::ExprKind::Float(_) => "float",
        ast::ExprKind::Str(_) | ast::ExprKind::JoinedStr(_) => "str",
        ast::ExprKind::NoneLit => "none",
        ast::ExprKind::Unary {
            op: ast::UnaryOp::Neg,
            operand,
        } => match operand.kind {
            ast::ExprKind::Int(_) | ast::ExprKind::IntDigits(_) => "int",
            ast::ExprKind::Float(_) => "float",
            _ => "other",
        },
        ast::ExprKind::Name(_) => "name",
        ast::ExprKind::Cast { ty, .. } => match ty {
            ast::TypeName::Int => "int",
            ast::TypeName::Float => "float",
            ast::TypeName::Str => "str",
            ast::TypeName::Bool => "bool",
            _ => "call",
        },
        ast::ExprKind::Call { func, .. } => match func.as_str() {
            "len" => "int",
            "int" => "int",
            "float" => "float",
            "str" => "str",
            "bool" => "bool",
            _ => "call",
        },
        ast::ExprKind::MethodCall { .. } => "call",
        ast::ExprKind::Attribute { .. } => "attr",
        ast::ExprKind::Index { .. } | ast::ExprKind::Slice { .. } => "sub",
        ast::ExprKind::ListLit(_) | ast::ExprKind::ListComp { .. } => "list",
        ast::ExprKind::DictLit(_) | ast::ExprKind::DictComp { .. } => "dict",
        ast::ExprKind::TupleLit(_) => "tuple",
        ast::ExprKind::Binary { op, .. } if binop_name(*op).is_some() => "binop",
        _ => "other",
    }
}

fn binop_name(op: ast::BinOp) -> Option<&'static str> {
    use ast::BinOp::*;
    Some(match op {
        Add => "add",
        Sub => "sub",
        Mul => "mul",
        Div => "div",
        FloorDiv => "floordiv",
        Mod => "mod",
        Pow => "pow",
        BitAnd => "bitand",
        BitOr => "bitor",
        BitXor => "bitxor",
        LShift => "lshift",
        RShift => "rshift",
        MatMul => "matmul",
        _ => return None,
    })
}

fn default_kind(d: Option<&ast::Expr>) -> &'static str {
    match d {
        None => "absent",
        Some(e) => match expr_kind(e) {
            k @ ("int" | "float" | "str" | "bool" | "none" | "list" | "dict" | "tuple") => k,
            _ => "other",
        },
    }
}

/// One use of the parameter, with where in the source it sits.
struct Use {
    at: usize,
    event: String,
}

/// Walks a function body collecting the uses of one name. Each `Expr` visit
/// knows its parent context, which is what an event is built from.
struct Uses<'a> {
    name: &'a str,
    out: Vec<Use>,
}

/// The syntactic position an expression occupies in its parent.
enum Ctx<'e> {
    Other,
    /// `name.attr`, as a value.
    AttrBase(&'e str),
    /// `name.method(...)`.
    MethodBase(&'e str),
    /// A positional argument of a call to `callee` at `index`.
    Arg(String, usize),
    /// A keyword argument of a call to `callee`.
    Kwarg(String),
    /// An operand of an arithmetic operator: (op, left side?, other operand).
    BinOperand(&'static str, bool, &'e ast::Expr),
    BoolOperand,
    /// A comparison operand: (op, is the first operand, other operand).
    Compared(ast::BinOp, bool, &'e ast::Expr),
    Unary(&'static str),
    /// `name[index]`.
    Subscripted(&'e ast::Expr),
    /// `name[lo:hi]`.
    Sliced,
    IndexOf,
    SliceBound,
    Iterated,
    Test,
    Returned,
    Yielded,
    /// The value of an assignment to a target of this kind.
    AssignedTo(String),
    AugValue(&'static str),
    FString,
    InLiteral,
    InDict,
}

impl<'a> Uses<'a> {
    fn hit(&mut self, span: common::Span, event: String) {
        if self.out.len() < 4 * MAX_EVENTS {
            self.out.push(Use {
                at: span.start,
                event,
            });
        }
    }

    fn stmts(&mut self, stmts: &[ast::Stmt]) {
        for s in stmts {
            self.stmt(s);
        }
    }

    fn target(&mut self, t: &ast::AssignTarget) {
        match t {
            ast::AssignTarget::Name { name, span } => {
                if name == self.name {
                    self.hit(*span, "rebound".into());
                }
            }
            ast::AssignTarget::Attr {
                base,
                attr,
                attr_span: _,
            } => {
                if matches!(&base.kind, ast::ExprKind::Name(n) if n == self.name) {
                    self.hit(base.span, format!("setattr:{attr}"));
                } else {
                    self.expr(base, Ctx::Other);
                }
            }
            ast::AssignTarget::Index { base, index } => {
                if matches!(&base.kind, ast::ExprKind::Name(n) if n == self.name) {
                    self.hit(base.span, format!("setitem:{}", expr_kind(index)));
                } else {
                    self.expr(base, Ctx::Other);
                }
                self.expr(index, Ctx::IndexOf);
            }
            ast::AssignTarget::Slice { base, lo, hi, step } => {
                if matches!(&base.kind, ast::ExprKind::Name(n) if n == self.name) {
                    self.hit(base.span, "slice".into());
                } else {
                    self.expr(base, Ctx::Other);
                }
                for b in [lo, hi, step].into_iter().flatten() {
                    self.expr(b, Ctx::SliceBound);
                }
            }
            ast::AssignTarget::Tuple(items) => {
                for i in items {
                    self.target(i);
                }
            }
            ast::AssignTarget::Starred { target, .. } => self.target(target),
        }
    }

    fn target_kind(t: &ast::AssignTarget) -> String {
        match t {
            ast::AssignTarget::Attr { attr, .. } => format!("stored:{attr}"),
            ast::AssignTarget::Index { .. } | ast::AssignTarget::Slice { .. } => {
                "stored:item".into()
            }
            _ => "assigned".into(),
        }
    }

    fn stmt(&mut self, s: &ast::Stmt) {
        match &s.kind {
            ast::StmtKind::FuncDef(f) => {
                for p in &f.params {
                    if let Some(d) = &p.default {
                        self.expr(d, Ctx::Other);
                    }
                }
                self.stmts(&f.body);
            }
            ast::StmtKind::ClassDef(c) => self.stmts(&c.body),
            ast::StmtKind::If { branches, orelse } => {
                for (c, b) in branches {
                    self.expr(c, Ctx::Test);
                    self.stmts(b);
                }
                self.stmts(orelse);
            }
            ast::StmtKind::While { cond, body, orelse } => {
                self.expr(cond, Ctx::Test);
                self.stmts(body);
                self.stmts(orelse);
            }
            ast::StmtKind::For {
                target,
                iter,
                body,
                orelse,
            } => {
                self.target(target);
                self.expr(iter, Ctx::Iterated);
                self.stmts(body);
                self.stmts(orelse);
            }
            ast::StmtKind::Return(Some(e)) => self.expr(e, Ctx::Returned),
            ast::StmtKind::Assign { targets, value, .. } => {
                for t in targets {
                    self.target(t);
                }
                let kind = targets
                    .first()
                    .map_or_else(|| "assigned".to_string(), Self::target_kind);
                self.expr(value, Ctx::AssignedTo(kind));
            }
            ast::StmtKind::AugAssign { target, op, value } => {
                let op_name = binop_name(*op).unwrap_or("other");
                match target {
                    // Python stores through a `Name` target, so it is a rebind.
                    ast::AssignTarget::Name { name, span } if name == self.name => {
                        self.hit(*span, "rebound".into());
                    }
                    other => self.target(other),
                }
                self.expr(value, Ctx::AugValue(op_name));
            }
            ast::StmtKind::Delete { target } => match target {
                ast::AssignTarget::Name { name, span } if name == self.name => {
                    self.hit(*span, "other_use".into());
                }
                other => self.target(other),
            },
            ast::StmtKind::ExprStmt(e) => self.expr(e, Ctx::Other),
            ast::StmtKind::With { item, target, body } => {
                self.expr(item, Ctx::Other);
                if let Some((n, sp)) = target
                    && n == self.name
                {
                    self.hit(*sp, "rebound".into());
                }
                self.stmts(body);
            }
            ast::StmtKind::Raise {
                exc,
                message: Some(m),
            } => self.expr(m, Ctx::Arg(exc.name.clone(), 0)),
            ast::StmtKind::Assert { test, msg } => {
                self.expr(test, Ctx::Test);
                if let Some(m) = msg {
                    self.expr(m, Ctx::Other);
                }
            }
            ast::StmtKind::Try {
                body,
                handlers,
                orelse,
                finally,
            } => {
                self.stmts(body);
                for h in handlers {
                    self.stmts(&h.body);
                }
                self.stmts(orelse);
                self.stmts(finally);
            }
            ast::StmtKind::Match { subject, cases } => {
                self.expr(subject, Ctx::Other);
                for c in cases {
                    if let Some(g) = &c.guard {
                        self.expr(g, Ctx::Other);
                    }
                    self.stmts(&c.body);
                }
            }
            _ => {}
        }
    }

    fn call_args(&mut self, callee: &str, args: &[ast::PosArg], keywords: &[ast::Keyword]) {
        for (i, a) in args.iter().enumerate() {
            match a {
                ast::PosArg::Pos(e) => self.expr(e, Ctx::Arg(callee.to_string(), i.min(3))),
                ast::PosArg::Star(e) => self.expr(e, Ctx::Other),
            }
        }
        for k in keywords {
            self.expr(&k.value, Ctx::Kwarg(callee.to_string()));
        }
    }

    fn expr(&mut self, e: &ast::Expr, ctx: Ctx<'_>) {
        if let ast::ExprKind::Name(n) = &e.kind {
            if n == self.name {
                let event = match ctx {
                    Ctx::Other => "other_use".to_string(),
                    Ctx::AttrBase(a) => format!("attr:{a}"),
                    Ctx::MethodBase(m) => format!("meth:{m}"),
                    Ctx::Arg(c, i) => format!("arg:{c}:{i}"),
                    Ctx::Kwarg(c) => format!("kwarg:{c}"),
                    Ctx::BinOperand(op, left, other) => {
                        format!(
                            "binop:{op}:{}:{}",
                            if left { "l" } else { "r" },
                            expr_kind(other)
                        )
                    }
                    Ctx::BoolOperand => "boolop".into(),
                    Ctx::Compared(op, first, other) => {
                        use ast::BinOp::*;
                        match op {
                            Is | IsNot => format!("is:{}", expr_kind(other)),
                            In | NotIn => format!(
                                "in:{}:{}",
                                if first { "needle" } else { "container" },
                                expr_kind(other)
                            ),
                            _ => {
                                let name = match op {
                                    Eq => "eq",
                                    NotEq => "ne",
                                    Lt => "lt",
                                    LtEq => "le",
                                    Gt => "gt",
                                    GtEq => "ge",
                                    _ => "other",
                                };
                                format!("cmp:{name}:{}", expr_kind(other))
                            }
                        }
                    }
                    Ctx::Unary(op) => format!("unary:{op}"),
                    Ctx::Subscripted(idx) => format!("getitem:{}", expr_kind(idx)),
                    Ctx::Sliced => "slice".into(),
                    Ctx::IndexOf => "index_of".into(),
                    Ctx::SliceBound => "slice_bound".into(),
                    Ctx::Iterated => "iterated".into(),
                    Ctx::Test => "truth".into(),
                    Ctx::Returned => "returned".into(),
                    Ctx::Yielded => "yielded".into(),
                    Ctx::AssignedTo(k) => k,
                    Ctx::AugValue(op) => format!("aug_value:{op}"),
                    Ctx::FString => "fstring".into(),
                    Ctx::InLiteral => "in_literal".into(),
                    Ctx::InDict => "in_dict".into(),
                };
                self.hit(e.span, event);
            }
            return;
        }
        match &e.kind {
            ast::ExprKind::Call {
                func,
                func_span,
                args,
                keywords,
                kwargs,
            } => {
                if func == self.name {
                    self.hit(*func_span, format!("called:{}", args.len().min(4)));
                }
                self.call_args(func, args, keywords);
                if let Some(k) = kwargs {
                    self.expr(k, Ctx::Other);
                }
            }
            ast::ExprKind::Cast { ty, arg } => {
                let callee = match ty {
                    ast::TypeName::Int => "int",
                    ast::TypeName::Float => "float",
                    ast::TypeName::Str => "str",
                    ast::TypeName::Bool => "bool",
                    _ => "?",
                };
                self.expr(arg, Ctx::Arg(callee.to_string(), 0));
            }
            ast::ExprKind::Attribute { base, attr, .. } => {
                self.expr(base, Ctx::AttrBase(attr));
            }
            ast::ExprKind::MethodCall {
                base,
                method,
                args,
                keywords,
                kwargs,
                ..
            } => {
                self.expr(base, Ctx::MethodBase(method));
                self.call_args(&format!(".{method}"), args, keywords);
                if let Some(k) = kwargs {
                    self.expr(k, Ctx::Other);
                }
            }
            ast::ExprKind::Index { base, index } => {
                self.expr(base, Ctx::Subscripted(index));
                self.expr(index, Ctx::IndexOf);
            }
            ast::ExprKind::Slice { base, lo, hi, step } => {
                self.expr(base, Ctx::Sliced);
                for b in [lo, hi, step].into_iter().flatten() {
                    self.expr(b, Ctx::SliceBound);
                }
            }
            ast::ExprKind::JoinedStr(parts) => {
                for p in parts {
                    if let ast::FStringPart::Expr {
                        expr, format_spec, ..
                    } = p
                    {
                        self.expr(expr, Ctx::FString);
                        if let Some(spec) = format_spec {
                            self.expr(spec, Ctx::Other);
                        }
                    }
                }
            }
            ast::ExprKind::ListLit(items) => {
                for it in items {
                    match it {
                        ast::ListElem::Item(x) => self.expr(x, Ctx::InLiteral),
                        ast::ListElem::Star(x) => self.expr(x, Ctx::Other),
                    }
                }
            }
            ast::ExprKind::TupleLit(items) | ast::ExprKind::SetLit(items) => {
                for it in items {
                    self.expr(it, Ctx::InLiteral);
                }
            }
            ast::ExprKind::DictLit(items) => {
                for (k, v) in items {
                    self.expr(k, Ctx::InDict);
                    self.expr(v, Ctx::InDict);
                }
            }
            ast::ExprKind::Lambda { params, body } => {
                for p in params {
                    if let Some(d) = &p.default {
                        self.expr(d, Ctx::Other);
                    }
                }
                self.expr(body, Ctx::Other);
            }
            ast::ExprKind::NamedExpr {
                target,
                target_span,
                value,
            } => {
                if target == self.name {
                    self.hit(*target_span, "rebound".into());
                }
                self.expr(value, Ctx::Other);
            }
            ast::ExprKind::GenExp { elem, generators }
            | ast::ExprKind::ListComp { elem, generators }
            | ast::ExprKind::SetComp { elem, generators } => {
                self.expr(elem, Ctx::Other);
                self.comps(generators);
            }
            ast::ExprKind::DictComp {
                key,
                value,
                generators,
            } => {
                self.expr(key, Ctx::Other);
                self.expr(value, Ctx::Other);
                self.comps(generators);
            }
            ast::ExprKind::IfExp { test, body, orelse } => {
                self.expr(test, Ctx::Test);
                self.expr(body, Ctx::Other);
                self.expr(orelse, Ctx::Other);
            }
            ast::ExprKind::Yield(Some(v)) => self.expr(v, Ctx::Yielded),
            ast::ExprKind::Yield(None) => {}
            ast::ExprKind::YieldFrom(v) | ast::ExprKind::Starred(v) => self.expr(v, Ctx::Other),
            ast::ExprKind::Binary { op, left, right } => {
                use ast::BinOp::*;
                match op {
                    And | Or => {
                        self.expr(left, Ctx::BoolOperand);
                        self.expr(right, Ctx::BoolOperand);
                    }
                    Eq | NotEq | Lt | LtEq | Gt | GtEq | In | NotIn | Is | IsNot => {
                        self.expr(left, Ctx::Compared(*op, true, right));
                        self.expr(right, Ctx::Compared(*op, false, left));
                    }
                    _ => {
                        let name = binop_name(*op).unwrap_or("other");
                        self.expr(left, Ctx::BinOperand(name, true, right));
                        self.expr(right, Ctx::BinOperand(name, false, left));
                    }
                }
            }
            ast::ExprKind::Compare { first, rest } => {
                let operands: Vec<&ast::Expr> = std::iter::once(first.as_ref())
                    .chain(rest.iter().map(|(_, e)| e))
                    .collect();
                for (i, operand) in operands.iter().enumerate() {
                    let (op, other) = if i > 0 {
                        (rest[i - 1].0, operands[i - 1])
                    } else {
                        (rest[0].0, operands[1])
                    };
                    self.expr(operand, Ctx::Compared(op, i == 0, other));
                }
            }
            ast::ExprKind::Unary { op, operand } => {
                let name = match op {
                    ast::UnaryOp::Neg => "neg",
                    ast::UnaryOp::Pos => "pos",
                    ast::UnaryOp::Not => "not",
                    _ => "invert",
                };
                self.expr(operand, Ctx::Unary(name));
            }
            _ => {}
        }
    }

    fn comps(&mut self, generators: &[ast::CompFor]) {
        for g in generators {
            self.target(&g.target);
            self.expr(&g.iter, Ctx::Iterated);
            for c in &g.ifs {
                self.expr(c, Ctx::Other);
            }
        }
    }
}

/// A parameter's evidence, as the models read it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParamEvidence {
    pub feats: Vec<String>,
    pub events: Vec<String>,
}

/// The positional (not keyword-only) parameters a predictor sees: after
/// `self`/`cls` for a method that is not static.
pub(crate) fn user_params(f: &ast::FuncDef, is_method: bool) -> &[ast::Param] {
    let is_static = f.decorators.iter().any(|d| d.name == "staticmethod");
    let positional = f.kwonly_start.unwrap_or(f.params.len()).min(f.params.len());
    let offset = usize::from(is_method && !is_static && positional > 0);
    &f.params[offset.min(positional)..positional]
}

/// Features for `f`'s user parameter at `index`; `class` is the enclosing
/// class of a method.
pub(crate) fn param_evidence(
    f: &ast::FuncDef,
    class: Option<&str>,
    index: usize,
) -> Option<ParamEvidence> {
    let short = f.name.rsplit('.').next().unwrap_or(&f.name);
    let user = user_params(f, class.is_some());
    let p = user.get(index)?;
    let mut feats = Vec::new();
    feats.extend(subtokens(&p.name).into_iter().map(|t| format!("pn:{t}")));
    feats.push(format!("pnf:{}", p.name.to_lowercase()));
    feats.extend(subtokens(short).into_iter().map(|t| format!("fn:{t}")));
    if let Some(c) = class {
        feats.extend(subtokens(c).into_iter().map(|t| format!("cls:{t}")));
        if short.starts_with("__") && short.ends_with("__") {
            feats.push(format!("dunder:{short}"));
        }
    }
    feats.push(format!("pos:{}", index.min(5)));
    feats.push(format!("npar:{}", user.len().min(6)));
    feats.push(format!("def:{}", default_kind(p.default.as_ref())));
    let mut uses = Uses {
        name: &p.name,
        out: Vec::new(),
    };
    uses.stmts(&f.body);
    uses.out.sort_by_key(|u| u.at);
    let events: Vec<String> = uses
        .out
        .into_iter()
        .map(|u| u.event)
        .take(MAX_EVENTS)
        .collect();
    feats.push(format!("nuse:{}", events.len().min(5)));
    Some(ParamEvidence { feats, events })
}

fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Every positional parameter of every function in `module`, with its
/// features, one JSON object per line: what `pyrs check --show-features`
/// prints, for checking against the Python extractor.
pub fn feature_lines(module: &ast::Module) -> Vec<String> {
    fn visit(stmts: &[ast::Stmt], class: Option<&str>, out: &mut Vec<String>) {
        for s in stmts {
            match &s.kind {
                ast::StmtKind::ClassDef(c) => visit(&c.body, Some(&c.name), out),
                ast::StmtKind::FuncDef(f) => {
                    let n = user_params(f, class.is_some()).len();
                    for i in 0..n {
                        let Some(ev) = param_evidence(f, class, i) else {
                            continue;
                        };
                        let func = match class {
                            Some(c) => format!("{c}.{}", f.name),
                            None => f.name.clone(),
                        };
                        let list = |xs: &[String]| {
                            xs.iter()
                                .map(|x| json_str(x))
                                .collect::<Vec<_>>()
                                .join(", ")
                        };
                        let label = |e| predict_label(&ev, e).map_or("", |(l, _)| l);
                        out.push(format!(
                            "{{\"func\": {}, \"param\": {}, \"feats\": [{}], \"events\": [{}], \
                             \"ml\": {}, \"dl\": {}}}",
                            json_str(&func),
                            json_str(&user_params(f, class.is_some())[i].name),
                            list(&ev.feats),
                            list(&ev.events),
                            json_str(label(Engine::Ml)),
                            json_str(label(Engine::Dl))
                        ));
                    }
                    visit(&f.body, None, out);
                }
                ast::StmtKind::If { branches, orelse } => {
                    for (_, b) in branches {
                        visit(b, class, out);
                    }
                    visit(orelse, class, out);
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    visit(&module.body, None, &mut out);
    out
}

// ---- models ----------------------------------------------------------------

/// Which predictor fills a parameter nothing else types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Engine {
    /// The compiler's own rules only.
    Static,
    /// The linear model.
    Ml,
    /// The transformer.
    Dl,
}

std::thread_local! {
    static ENGINE: std::cell::Cell<Engine> = const { std::cell::Cell::new(Engine::Static) };
}

/// Choose the predictor for the analyses that follow on this thread.
pub fn set_engine(engine: Engine) {
    ENGINE.with(|e| e.set(engine));
}

pub(crate) fn engine() -> Engine {
    ENGINE.with(std::cell::Cell::get)
}

static ML_BIN: &[u8] = include_bytes!("../models/ml.bin");
static DL_BIN: &[u8] = include_bytes!("../models/dl.bin");

struct Tensor {
    dims: Vec<usize>,
    data: Vec<f32>,
}

/// The `PYRSMDL1` container `scripts/infer_eval/export_models.py` writes.
fn read_container(bytes: &[u8]) -> std::collections::HashMap<String, Tensor> {
    let mut out = std::collections::HashMap::new();
    let u16_at = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]) as usize;
    let u32_at = |o: usize| {
        u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]) as usize
    };
    let f32_at =
        |o: usize| f32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
    assert_eq!(&bytes[..8], b"PYRSMDL1", "model container magic");
    let n = u32_at(8);
    let mut off = 12;
    for _ in 0..n {
        let len = u16_at(off);
        off += 2;
        let name = String::from_utf8_lossy(&bytes[off..off + len]).into_owned();
        off += len;
        let (kind, rank) = (bytes[off], bytes[off + 1] as usize);
        off += 2;
        let dims: Vec<usize> = (0..rank).map(|i| u32_at(off + 4 * i)).collect();
        off += 4 * rank;
        let count: usize = dims.iter().product();
        let mut data = Vec::with_capacity(count);
        if kind == 1 {
            let rows = dims[0];
            let per = count / rows.max(1);
            let scales: Vec<f32> = (0..rows).map(|r| f32_at(off + 4 * r)).collect();
            off += 4 * rows;
            for r in 0..rows {
                for c in 0..per {
                    data.push(bytes[off + r * per + c] as i8 as f32 * scales[r]);
                }
            }
            off += count;
        } else {
            for i in 0..count {
                data.push(f32_at(off + 4 * i));
            }
            off += 4 * count;
        }
        out.insert(name, Tensor { dims, data });
    }
    out
}

fn fnv1a(token: &str) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for b in token.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

struct LinearModel {
    coef: Tensor,
    intercept: Vec<f32>,
}

fn linear_model() -> &'static LinearModel {
    static MODEL: std::sync::OnceLock<LinearModel> = std::sync::OnceLock::new();
    MODEL.get_or_init(|| {
        let mut t = read_container(ML_BIN);
        LinearModel {
            coef: t.remove("coef").expect("coef"),
            intercept: t.remove("intercept").expect("intercept").data,
        }
    })
}

/// The linear model's label scores: hashed features, `1 + ln(count)`.
fn ml_scores(ev: &ParamEvidence) -> Vec<f32> {
    let m = linear_model();
    let dim = m.coef.dims[1];
    let mut counts: std::collections::BTreeMap<usize, u32> = std::collections::BTreeMap::new();
    let mut add = |t: &str| *counts.entry(fnv1a(t) as usize % dim).or_default() += 1;
    for f in &ev.feats {
        add(f);
    }
    for e in &ev.events {
        add(&format!("ev:{e}"));
    }
    for e in &ev.events {
        let mut parts = e.split(':');
        let op = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("");
        add(&format!("evop:{op}:{arg}"));
    }
    let mut scores = m.intercept.clone();
    for (bucket, count) in counts {
        let x = 1.0 + (count as f32).ln();
        for (l, s) in scores.iter_mut().enumerate() {
            *s += m.coef.data[l * dim + bucket] * x;
        }
    }
    scores
}

const DL_BUCKETS: usize = 1 << 14;
const DL_DIM: usize = 64;
const DL_MAX_NAME: usize = 24;
const DL_PARTS: usize = 4;
const DL_HEADS: usize = 4;
const DL_LAYERS: usize = 2;

fn transformer() -> &'static std::collections::HashMap<String, Tensor> {
    static MODEL: std::sync::OnceLock<std::collections::HashMap<String, Tensor>> =
        std::sync::OnceLock::new();
    MODEL.get_or_init(|| read_container(DL_BIN))
}

/// `y = x W^T + b` for each row of `x` (`rows × in`).
fn affine(x: &[f32], rows: usize, w: &Tensor, b: &Tensor) -> Vec<f32> {
    let (out_dim, in_dim) = (w.dims[0], w.dims[1]);
    let mut y = vec![0.0f32; rows * out_dim];
    for (xr, yr) in x
        .chunks_exact(in_dim)
        .take(rows)
        .zip(y.chunks_exact_mut(out_dim))
    {
        for ((yo, wr), bo) in yr.iter_mut().zip(w.data.chunks_exact(in_dim)).zip(&b.data) {
            *yo = bo + wr.iter().zip(xr).map(|(a, b)| a * b).sum::<f32>();
        }
    }
    y
}

fn layer_norm(x: &mut [f32], rows: usize, g: &Tensor, b: &Tensor) {
    let d = g.data.len();
    for r in 0..rows {
        let row = &mut x[r * d..(r + 1) * d];
        let mean = row.iter().sum::<f32>() / d as f32;
        let var = row.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / d as f32;
        let inv = 1.0 / (var + 1e-5).sqrt();
        for (i, v) in row.iter_mut().enumerate() {
            *v = (*v - mean) * inv * g.data[i] + b.data[i];
        }
    }
}

/// The transformer's label scores. Mirrors `dl_scores` in export_models.py,
/// which is checked against PyTorch.
fn dl_scores(ev: &ParamEvidence) -> Vec<f32> {
    let w = transformer();
    let t = |name: &str| w.get(name).expect("transformer tensor");
    let emb = t("emb");
    let row_of = |id: usize| &emb.data[id * DL_DIM..(id + 1) * DL_DIM];
    let bucket = |token: String| 1 + (fnv1a(&token) as usize) % (DL_BUCKETS - 1);

    let mut name_vec = [0.0f32; DL_DIM];
    let names: Vec<usize> = ev
        .feats
        .iter()
        .take(DL_MAX_NAME)
        .map(|f| bucket(format!("n:{f}")))
        .collect();
    for &id in &names {
        for (i, v) in row_of(id).iter().enumerate() {
            name_vec[i] += v;
        }
    }
    if !names.is_empty() {
        for v in &mut name_vec {
            *v /= names.len() as f32;
        }
    }

    let n_ev = ev.events.len().min(MAX_EVENTS);
    let len = n_ev + 1;
    let mut x = vec![0.0f32; len * DL_DIM];
    x[..DL_DIM].copy_from_slice(&t("cls").data);
    for (j, e) in ev.events.iter().take(n_ev).enumerate() {
        for (i, part) in e.split(':').take(DL_PARTS).enumerate() {
            let id = bucket(format!("e{i}:{part}"));
            for (k, v) in row_of(id).iter().enumerate() {
                x[(j + 1) * DL_DIM + k] += v;
            }
        }
    }
    let pos = t("pos");
    for r in 0..len {
        for k in 0..DL_DIM {
            x[r * DL_DIM + k] += pos.data[r * DL_DIM + k];
        }
    }

    let hd = DL_DIM / DL_HEADS;
    for l in 0..DL_LAYERS {
        let p = |name: &str| t(&format!("layer{l}.{name}"));
        let qkv = affine(&x, len, p("in_proj_weight"), p("in_proj_bias"));
        let mut attended = vec![0.0f32; len * DL_DIM];
        let mut logits = vec![0.0f32; len];
        for h in 0..DL_HEADS {
            for i in 0..len {
                let q = &qkv[i * 3 * DL_DIM + h * hd..i * 3 * DL_DIM + (h + 1) * hd];
                let mut max = f32::NEG_INFINITY;
                for (j, logit) in logits.iter_mut().enumerate() {
                    let kbase = j * 3 * DL_DIM + DL_DIM + h * hd;
                    let k = &qkv[kbase..kbase + hd];
                    let dot: f32 = q.iter().zip(k).map(|(a, b)| a * b).sum();
                    *logit = dot / (hd as f32).sqrt();
                    max = max.max(*logit);
                }
                let mut total = 0.0f32;
                for logit in logits.iter_mut() {
                    *logit = (*logit - max).exp();
                    total += *logit;
                }
                for (j, logit) in logits.iter().enumerate() {
                    let a = logit / total;
                    let vbase = j * 3 * DL_DIM + 2 * DL_DIM + h * hd;
                    for k in 0..hd {
                        attended[i * DL_DIM + h * hd + k] += a * qkv[vbase + k];
                    }
                }
            }
        }
        let o = affine(&attended, len, p("out_proj_weight"), p("out_proj_bias"));
        for (xi, oi) in x.iter_mut().zip(&o) {
            *xi += oi;
        }
        layer_norm(&mut x, len, p("norm1_weight"), p("norm1_bias"));
        let mut ff = affine(&x, len, p("linear1_weight"), p("linear1_bias"));
        for v in &mut ff {
            *v = v.max(0.0);
        }
        let ff = affine(&ff, len, p("linear2_weight"), p("linear2_bias"));
        for (xi, fi) in x.iter_mut().zip(&ff) {
            *xi += fi;
        }
        layer_norm(&mut x, len, p("norm2_weight"), p("norm2_bias"));
    }

    let mut h = Vec::with_capacity(2 * DL_DIM);
    h.extend_from_slice(&name_vec);
    h.extend_from_slice(&x[..DL_DIM]);
    let mut hidden = affine(&h, 1, t("head0_weight"), t("head0_bias"));
    for v in &mut hidden {
        *v = v.max(0.0);
    }
    affine(&hidden, 1, t("head2_weight"), t("head2_bias"))
}

/// The most likely label and its probability.
pub(crate) fn predict_label(ev: &ParamEvidence, engine: Engine) -> Option<(&'static str, f32)> {
    let scores = match engine {
        Engine::Static => return None,
        Engine::Ml => ml_scores(ev),
        Engine::Dl => dl_scores(ev),
    };
    let max = scores.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let total: f32 = scores.iter().map(|s| (s - max).exp()).sum();
    let (best, score) = scores
        .iter()
        .enumerate()
        .fold((0, f32::NEG_INFINITY), |acc, (i, s)| {
            if *s > acc.1 { (i, *s) } else { acc }
        });
    Some((LABELS[best], (score - max).exp() / total))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(src: &str) -> ParamEvidence {
        let module = parser::parse(src).expect("parses");
        let ast::StmtKind::FuncDef(f) = &module.body[0].kind else {
            panic!("a function");
        };
        param_evidence(f, None, 0).expect("a parameter")
    }

    /// The events are the shared definition with scripts/infer_eval/features.py.
    #[test]
    fn usage_events_follow_the_definition() {
        let ev = evidence(
            "def f(xs, k=2):\n    for x in xs:\n        print(x)\n    return len(xs) + xs[0] * k\n",
        );
        assert_eq!(ev.events, ["iterated", "arg:len:0", "getitem:int"]);
        assert!(ev.feats.contains(&"pn:xs".to_string()));
        assert!(ev.feats.contains(&"npar:2".to_string()));
    }

    /// Both models are deterministic and read the shipped weights.
    #[test]
    fn the_models_predict_from_names_and_uses() {
        let ev = evidence("def shout(name):\n    return name.upper()\n");
        assert_eq!(predict_label(&ev, Engine::Ml).map(|p| p.0), Some("str"));
        assert_eq!(predict_label(&ev, Engine::Dl).map(|p| p.0), Some("str"));
        assert_eq!(
            predict_label(&ev, Engine::Dl),
            predict_label(&ev, Engine::Dl)
        );
        assert_eq!(predict_label(&ev, Engine::Static), None);
    }

    /// The enclosing function's name is evidence too, and the linear model
    /// weighs it heavily: the same body under `def f` reads as `Any`, because
    /// in the training projects a function called `f` is a test fixture whose
    /// parameters are annotated `Any`. Worth pinning -- it is why a placeholder
    /// name is a bad way to sample a prediction, and why the corpus programs
    /// these models are measured on carry the names real code uses.
    #[test]
    fn the_function_name_is_evidence() {
        let ev = evidence("def f(name):\n    return name.upper()\n");
        assert_eq!(predict_label(&ev, Engine::Ml).map(|p| p.0), Some("any"));
        assert_eq!(predict_label(&ev, Engine::Dl).map(|p| p.0), Some("str"));
    }

    #[test]
    fn subtokens_match_the_python_split() {
        assert_eq!(subtokens("max_value"), ["max", "value"]);
        assert_eq!(subtokens("maxValue2"), ["max", "value", "2"]);
        assert_eq!(subtokens("HTTPServer"), ["http", "server"]);
        assert_eq!(subtokens("__init__"), ["init"]);
        assert_eq!(subtokens("URL"), ["url"]);
        assert_eq!(subtokens("x"), ["x"]);
    }
}
