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
    /// `None` type (annotation or union member). Function `-> None` returns nothing.
    None,
    /// `A | B | ...` or `Optional[T]` (flattened/sorted in semantic).
    /// At least two members after parsing (parser intern helper).
    Union(&'static [TypeName]),
    /// User class type annotation (`def f(p: Point)`). Resolved in semantic.
    Class(&'static str),
    /// Limited dynamic type (`x: Any`). Resolved to `ir::Ty::Any`.
    Any,
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
            TypeName::Union(ms) => {
                for (i, m) in ms.iter().enumerate() {
                    if i > 0 {
                        write!(f, " | ")?;
                    }
                    write!(f, "{m}")?;
                }
                Ok(())
            }
            TypeName::Class(name) => write!(f, "{name}"),
            TypeName::Any => write!(f, "Any"),
        }
    }
}

/// Exception type name used in `raise` / `except`.
/// Matching uses CPython-like subclass checks for `Exception` and `OSError`
/// (e.g. `except OSError` catches `FileNotFoundError`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExcType {
    ValueError,
    KeyError,
    IndexError,
    ZeroDivisionError,
    TypeError,
    RuntimeError,
    GeneratorExit,
    OverflowError,
    EOFError,
    FileNotFoundError,
    OSError,
    NameError,
    UnboundLocalError,
    StopIteration,
    Exception,
    PermissionError,
    IsADirectoryError,
    AssertionError,
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
            ExcType::GeneratorExit => "GeneratorExit",
            ExcType::OverflowError => "OverflowError",
            ExcType::EOFError => "EOFError",
            ExcType::FileNotFoundError => "FileNotFoundError",
            ExcType::OSError => "OSError",
            ExcType::NameError => "NameError",
            ExcType::UnboundLocalError => "UnboundLocalError",
            ExcType::StopIteration => "StopIteration",
            ExcType::Exception => "Exception",
            ExcType::PermissionError => "PermissionError",
            ExcType::IsADirectoryError => "IsADirectoryError",
            ExcType::AssertionError => "AssertionError",
        }
    }

    /// All recognized exception type names (for diagnostics).
    pub fn all_names() -> &'static str {
        "ValueError, KeyError, IndexError, ZeroDivisionError, TypeError, \
         RuntimeError, GeneratorExit, OverflowError, EOFError, FileNotFoundError, \
         OSError, NameError, UnboundLocalError, StopIteration, Exception, \
         PermissionError, IsADirectoryError, AssertionError"
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
    /// Parameter type annotation; `None` means omitted (inferred in semantic).
    pub ty: Option<TypeName>,
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

/// `class Name[(Base, ...)]:` body.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassDef {
    pub name: String,
    /// Base class names (empty = no base). Multiple bases rejected in semantic.
    pub bases: Vec<(String, Span)>,
    /// Class body statements (methods, `pass`, annotated class attrs).
    pub body: Vec<Stmt>,
    /// Span of the `class Name(...)` header.
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
    /// `*rest` in unpacking (`a, *rest, b = xs`). At most one per tuple level.
    Starred {
        target: Box<AssignTarget>,
        span: Span,
    },
    /// `obj.attr = ...` (instance field store).
    Attr {
        base: Expr,
        attr: String,
        attr_span: Span,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum StmtKind {
    FuncDef(FuncDef),
    ClassDef(ClassDef),
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
    /// `for target in iter:` — iter is `range(...)`, a list, a str, or a file.
    /// `target` may be a name, unpacking tuple, or other assignment target.
    For {
        target: AssignTarget,
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
    ///
    /// `from module import *` sets `star = true` and leaves `names` empty.
    FromImport {
        module: String,
        /// Number of leading dots: 0 absolute, 1 = `.`, 2 = `..`, …
        level: u32,
        /// (imported name, optional local alias, span of the name).
        /// Empty when `star` is true.
        names: Vec<(String, Option<String>, Span)>,
        /// `from module import *`
        star: bool,
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
    /// `assert test` / `assert test, msg` — desugared in semantic to raise AssertionError.
    Assert {
        test: Expr,
        msg: Option<Expr>,
    },
    /// `try` / `except` / `else` / `finally`.
    /// `orelse` is only valid when there is at least one `except` (CPython).
    Try {
        body: Vec<Stmt>,
        handlers: Vec<ExceptHandler>,
        orelse: Vec<Stmt>,
        finally: Vec<Stmt>,
    },
    /// `nonlocal name, ...` — assignments target an enclosing function local.
    Nonlocal(Vec<(String, Span)>),
    /// `match subject:` with `case` arms (structural pattern matching subset).
    Match {
        subject: Expr,
        cases: Vec<MatchCase>,
    },
    Pass,
    Break,
    Continue,
}

/// One `case pattern [if guard]:` arm under `match`.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchCase {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

/// Structural patterns for `match`/`case` (subset of PEP 634).
#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    /// `_`
    Wildcard,
    /// Capture name `x` (also value pattern when name is a constant — treated as capture).
    Capture(String),
    /// Integer literal that fits in i64.
    Int(i64),
    /// Integer literal with digits that exceed i64.
    IntDigits(String),
    /// String literal.
    Str(String),
    /// `True` / `False`.
    Bool(bool),
    /// `None`
    None,
    /// `p1 | p2 | ...`
    Or(Vec<Pattern>),
    /// Sequence `[a, b]` / `(a, b)`, optionally with one starred rest (`[a, *rest, b]`).
    Sequence {
        items: Vec<Pattern>,
        /// Index of a starred rest capture in `items`, if any.
        star: Option<usize>,
    },
    /// Mapping `{ "k": v, ... }` with optional `**rest` capture — string keys only.
    Mapping {
        items: Vec<(String, Pattern)>,
        /// Optional `**rest` binding (remaining key/value pairs as `dict[str, V]`).
        rest: Option<String>,
    },
    /// `pattern as name` (PEP 634 as-pattern).
    As { pattern: Box<Pattern>, name: String },
}

/// One `except` clause under a `try`.
#[derive(Debug, Clone, PartialEq)]
pub struct ExceptHandler {
    /// `None` = bare `except:`. One or more types for `except E:` / `except (A, B):`.
    pub exc: Option<Vec<ExcType>>,
    /// Optional `as name` binding (exception **object** at runtime).
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
    /// `is` — identity; only `is None` / `is not None` in semantic for now
    Is,
    /// `is not`
    IsNot,
    And,
    Or,
    /// Bitwise AND `&` (ints / bools).
    BitAnd,
    /// Bitwise OR `|` (ints / bools). Distinct from type-union `|` by context.
    BitOr,
    /// Bitwise XOR `^`.
    BitXor,
    /// Left shift `<<`.
    LShift,
    /// Right shift `>>` (arithmetic on signed i64).
    RShift,
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
            BinOp::Is => "is",
            BinOp::IsNot => "is not",
            BinOp::And => "and",
            BinOp::Or => "or",
            BinOp::BitAnd => "&",
            BinOp::BitOr => "|",
            BinOp::BitXor => "^",
            BinOp::LShift => "<<",
            BinOp::RShift => ">>",
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
    /// `~x` bitwise invert (ints / bools as int).
    Invert,
}

/// f-string conversion `!s` / `!r` / `!a` (applied before formatting).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FStringConversion {
    /// `!s` — `str(value)`
    Str,
    /// `!r` — `repr(value)`
    Repr,
    /// `!a` — `ascii(value)`
    Ascii,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FStringPart {
    Literal(String),
    Expr {
        expr: Expr,
        conversion: Option<FStringConversion>,
        /// Format mini-language after `:`, typically a [`ExprKind::JoinedStr`]
        /// of literal chunks and nested `{field}` expressions. `None` when no
        /// `:` was present (plain `str()` conversion path).
        format_spec: Option<Box<Expr>>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    /// Integer literal that fits in i64 (after parsing).
    Int(i64),
    /// Integer literal whose decimal digits exceed i64 (no underscores).
    IntDigits(String),
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
    /// `[a, b, c]` — elements may include `*starred` (see [`ListElem`]).
    ListLit(Vec<ListElem>),
    /// `(a, b)`, `(a,)`, `()` — also bare `a, b` in assign/return contexts.
    TupleLit(Vec<Expr>),
    /// `{k: v, ...}` — values may include `**` unpack later; keys plain for now.
    DictLit(Vec<(Expr, Expr)>),
    /// `{a, b, ...}` — nonempty; empty set is `set()`.
    SetLit(Vec<Expr>),
    /// `lambda params: expr` — expression body only.
    Lambda {
        params: Vec<Param>,
        body: Box<Expr>,
    },
    /// `yield value` / bare `yield` (None). Only valid inside functions.
    Yield(Option<Box<Expr>>),
    /// `yield from iterable`.
    YieldFrom(Box<Expr>),
    /// `*expr` — only legal inside list displays and unpack targets (validated
    /// in semantic / when converting to assign targets).
    Starred(Box<Expr>),
    /// `[elem for target in iter if cond ... for ...]` — one or more generators.
    ListComp {
        elem: Box<Expr>,
        generators: Vec<CompFor>,
    },
    /// `{k: v for ...}` dict comprehension.
    DictComp {
        key: Box<Expr>,
        value: Box<Expr>,
        generators: Vec<CompFor>,
    },
    /// `{x for ...}` set comprehension.
    SetComp {
        elem: Box<Expr>,
        generators: Vec<CompFor>,
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

/// One `for target in iter [if ...]...` clause in a comprehension.
#[derive(Debug, Clone, PartialEq)]
pub struct CompFor {
    pub target: AssignTarget,
    pub iter: Expr,
    pub ifs: Vec<Expr>,
}

/// One element of a list display: a value or `*iterable` unpack.
#[derive(Debug, Clone, PartialEq)]
pub enum ListElem {
    Item(Expr),
    Star(Expr),
}
