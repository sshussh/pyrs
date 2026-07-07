//! The contract between semantic analysis and codegen.
//!
//! A fully resolved, fully typed tree: every expression carries its type,
//! all implicit conversions have been made explicit casts, and every local
//! variable is pre-declared on its function. Codegen consumes this without
//! doing any inference of its own.

pub fn ping() -> String {
    String::from("pong")
}

/// Element type of a list. Kept scalar (no nesting) so [`Ty`] stays `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elem {
    Int,
    Float,
    Bool,
    Str,
}

impl Elem {
    pub fn ty(self) -> Ty {
        match self {
            Elem::Int => Ty::Int,
            Elem::Float => Ty::Float,
            Elem::Bool => Ty::Bool,
            Elem::Str => Ty::Str,
        }
    }
}

/// A resolved runtime type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    /// 64-bit signed integer
    Int,
    /// 64-bit IEEE-754 float
    Float,
    Bool,
    /// Immutable, heap-allocated, length-prefixed string
    Str,
    /// Growable list of scalar elements
    List(Elem),
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
            Ty::List(e) => write!(f, "list[{}]", e.ty()),
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
    /// can emit all allocas up front in the entry block. Includes
    /// compiler-synthesized temporaries (names starting with '.').
    pub locals: Vec<(String, Ty)>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// Store into a local (params included).
    Assign {
        name: String,
        value: Expr,
    },
    /// `base[index] = value` — base is a list.
    IndexAssign {
        base: Expr,
        index: Expr,
        value: Expr,
    },
    /// `list.append(value)`
    ListAppend {
        list: Expr,
        value: Expr,
    },
    If {
        branches: Vec<(Expr, Vec<Stmt>)>,
        orelse: Vec<Stmt>,
    },
    /// A loop. `continue` jumps to `step` (then the condition); a plain
    /// `while` has an empty `step`, a desugared `for` increments there.
    While {
        cond: Expr,
        body: Vec<Stmt>,
        step: Vec<Stmt>,
    },
    Return(Option<Expr>),
    /// Evaluate and discard (calls with side effects).
    ExprStmt(Expr),
    /// The `print` builtin: space-separated values, trailing newline.
    Print(Vec<Expr>),
    /// Abort with a runtime error message (exit code 1).
    Die(String),
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
    /// Evaluate `value`, store it in local `name`, then evaluate `body`.
    /// Used for compiler temps (e.g. comparison chaining).
    Let {
        name: String,
        value: Box<Expr>,
        body: Box<Expr>,
    },
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
    /// `base[index]`: str → str (one character), list[T] → T.
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
    },
    /// `base[lo:hi]` (no step). Semantic fills defaults: missing lo → 0,
    /// missing hi → i64::MAX; the runtime clamps Python-style.
    Slice {
        base: Box<Expr>,
        lo: Box<Expr>,
        hi: Box<Expr>,
    },
    /// `needle in haystack`: str-in-str substring or element-in-list.
    /// The needle is already coerced to the element type. Result is Bool.
    Contains {
        needle: Box<Expr>,
        haystack: Box<Expr>,
    },
    /// `list.pop(index)`; index defaults to -1 (the last element).
    ListPop {
        list: Box<Expr>,
        index: Box<Expr>,
    },
    /// A list literal; `ty` is `List(elem)` and items are already coerced.
    ListLit(Vec<Expr>),
    /// `len(x)` for str or list.
    Len(Box<Expr>),
    /// int → float (sitofp)
    IntToFloat(Box<Expr>),
    /// float → int, truncating toward zero (Python's `int()`)
    FloatToInt(Box<Expr>),
    /// bool → int (zext)
    BoolToInt(Box<Expr>),
    /// `str(x)` conversions
    IntToStr(Box<Expr>),
    FloatToStr(Box<Expr>),
    BoolToStr(Box<Expr>),
    /// truthiness test → bool: numerics `!= 0`, str/list `len != 0`
    ToBool(Box<Expr>),
}

/// Binary operations. Operand types are encoded in the operand `Expr`s and
/// are always equal on both sides; comparison results are `Bool`.
///
/// On `Str` operands: `Add` is concatenation, `Mul` is repetition (the int
/// count is always the right operand), comparisons are lexicographic.
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
    /// `**`: int**int → int (negative exponent traps), float → llvm.pow
    Pow,
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
