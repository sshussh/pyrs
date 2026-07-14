//! Abstract syntax tree produced by the parser.

use common::Span;

/// A builtin type name as written in annotations (`x: int`, `-> float`).
/// `List` / container types intern nested pieces (`&'static`) so the enum
/// stays `Copy` while types nest (`list[list[int]]`, `tuple[int, str]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeName {
    Int,
    Float,
    Bool,
    Str,
    /// Open file handle from `open(...)` (not CPython's typing.IO name).
    File,
    List(&'static TypeName),
    /// Heterogeneous fixed-arity tuple: `tuple[int, str]`, empty `tuple[()]`.
    Tuple(&'static [TypeName]),
    /// `dict[K, V]` — keys are restricted in semantic (int/str).
    Dict {
        key: &'static TypeName,
        value: &'static TypeName,
    },
    /// `set[T]` — elements restricted like dict keys in semantic.
    Set(&'static TypeName),
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
            TypeName::File => write!(f, "file"),
            TypeName::List(e) => write!(f, "list[{e}]"),
            TypeName::Tuple(elems) => {
                if elems.is_empty() {
                    return write!(f, "tuple[()]");
                }
                write!(f, "tuple[")?;
                for (i, e) in elems.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{e}")?;
                }
                write!(f, "]")
            }
            TypeName::Dict { key, value } => write!(f, "dict[{key}, {value}]"),
            TypeName::Set(e) => write!(f, "set[{e}]"),
            TypeName::None => write!(f, "None"),
        }
    }
}

/// Exception type name used in `raise` / `except`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExcType {
    ValueError,
    KeyError,
    IndexError,
    ZeroDivisionError,
    TypeError,
    RuntimeError,
}

impl ExcType {
    pub fn as_str(self) -> &'static str {
        match self {
            ExcType::ValueError => "ValueError",
            ExcType::KeyError => "KeyError",
            ExcType::IndexError => "IndexError",
            ExcType::ZeroDivisionError => "ZeroDivisionError",
            ExcType::TypeError => "TypeError",
            ExcType::RuntimeError => "RuntimeError",
        }
    }
}

impl std::fmt::Display for ExcType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
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
    /// Default value after `=`; if present, later params must also have defaults.
    pub default: Option<Expr>,
}

/// `name=value` in a call.
#[derive(Debug, Clone, PartialEq)]
pub struct Keyword {
    pub name: String,
    pub name_span: Span,
    pub value: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FuncDef {
    pub name: String,
    pub params: Vec<Param>,
    /// `*args: T` — remaining positionals as `list[T]`.
    pub vararg: Option<Param>,
    /// `**kwargs: T` — remaining keywords as `dict[str, T]`.
    pub kwarg: Option<Param>,
    /// Annotated return type; `None` means no annotation (returns nothing).
    pub ret: Option<TypeName>,
    pub body: Vec<Stmt>,
    /// Span of the `def name(...)` header, for diagnostics.
    pub span: Span,
}

/// One positional slot in a call: a value or `*iterable` unpack.
#[derive(Debug, Clone, PartialEq)]
pub enum PosArg {
    Pos(Expr),
    Star(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

/// The left-hand side of an assignment.
#[derive(Debug, Clone, PartialEq)]
pub enum AssignTarget {
    Name {
        name: String,
        span: Span,
    },
    /// `base[index] = ...`
    Index {
        base: Expr,
        index: Expr,
    },
    /// `a, b = ...` / nested unpacking targets.
    Tuple(Vec<AssignTarget>),
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
        /// Runs when the loop ends without `break`.
        orelse: Vec<Stmt>,
    },
    /// `for var in iter:` — iter is `range(...)`, a list, a str, or a file.
    For {
        var: String,
        var_span: Span,
        iter: Expr,
        body: Vec<Stmt>,
        /// Runs when the loop ends without `break`.
        orelse: Vec<Stmt>,
    },
    Return(Option<Expr>),
    /// `a = b = value` or `name: ty = value` (annotation only with a single name target).
    Assign {
        /// Left-to-right targets; assignment runs right-to-left after evaluating `value` once.
        targets: Vec<AssignTarget>,
        annotation: Option<TypeName>,
        value: Expr,
    },
    /// `target op= value`
    AugAssign {
        target: AssignTarget,
        op: BinOp,
        value: Expr,
    },
    /// `del target` — currently `del d[k]` for dicts.
    Delete {
        target: AssignTarget,
    },
    ExprStmt(Expr),
    /// `global name, ...` — declares that assignments in this function
    /// target module-level variables.
    Global(Vec<(String, Span)>),
    /// `import module [as alias], ...` — each module may be dotted (`pkg.mod`).
    /// `names` is `(module, optional alias, span of the module name)`.
    Import {
        names: Vec<(String, Option<String>, Span)>,
    },
    /// `from module import name [as alias], ...` and relative forms
    /// (`from . import x`, `from ..pkg import y`). `level` is the number of
    /// leading dots (0 = absolute). `module` is the path after the dots
    /// (empty for `from . import x`).
    FromImport {
        module: String,
        /// Number of leading dots: 0 absolute, 1 = `.`, 2 = `..`, …
        level: u32,
        /// (imported name, optional local alias, span of the name)
        names: Vec<(String, Option<String>, Span)>,
        span: Span,
    },
    /// `with expr [as name]:` — files only; close() runs on every exit
    /// path out of the body.
    With {
        item: Expr,
        target: Option<(String, Span)>,
        body: Vec<Stmt>,
    },
    /// `raise ExcType(msg)` — msg is a str expression.
    Raise {
        exc: ExcType,
        message: Expr,
    },
    /// `try` / `except` / `else` / `finally`.
    /// `orelse` is only valid when there is at least one `except` (CPython).
    Try {
        body: Vec<Stmt>,
        handlers: Vec<ExceptHandler>,
        orelse: Vec<Stmt>,
        finally: Vec<Stmt>,
    },
    Pass,
    Break,
    Continue,
}

/// One `except` clause under a `try`.
#[derive(Debug, Clone, PartialEq)]
pub struct ExceptHandler {
    /// `None` = bare `except:`.
    pub exc: Option<ExcType>,
    /// Optional `as name` binding (message string at runtime).
    pub bind: Option<(String, Span)>,
    pub body: Vec<Stmt>,
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

/// Supported f-string format specs (minimal subset of PEP 3101).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FStringFormat {
    /// `{x:.Nf}` — fixed-point with `N` digits after the decimal.
    DotNf { precision: u32 },
}

#[derive(Debug, Clone, PartialEq)]
pub enum FStringPart {
    Literal(String),
    Expr {
        expr: Expr,
        format: Option<FStringFormat>,
    },
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
        args: Vec<PosArg>,
        keywords: Vec<Keyword>,
        /// `**mapping` at the end of the call (at most one).
        kwargs: Option<Box<Expr>>,
    },
    /// `obj.attr` without a call — currently only `sys.argv`.
    Attribute {
        base: Box<Expr>,
        attr: String,
        attr_span: Span,
    },
    /// `obj.method(args)` — list and str methods.
    MethodCall {
        base: Box<Expr>,
        method: String,
        method_span: Span,
        args: Vec<PosArg>,
        keywords: Vec<Keyword>,
        kwargs: Option<Box<Expr>>,
    },
    /// `base[index]`
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
    },
    /// `base[lo:hi:step]` — any part may be omitted.
    Slice {
        base: Box<Expr>,
        lo: Option<Box<Expr>>,
        hi: Option<Box<Expr>>,
        step: Option<Box<Expr>>,
    },
    /// `f"..."`: literal chunks interleaved with interpolated expressions.
    JoinedStr(Vec<FStringPart>),
    /// `[a, b, c]`
    ListLit(Vec<Expr>),
    /// `(a, b)`, `(a,)`, `()` — also bare `a, b` in assign/return contexts.
    TupleLit(Vec<Expr>),
    /// `{k: v, ...}`
    DictLit(Vec<(Expr, Expr)>),
    /// `{a, b, ...}` — nonempty; empty set is `set()`.
    SetLit(Vec<Expr>),
    /// `[elem for var in iter if cond]`
    ListComp {
        elem: Box<Expr>,
        var: String,
        var_span: Span,
        iter: Box<Expr>,
        cond: Option<Box<Expr>>,
    },
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
