//! Abstract syntax tree produced by the parser.

use common::Span;

/// Element type inside `list[...]` (lists don't nest yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElemName {
    Int,
    Float,
    Bool,
    Str,
}

/// A builtin type name as written in annotations (`x: int`, `-> float`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeName {
    Int,
    Float,
    Bool,
    Str,
    List(ElemName),
    /// `-> None`: the function returns nothing.
    None,
}

impl std::fmt::Display for TypeName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeName::Int => write!(f, "int"),
            TypeName::Float => write!(f, "float"),
            TypeName::Bool => write!(f, "bool"),
            TypeName::Str => write!(f, "str"),
            TypeName::List(e) => {
                let e = match e {
                    ElemName::Int => "int",
                    ElemName::Float => "float",
                    ElemName::Bool => "bool",
                    ElemName::Str => "str",
                };
                write!(f, "list[{e}]")
            }
            TypeName::None => write!(f, "None"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Module {
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: TypeName,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FuncDef {
    pub name: String,
    pub params: Vec<Param>,
    /// Annotated return type; `None` means no annotation (returns nothing).
    pub ret: Option<TypeName>,
    pub body: Vec<Stmt>,
    /// Span of the `def name(...)` header, for diagnostics.
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

/// The left-hand side of an assignment.
#[derive(Debug, Clone, PartialEq)]
pub enum AssignTarget {
    Name { name: String, span: Span },
    /// `base[index] = ...`
    Index { base: Expr, index: Expr },
}

#[derive(Debug, Clone, PartialEq)]
pub enum StmtKind {
    FuncDef(FuncDef),
    /// `if`/`elif` chain: each branch is (condition, body).
    If {
        branches: Vec<(Expr, Vec<Stmt>)>,
        orelse: Vec<Stmt>,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
    },
    /// `for var in iter:` — iter is `range(...)`, a list, or a str.
    For {
        var: String,
        var_span: Span,
        iter: Expr,
        body: Vec<Stmt>,
    },
    Return(Option<Expr>),
    /// `target = value` or `name: ty = value`
    Assign {
        target: AssignTarget,
        annotation: Option<TypeName>,
        value: Expr,
    },
    /// `target op= value`
    AugAssign {
        target: AssignTarget,
        op: BinOp,
        value: Expr,
    },
    ExprStmt(Expr),
    Pass,
    Break,
    Continue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    /// `/` — true division, always produces float (Python semantics)
    Div,
    /// `//` — floor division
    FloorDiv,
    /// `%` — Python modulo (result takes the sign of the divisor)
    Mod,
    /// `**` — power, right-associative
    Pow,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    /// `in` — substring or list membership
    In,
    /// `not in`
    NotIn,
    And,
    Or,
}

impl std::fmt::Display for BinOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::FloorDiv => "//",
            BinOp::Mod => "%",
            BinOp::Pow => "**",
            BinOp::Eq => "==",
            BinOp::NotEq => "!=",
            BinOp::Lt => "<",
            BinOp::LtEq => "<=",
            BinOp::Gt => ">",
            BinOp::GtEq => ">=",
            BinOp::In => "in",
            BinOp::NotIn => "not in",
            BinOp::And => "and",
            BinOp::Or => "or",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// `-x`
    Neg,
    /// `not x`
    Not,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FStringPart {
    Literal(String),
    Expr(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    NoneLit,
    Name(String),
    Call {
        func: String,
        func_span: Span,
        args: Vec<Expr>,
    },
    /// `obj.method(args)` — currently only list methods.
    MethodCall {
        base: Box<Expr>,
        method: String,
        method_span: Span,
        args: Vec<Expr>,
    },
    /// `base[index]`
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
    },
    /// `base[lo:hi]` — either bound may be omitted; no step.
    Slice {
        base: Box<Expr>,
        lo: Option<Box<Expr>>,
        hi: Option<Box<Expr>>,
    },
    /// `f"..."`: literal chunks interleaved with interpolated expressions.
    JoinedStr(Vec<FStringPart>),
    /// `[a, b, c]`
    ListLit(Vec<Expr>),
    /// `int(x)`, `float(x)`, `bool(x)`, `str(x)`
    Cast {
        ty: TypeName,
        arg: Box<Expr>,
    },
    Binary {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// A comparison chain `a < b <= c`: `first` then (op, operand) pairs.
    /// A single comparison is represented as `Binary`.
    Compare {
        first: Box<Expr>,
        rest: Vec<(BinOp, Expr)>,
    },
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
    },
}
