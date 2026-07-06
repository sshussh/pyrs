//! The contract between semantic analysis and codegen.
//!
//! A fully resolved, fully typed tree: every expression carries its type,
//! all implicit conversions have been made explicit casts, and every local
//! variable is pre-declared on its function. Codegen consumes this without
//! doing any inference of its own.

pub fn ping() -> String {
    String::from("pong")
}

/// A resolved runtime type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    /// 64-bit signed integer
    Int,
    /// 64-bit IEEE-754 float
    Float,
    Bool,
    /// String constants (only valid in `print` arguments for now)
    Str,
    /// Absence of a value: `None` returns / bare functions
    None,
}

impl std::fmt::Display for Ty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ty::Int => write!(f, "int"),
            Ty::Float => write!(f, "float"),
            Ty::Bool => write!(f, "bool"),
            Ty::Str => write!(f, "str"),
            Ty::None => write!(f, "None"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Module {
    pub funcs: Vec<Function>,
    /// Name of the function that is the program entry point.
    pub entry: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    pub name: String,
    pub params: Vec<(String, Ty)>,
    pub ret: Ty,
    /// Every local variable (excluding params) with its type, so codegen
    /// can emit all allocas up front in the entry block.
    pub locals: Vec<(String, Ty)>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// Store into a local (params included).
    Assign { name: String, value: Expr },
    If {
        branches: Vec<(Expr, Vec<Stmt>)>,
        orelse: Vec<Stmt>,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
    },
    Return(Option<Expr>),
    /// Evaluate and discard (calls with side effects).
    ExprStmt(Expr),
    /// The `print` builtin: space-separated values, trailing newline.
    Print(Vec<Expr>),
    Break,
    Continue,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    pub ty: Ty,
    pub kind: ExprKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    ConstInt(i64),
    ConstFloat(f64),
    ConstBool(bool),
    ConstStr(String),
    /// Load a local variable.
    Local(String),
    Call {
        func: String,
        args: Vec<Expr>,
    },
    Binary {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Unary {
        op: UnOp,
        operand: Box<Expr>,
    },
    /// int → float (sitofp)
    IntToFloat(Box<Expr>),
    /// float → int, truncating toward zero (fptosi, Python's `int()`)
    FloatToInt(Box<Expr>),
    /// bool → int (zext)
    BoolToInt(Box<Expr>),
    /// truthiness test: int/float/bool → bool (`x != 0`)
    ToBool(Box<Expr>),
}

/// Binary operations. Operand types are encoded in the operand `Expr`s and
/// are always equal on both sides; comparison results are `Bool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    /// True division; semantic guarantees both operands are Float.
    Div,
    /// Floor division (Python semantics: rounds toward negative infinity).
    FloorDiv,
    /// Python modulo (result takes the sign of the divisor).
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// Short-circuit; operands are Bool.
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}
