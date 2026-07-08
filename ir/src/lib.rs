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
///
/// `List` holds a `&'static` element type (interned via [`list_of`]) so
/// `Ty` stays `Copy` while types nest arbitrarily (`list[list[int]]`).
/// The tiny leaked allocations live for the compiler process — fine for a
/// batch compiler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    /// 64-bit signed integer
    Int,
    /// 64-bit IEEE-754 float
    Float,
    Bool,
    /// Immutable, heap-allocated, length-prefixed string
    Str,
    /// Growable list; elements may themselves be lists
    List(&'static Ty),
    /// Absence of a value: `None` returns / bare functions
    None,
}

/// Intern a list type: `list_of(Ty::Int)` is `list[int]`.
pub fn list_of(elem: Ty) -> Ty {
    Ty::List(Box::leak(Box::new(elem)))
}

impl std::fmt::Display for Ty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ty::Int => write!(f, "int"),
            Ty::Float => write!(f, "float"),
            Ty::Bool => write!(f, "bool"),
            Ty::Str => write!(f, "str"),
            Ty::List(e) => write!(f, "list[{e}]"),
            Ty::None => write!(f, "None"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Module {
    pub funcs: Vec<Function>,
    /// Module-level variables (assigned by top-level statements, readable
    /// from any function, writable with `global`). Zero/null-initialized.
    pub globals: Vec<(String, Ty)>,
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
    /// Store into a module-level global.
    GlobalAssign {
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
    /// Load a module-level global.
    GlobalLoad(String),
    /// `input()` / `input(prompt)`: read a line from stdin → str.
    Input {
        prompt: Option<Box<Expr>>,
    },
    /// `sys.argv` → list[str] (requires `import sys`).
    Argv,
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
    /// `base[lo:hi:step]`. Missing bounds are encoded as i64::MIN (their
    /// meaning depends on the step's sign); the runtime resolves and
    /// clamps exactly like CPython's PySlice_AdjustIndices. A missing
    /// step is ConstInt(1); step 0 traps.
    Slice {
        base: Box<Expr>,
        lo: Box<Expr>,
        hi: Box<Expr>,
        step: Box<Expr>,
    },
    /// A `str` method call (`base` is the first argument).
    StrCall {
        func: StrFn,
        args: Vec<Expr>,
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

/// String methods implemented by the C runtime. ASCII-only case and
/// whitespace handling (documented deviation from Python's Unicode rules).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrFn {
    /// `s.upper()` → str
    Upper,
    /// `s.lower()` → str
    Lower,
    /// `s.strip()` / `s.lstrip()` / `s.rstrip()` → str
    Strip,
    Lstrip,
    Rstrip,
    /// `s.startswith(t)` / `s.endswith(t)` → bool
    StartsWith,
    EndsWith,
    /// `s.find(t)` → int (-1 when absent)
    Find,
    /// `s.count(t)` → int (non-overlapping)
    Count,
    /// `s.replace(old, new)` → str
    Replace,
    /// `s.split()` → list[str] (whitespace runs, no empty parts)
    SplitWs,
    /// `s.split(sep)` → list[str] (empty parts kept; sep must be nonempty)
    Split,
    /// `sep.join(parts)` → str
    Join,
}
