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
/// Container variants hold interned pieces so `Ty` stays `Copy` while types
/// nest (`list[list[int]]`, `tuple[int, str]`, `dict[str, list[int]]`).
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
    /// Fixed-arity heterogeneous tuple
    Tuple(&'static [Ty]),
    /// Hash map; key is Int or Str
    Dict {
        key: &'static Ty,
        value: &'static Ty,
    },
    /// Hash set; element is Int or Str
    Set(&'static Ty),
    /// An open file handle from `open(...)`
    File,
    /// The `None` value type (first-class). Function `-> None` still lowers
    /// to LLVM `void`; expression-level None is a real value (`i8` 0).
    None,
    /// Tagged union of at least two distinct members (flattened, sorted,
    /// interned). Lowers to LLVM `{ i32, i64 }` (tag + payload slot).
    Union(&'static [Ty]),
    /// First-class nested function / lambda. Points at a heap closure
    /// `{ code_ptr, env }`. `params` are the user-facing parameters
    /// (captures live in the env as leading IR params); `ret` is the return
    /// type. `capture_tys` / `func` identify the nested IR function so
    /// CallClosure can unpack the env and invoke it with the right types.
    Closure {
        params: &'static [Ty],
        ret: &'static Ty,
        capture_tys: &'static [Ty],
        /// Fully-qualified IR function name (empty for unknown).
        func: &'static str,
    },
    /// Heap cell (box) holding a single value of `inner` for `nonlocal` /
    /// mutable free-variable capture. Represented as `ptr` in LLVM.
    Cell(&'static Ty),
    /// Generator / iterator object produced by calling a generator function.
    /// Yields values of type `yield_ty`. Represented as `ptr` in LLVM.
    Generator {
        yield_ty: &'static Ty,
    },
}

/// Intern a list type: `list_of(Ty::Int)` is `list[int]`.
pub fn list_of(elem: Ty) -> Ty {
    Ty::List(Box::leak(Box::new(elem)))
}

/// Intern a tuple type from element types.
pub fn tuple_of(elems: &[Ty]) -> Ty {
    Ty::Tuple(Box::leak(elems.to_vec().into_boxed_slice()))
}

/// Intern a dict type.
pub fn dict_of(key: Ty, value: Ty) -> Ty {
    Ty::Dict {
        key: Box::leak(Box::new(key)),
        value: Box::leak(Box::new(value)),
    }
}

/// Intern a set type.
pub fn set_of(elem: Ty) -> Ty {
    Ty::Set(Box::leak(Box::new(elem)))
}

/// Total order for union members: None < Bool < Int < Float < Str < List <
/// Tuple < Dict < Set < File < Closure < Cell < Generator. Nested containers
/// compare recursively. Unions should not nest (flatten first).
pub fn ty_cmp(a: &Ty, b: &Ty) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    fn rank(t: &Ty) -> u8 {
        match t {
            Ty::None => 0,
            Ty::Bool => 1,
            Ty::Int => 2,
            Ty::Float => 3,
            Ty::Str => 4,
            Ty::List(_) => 5,
            Ty::Tuple(_) => 6,
            Ty::Dict { .. } => 7,
            Ty::Set(_) => 8,
            Ty::File => 9,
            Ty::Union(_) => 10,
            Ty::Closure { .. } => 11,
            Ty::Cell(_) => 12,
            Ty::Generator { .. } => 13,
        }
    }
    match (a, b) {
        (Ty::List(x), Ty::List(y)) => ty_cmp(x, y),
        (Ty::Set(x), Ty::Set(y)) => ty_cmp(x, y),
        (Ty::Cell(x), Ty::Cell(y)) => ty_cmp(x, y),
        (Ty::Dict { key: k1, value: v1 }, Ty::Dict { key: k2, value: v2 }) => {
            ty_cmp(k1, k2).then_with(|| ty_cmp(v1, v2))
        }
        (Ty::Tuple(x), Ty::Tuple(y)) => {
            for (ex, ey) in x.iter().zip(y.iter()) {
                let c = ty_cmp(ex, ey);
                if c != Ordering::Equal {
                    return c;
                }
            }
            x.len().cmp(&y.len())
        }
        (Ty::Union(x), Ty::Union(y)) => {
            for (ex, ey) in x.iter().zip(y.iter()) {
                let c = ty_cmp(ex, ey);
                if c != Ordering::Equal {
                    return c;
                }
            }
            x.len().cmp(&y.len())
        }
        (
            Ty::Closure {
                params: p1,
                ret: r1,
                capture_tys: c1,
                func: f1,
            },
            Ty::Closure {
                params: p2,
                ret: r2,
                capture_tys: c2,
                func: f2,
            },
        ) => {
            let c = p1.len().cmp(&p2.len());
            if c != Ordering::Equal {
                return c;
            }
            for (a, b) in p1.iter().zip(p2.iter()) {
                let c = ty_cmp(a, b);
                if c != Ordering::Equal {
                    return c;
                }
            }
            let c = ty_cmp(r1, r2);
            if c != Ordering::Equal {
                return c;
            }
            let c = c1.len().cmp(&c2.len());
            if c != Ordering::Equal {
                return c;
            }
            for (a, b) in c1.iter().zip(c2.iter()) {
                let c = ty_cmp(a, b);
                if c != Ordering::Equal {
                    return c;
                }
            }
            f1.cmp(f2)
        }
        (Ty::Generator { yield_ty: a }, Ty::Generator { yield_ty: b }) => ty_cmp(a, b),
        _ => rank(a).cmp(&rank(b)),
    }
}

/// Intern a closure type.
pub fn closure_of(params: &[Ty], ret: Ty) -> Ty {
    closure_of_full(params, ret, &[], "")
}

/// Intern a closure type with capture metadata for CallClosure.
pub fn closure_of_full(params: &[Ty], ret: Ty, capture_tys: &[Ty], func: &str) -> Ty {
    Ty::Closure {
        params: Box::leak(params.to_vec().into_boxed_slice()),
        ret: Box::leak(Box::new(ret)),
        capture_tys: Box::leak(capture_tys.to_vec().into_boxed_slice()),
        func: Box::leak(func.to_string().into_boxed_str()),
    }
}

/// Intern a cell type.
pub fn cell_of(inner: Ty) -> Ty {
    Ty::Cell(Box::leak(Box::new(inner)))
}

/// Intern a generator type.
pub fn generator_of(yield_ty: Ty) -> Ty {
    Ty::Generator {
        yield_ty: Box::leak(Box::new(yield_ty)),
    }
}

/// Flatten nested unions into a flat list of atomic members.
pub fn flatten_union_members(ty: Ty) -> Vec<Ty> {
    match ty {
        Ty::Union(ms) => {
            let mut out = Vec::new();
            for m in ms {
                out.extend(flatten_union_members(*m));
            }
            out
        }
        other => vec![other],
    }
}

/// Build a union type from members: flatten, dedup, sort, intern.
/// Returns a single type if only one unique member remains.
/// Panics if `members` is empty after flatten (callers must not pass empty).
pub fn union_of(members: &[Ty]) -> Ty {
    let mut flat = Vec::new();
    for m in members {
        flat.extend(flatten_union_members(*m));
    }
    // dedup then sort (stable total order)
    let mut unique: Vec<Ty> = Vec::new();
    for m in flat {
        if !unique.contains(&m) {
            unique.push(m);
        }
    }
    unique.sort_by(ty_cmp);
    match unique.len() {
        0 => panic!("union_of: empty member list"),
        1 => unique[0],
        _ => Ty::Union(Box::leak(unique.into_boxed_slice())),
    }
}

/// `T | None` (Optional[T]).
pub fn optional_of(t: Ty) -> Ty {
    union_of(&[t, Ty::None])
}

/// Index of `member` in a union's sorted member list, if present.
pub fn union_member_index(union: Ty, member: Ty) -> Option<usize> {
    match union {
        Ty::Union(ms) => ms.iter().position(|m| *m == member),
        _ => None,
    }
}

/// Whether `ty` is a union that includes `None`.
pub fn is_optional(ty: Ty) -> bool {
    match ty {
        Ty::None => true,
        Ty::Union(ms) => ms.contains(&Ty::None),
        _ => false,
    }
}

impl std::fmt::Display for Ty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ty::Int => write!(f, "int"),
            Ty::Float => write!(f, "float"),
            Ty::Bool => write!(f, "bool"),
            Ty::Str => write!(f, "str"),
            Ty::List(e) => write!(f, "list[{e}]"),
            Ty::Tuple(elems) => {
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
            Ty::Dict { key, value } => write!(f, "dict[{key}, {value}]"),
            Ty::Set(e) => write!(f, "set[{e}]"),
            Ty::File => write!(f, "file"),
            Ty::None => write!(f, "None"),
            Ty::Union(ms) => {
                for (i, m) in ms.iter().enumerate() {
                    if i > 0 {
                        write!(f, " | ")?;
                    }
                    write!(f, "{m}")?;
                }
                Ok(())
            }
            Ty::Closure { params, ret, .. } => {
                write!(f, "closure[(")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{p}")?;
                }
                write!(f, ") -> {ret}]")
            }
            Ty::Cell(inner) => write!(f, "cell[{inner}]"),
            Ty::Generator { yield_ty } => write!(f, "generator[{yield_ty}]"),
        }
    }
}

/// Exception type tags matching the C runtime (`pyrs_raise` / handlers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExcType {
    ValueError = 1,
    KeyError = 2,
    IndexError = 3,
    ZeroDivisionError = 4,
    TypeError = 5,
    RuntimeError = 6,
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

    pub fn tag(self) -> i32 {
        self as i32
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
    /// When set, this is a generator resume function: first param is the
    /// generator frame pointer (`ptr`), and `Yield` stmts suspend into it.
    /// The function returns `i1` done-flag via a side channel is not used;
    /// instead yield stores into the frame and returns a special sentinel.
    /// `None` for ordinary functions.
    pub is_generator: bool,
    /// Yield element type when `is_generator` (otherwise ignored).
    pub yield_ty: Option<Ty>,
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
    /// `base[index] = value` — base is a list or dict.
    IndexAssign {
        base: Expr,
        index: Expr,
        value: Expr,
    },
    /// `del base[index]` — dict only.
    IndexDelete {
        base: Expr,
        index: Expr,
    },
    /// `list.append(value)`
    ListAppend {
        list: Expr,
        value: Expr,
    },
    /// Append without a capacity check. Only emitted where the list was
    /// created with guaranteed-sufficient capacity (comprehension fast
    /// path); codegen inlines the store + length bump.
    ListAppendUnchecked {
        list: Expr,
        value: Expr,
    },
    /// `list.insert(index, value)` — index clamped like CPython.
    ListInsert {
        list: Expr,
        index: Expr,
        value: Expr,
    },
    /// `list.remove(value)` — first match; traps if missing.
    ListRemove {
        list: Expr,
        value: Expr,
    },
    /// `list.clear()`
    ListClear {
        list: Expr,
    },
    /// `list.sort()` — in-place; element type carried on `list.ty`.
    ListSort {
        list: Expr,
    },
    /// `dict.clear()`
    DictClear {
        dict: Expr,
    },
    /// `set.add(value)`
    SetAdd {
        set: Expr,
        value: Expr,
    },
    /// `set.remove(value)` — traps if missing.
    SetRemove {
        set: Expr,
        value: Expr,
    },
    /// `set.discard(value)` — no-op if missing.
    SetDiscard {
        set: Expr,
        value: Expr,
    },
    /// `set.clear()`
    SetClear {
        set: Expr,
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
    /// Abort with a runtime error message (exit code 1), or raise into a
    /// surrounding try frame when one is active.
    Die(String),
    /// Runtime length check for unpacking: `pyrs_unpack_check(len, expected)`.
    UnpackCheck {
        len: Expr,
        expected: i64,
    },
    /// Runtime length check for starred unpack: require `len >= minimum`.
    UnpackCheckMin {
        len: Expr,
        minimum: i64,
    },
    /// `raise ExcType(msg)` — msg is a str.
    Raise {
        exc: ExcType,
        message: Expr,
    },
    /// try / except / else / finally. Handlers: (type filter or catch-all,
    /// optional local name bound to the message str, body). `orelse` runs
    /// only on normal completion of `body` (not after a handled exception).
    Try {
        body: Vec<Stmt>,
        handlers: Vec<(Option<ExcType>, Option<String>, Vec<Stmt>)>,
        orelse: Vec<Stmt>,
        finally: Vec<Stmt>,
    },
    /// Store `value` into a heap cell (nonlocal / mutable capture).
    CellStore {
        cell: Expr,
        value: Expr,
    },
    /// `yield value` inside a generator function — suspend and produce value.
    Yield(Expr),
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
    /// The `None` literal. Type is always `Ty::None` (value-level; not void).
    ConstNone,
    /// Wrap a concrete value (or sub-union) into a union type.
    /// `expr.ty` is the target union; `value.ty` is a member or sub-union.
    ToUnion {
        value: Box<Expr>,
    },
    /// Extract a concrete member from a union (after narrowing). `expr.ty` is
    /// the member type; `value.ty` is the union. No runtime tag check (semantic
    /// proves the refinement).
    FromUnion {
        value: Box<Expr>,
    },
    /// `value is None` / `value is not None`. Result is Bool.
    IsNone {
        value: Box<Expr>,
        /// When true, this is `is not None`.
        not: bool,
    },
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
    /// `open(path, mode)` → file. Mode is a str ("r"/"w"/"a"), validated
    /// at compile time when constant, at runtime otherwise.
    Open {
        path: Box<Expr>,
        mode: Box<Expr>,
    },
    /// A file method call (`base` is the first argument).
    FileCall {
        func: FileFn,
        args: Vec<Expr>,
    },
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
    /// `base[index]`: str → str, list[T] → T, tuple → element, dict → value.
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
    /// `needle in haystack`: str-in-str, element-in-list/set, key-in-dict.
    /// The needle is already coerced. Result is Bool.
    Contains {
        needle: Box<Expr>,
        haystack: Box<Expr>,
    },
    /// `list.pop(index)`; index defaults to -1 (the last element).
    ListPop {
        list: Box<Expr>,
        index: Box<Expr>,
    },
    /// `list.index(value)` — first match; traps if missing. Result is `int`.
    ListIndexOf {
        list: Box<Expr>,
        value: Box<Expr>,
    },
    /// A list literal; `ty` is `List(elem)` and items are already coerced.
    ListLit(Vec<Expr>),
    /// A fresh empty list with the given capacity (comprehension results).
    ListNew {
        cap: Box<Expr>,
    },
    /// Tuple literal; `ty` is `Tuple([...])`.
    TupleLit(Vec<Expr>),
    /// Dict literal; `ty` is `Dict { key, value }`.
    DictLit(Vec<(Expr, Expr)>),
    /// Empty dict with known key/value types (from annotation or `dict()`).
    DictNew,
    /// Set literal; `ty` is `Set(elem)`.
    SetLit(Vec<Expr>),
    /// Empty set with known element type.
    SetNew,
    /// `d.get(key, default)`. Result type is on `expr.ty` (may be
    /// `optional_of(val)` for bare get). On hit the value is converted to
    /// `expr.ty` when needed; on miss the default is used as-is.
    DictGet {
        dict: Box<Expr>,
        key: Box<Expr>,
        default: Box<Expr>,
    },
    /// `d.pop(key)` / `d.pop(key, default)`.
    DictPop {
        dict: Box<Expr>,
        key: Box<Expr>,
        default: Option<Box<Expr>>,
    },
    /// `d.keys()` → list of keys.
    DictKeys(Box<Expr>),
    /// `d.values()` → list of values.
    DictValues(Box<Expr>),
    /// `d.items()` → list of `(key, value)` tuples.
    DictItems(Box<Expr>),
    /// Materialize set elements as a list (for iteration).
    SetToList(Box<Expr>),
    /// Statements evaluated for effect, then a result expression — the
    /// hook that lets loops live inside expressions (comprehensions).
    Block {
        stmts: Vec<Stmt>,
        result: Box<Expr>,
    },
    /// `len(x)` for str, list, tuple, dict, set.
    Len(Box<Expr>),
    /// `abs(x)` for int or float (bool is promoted to int first).
    /// Result type matches the operand. `abs(i64::MIN)` wraps (no bigints).
    Abs(Box<Expr>),
    /// `min(a, b)` - operands share a numeric type; on ties return the left.
    Min {
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// `max(a, b)` - operands share a numeric type; on ties return the left.
    Max {
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// `min(xs)` for `list[int|float|bool]`; empty list traps ValueError.
    MinList(Box<Expr>),
    /// `max(xs)` for `list[int|float|bool]`; empty list traps ValueError.
    MaxList(Box<Expr>),
    /// `sum(xs)` for `list[int]` or `list[float]`; empty lists yield 0 / 0.0.
    Sum(Box<Expr>),
    /// `math.<op>(x)` — float unary from the `math` stdlib module.
    MathCall {
        op: MathOp,
        arg: Box<Expr>,
    },
    /// `os.getcwd()` → str (POSIX getcwd via runtime).
    OsGetcwd,
    /// `json.dumps(x)` for a json-able value (type on the arg).
    JsonDumps(Box<Expr>),
    /// Scalar / container `json.loads_*` helpers (type is the result).
    JsonLoads {
        kind: JsonLoadsKind,
        arg: Box<Expr>,
    },
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
    /// f-string `{x:.Nf}` fixed-point format; operand is always `Float`.
    FloatFormat {
        value: Box<Expr>,
        precision: u32,
    },
    /// truthiness test → bool: numerics `!= 0`, containers `len != 0`
    ToBool(Box<Expr>),
    /// Build a heap closure for nested function / lambda `func`.
    /// `captures` are env slots (values or cell pointers) in declaration order.
    MakeClosure {
        /// Fully-qualified IR function name (mangled with module/outer).
        func: String,
        captures: Vec<Expr>,
        /// Capture is a cell pointer (true) vs by-value payload (false), parallel to captures.
        capture_is_cell: Vec<bool>,
    },
    /// Call a first-class closure value.
    CallClosure {
        closure: Box<Expr>,
        args: Vec<Expr>,
        /// Types of the leading capture parameters (env slots), in order.
        capture_tys: Vec<Ty>,
        /// Fully-qualified IR function name to call (captures + user args).
        func: String,
    },
    /// Allocate a cell and store the initial value.
    CellNew(Box<Expr>),
    /// Load the value from a cell.
    CellLoad(Box<Expr>),
    /// Create a generator object by calling a generator function's constructor.
    /// `func` is the resume IR function; `args` are the original call args
    /// (stored as generator locals). Yield type is on `expr.ty`.
    MakeGenerator {
        func: String,
        args: Vec<Expr>,
        /// Total frame slots (params + locals + temps). Over-estimate is fine.
        nlocals: i64,
    },
    /// Advance a generator: returns a union `yield_ty | None` where None means
    /// StopIteration (exhausted). Used by `for` desugaring.
    GeneratorNext(Box<Expr>),
}

/// Unary ops from the pure-PyRs `math` module (bodies replaced at lower).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MathOp {
    Sqrt,
    Sin,
    Cos,
    Tan,
    Log,
    Log10,
    Exp,
    /// Toward −∞, result type `Int` (CPython `math.floor`).
    Floor,
    /// Toward +∞, result type `Int` (CPython `math.ceil`).
    Ceil,
    Fabs,
}

impl MathOp {
    pub fn as_str(self) -> &'static str {
        match self {
            MathOp::Sqrt => "sqrt",
            MathOp::Sin => "sin",
            MathOp::Cos => "cos",
            MathOp::Tan => "tan",
            MathOp::Log => "log",
            MathOp::Log10 => "log10",
            MathOp::Exp => "exp",
            MathOp::Floor => "floor",
            MathOp::Ceil => "ceil",
            MathOp::Fabs => "fabs",
        }
    }
}

/// Typed `json.loads_*` forms (full dynamic `json.loads` is not supported).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonLoadsKind {
    Int,
    Float,
    Bool,
    Str,
    ListInt,
    ListFloat,
    ListStr,
    ListBool,
    DictStrInt,
    DictStrFloat,
    DictStrStr,
    DictStrBool,
}

impl JsonLoadsKind {
    pub fn as_str(self) -> &'static str {
        match self {
            JsonLoadsKind::Int => "loads_int",
            JsonLoadsKind::Float => "loads_float",
            JsonLoadsKind::Bool => "loads_bool",
            JsonLoadsKind::Str => "loads_str",
            JsonLoadsKind::ListInt => "loads_list_int",
            JsonLoadsKind::ListFloat => "loads_list_float",
            JsonLoadsKind::ListStr => "loads_list_str",
            JsonLoadsKind::ListBool => "loads_list_bool",
            JsonLoadsKind::DictStrInt => "loads_dict_str_int",
            JsonLoadsKind::DictStrFloat => "loads_dict_str_float",
            JsonLoadsKind::DictStrStr => "loads_dict_str_str",
            JsonLoadsKind::DictStrBool => "loads_dict_str_bool",
        }
    }
}

/// Binary operations. Operand types are encoded in the operand `Expr`s and
/// are always equal on both sides; comparison results are `Bool`.
///
/// `And`/`Or` short-circuit and yield an operand (not always `Bool`); both
/// sides share the result type after numeric unify when needed.
///
/// On `Str`/`List` operands: `Add` is concatenation, `Mul` is repetition
/// (the int count is always the right operand). Str comparisons are
/// lexicographic.
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
    /// Bitwise ops — both operands Int (bools promoted in semantic).
    BitAnd,
    BitOr,
    BitXor,
    LShift,
    RShift,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    /// Bitwise invert `~` on Int.
    Invert,
}

/// File methods implemented by the C runtime. Errors (closed file,
/// wrong mode) trap with CPython's exact messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFn {
    /// `f.read()` → str (everything remaining)
    Read,
    /// `f.readline()` → str (keeps the trailing newline; "" at EOF)
    ReadLine,
    /// `f.readlines()` → list[str]
    ReadLines,
    /// `f.write(s)` → int (characters written)
    Write,
    /// `f.close()` → None (idempotent)
    Close,
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
    /// `s.rfind(t)` → int (-1 when absent; empty needle → `len(s)`)
    RFind,
    /// `s.rindex(t)` → int (traps with ValueError when absent)
    RIndex,
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
    /// `s.isdigit()` -> bool (ASCII digits)
    IsDigit,
    /// `s.isalpha()` -> bool (ASCII letters)
    IsAlpha,
    /// `s.isspace()` -> bool (ASCII whitespace; same set as strip/split)
    IsSpace,
    /// `s.isupper()` -> bool (ASCII: >=1 letter, all letters upper)
    IsUpper,
    /// `s.islower()` -> bool (ASCII: >=1 letter, all letters lower)
    IsLower,
}
