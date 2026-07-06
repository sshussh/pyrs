//! Abstract syntax tree produced by the parser.

use common::Span;

/// A builtin type name as written in annotations (`x: int`, `-> float`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeName {
    Int,
    Float,
    Bool,
    /// `-> None`: the function returns nothing.
    None,
}

impl std::fmt::Display for TypeName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeName::Int => write!(f, "int"),
            TypeName::Float => write!(f, "float"),
            TypeName::Bool => write!(f, "bool"),
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
    Return(Option<Expr>),
    /// `name = value` or `name: ty = value`
    Assign {
        name: String,
        name_span: Span,
        annotation: Option<TypeName>,
        value: Expr,
    },
    /// `name op= value`
    AugAssign {
        name: String,
        name_span: Span,
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
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
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
            BinOp::Eq => "==",
            BinOp::NotEq => "!=",
            BinOp::Lt => "<",
            BinOp::LtEq => "<=",
            BinOp::Gt => ">",
            BinOp::GtEq => ">=",
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
    /// `int(x)`, `float(x)`, `bool(x)`
    Cast {
        ty: TypeName,
        arg: Box<Expr>,
    },
    Binary {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
    },
}
