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
    /// First-class exception instance (`except E as e`). Heap object with a
    /// fixed exception type tag and message; never freed. Not a user class.
    Exception,
    /// User-defined class instance. `ClassId` indexes [`Module::classes`].
    /// Layout: header `i64 type_id` then fixed fields (never freed; GC-ready).
    Class(ClassId),
    /// Bound method value (`obj.method` without call). Heap `{ ptr object }`;
    /// call supplies user args after the captured self. `func` is the IR
    /// method name for static dispatch (virtual when `virtual` is true —
    /// codegen switches on the object's type_id).
    BoundMethod {
        class_id: ClassId,
        params: &'static [Ty],
        ret: &'static Ty,
        func: &'static str,
        is_virtual: bool,
    },
    /// Limited dynamic value (annotation `Any`). LLVM `i64` holding a heap
    /// box `{ i32 print_tag, i64 payload }` — the same encoding as union
    /// elements in containers. Not full CPython dynamism: no open attrs,
    /// method calls on bare Any are limited; coerce to concrete types with
    /// a runtime TypeError check when the tag does not match.
    Any,
}

/// Intern a bound-method type.
pub fn bound_method_of(
    class_id: ClassId,
    params: &[Ty],
    ret: Ty,
    func: &str,
    virtual_dispatch: bool,
) -> Ty {
    Ty::BoundMethod {
        class_id,
        params: Box::leak(params.to_vec().into_boxed_slice()),
        ret: Box::leak(Box::new(ret)),
        func: Box::leak(func.to_string().into_boxed_str()),
        is_virtual: virtual_dispatch,
    }
}

/// Stable id for a user class in a compiled module (index into `Module::classes`).
pub type ClassId = u32;

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
/// Tuple < Dict < Set < File < Closure < Cell < Generator < Exception < Class
/// < Any. Nested containers compare recursively. Unions should not nest
/// (flatten first).
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
            Ty::Exception => 14,
            Ty::Class(_) => 15,
            Ty::BoundMethod { .. } => 16,
            Ty::Any => 17,
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
        (Ty::Class(a), Ty::Class(b)) => a.cmp(b),
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
            Ty::Exception => write!(f, "exception"),
            Ty::Class(id) => write!(f, "class#{id}"),
            Ty::BoundMethod { params, ret, .. } => {
                write!(f, "bound_method[(")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{p}")?;
                }
                write!(f, ") -> {ret}]")
            }
            Ty::Any => write!(f, "Any"),
        }
    }
}

/// Compile-time class layout and method table (closed world).
#[derive(Debug, Clone, PartialEq)]
pub struct ClassInfo {
    pub id: ClassId,
    /// Display / debug name (e.g. `Point` or `pkg.Point`).
    pub name: String,
    /// Single base class, if any.
    pub parent: Option<ClassId>,
    /// Instance fields in layout order (parent fields first, then own).
    pub fields: Vec<(String, Ty)>,
    /// Method name → fully-qualified IR function (most specific implementation).
    pub methods: Vec<(String, String)>,
}

/// Exception type tags matching the C runtime (`pyrs_raise` / handlers).
/// Matching uses CPython-like subclass checks for `Exception` and `OSError`
/// (see `ExcType::matches_raised` / `pyrs_exc_matches`). OTHER=99 is catchable
/// by bare `except:` and by `except Exception:`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExcType {
    ValueError = 1,
    KeyError = 2,
    IndexError = 3,
    ZeroDivisionError = 4,
    TypeError = 5,
    RuntimeError = 6,
    /// Injected by `generator.close()` (CPython BaseException subclass only —
    /// not under Exception).
    GeneratorExit = 7,
    OverflowError = 8,
    EOFError = 9,
    FileNotFoundError = 10,
    OSError = 11,
    NameError = 12,
    UnboundLocalError = 13,
    /// User-level catch; generator protocol still uses exhaustion as Optional.
    StopIteration = 14,
    /// Base of the Exception hierarchy (not GeneratorExit).
    Exception = 15,
    PermissionError = 16,
    IsADirectoryError = 17,
    AssertionError = 18,
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

    pub fn tag(self) -> i32 {
        self as i32
    }

    /// Whether a handler filtering on `self` catches a raised exception of
    /// type `raised` (CPython subclass rules for Exception / OSError).
    pub fn matches_raised(self, raised: ExcType) -> bool {
        if self == raised {
            return true;
        }
        match self {
            ExcType::Exception => raised != ExcType::GeneratorExit,
            ExcType::OSError => matches!(
                raised,
                ExcType::FileNotFoundError | ExcType::PermissionError | ExcType::IsADirectoryError
            ),
            _ => false,
        }
    }

    pub fn all_names() -> &'static str {
        "ValueError, KeyError, IndexError, ZeroDivisionError, TypeError, \
         RuntimeError, GeneratorExit, OverflowError, EOFError, FileNotFoundError, \
         OSError, NameError, UnboundLocalError, StopIteration, Exception, \
         PermissionError, IsADirectoryError, AssertionError"
    }
}

/// One `except` clause under a try: filter types (or bare), optional bind name,
/// body. Multi-type `except (A, B):` is a non-empty filter list.
pub type ExceptHandler = (Option<Vec<ExcType>>, Option<String>, Vec<Stmt>);

#[derive(Debug, Clone, PartialEq)]
pub struct Module {
    pub funcs: Vec<Function>,
    /// Module-level variables (assigned by top-level statements, readable
    /// from any function, writable with `global`). Zero/null-initialized.
    pub globals: Vec<(String, Ty)>,
    /// User class layouts / method tables (indexed by [`ClassId`]).
    pub classes: Vec<ClassInfo>,
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
    /// `list.extend(other)` — append all elements of another list (same elem ty).
    ListExtend {
        list: Expr,
        other: Expr,
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
    /// `set |= other` / in-place union; same elem type.
    SetUpdate {
        set: Expr,
        other: Expr,
    },
    /// `dict.update(other)` — merge keys from `other` (same K/V types).
    DictUpdate {
        dict: Expr,
        other: Expr,
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
    /// try / except / else / finally. See [`ExceptHandler`].
    /// `orelse` runs only on normal completion of `body` (not after a handled
    /// exception).
    Try {
        body: Vec<Stmt>,
        handlers: Vec<ExceptHandler>,
        orelse: Vec<Stmt>,
        finally: Vec<Stmt>,
    },
    /// Store `value` into a heap cell (nonlocal / mutable capture).
    CellStore {
        cell: Expr,
        value: Expr,
    },
    /// `obj.field = value` for a known instance field (layout index).
    SetField {
        object: Expr,
        class_id: ClassId,
        field_index: u32,
        value: Expr,
    },
    /// `yield value` inside a generator function — suspend and produce value.
    Yield(Expr),
    /// `gen.close()` — inject GeneratorExit, run finally, mark done.
    GenClose {
        generator: Expr,
    },
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
    /// Decimal digits of a source int literal that does not fit in i64.
    ConstIntDigits(String),
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
    /// Box a concrete (or union) value into [`Ty::Any`] (heap print_tag + payload).
    ToAny {
        value: Box<Expr>,
    },
    /// Unbox [`Ty::Any`] to a concrete type (`expr.ty`) with a runtime tag check
    /// (TypeError on mismatch). Class targets accept subclasses via type_id.
    FromAny {
        value: Box<Expr>,
    },
    /// `value is None` / `value is not None`. Result is Bool.
    IsNone {
        value: Box<Expr>,
        /// When true, this is `is not None`.
        not: bool,
    },
    /// Pointer / slot identity: `a is b` / `a is not b` for same-type values.
    IsIdentity {
        left: Box<Expr>,
        right: Box<Expr>,
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
    /// `needle in haystack`: str-in-str, element-in-list/tuple/set, key-in-dict.
    /// The needle is already coerced. Result is Bool.
    Contains {
        needle: Box<Expr>,
        haystack: Box<Expr>,
    },
    /// Runtime `isinstance(value, …)` when static fold is impossible (unions).
    /// `type_tags` are print/member tags: -1=None, 0=int, 1=float, 2=bool,
    /// 3=str, 4=any list (tag%8==4), 5=tuple, 6=dict, 7=set. When
    /// `bool_is_int` is true, tag 2 also matches an int check (CPython).
    /// When the union includes `Exception` and `exc_filters` is non-empty, the
    /// Exception member is checked via hierarchy on the payload (not print tag).
    /// When the union includes `Class` members and `class_filters` is non-empty,
    /// those members are checked via `pyrs_isinstance_class` on the payload.
    IsInstance {
        value: Box<Expr>,
        type_tags: Vec<i32>,
        bool_is_int: bool,
        /// `ExcType` tags for hierarchy match when the active union member is
        /// `Exception`. Empty if no exception filters were requested.
        exc_filters: Vec<i32>,
        /// User class ids for hierarchy match when the active member is a Class.
        class_filters: Vec<ClassId>,
    },
    /// Runtime `isinstance(exc, ExcType)` / tuple of exception types when
    /// `value` is an exception object. Filters are `ExcType` tags; matching
    /// uses the OSError / Exception hierarchy (`pyrs_exc_matches`).
    ExcIsInstance {
        value: Box<Expr>,
        filters: Vec<i32>,
    },
    /// `str(exc)` for an exception object → message body.
    ExcToStr(Box<Expr>),
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
    /// `a | b` / `set.union(other)` — new set; same elem type.
    SetUnion {
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// `a & b` / `set.intersection(other)`.
    SetIntersect {
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// `a - b` / `set.difference(other)`.
    SetDiff {
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// `a ^ b` / `set.symmetric_difference(other)`.
    SetSymDiff {
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// Shallow copy of a list (`list.copy()` / `list(xs)` when xs is a list).
    ListCopy(Box<Expr>),
    /// `list(s)` for a str → list of 1-char strings.
    ListFromStr(Box<Expr>),
    /// Shallow copy of a dict (`dict.copy()`).
    DictCopy(Box<Expr>),
    /// `set(xs)` from a homogeneous list of int/str.
    SetFromList {
        list: Box<Expr>,
        /// Element type tag for runtime.
        elem: Box<Ty>,
    },
    /// `set(s)` from a str → set of 1-char strings.
    SetFromStr(Box<Expr>),
    /// `dict(pairs)` where pairs is `list[tuple[K, V]]`.
    DictFromPairs {
        pairs: Box<Expr>,
        key: Box<Ty>,
        value: Box<Ty>,
    },
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
    /// `repr(s)` for a string value (quotes + escapes); result is `Str`.
    StrRepr(Box<Expr>),
    /// `ascii(s)` for a string value (like repr, non-ASCII → `\xHH` / `\uXXXX`); result is `Str`.
    StrAscii(Box<Expr>),
    /// `format(value, spec)` — free-form format mini-language; `spec` is `Str`.
    /// `value` is Int / Float / Bool / Str. Empty `spec` matches `str(value)`.
    FormatValue {
        value: Box<Expr>,
        spec: Box<Expr>,
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
    /// Load capture slot `index` from a closure env (for MakeGenerator from
    /// an escaped generator function value).
    ClosureCap {
        closure: Box<Expr>,
        index: i64,
        /// Type of the capture slot (cell ptr or by-value payload).
        cap_ty: Ty,
    },
    /// Allocate a cell and store the initial value.
    CellNew(Box<Expr>),
    /// Allocate an unbound cell (load traps until the first store). Used for
    /// late free-var capture before the outer assignment runs.
    CellNewUnbound,
    /// Load the value from a cell.
    CellLoad(Box<Expr>),
    /// Create a generator object by calling a generator function's constructor.
    /// `func` is the resume IR function; when empty, `code_from` supplies a
    /// closure whose code pointer is the resume function (container of gens).
    /// `args` are frame locals (captures then call args). Yield type is on `expr.ty`.
    MakeGenerator {
        func: String,
        /// When `func` is empty, load `@code` via `pyrs_closure_code` on this value.
        code_from: Option<Box<Expr>>,
        args: Vec<Expr>,
        /// Total frame slots (params + locals + temps). Over-estimate is fine.
        nlocals: i64,
    },
    /// Advance a generator: returns a union `yield_ty | None` where None means
    /// StopIteration (exhausted). Used by `for` desugaring and `send`.
    /// `send` is delivered as the value of the suspended `yield` expression
    /// (`None` / `ConstNone` for `next` / `send(None)`).
    GeneratorNext {
        generator: Box<Expr>,
        send: Box<Expr>,
    },
    /// Inject an exception at the suspended yield point (`g.throw(...)`).
    /// Returns `yield_ty | None` like `GeneratorNext` when the generator
    /// catches and yields again; uncaught exceptions propagate via the runtime.
    GeneratorThrow {
        generator: Box<Expr>,
        exc: ExcType,
        message: Box<Expr>,
    },
    /// Value delivered to a `yield` expression after resume (`send` / `next`).
    /// Type on `expr.ty` is `Optional[yield_ty]`. Valid only as the result of
    /// a Block that executed `Yield` in a generator resume function.
    GenSentValue,
    /// Load the StopIteration / `return` payload of a finished generator.
    /// Type on `expr.ty` is `Optional[Y]`: None when the subgen used bare
    /// `return` / fell off the end; `Some(v)` after `return v`.
    GeneratorReturnValue(Box<Expr>),
    /// Allocate a zeroed instance of `class_id` (header type_id + fields).
    /// Does not call `__init__` — semantic wraps that separately.
    NewObject {
        class_id: ClassId,
    },
    /// Load instance field at layout index (0-based into `ClassInfo::fields`).
    GetField {
        object: Box<Expr>,
        class_id: ClassId,
        field_index: u32,
    },
    /// Load a field that exists only on a subset of possible runtime classes
    /// (e.g. after `isinstance(x, (B, C))` and `x.b` where only `B` has `b`).
    /// Codegen switches on `type_id`: matching candidates GEP-load; others
    /// raise AttributeError. `candidates` are `(class_id, field_index)` pairs
    /// for every closed-world concrete class that has the field.
    GetFieldPartial {
        object: Box<Expr>,
        candidates: Vec<(ClassId, u32)>,
        /// Attribute name for AttributeError messages.
        attr: String,
    },
    /// Instance method call. When `virtual` is true, dispatch on runtime
    /// type_id among `candidates` (pairs of (type_id, ir_func)); otherwise
    /// call `direct_func` statically. `args` is self then user args.
    CallMethod {
        /// Fully-qualified IR function when not virtual.
        direct_func: String,
        /// (class_id, ir_func) for each concrete class that may appear;
        /// empty when not virtual.
        candidates: Vec<(ClassId, String)>,
        args: Vec<Expr>,
        /// When true, load type_id from `args[0]` and switch.
        virtual_dispatch: bool,
    },
    /// `obj.method` as a first-class bound-method value.
    BindMethod {
        object: Box<Expr>,
        class_id: ClassId,
        method: String,
        direct_func: String,
        candidates: Vec<(ClassId, String)>,
        virtual_dispatch: bool,
    },
    /// Call a bound-method value with user args (self from the binding).
    CallBoundMethod {
        bound: Box<Expr>,
        args: Vec<Expr>,
        /// IR metadata mirrored from the bound type for codegen.
        direct_func: String,
        candidates: Vec<(ClassId, String)>,
        virtual_dispatch: bool,
    },
    /// `isinstance(obj, Class)` with inheritance: walk parent chain.
    ClassIsInstance {
        value: Box<Expr>,
        /// Target class id (True if value's type is this or a subclass).
        class_id: ClassId,
    },
    /// Default `str(obj)` for an instance: runtime type_id → `"<Name object>"`.
    ObjectToStr(Box<Expr>),
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

#[cfg(test)]
mod tests {
    use super::ExcType;

    #[test]
    fn matches_raised_equality() {
        assert!(ExcType::ValueError.matches_raised(ExcType::ValueError));
        assert!(!ExcType::ValueError.matches_raised(ExcType::KeyError));
        assert!(ExcType::OSError.matches_raised(ExcType::OSError));
        assert!(ExcType::Exception.matches_raised(ExcType::Exception));
    }

    #[test]
    fn matches_raised_oserror_hierarchy() {
        assert!(ExcType::OSError.matches_raised(ExcType::FileNotFoundError));
        assert!(ExcType::OSError.matches_raised(ExcType::PermissionError));
        assert!(ExcType::OSError.matches_raised(ExcType::IsADirectoryError));
        assert!(!ExcType::OSError.matches_raised(ExcType::ValueError));
        assert!(!ExcType::OSError.matches_raised(ExcType::Exception));
        assert!(!ExcType::FileNotFoundError.matches_raised(ExcType::OSError));
    }

    #[test]
    fn matches_raised_exception_base() {
        assert!(ExcType::Exception.matches_raised(ExcType::ValueError));
        assert!(ExcType::Exception.matches_raised(ExcType::OSError));
        assert!(ExcType::Exception.matches_raised(ExcType::FileNotFoundError));
        assert!(ExcType::Exception.matches_raised(ExcType::StopIteration));
        // GeneratorExit is BaseException-only in CPython.
        assert!(!ExcType::Exception.matches_raised(ExcType::GeneratorExit));
        assert!(ExcType::GeneratorExit.matches_raised(ExcType::GeneratorExit));
        assert!(!ExcType::GeneratorExit.matches_raised(ExcType::Exception));
    }
}
