//! Lowers the typed IR to LLVM IR in its textual format.
//!
//! The textual IR is the serialization boundary handed to the C++ shim,
//! which parses, verifies, optimizes and emits native object code.
//!
//! Semantics preserved from Python:
//! - `//` and `%` use floored division (result sign follows the divisor)
//! - division by zero traps with a ZeroDivisionError message instead of UB
//! - `int` values are tagged i64 (small) or heap bigints via the runtime
//! - `int(float)` / arithmetic / shifts go through runtime helpers
//! - `**` on ints uses runtime pow (negative exponent traps);
//!   `0.0 ** negative` traps like Python instead of returning inf
//!
//! Strings are length-prefixed `{ i64, [n x i8] }` blobs; lists are
//! `{ len, cap, data }` headers with 8-byte value slots. Both live behind
//! `ptr` and are managed by the C runtime (allocated, never freed).
//!
//! All user symbols are prefixed `pyrs_` so they can never collide with
//! libc symbols (a user function named `printf` is fine).

use std::collections::HashMap;
use std::fmt::Write;

use ir::{
    BinOp, ClassInfo, Expr, ExprKind, FileFn, Function, JsonLoadsKind, MathOp, Module, Stmt, StrFn,
    Ty, UnOp,
};

pub fn emit_llvm_ir(module: &Module) -> String {
    let mut e = Emitter::default();
    e.emit_module(module);
    e.finish()
}

/// LLVM type for a value of `ty` (locals, params, SSA). Pure `None` is `i8`;
/// unions are `{ i32, i64 }`. Function returns of `None` use [`lty_ret`].
fn lty(ty: Ty) -> &'static str {
    match ty {
        Ty::Int | Ty::Any => "i64",
        Ty::Float => "double",
        Ty::Bool => "i1",
        Ty::Str => "ptr",
        Ty::List(_)
        | Ty::Tuple(_)
        | Ty::Dict { .. }
        | Ty::Set(_)
        | Ty::File
        | Ty::Closure { .. }
        | Ty::Cell(_)
        | Ty::Generator { .. }
        | Ty::Exception
        | Ty::Class(_)
        | Ty::BoundMethod { .. } => "ptr",
        // Value-level None (expression); not the void function return.
        Ty::None => "i8",
        Ty::Union(_) => "{ i32, i64 }",
    }
}

/// LLVM return type: `-> None` stays `void`; everything else matches [`lty`].
fn lty_ret(ty: Ty) -> &'static str {
    match ty {
        Ty::None => "void",
        other => lty(other),
    }
}

fn mangle(name: &str) -> String {
    format!("pyrs_{name}")
}

/// LLVM hexadecimal float syntax: the raw IEEE-754 bits of the double.
fn fconst(v: f64) -> String {
    format!("0x{:016X}", v.to_bits())
}

/// Tag values understood by the runtime's print/contains helpers.
/// Scalars 0-3; nested list `4 + 8 * inner`; tuple=5, dict=6, set=7;
/// union (heap box) = 8. None is not stored as a bare slot tag (only inside
/// a union box as print_tag = -1).
fn elem_tag(ty: &Ty) -> u32 {
    match ty {
        Ty::Int => 0,
        Ty::Float => 1,
        Ty::Bool => 2,
        Ty::Str => 3,
        Ty::List(inner) => 4 + 8 * elem_tag(inner),
        Ty::Tuple(_) => 5,
        Ty::Dict { .. } => 6,
        Ty::Set(_) => 7,
        // Any is always a heap box {print_tag, payload}; reuse union list tag.
        Ty::Union(_) | Ty::Any => 8,
        // Closures / generators / bound methods as list/dict values (pointer slots).
        Ty::Closure { .. } | Ty::BoundMethod { .. } => 9,
        Ty::Generator { .. } => 10,
        // Exception objects: union-box print tag only (not list/dict elements).
        Ty::Exception => 11,
        // User class instances: per-class tag so multi-class unions in containers
        // do not share one print_tag (switch cases must be unique).
        // Encoding 13 + 8*class_id avoids collision with list tags (4+8*k).
        Ty::Class(id) => 13 + 8 * id,
        Ty::File | Ty::None | Ty::Cell(_) => {
            unreachable!("no print tag for {ty:?}")
        }
    }
}

/// LLVM struct type for a class instance: `{ i64 type_id, field0, field1, ... }`.
fn class_struct_ty(info: &ClassInfo) -> String {
    let mut s = String::from("{ i64");
    for (_, ty) in &info.fields {
        s.push_str(", ");
        s.push_str(lty(*ty));
    }
    s.push_str(" }");
    s
}

/// Print tag stored inside a heap union box for the active member (-1 = None).
fn member_print_tag(ty: Ty) -> i32 {
    match ty {
        Ty::None => -1,
        other => elem_tag(&other) as i32,
    }
}

fn escape_bytes(s: &str) -> (String, usize) {
    let mut out = String::new();
    let bytes = s.as_bytes();
    for &b in bytes {
        if (0x20..0x7f).contains(&b) && b != b'"' && b != b'\\' {
            out.push(b as char);
        } else {
            write!(out, "\\{b:02X}").unwrap();
        }
    }
    out.push_str("\\00");
    (out, bytes.len() + 1)
}

/// Exit kind stored in a try scope's `exit_ptr` alloca (i32).
const TRY_EXIT_NORMAL: i32 = 0;
const TRY_EXIT_RETURN: i32 = 1;
const TRY_EXIT_BREAK: i32 = 2;
const TRY_EXIT_CONTINUE: i32 = 3;
const TRY_EXIT_RERAISE: i32 = 4;

/// One enclosing `try` while emitting; used to route return/break/continue
/// through `finally` and to always pop the setjmp frame.
///
/// The setjmp frame stays on the stack for the whole try construct (body +
/// handlers). A runtime `phase_ptr` (0=body, 1=handler) distinguishes the
/// first longjmp (dispatch to handlers) from a raise/trap during a handler
/// (go to finally with RERAISE — do not re-enter handlers).
#[derive(Clone)]
struct TryScope {
    fin_l: String,
    end_l: String,
    /// Exception dispatch label (setjmp non-zero path).
    exc_l: String,
    /// `alloca i32` holding TRY_EXIT_*
    exit_ptr: String,
    /// Runtime flag (`alloca i32`, 1=live): pop at most once on structured exit.
    live_ptr: String,
    /// 0=body, 1=handler, 2=else
    phase_ptr: String,
    /// `loops.len()` when this try was entered.
    loops_at_entry: usize,
    /// Index into `gen_try_pool` / gen frame try phase+exit slots (generators).
    pool_idx: usize,
}

/// A try whose body/handlers finished and whose `finally` is currently
/// executing. Setjmp is already popped; only `exit_ptr` must survive yield.
#[derive(Clone)]
struct FinallyScope {
    exit_ptr: String,
    pool_idx: usize,
}

struct Emitter {
    /// finished function definitions
    funcs: String,
    /// body of the function currently being emitted
    body: String,
    /// interned string constants: content → global name
    strings: HashMap<String, String>,
    string_defs: String,
    /// module-level variable definitions (@g.<name>)
    global_defs: String,
    tmp: usize,
    blk: usize,
    cur_block: String,
    terminated: bool,
    /// (continue target, break target) for enclosing loops
    loops: Vec<(String, String)>,
    /// Enclosing try scopes (innermost last).
    tries: Vec<TryScope>,
    /// Current function return type (for try-return plumbing).
    fn_ret: Ty,
    /// Shared alloca for a pending return value while unwinding through finally.
    try_ret_ptr: Option<String>,
    /// When emitting a generator resume: frame pointer name.
    gen_frame: Option<String>,
    /// Local name → frame slot index.
    gen_local_index: HashMap<String, i64>,
    /// Next yield resume state id.
    gen_next_state: i64,
    gen_yield_ty: Ty,
    /// Preallocated try control allocas for generators (dominate all gstates).
    /// Each entry: (exit_ptr, live_ptr, phase_ptr).
    gen_try_pool: Vec<(String, String, String)>,
    /// Next free index into `gen_try_pool`.
    gen_try_pool_next: usize,
    /// Tries currently executing their `finally` (innermost last).
    gen_fin_stack: Vec<FinallyScope>,
    /// Class layouts for field GEP / isinstance parent walk.
    classes: Vec<ClassInfo>,
    /// Alloca storage types for locals/params (load/store ABI). Expression
    /// peels may retype a `Local` to a semantic subtype/union without changing
    /// the alloca (e.g. `Class(A)` → `Class(B)|Class(C)` for isinstance).
    local_storage: HashMap<String, Ty>,
}

impl Default for Emitter {
    fn default() -> Self {
        Self {
            funcs: String::new(),
            body: String::new(),
            strings: HashMap::new(),
            string_defs: String::new(),
            global_defs: String::new(),
            tmp: 0,
            blk: 0,
            cur_block: String::new(),
            terminated: false,
            loops: Vec::new(),
            tries: Vec::new(),
            fn_ret: Ty::None,
            try_ret_ptr: None,
            gen_frame: None,
            gen_local_index: HashMap::new(),
            gen_next_state: 1,
            gen_yield_ty: Ty::Int,
            gen_try_pool: Vec::new(),
            gen_try_pool_next: 0,
            gen_fin_stack: Vec::new(),
            classes: Vec::new(),
            local_storage: HashMap::new(),
        }
    }
}

fn max_try_depth_in_stmts(stmts: &[Stmt]) -> usize {
    let mut max = 0usize;
    for s in stmts {
        max = max.max(max_try_depth_in_stmt(s));
    }
    max
}

fn max_try_depth_in_stmt(s: &Stmt) -> usize {
    match s {
        Stmt::Try {
            body,
            handlers,
            orelse,
            finally,
        } => {
            let mut inner = max_try_depth_in_stmts(body);
            for (_, _, b) in handlers {
                inner = inner.max(max_try_depth_in_stmts(b));
            }
            inner = inner.max(max_try_depth_in_stmts(orelse));
            inner = inner.max(max_try_depth_in_stmts(finally));
            1 + inner
        }
        Stmt::If { branches, orelse } => {
            let mut m = max_try_depth_in_stmts(orelse);
            for (c, b) in branches {
                m = m.max(max_try_depth_in_expr(c));
                m = m.max(max_try_depth_in_stmts(b));
            }
            m
        }
        Stmt::While {
            cond, body, step, ..
        } => max_try_depth_in_expr(cond)
            .max(max_try_depth_in_stmts(body))
            .max(max_try_depth_in_stmts(step)),
        // yield-from desugars to Block{Try…} inside Assign / ExprStmt / etc.
        Stmt::Assign { value: e, .. }
        | Stmt::GlobalAssign { value: e, .. }
        | Stmt::ExprStmt(e)
        | Stmt::Return(Some(e))
        | Stmt::Yield(e)
        | Stmt::Raise { message: e, .. }
        | Stmt::GenClose { generator: e }
        | Stmt::ListAppend { value: e, .. }
        | Stmt::ListRemove { value: e, .. }
        | Stmt::CellStore { value: e, .. } => max_try_depth_in_expr(e),
        Stmt::SetField {
            object: o,
            value: v,
            ..
        } => max_try_depth_in_expr(o).max(max_try_depth_in_expr(v)),
        Stmt::Print(args) => args.iter().map(max_try_depth_in_expr).max().unwrap_or(0),
        Stmt::ListInsert {
            index: i, value: v, ..
        }
        | Stmt::IndexAssign {
            index: i, value: v, ..
        } => max_try_depth_in_expr(i).max(max_try_depth_in_expr(v)),
        Stmt::IndexDelete { index: i, .. } => max_try_depth_in_expr(i),
        Stmt::DictUpdate { dict, other } | Stmt::SetUpdate { set: dict, other } => {
            max_try_depth_in_expr(dict).max(max_try_depth_in_expr(other))
        }
        Stmt::ListExtend { list, other } => {
            max_try_depth_in_expr(list).max(max_try_depth_in_expr(other))
        }
        _ => 0,
    }
}

fn max_try_depth_in_expr(e: &Expr) -> usize {
    use ExprKind::*;
    match &e.kind {
        Block { stmts, result } => max_try_depth_in_stmts(stmts).max(max_try_depth_in_expr(result)),
        Let { value, body, .. } => max_try_depth_in_expr(value).max(max_try_depth_in_expr(body)),
        Binary { left, right, .. } | IsIdentity { left, right, .. } => {
            max_try_depth_in_expr(left).max(max_try_depth_in_expr(right))
        }
        Unary { operand, .. }
        | ToBool(operand)
        | Abs(operand)
        | Len(operand)
        | IntToFloat(operand)
        | BoolToInt(operand)
        | FloatToInt(operand)
        | IntToStr(operand)
        | FloatToStr(operand)
        | BoolToStr(operand)
        | ExcToStr(operand)
        | StrRepr(operand)
        | StrAscii(operand)
        | IsNone { value: operand, .. }
        | CellNew(operand)
        | CellLoad(operand)
        | GeneratorReturnValue(operand)
        | FromUnion { value: operand, .. }
        | ToUnion { value: operand, .. }
        | ToAny { value: operand }
        | FromAny { value: operand }
        | DictKeys(operand)
        | DictValues(operand)
        | DictItems(operand)
        | SetToList(operand)
        | ListCopy(operand)
        | ListFromStr(operand)
        | DictCopy(operand)
        | SetFromStr(operand)
        | Sum(operand)
        | MinList(operand)
        | MaxList(operand)
        | JsonDumps(operand)
        | JsonLoads { arg: operand, .. }
        | MathCall { arg: operand, .. }
        | ClosureCap {
            closure: operand, ..
        } => max_try_depth_in_expr(operand),
        FormatValue { value, spec } => {
            max_try_depth_in_expr(value).max(max_try_depth_in_expr(spec))
        }
        GeneratorNext { generator, send } => {
            max_try_depth_in_expr(generator).max(max_try_depth_in_expr(send))
        }
        GeneratorThrow {
            generator, message, ..
        } => max_try_depth_in_expr(generator).max(max_try_depth_in_expr(message)),
        GenSentValue => 0,
        Call { args, .. } => args.iter().map(max_try_depth_in_expr).max().unwrap_or(0),
        CallClosure { closure, args, .. } => args
            .iter()
            .map(max_try_depth_in_expr)
            .max()
            .unwrap_or(0)
            .max(max_try_depth_in_expr(closure)),
        MakeClosure { captures, .. } => captures
            .iter()
            .map(max_try_depth_in_expr)
            .max()
            .unwrap_or(0),
        MakeGenerator {
            args, code_from, ..
        } => {
            let m = args.iter().map(max_try_depth_in_expr).max().unwrap_or(0);
            m.max(
                code_from
                    .as_ref()
                    .map(|c| max_try_depth_in_expr(c))
                    .unwrap_or(0),
            )
        }
        Index { base, index } => max_try_depth_in_expr(base).max(max_try_depth_in_expr(index)),
        Slice {
            base, lo, hi, step, ..
        } => max_try_depth_in_expr(base)
            .max(max_try_depth_in_expr(lo))
            .max(max_try_depth_in_expr(hi))
            .max(max_try_depth_in_expr(step)),
        ListLit(items) | TupleLit(items) | SetLit(items) => {
            items.iter().map(max_try_depth_in_expr).max().unwrap_or(0)
        }
        DictLit(pairs) => pairs
            .iter()
            .map(|(k, v)| max_try_depth_in_expr(k).max(max_try_depth_in_expr(v)))
            .max()
            .unwrap_or(0),
        Contains { needle, haystack } => {
            max_try_depth_in_expr(needle).max(max_try_depth_in_expr(haystack))
        }
        IsInstance { value, .. }
        | ExcIsInstance { value, .. }
        | ClassIsInstance { value, .. }
        | GetField { object: value, .. }
        | GetFieldPartial { object: value, .. } => max_try_depth_in_expr(value),
        CallMethod { args, .. } => args.iter().map(max_try_depth_in_expr).max().unwrap_or(0),
        BindMethod { object, .. } => max_try_depth_in_expr(object),
        CallBoundMethod { bound, args, .. } => args
            .iter()
            .map(max_try_depth_in_expr)
            .max()
            .unwrap_or(0)
            .max(max_try_depth_in_expr(bound)),
        NewObject { .. } => 0,
        ObjectToStr(operand) => max_try_depth_in_expr(operand),
        SetUnion { left, right }
        | SetIntersect { left, right }
        | SetDiff { left, right }
        | SetSymDiff { left, right } => {
            max_try_depth_in_expr(left).max(max_try_depth_in_expr(right))
        }
        SetFromList { list, .. } => max_try_depth_in_expr(list),
        DictFromPairs { pairs, .. } => max_try_depth_in_expr(pairs),
        ListPop { list, index } | ListIndexOf { list, value: index } => {
            max_try_depth_in_expr(list).max(max_try_depth_in_expr(index))
        }
        DictGet { dict, key, default } => max_try_depth_in_expr(dict)
            .max(max_try_depth_in_expr(key))
            .max(max_try_depth_in_expr(default)),
        DictPop { dict, key, default } => max_try_depth_in_expr(dict)
            .max(max_try_depth_in_expr(key))
            .max(
                default
                    .as_ref()
                    .map(|d| max_try_depth_in_expr(d))
                    .unwrap_or(0),
            ),
        StrCall { args, .. } | FileCall { args, .. } => {
            args.iter().map(max_try_depth_in_expr).max().unwrap_or(0)
        }
        Min { left, right } | Max { left, right } => {
            max_try_depth_in_expr(left).max(max_try_depth_in_expr(right))
        }
        ListNew { cap } => max_try_depth_in_expr(cap),
        Input { prompt } => prompt
            .as_ref()
            .map(|p| max_try_depth_in_expr(p))
            .unwrap_or(0),
        Open { path, mode } => max_try_depth_in_expr(path).max(max_try_depth_in_expr(mode)),
        _ => 0,
    }
}

fn count_yields_in_stmts(stmts: &[Stmt]) -> i64 {
    let mut n = 0i64;
    for s in stmts {
        n += count_yields_in_stmt(s);
    }
    n
}

fn count_yields_in_stmt(s: &Stmt) -> i64 {
    match s {
        Stmt::Yield(e) => 1 + count_yields_in_expr(e),
        Stmt::If { branches, orelse } => {
            branches
                .iter()
                .map(|(c, b)| count_yields_in_expr(c) + count_yields_in_stmts(b))
                .sum::<i64>()
                + count_yields_in_stmts(orelse)
        }
        Stmt::While { cond, body, step } => {
            count_yields_in_expr(cond) + count_yields_in_stmts(body) + count_yields_in_stmts(step)
        }
        Stmt::Try {
            body,
            handlers,
            orelse,
            finally,
        } => {
            count_yields_in_stmts(body)
                + handlers
                    .iter()
                    .map(|(_, _, b)| count_yields_in_stmts(b))
                    .sum::<i64>()
                + count_yields_in_stmts(orelse)
                + count_yields_in_stmts(finally)
        }
        Stmt::Print(args) => args.iter().map(count_yields_in_expr).sum(),
        Stmt::ExprStmt(e)
        | Stmt::Assign { value: e, .. }
        | Stmt::GlobalAssign { value: e, .. }
        | Stmt::Return(Some(e))
        | Stmt::Raise { message: e, .. }
        | Stmt::GenClose { generator: e }
        | Stmt::ListAppend { value: e, .. }
        | Stmt::ListRemove { value: e, .. }
        | Stmt::CellStore { value: e, .. } => count_yields_in_expr(e),
        Stmt::SetField {
            object: o,
            value: v,
            ..
        } => count_yields_in_expr(o) + count_yields_in_expr(v),
        Stmt::ListInsert {
            index: i, value: v, ..
        }
        | Stmt::IndexAssign {
            index: i, value: v, ..
        } => count_yields_in_expr(i) + count_yields_in_expr(v),
        Stmt::IndexDelete { index: i, .. } => count_yields_in_expr(i),
        Stmt::DictUpdate { dict, other } | Stmt::SetUpdate { set: dict, other } => {
            count_yields_in_expr(dict) + count_yields_in_expr(other)
        }
        Stmt::ListExtend { list, other } => {
            count_yields_in_expr(list) + count_yields_in_expr(other)
        }
        _ => 0,
    }
}

fn count_yields_in_expr(e: &Expr) -> i64 {
    use ExprKind::*;
    match &e.kind {
        Block { stmts, result } => count_yields_in_stmts(stmts) + count_yields_in_expr(result),
        Let { value, body, .. } => count_yields_in_expr(value) + count_yields_in_expr(body),
        Binary { left, right, .. } | IsIdentity { left, right, .. } => {
            count_yields_in_expr(left) + count_yields_in_expr(right)
        }
        Unary { operand, .. }
        | ToBool(operand)
        | Abs(operand)
        | Len(operand)
        | IntToFloat(operand)
        | BoolToInt(operand)
        | FloatToInt(operand)
        | IntToStr(operand)
        | FloatToStr(operand)
        | BoolToStr(operand)
        | ExcToStr(operand)
        | StrRepr(operand)
        | StrAscii(operand)
        | IsNone { value: operand, .. }
        | CellNew(operand)
        | CellLoad(operand)
        | GeneratorReturnValue(operand)
        | FromUnion { value: operand, .. }
        | ToUnion { value: operand, .. }
        | ToAny { value: operand }
        | FromAny { value: operand }
        | DictKeys(operand)
        | DictValues(operand)
        | DictItems(operand)
        | SetToList(operand)
        | ListCopy(operand)
        | ListFromStr(operand)
        | DictCopy(operand)
        | SetFromStr(operand)
        | Sum(operand)
        | MinList(operand)
        | MaxList(operand)
        | JsonDumps(operand)
        | JsonLoads { arg: operand, .. }
        | MathCall { arg: operand, .. } => count_yields_in_expr(operand),
        FormatValue { value, spec } => count_yields_in_expr(value) + count_yields_in_expr(spec),
        GeneratorNext { generator, send } => {
            count_yields_in_expr(generator) + count_yields_in_expr(send)
        }
        GeneratorThrow {
            generator, message, ..
        } => count_yields_in_expr(generator) + count_yields_in_expr(message),
        GenSentValue => 0,
        Call { args, .. } => args.iter().map(count_yields_in_expr).sum(),
        CallClosure { closure, args, .. } => {
            count_yields_in_expr(closure) + args.iter().map(count_yields_in_expr).sum::<i64>()
        }
        MakeClosure { captures, .. } => captures.iter().map(count_yields_in_expr).sum(),
        MakeGenerator {
            args, code_from, ..
        } => {
            args.iter().map(count_yields_in_expr).sum::<i64>()
                + code_from
                    .as_ref()
                    .map(|c| count_yields_in_expr(c))
                    .unwrap_or(0)
        }
        ClosureCap { closure, .. } => count_yields_in_expr(closure),
        Index { base, index } => count_yields_in_expr(base) + count_yields_in_expr(index),
        Slice {
            base, lo, hi, step, ..
        } => {
            count_yields_in_expr(base)
                + count_yields_in_expr(lo)
                + count_yields_in_expr(hi)
                + count_yields_in_expr(step)
        }
        ListLit(items) | TupleLit(items) | SetLit(items) => {
            items.iter().map(count_yields_in_expr).sum()
        }
        DictLit(pairs) => pairs
            .iter()
            .map(|(k, v)| count_yields_in_expr(k) + count_yields_in_expr(v))
            .sum(),
        Contains { needle, haystack } => {
            count_yields_in_expr(needle) + count_yields_in_expr(haystack)
        }
        IsInstance { value, .. }
        | ExcIsInstance { value, .. }
        | ClassIsInstance { value, .. }
        | GetField { object: value, .. }
        | GetFieldPartial { object: value, .. } => count_yields_in_expr(value),
        CallMethod { args, .. } => args.iter().map(count_yields_in_expr).sum(),
        BindMethod { object, .. } => count_yields_in_expr(object),
        CallBoundMethod { bound, args, .. } => {
            count_yields_in_expr(bound) + args.iter().map(count_yields_in_expr).sum::<i64>()
        }
        NewObject { .. } => 0,
        ObjectToStr(operand) => count_yields_in_expr(operand),
        SetUnion { left, right }
        | SetIntersect { left, right }
        | SetDiff { left, right }
        | SetSymDiff { left, right } => count_yields_in_expr(left) + count_yields_in_expr(right),
        SetFromList { list, .. } => count_yields_in_expr(list),
        DictFromPairs { pairs, .. } => count_yields_in_expr(pairs),
        ListPop { list, index } | ListIndexOf { list, value: index } => {
            count_yields_in_expr(list) + count_yields_in_expr(index)
        }
        DictGet { dict, key, default } => {
            count_yields_in_expr(dict) + count_yields_in_expr(key) + count_yields_in_expr(default)
        }
        DictPop { dict, key, default } => {
            count_yields_in_expr(dict)
                + count_yields_in_expr(key)
                + default
                    .as_ref()
                    .map(|d| count_yields_in_expr(d))
                    .unwrap_or(0)
        }
        StrCall { args, .. } | FileCall { args, .. } => args.iter().map(count_yields_in_expr).sum(),
        Min { left, right } | Max { left, right } => {
            count_yields_in_expr(left) + count_yields_in_expr(right)
        }
        ListNew { cap } => count_yields_in_expr(cap),
        Input { prompt } => prompt
            .as_ref()
            .map(|p| count_yields_in_expr(p))
            .unwrap_or(0),
        Open { path, mode } => count_yields_in_expr(path) + count_yields_in_expr(mode),
        // Leafs / no nested yield: consts, locals, globals, Argv, OsGetcwd,
        // DictNew, SetNew, CellNewUnbound. Yield-as-expr is a Block with Yield.
        _ => 0,
    }
}

impl Emitter {
    fn finish(self) -> String {
        let mut out = String::new();
        out.push_str("; generated by pyrs\n\n");
        out.push_str("declare void @pyrs_print_int(i64)\n");
        out.push_str("declare void @pyrs_print_float(double)\n");
        out.push_str("declare void @pyrs_print_bool(i32)\n");
        out.push_str("declare void @pyrs_print_str(ptr)\n");
        out.push_str("declare void @pyrs_print_list(ptr, i32)\n");
        out.push_str("declare void @pyrs_print_tuple(ptr)\n");
        out.push_str("declare void @pyrs_print_dict(ptr)\n");
        out.push_str("declare void @pyrs_print_set(ptr)\n");
        out.push_str("declare void @pyrs_print_any(i64)\n");
        out.push_str("declare i32 @pyrs_any_truth(i64)\n");
        out.push_str("declare void @pyrs_print_sep()\n");
        out.push_str("declare void @pyrs_print_end()\n");
        out.push_str("declare void @pyrs_die(ptr)\n");
        out.push_str("declare void @pyrs_raise(i32, ptr) noreturn\n");
        out.push_str("declare void @pyrs_reraise() noreturn\n");
        out.push_str("declare void @pyrs_set_exc(i32, ptr)\n");
        out.push_str("declare void @pyrs_set_exc_msg(ptr)\n");
        out.push_str("declare ptr @pyrs_try_push()\n");
        // setjmp must be called directly (not via a C wrapper): longjmp restores
        // to the setjmp call site. jmp_buf is the first field of PyrsExcFrame.
        // returns_twice is required so LLVM does not clobber the stack across longjmp.
        out.push_str("declare i32 @setjmp(ptr) returns_twice\n");
        out.push_str("declare void @pyrs_try_pop()\n");
        out.push_str("declare i32 @pyrs_exc_type()\n");
        out.push_str("declare ptr @pyrs_exc_message()\n");
        out.push_str("declare ptr @pyrs_exc_object()\n");
        out.push_str("declare void @pyrs_print_exc(ptr)\n");
        out.push_str("declare ptr @pyrs_str_from_exc(ptr)\n");
        out.push_str("declare i32 @pyrs_exc_matches(i32, i32)\n");
        out.push_str("declare i32 @pyrs_exc_isinstance(ptr, i32)\n");
        out.push_str("declare void @pyrs_exc_clear()\n");
        out.push_str("declare ptr @pyrs_tuple_new(i64)\n");
        out.push_str("declare void @pyrs_tuple_set(ptr, i64, i64, i32)\n");
        out.push_str("declare i64 @pyrs_tuple_get(ptr, i64)\n");
        out.push_str("declare i32 @pyrs_tuple_eq(ptr, ptr)\n");
        out.push_str("declare i32 @pyrs_tuple_contains(ptr, i64, i32)\n");
        out.push_str("declare void @pyrs_unpack_check(i64, i64)\n");
        out.push_str("declare void @pyrs_unpack_check_min(i64, i64)\n");
        out.push_str("declare ptr @pyrs_cell_new(i64)\n");
        out.push_str("declare ptr @pyrs_cell_new_unbound()\n");
        out.push_str("declare i64 @pyrs_cell_load(ptr)\n");
        out.push_str("declare void @pyrs_cell_store(ptr, i64)\n");
        out.push_str("declare ptr @pyrs_closure_new(ptr, i64)\n");
        out.push_str("declare void @pyrs_closure_set(ptr, i64, i64)\n");
        out.push_str("declare ptr @pyrs_closure_code(ptr)\n");
        out.push_str("declare i64 @pyrs_closure_get(ptr, i64)\n");
        out.push_str("declare ptr @pyrs_gen_new(ptr, i64)\n");
        out.push_str("declare i64 @pyrs_gen_get_local(ptr, i64)\n");
        out.push_str("declare void @pyrs_gen_set_local(ptr, i64, i64)\n");
        out.push_str("declare i64 @pyrs_gen_state(ptr)\n");
        out.push_str("declare void @pyrs_gen_set_state(ptr, i64)\n");
        out.push_str("declare void @pyrs_gen_set_yield(ptr, i64)\n");
        out.push_str("declare i64 @pyrs_gen_yield_value(ptr)\n");
        out.push_str("declare i32 @pyrs_gen_done(ptr)\n");
        out.push_str("declare void @pyrs_gen_set_done(ptr)\n");
        out.push_str("declare void @pyrs_gen_close(ptr)\n");
        out.push_str("declare void @pyrs_gen_save_try_phase(ptr, i64, i64)\n");
        out.push_str("declare i64 @pyrs_gen_load_try_phase(ptr, i64)\n");
        out.push_str("declare void @pyrs_gen_save_try_exit(ptr, i64, i64)\n");
        out.push_str("declare i64 @pyrs_gen_load_try_exit(ptr, i64)\n");
        out.push_str("declare i32 @pyrs_gen_closing(ptr)\n");
        out.push_str("declare i32 @pyrs_gen_is_genexit()\n");
        out.push_str("declare void @pyrs_gen_set_return(ptr, i64)\n");
        out.push_str("declare i64 @pyrs_gen_return_value(ptr)\n");
        out.push_str("declare i32 @pyrs_gen_has_return(ptr)\n");
        out.push_str("declare void @pyrs_gen_set_send(ptr, i64, i64)\n");
        out.push_str("declare i64 @pyrs_gen_send_slot(ptr)\n");
        out.push_str("declare i32 @pyrs_gen_send_is_none(ptr)\n");
        out.push_str("declare void @pyrs_gen_set_throw(ptr, i64, ptr)\n");
        out.push_str("declare i32 @pyrs_gen_throwing(ptr)\n");
        out.push_str("declare i64 @pyrs_gen_throw_type(ptr)\n");
        out.push_str("declare ptr @pyrs_gen_throw_msg(ptr)\n");
        out.push_str("declare void @pyrs_gen_clear_throw(ptr)\n");
        out.push_str("declare ptr @pyrs_dict_new()\n");
        out.push_str("declare void @pyrs_dict_set(ptr, i64, i32, i64, i32)\n");
        out.push_str("declare i64 @pyrs_dict_get(ptr, i64, i32)\n");
        out.push_str("declare i32 @pyrs_dict_get_default(ptr, i64, i32, ptr)\n");
        out.push_str("declare void @pyrs_dict_del(ptr, i64, i32)\n");
        out.push_str("declare i32 @pyrs_dict_contains(ptr, i64, i32)\n");
        out.push_str("declare void @pyrs_dict_clear(ptr)\n");
        out.push_str("declare void @pyrs_dict_update(ptr, ptr)\n");
        out.push_str("declare i64 @pyrs_dict_pop(ptr, i64, i32, i32, i64, ptr)\n");
        out.push_str("declare ptr @pyrs_dict_keys(ptr)\n");
        out.push_str("declare ptr @pyrs_dict_values(ptr)\n");
        out.push_str("declare ptr @pyrs_dict_items(ptr)\n");
        out.push_str("declare ptr @pyrs_set_new()\n");
        out.push_str("declare void @pyrs_set_add(ptr, i64, i32)\n");
        out.push_str("declare void @pyrs_set_remove(ptr, i64, i32)\n");
        out.push_str("declare void @pyrs_set_discard(ptr, i64, i32)\n");
        out.push_str("declare ptr @pyrs_set_union(ptr, ptr)\n");
        out.push_str("declare ptr @pyrs_set_intersect(ptr, ptr)\n");
        out.push_str("declare ptr @pyrs_set_diff(ptr, ptr)\n");
        out.push_str("declare ptr @pyrs_set_symdiff(ptr, ptr)\n");
        out.push_str("declare void @pyrs_set_update(ptr, ptr)\n");
        out.push_str("declare ptr @malloc(i64)\n");
        out.push_str("declare i32 @pyrs_set_contains(ptr, i64, i32)\n");
        out.push_str("declare void @pyrs_set_clear(ptr)\n");
        out.push_str("declare ptr @pyrs_set_elements(ptr)\n");
        out.push_str("declare ptr @pyrs_set_from_list(ptr, i32)\n");
        out.push_str("declare ptr @pyrs_set_from_str(ptr)\n");
        out.push_str("declare ptr @pyrs_dict_copy(ptr)\n");
        out.push_str("declare ptr @pyrs_dict_from_pairs(ptr, i32, i32)\n");
        out.push_str("declare ptr @pyrs_str_concat(ptr, ptr)\n");
        out.push_str("declare ptr @pyrs_str_repeat(ptr, i64)\n");
        out.push_str("declare i32 @pyrs_str_cmp(ptr, ptr)\n");
        out.push_str("declare ptr @pyrs_str_index(ptr, i64)\n");
        out.push_str("declare ptr @pyrs_str_from_int(i64)\n");
        out.push_str("declare ptr @pyrs_str_from_float(double)\n");
        out.push_str("declare ptr @pyrs_str_format_float(double, i64)\n");
        out.push_str("declare ptr @pyrs_str_from_bool(i32)\n");
        out.push_str("declare ptr @pyrs_str_repr(ptr)\n");
        out.push_str("declare ptr @pyrs_str_ascii(ptr)\n");
        out.push_str("declare ptr @pyrs_format_int(i64, ptr)\n");
        out.push_str("declare ptr @pyrs_format_float(double, ptr)\n");
        out.push_str("declare ptr @pyrs_format_bool(i32, ptr)\n");
        out.push_str("declare ptr @pyrs_format_str(ptr, ptr)\n");
        out.push_str("declare ptr @pyrs_str_slice(ptr, i64, i64, i64)\n");
        out.push_str("declare i32 @pyrs_str_contains(ptr, ptr)\n");
        out.push_str("declare ptr @pyrs_str_upper(ptr)\n");
        out.push_str("declare ptr @pyrs_str_lower(ptr)\n");
        out.push_str("declare ptr @pyrs_str_strip(ptr)\n");
        out.push_str("declare ptr @pyrs_str_lstrip(ptr)\n");
        out.push_str("declare ptr @pyrs_str_rstrip(ptr)\n");
        out.push_str("declare i32 @pyrs_str_startswith(ptr, ptr)\n");
        out.push_str("declare i32 @pyrs_str_endswith(ptr, ptr)\n");
        out.push_str("declare i64 @pyrs_str_find(ptr, ptr)\n");
        out.push_str("declare i64 @pyrs_str_rfind(ptr, ptr)\n");
        out.push_str("declare i64 @pyrs_str_rindex(ptr, ptr)\n");
        out.push_str("declare i64 @pyrs_str_count(ptr, ptr)\n");
        out.push_str("declare ptr @pyrs_str_replace(ptr, ptr, ptr)\n");
        out.push_str("declare ptr @pyrs_str_split_ws(ptr)\n");
        out.push_str("declare ptr @pyrs_str_split(ptr, ptr)\n");
        out.push_str("declare ptr @pyrs_str_join(ptr, ptr)\n");
        out.push_str("declare i32 @pyrs_str_isdigit(ptr)\n");
        out.push_str("declare i32 @pyrs_str_isalpha(ptr)\n");
        out.push_str("declare i32 @pyrs_str_isspace(ptr)\n");
        out.push_str("declare i32 @pyrs_str_isupper(ptr)\n");
        out.push_str("declare i32 @pyrs_str_islower(ptr)\n");
        out.push_str("declare ptr @pyrs_list_new(i64)\n");
        out.push_str("declare void @pyrs_list_push(ptr, i64)\n");
        out.push_str("declare ptr @pyrs_list_concat(ptr, ptr)\n");
        out.push_str("declare ptr @pyrs_list_repeat(ptr, i64)\n");
        out.push_str("declare void @pyrs_list_insert(ptr, i64, i64)\n");
        out.push_str("declare void @pyrs_list_remove(ptr, i64, i32)\n");
        out.push_str("declare i64 @pyrs_list_index(ptr, i64, i32)\n");
        out.push_str("declare void @pyrs_list_clear(ptr)\n");
        out.push_str("declare void @pyrs_list_sort(ptr, i32)\n");
        out.push_str("declare void @pyrs_list_extend(ptr, ptr)\n");
        out.push_str("declare ptr @pyrs_list_copy(ptr)\n");
        out.push_str("declare ptr @pyrs_list_from_str(ptr)\n");
        out.push_str("declare ptr @pyrs_list_slice(ptr, i64, i64, i64)\n");
        out.push_str("declare i32 @pyrs_list_contains(ptr, i64, i32)\n");
        out.push_str("declare i32 @pyrs_list_eq(ptr, ptr, i32)\n");
        out.push_str("declare i64 @pyrs_list_pop(ptr, i64)\n");
        out.push_str("declare ptr @pyrs_input(ptr)\n");
        out.push_str("declare ptr @pyrs_argv()\n");
        out.push_str("declare void @pyrs_set_args(i32, ptr)\n");
        out.push_str("declare ptr @pyrs_open(ptr, ptr)\n");
        out.push_str("declare ptr @pyrs_file_read(ptr)\n");
        out.push_str("declare ptr @pyrs_file_readline(ptr)\n");
        out.push_str("declare ptr @pyrs_file_readlines(ptr)\n");
        out.push_str("declare i64 @pyrs_file_write(ptr, ptr)\n");
        out.push_str("declare void @pyrs_file_close(ptr)\n");
        out.push_str("declare i64 @pyrs_ipow(i64, i64)\n");
        out.push_str("declare i64 @pyrs_int_from_i64(i64)\n");
        out.push_str("declare i64 @pyrs_int_from_str(ptr, i64)\n");
        out.push_str("declare i64 @pyrs_int_as_i64(i64)\n");
        out.push_str("declare double @pyrs_int_to_float(i64)\n");
        out.push_str("declare i64 @pyrs_int_from_float(double)\n");
        out.push_str("declare i32 @pyrs_int_cmp(i64, i64)\n");
        out.push_str("declare i32 @pyrs_int_eq(i64, i64)\n");
        out.push_str("declare i32 @pyrs_int_truth(i64)\n");
        out.push_str("declare i64 @pyrs_int_add(i64, i64)\n");
        out.push_str("declare i64 @pyrs_int_sub(i64, i64)\n");
        out.push_str("declare i64 @pyrs_int_mul(i64, i64)\n");
        out.push_str("declare i64 @pyrs_int_floordiv(i64, i64)\n");
        out.push_str("declare i64 @pyrs_int_mod(i64, i64)\n");
        out.push_str("declare i64 @pyrs_int_pow(i64, i64)\n");
        out.push_str("declare i64 @pyrs_int_and(i64, i64)\n");
        out.push_str("declare i64 @pyrs_int_or(i64, i64)\n");
        out.push_str("declare i64 @pyrs_int_xor(i64, i64)\n");
        out.push_str("declare i64 @pyrs_int_lshift(i64, i64)\n");
        out.push_str("declare i64 @pyrs_int_rshift(i64, i64)\n");
        out.push_str("declare i64 @pyrs_int_neg(i64)\n");
        out.push_str("declare i64 @pyrs_int_invert(i64)\n");
        out.push_str("declare i64 @pyrs_int_abs(i64)\n");
        out.push_str("declare double @pyrs_ffloordiv(double, double)\n");
        out.push_str("declare double @pyrs_fmod_floored(double, double)\n");
        out.push_str("declare double @llvm.fabs.f64(double)\n");
        out.push_str("declare double @llvm.floor.f64(double)\n");
        out.push_str("declare double @llvm.ceil.f64(double)\n");
        out.push_str("declare double @llvm.sqrt.f64(double)\n");
        out.push_str("declare double @llvm.sin.f64(double)\n");
        out.push_str("declare double @llvm.cos.f64(double)\n");
        out.push_str("declare double @llvm.exp.f64(double)\n");
        out.push_str("declare double @llvm.log.f64(double)\n");
        out.push_str("declare double @llvm.log10.f64(double)\n");
        out.push_str("declare double @llvm.pow.f64(double, double)\n");
        // libm (linked with -lm); no reliable LLVM intrinsic for tan
        out.push_str("declare double @tan(double)\n");
        out.push_str("declare ptr @pyrs_os_getcwd()\n");
        out.push_str("declare ptr @pyrs_json_dumps(i64, i32)\n");
        out.push_str("declare i64 @pyrs_json_loads_int(ptr)\n");
        out.push_str("declare double @pyrs_json_loads_float(ptr)\n");
        out.push_str("declare i32 @pyrs_json_loads_bool(ptr)\n");
        out.push_str("declare ptr @pyrs_json_loads_str(ptr)\n");
        out.push_str("declare ptr @pyrs_json_loads_list_int(ptr)\n");
        out.push_str("declare ptr @pyrs_json_loads_list_float(ptr)\n");
        out.push_str("declare ptr @pyrs_json_loads_list_str(ptr)\n");
        out.push_str("declare ptr @pyrs_json_loads_list_bool(ptr)\n");
        out.push_str("declare ptr @pyrs_json_loads_dict_str_int(ptr)\n");
        out.push_str("declare ptr @pyrs_json_loads_dict_str_float(ptr)\n");
        out.push_str("declare ptr @pyrs_json_loads_dict_str_str(ptr)\n");
        out.push_str("declare ptr @pyrs_json_loads_dict_str_bool(ptr)\n");
        out.push_str("declare ptr @pyrs_object_new(i64, i64)\n");
        out.push_str("declare i32 @pyrs_isinstance_class(ptr, i64, ptr, i64)\n");
        out.push_str("declare void @pyrs_print_object(ptr)\n");
        out.push_str("declare void @pyrs_print_class_instance(ptr)\n");
        out.push_str("declare ptr @pyrs_str_from_object(ptr)\n");
        out.push_str("declare void @pyrs_set_class_names(ptr, i64)\n\n");
        out.push_str(&self.global_defs);
        out.push_str(&self.string_defs);
        out.push('\n');
        out.push_str(&self.funcs);
        out
    }

    fn emit_module(&mut self, module: &Module) {
        self.classes = module.classes.clone();
        // Class parent table + C-string name table for isinstance / print / str.
        if !module.classes.is_empty() {
            let n = module.classes.len();
            let mut elems = String::new();
            for (i, c) in module.classes.iter().enumerate() {
                if i > 0 {
                    elems.push_str(", ");
                }
                let p = c.parent.map(|p| p as i64).unwrap_or(-1);
                elems.push_str(&format!("i64 {p}"));
            }
            self.global_defs.push_str(&format!(
                "@pyrs_class_parents = internal constant [{n} x i64] [{elems}]\n"
            ));
            // C-string globals for each class simple name (for runtime print/str).
            for c in &module.classes {
                let (esc, len) = escape_bytes(&c.name);
                self.global_defs.push_str(&format!(
                    "@.cn.{} = private unnamed_addr constant [{len} x i8] c\"{esc}\", align 1\n",
                    c.id
                ));
            }
            let mut name_ptrs = String::new();
            for (i, c) in module.classes.iter().enumerate() {
                if i > 0 {
                    name_ptrs.push_str(", ");
                }
                let blen = c.name.len() + 1;
                name_ptrs.push_str(&format!(
                    "ptr getelementptr inbounds ([{blen} x i8], ptr @.cn.{}, i32 0, i32 0)",
                    c.id
                ));
            }
            self.global_defs.push_str(&format!(
                "@pyrs_class_name_ptrs = internal constant [{n} x ptr] [{name_ptrs}]\n"
            ));
            // Also intern display forms for static ObjectDefaultStr paths.
            for c in &module.classes {
                let label = format!("<{} object>", c.name);
                let _ = self.intern_string(&label);
            }
        }
        // module globals, zero/null-initialized; assigned when the entry
        // function runs its top-level statements
        for (name, ty) in &module.globals {
            let init = match ty {
                Ty::Float => fconst(0.0),
                Ty::Bool => "false".to_string(),
                Ty::Int => "1".to_string(), // tagged small 0
                Ty::Str
                | Ty::List(_)
                | Ty::Tuple(_)
                | Ty::Dict { .. }
                | Ty::Set(_)
                | Ty::File
                | Ty::Closure { .. }
                | Ty::BoundMethod { .. }
                | Ty::Cell(_)
                | Ty::Generator { .. }
                | Ty::Exception
                | Ty::Class(_) => "null".to_string(),
                Ty::Union(_) => "zeroinitializer".to_string(),
                // Any / None / etc.: zero i64 (null box for Any)
                _ => "0".to_string(),
            };
            self.global_defs.push_str(&format!(
                "@g.{name} = internal global {} {init}\n",
                lty(*ty)
            ));
        }
        for func in &module.funcs {
            self.emit_function(func);
        }
        // the real C main: call the entry function and exit 0
        let entry = mangle(&module.entry);
        let class_setup = if module.classes.is_empty() {
            String::new()
        } else {
            format!(
                "  call void @pyrs_set_class_names(ptr @pyrs_class_name_ptrs, i64 {})\n",
                module.classes.len()
            )
        };
        self.funcs.push_str(&format!(
            "define i32 @main(i32 %argc, ptr %argv) {{\nentry:\n  \
             call void @pyrs_set_args(i32 %argc, ptr %argv)\n\
             {class_setup}  \
             call void @{entry}()\n  ret i32 0\n}}\n\n"
        ));
    }

    // ---- low-level helpers ----

    /// Store to try control (phase/exit/live). Must be volatile so LLVM does
    /// not DSE across `setjmp`/`longjmp` (raise is `noreturn` but longjmps).
    fn store_try_i32(&mut self, val: impl std::fmt::Display, ptr: &str) {
        self.line(format!("store volatile i32 {val}, ptr {ptr}"));
    }

    /// Load try control (phase/exit/live); volatile for setjmp correctness.
    fn load_try_i32(&mut self, ptr: &str) -> String {
        let t = self.tmp();
        self.line(format!("{t} = load volatile i32, ptr {ptr}"));
        t
    }

    fn line(&mut self, s: impl AsRef<str>) {
        self.body.push_str("  ");
        self.body.push_str(s.as_ref());
        self.body.push('\n');
    }

    fn tmp(&mut self) -> String {
        self.tmp += 1;
        format!("%t{}", self.tmp)
    }

    fn fresh_block(&mut self, hint: &str) -> String {
        self.blk += 1;
        format!("{hint}.{}", self.blk)
    }

    fn start_block(&mut self, label: &str) {
        self.body.push_str(label);
        self.body.push_str(":\n");
        self.cur_block = label.to_string();
        self.terminated = false;
    }

    /// A string constant as a `{ i64 len, [n x i8] }` global; the pointer
    /// doubles as the runtime `PyrsStr*`.
    fn intern_string(&mut self, content: &str) -> String {
        if let Some(name) = self.strings.get(content) {
            return name.clone();
        }
        let name = format!("@.str.{}", self.strings.len());
        let (escaped, storage) = escape_bytes(content);
        let len = content.len();
        self.string_defs.push_str(&format!(
            "{name} = private unnamed_addr constant {{ i64, [{storage} x i8] }} \
             {{ i64 {len}, [{storage} x i8] c\"{escaped}\" }}\n"
        ));
        self.strings.insert(content.to_string(), name.clone());
        name
    }

    /// Trap like the runtime's check_ref when a str/list local is still
    /// null (read before assignment).
    fn emit_ref_check(&mut self, ptr: &str) {
        let is_null = self.tmp();
        self.line(format!("{is_null} = icmp eq ptr {ptr}, null"));
        let trap_l = self.fresh_block("null.trap");
        let ok_l = self.fresh_block("null.ok");
        self.line(format!("br i1 {is_null}, label %{trap_l}, label %{ok_l}"));
        self.start_block(&trap_l);
        self.emit_die("UnboundLocalError: value used before assignment");
        self.start_block(&ok_l);
    }

    /// str and list both lead with an i64 length; load it inline.
    fn emit_len(&mut self, ptr: &str) -> String {
        self.emit_ref_check(ptr);
        let t = self.tmp();
        self.line(format!("{t} = load i64, ptr {ptr}"));
        t
    }

    /// `min`/`max`: pick `right` only when it strictly compares less/greater
    /// than `left` (matches CPython's "prefer the first on ties / NaN").
    fn emit_min_max(&mut self, is_max: bool, left: &Expr, right: &Expr) -> String {
        let l = self.emit_expr(left);
        let r = self.emit_expr(right);
        let pick_r = self.tmp();
        match left.ty {
            Ty::Int => {
                let c = self.tmp();
                self.line(format!("{c} = call i32 @pyrs_int_cmp(i64 {r}, i64 {l})"));
                let pred = if is_max { "sgt" } else { "slt" };
                self.line(format!("{pick_r} = icmp {pred} i32 {c}, 0"));
            }
            Ty::Float => {
                // olt/ogt are false for NaN, so the left operand is kept
                let pred = if is_max { "ogt" } else { "olt" };
                self.line(format!("{pick_r} = fcmp {pred} double {r}, {l}"));
            }
            other => unreachable!("min/max on {other:?}"),
        }
        let t = self.tmp();
        let lty = lty(left.ty);
        self.line(format!("{t} = select i1 {pick_r}, {lty} {r}, {lty} {l}"));
        t
    }

    /// `sum(xs)` for homogeneous numeric lists: open-coded loop over slots.
    fn emit_sum(&mut self, list: &Expr) -> String {
        let Ty::List(elem) = list.ty else {
            unreachable!("sum of non-list");
        };
        let hdr = self.emit_expr(list);
        let len = self.emit_len(&hdr);
        let data_pp = self.tmp();
        self.line(format!(
            "{data_pp} = getelementptr inbounds i8, ptr {hdr}, i64 16"
        ));
        let data_p = self.tmp();
        self.line(format!("{data_p} = load ptr, ptr {data_pp}"));

        // Pre-allocate temp names used in phis (body writes i_next / acc_next).
        self.tmp += 1;
        let i = format!("%t{}", self.tmp);
        self.tmp += 1;
        let acc = format!("%t{}", self.tmp);
        self.tmp += 1;
        let i_next = format!("%t{}", self.tmp);
        self.tmp += 1;
        let acc_next = format!("%t{}", self.tmp);

        let zero = match *elem {
            Ty::Int => "1".to_string(), // tagged 0
            Ty::Float => fconst(0.0),
            other => unreachable!("sum element {other:?}"),
        };
        let elty = lty(*elem);

        let pred = self.cur_block.clone();
        let loop_l = self.fresh_block("sum.loop");
        let body_l = self.fresh_block("sum.body");
        let end_l = self.fresh_block("sum.end");
        self.line(format!("br label %{loop_l}"));

        self.start_block(&loop_l);
        self.line(format!(
            "{i} = phi i64 [ 0, %{pred} ], [ {i_next}, %{body_l} ]"
        ));
        self.line(format!(
            "{acc} = phi {elty} [ {zero}, %{pred} ], [ {acc_next}, %{body_l} ]"
        ));
        let done = self.tmp();
        self.line(format!("{done} = icmp sge i64 {i}, {len}"));
        self.line(format!("br i1 {done}, label %{end_l}, label %{body_l}"));

        self.start_block(&body_l);
        let addr = self.tmp();
        self.line(format!(
            "{addr} = getelementptr inbounds i64, ptr {data_p}, i64 {i}"
        ));
        let slot = self.tmp();
        self.line(format!("{slot} = load i64, ptr {addr}"));
        match *elem {
            Ty::Int => {
                self.line(format!(
                    "{acc_next} = call i64 @pyrs_int_add(i64 {acc}, i64 {slot})"
                ));
            }
            Ty::Float => {
                let v = self.tmp();
                self.line(format!("{v} = bitcast i64 {slot} to double"));
                self.line(format!("{acc_next} = fadd double {acc}, {v}"));
            }
            other => unreachable!("sum element {other:?}"),
        }
        self.line(format!("{i_next} = add i64 {i}, 1"));
        self.line(format!("br label %{loop_l}"));

        self.start_block(&end_l);
        acc
    }

    /// `min(xs)` / `max(xs)` over a numeric list; empty → ValueError.
    fn emit_min_max_list(&mut self, is_max: bool, list: &Expr) -> String {
        let Ty::List(elem) = list.ty else {
            unreachable!("min/max list of non-list");
        };
        let hdr = self.emit_expr(list);
        let len = self.emit_len(&hdr);
        // empty sequence trap (CPython wording)
        let empty = self.tmp();
        self.line(format!("{empty} = icmp eq i64 {len}, 0"));
        let trap_l = self.fresh_block("mml.trap");
        let ok_l = self.fresh_block("mml.ok");
        self.line(format!("br i1 {empty}, label %{trap_l}, label %{ok_l}"));
        self.start_block(&trap_l);
        let msg = if is_max {
            "ValueError: max() iterable argument is empty"
        } else {
            "ValueError: min() iterable argument is empty"
        };
        self.emit_die(msg);
        self.start_block(&ok_l);

        let data_pp = self.tmp();
        self.line(format!(
            "{data_pp} = getelementptr inbounds i8, ptr {hdr}, i64 16"
        ));
        let data_p = self.tmp();
        self.line(format!("{data_p} = load ptr, ptr {data_pp}"));

        // first element as initial best
        let addr0 = self.tmp();
        self.line(format!(
            "{addr0} = getelementptr inbounds i64, ptr {data_p}, i64 0"
        ));
        let slot0 = self.tmp();
        self.line(format!("{slot0} = load i64, ptr {addr0}"));
        let init = self.value_from_slot(&slot0, *elem);

        self.tmp += 1;
        let i = format!("%t{}", self.tmp);
        self.tmp += 1;
        let best = format!("%t{}", self.tmp);
        self.tmp += 1;
        let i_next = format!("%t{}", self.tmp);
        self.tmp += 1;
        let best_next = format!("%t{}", self.tmp);

        let elty = lty(*elem);
        let pred = self.cur_block.clone();
        let loop_l = self.fresh_block("mml.loop");
        let body_l = self.fresh_block("mml.body");
        let end_l = self.fresh_block("mml.end");
        self.line(format!("br label %{loop_l}"));

        self.start_block(&loop_l);
        self.line(format!(
            "{i} = phi i64 [ 1, %{pred} ], [ {i_next}, %{body_l} ]"
        ));
        self.line(format!(
            "{best} = phi {elty} [ {init}, %{pred} ], [ {best_next}, %{body_l} ]"
        ));
        let done = self.tmp();
        self.line(format!("{done} = icmp sge i64 {i}, {len}"));
        self.line(format!("br i1 {done}, label %{end_l}, label %{body_l}"));

        self.start_block(&body_l);
        let addr = self.tmp();
        self.line(format!(
            "{addr} = getelementptr inbounds i64, ptr {data_p}, i64 {i}"
        ));
        let slot = self.tmp();
        self.line(format!("{slot} = load i64, ptr {addr}"));
        let cur = self.value_from_slot(&slot, *elem);
        // pick cur when strictly better (ties keep best = left/first)
        let pick = self.tmp();
        match *elem {
            Ty::Int => {
                let c = self.tmp();
                self.line(format!(
                    "{c} = call i32 @pyrs_int_cmp(i64 {cur}, i64 {best})"
                ));
                let pred = if is_max { "sgt" } else { "slt" };
                self.line(format!("{pick} = icmp {pred} i32 {c}, 0"));
            }
            Ty::Float => {
                let pred = if is_max { "ogt" } else { "olt" };
                self.line(format!("{pick} = fcmp {pred} double {cur}, {best}"));
            }
            Ty::Bool => {
                // bool stored as i1; treat as unsigned 0/1
                let pred = if is_max { "ugt" } else { "ult" };
                self.line(format!("{pick} = icmp {pred} i1 {cur}, {best}"));
            }
            other => unreachable!("min/max list element {other:?}"),
        }
        self.line(format!(
            "{best_next} = select i1 {pick}, {elty} {cur}, {elty} {best}"
        ));
        self.line(format!("{i_next} = add i64 {i}, 1"));
        self.line(format!("br label %{loop_l}"));

        self.start_block(&end_l);
        best
    }

    fn emit_math_call(&mut self, op: MathOp, arg: &Expr) -> String {
        let v = self.emit_expr(arg);
        let t = self.tmp();
        match op {
            MathOp::Sqrt => {
                self.line(format!("{t} = call double @llvm.sqrt.f64(double {v})"));
            }
            MathOp::Sin => {
                self.line(format!("{t} = call double @llvm.sin.f64(double {v})"));
            }
            MathOp::Cos => {
                self.line(format!("{t} = call double @llvm.cos.f64(double {v})"));
            }
            MathOp::Tan => {
                self.line(format!("{t} = call double @tan(double {v})"));
            }
            MathOp::Log => {
                self.line(format!("{t} = call double @llvm.log.f64(double {v})"));
            }
            MathOp::Log10 => {
                self.line(format!("{t} = call double @llvm.log10.f64(double {v})"));
            }
            MathOp::Exp => {
                self.line(format!("{t} = call double @llvm.exp.f64(double {v})"));
            }
            MathOp::Fabs => {
                self.line(format!("{t} = call double @llvm.fabs.f64(double {v})"));
            }
            MathOp::Floor => {
                let floored = self.tmp();
                self.line(format!(
                    "{floored} = call double @llvm.floor.f64(double {v})"
                ));
                // CPython math.floor → int (bigint-capable)
                self.line(format!(
                    "{t} = call i64 @pyrs_int_from_float(double {floored})"
                ));
            }
            MathOp::Ceil => {
                let ceiled = self.tmp();
                self.line(format!("{ceiled} = call double @llvm.ceil.f64(double {v})"));
                self.line(format!(
                    "{t} = call i64 @pyrs_int_from_float(double {ceiled})"
                ));
            }
        }
        t
    }

    /// Tag a machine i64 as a Python int (small or heap).
    fn emit_box_i64(&mut self, machine: &str) -> String {
        let t = self.tmp();
        self.line(format!("{t} = call i64 @pyrs_int_from_i64(i64 {machine})"));
        t
    }

    /// Unbox a Python int to a machine i64 (OverflowError if out of range).
    fn emit_unbox_i64(&mut self, tagged: &str) -> String {
        let t = self.tmp();
        self.line(format!("{t} = call i64 @pyrs_int_as_i64(i64 {tagged})"));
        t
    }

    /// Emit a tagged Python int constant from an IR `ConstInt` payload.
    fn emit_const_int(&mut self, v: i64) -> String {
        // Small range ±2^62: tag inline. Larger values go through runtime.
        const SMALL_MIN: i64 = -(1i64 << 62);
        const SMALL_MAX: i64 = (1i64 << 62) - 1;
        if (SMALL_MIN..=SMALL_MAX).contains(&v) {
            let tagged = ((v as u64) << 1) | 1;
            format!("{}", tagged as i64)
        } else {
            self.emit_box_i64(&v.to_string())
        }
    }

    /// Slice bound: missing bound uses untagged i64::MIN sentinel; else unbox.
    fn emit_slice_bound(&mut self, e: &Expr) -> String {
        if let ExprKind::ConstInt(v) = e.kind
            && v == i64::MIN
        {
            return i64::MIN.to_string();
        }
        let tagged = self.emit_expr(e);
        self.emit_unbox_i64(&tagged)
    }

    /// Inline list element addressing: negative-index adjustment, bounds
    /// check (trapping with `message`), then the slot address. Much faster
    /// than an out-of-line runtime call in hot loops.
    fn emit_list_elem_addr(&mut self, hdr: &str, index: &str, message: &str) -> String {
        let len = self.emit_len(hdr);
        let neg = self.tmp();
        self.line(format!("{neg} = icmp slt i64 {index}, 0"));
        let plus = self.tmp();
        self.line(format!("{plus} = add i64 {index}, {len}"));
        let adj = self.tmp();
        self.line(format!("{adj} = select i1 {neg}, i64 {plus}, i64 {index}"));
        let below = self.tmp();
        self.line(format!("{below} = icmp slt i64 {adj}, 0"));
        let above = self.tmp();
        self.line(format!("{above} = icmp sge i64 {adj}, {len}"));
        let oob = self.tmp();
        self.line(format!("{oob} = or i1 {below}, {above}"));
        let trap_l = self.fresh_block("idx.trap");
        let ok_l = self.fresh_block("idx.ok");
        self.line(format!("br i1 {oob}, label %{trap_l}, label %{ok_l}"));
        self.start_block(&trap_l);
        self.emit_die(message);
        self.start_block(&ok_l);
        // PyrsList layout: { i64 len; i64 cap; i64* data } — data at +16
        let data_pp = self.tmp();
        self.line(format!(
            "{data_pp} = getelementptr inbounds i8, ptr {hdr}, i64 16"
        ));
        let data_p = self.tmp();
        self.line(format!("{data_p} = load ptr, ptr {data_pp}"));
        let addr = self.tmp();
        self.line(format!(
            "{addr} = getelementptr inbounds i64, ptr {data_p}, i64 {adj}"
        ));
        addr
    }

    /// Trap with `message` and mark the block terminated.
    fn emit_die(&mut self, message: &str) {
        let msg = self.intern_string(message);
        // pyrs_die takes a plain C string: skip the 8-byte length header
        self.line(format!(
            "call void @pyrs_die(ptr getelementptr inbounds (i8, ptr {msg}, i64 8))"
        ));
        self.line("unreachable");
        self.terminated = true;
    }

    // ---- value slots (8-byte list elements) ----

    fn slot_from_value(&mut self, value: &str, ty: Ty) -> String {
        match ty {
            Ty::Int | Ty::Any => value.to_string(),
            Ty::Float => {
                let t = self.tmp();
                self.line(format!("{t} = bitcast double {value} to i64"));
                t
            }
            Ty::Bool => {
                let t = self.tmp();
                self.line(format!("{t} = zext i1 {value} to i64"));
                t
            }
            Ty::None => {
                // payload unused for None
                "0".to_string()
            }
            Ty::Str
            | Ty::List(_)
            | Ty::Tuple(_)
            | Ty::Dict { .. }
            | Ty::Set(_)
            | Ty::File
            | Ty::Closure { .. }
            | Ty::BoundMethod { .. }
            | Ty::Cell(_)
            | Ty::Generator { .. }
            | Ty::Exception
            | Ty::Class(_) => {
                let t = self.tmp();
                self.line(format!("{t} = ptrtoint ptr {value} to i64"));
                t
            }
            Ty::Union(members) => {
                // Heap-box: { i32 print_tag, i64 payload } so containers can print
                // without knowing the union's member list.
                let tag = self.tmp();
                self.line(format!("{tag} = extractvalue {{ i32, i64 }} {value}, 0"));
                let payload = self.tmp();
                self.line(format!(
                    "{payload} = extractvalue {{ i32, i64 }} {value}, 1"
                ));
                // Map member index → print tag via switch
                let print_tag = self.emit_union_index_to_print_tag(&tag, members);
                let box_p = self.tmp();
                self.line(format!("{box_p} = call ptr @malloc(i64 16)"));
                let tag_p = self.tmp();
                self.line(format!(
                    "{tag_p} = getelementptr inbounds {{ i32, i64 }}, ptr {box_p}, i32 0, i32 0"
                ));
                self.line(format!("store i32 {print_tag}, ptr {tag_p}"));
                let pay_p = self.tmp();
                self.line(format!(
                    "{pay_p} = getelementptr inbounds {{ i32, i64 }}, ptr {box_p}, i32 0, i32 1"
                ));
                self.line(format!("store i64 {payload}, ptr {pay_p}"));
                let slot = self.tmp();
                self.line(format!("{slot} = ptrtoint ptr {box_p} to i64"));
                slot
            }
        }
    }

    fn value_from_slot(&mut self, slot: &str, ty: Ty) -> String {
        match ty {
            Ty::Int | Ty::Any => slot.to_string(),
            Ty::Float => {
                let t = self.tmp();
                self.line(format!("{t} = bitcast i64 {slot} to double"));
                t
            }
            Ty::Bool => {
                let t = self.tmp();
                self.line(format!("{t} = trunc i64 {slot} to i1"));
                t
            }
            Ty::None => "0".to_string(),
            Ty::Str
            | Ty::List(_)
            | Ty::Tuple(_)
            | Ty::Dict { .. }
            | Ty::Set(_)
            | Ty::File
            | Ty::Closure { .. }
            | Ty::BoundMethod { .. }
            | Ty::Cell(_)
            | Ty::Generator { .. }
            | Ty::Exception
            | Ty::Class(_) => {
                let t = self.tmp();
                self.line(format!("{t} = inttoptr i64 {slot} to ptr"));
                t
            }
            Ty::Union(members) => {
                let box_p = self.tmp();
                self.line(format!("{box_p} = inttoptr i64 {slot} to ptr"));
                let tag_p = self.tmp();
                self.line(format!(
                    "{tag_p} = getelementptr inbounds {{ i32, i64 }}, ptr {box_p}, i32 0, i32 0"
                ));
                let print_tag = self.tmp();
                self.line(format!("{print_tag} = load i32, ptr {tag_p}"));
                let pay_p = self.tmp();
                self.line(format!(
                    "{pay_p} = getelementptr inbounds {{ i32, i64 }}, ptr {box_p}, i32 0, i32 1"
                ));
                let payload = self.tmp();
                self.line(format!("{payload} = load i64, ptr {pay_p}"));
                let member_idx = self.emit_union_print_tag_to_index(&print_tag, members);
                let u0 = self.tmp();
                self.line(format!(
                    "{u0} = insertvalue {{ i32, i64 }} undef, i32 {member_idx}, 0"
                ));
                let u1 = self.tmp();
                self.line(format!(
                    "{u1} = insertvalue {{ i32, i64 }} {u0}, i64 {payload}, 1"
                ));
                u1
            }
        }
    }

    /// Switch on union member index → print tag for the active member.
    fn emit_union_index_to_print_tag(&mut self, index: &str, members: &[Ty]) -> String {
        let end_l = self.fresh_block("utag.end");
        let mut preds = Vec::new();
        let default_l = self.fresh_block("utag.def");
        let mut cases = String::new();
        for (i, m) in members.iter().enumerate() {
            let bl = self.fresh_block(&format!("utag.{i}"));
            cases.push_str(&format!(" i32 {i}, label %{bl}"));
            // emit later
            preds.push((bl, member_print_tag(*m)));
        }
        self.line(format!("switch i32 {index}, label %{default_l} [{cases} ]"));
        let mut phi_args = Vec::new();
        for (bl, ptag) in &preds {
            self.start_block(bl);
            self.line(format!("br label %{end_l}"));
            phi_args.push(format!("[ {ptag}, %{bl} ]"));
        }
        self.start_block(&default_l);
        self.line(format!("br label %{end_l}"));
        phi_args.push(format!("[ -1, %{default_l} ]"));
        self.start_block(&end_l);
        let t = self.tmp();
        self.line(format!("{t} = phi i32 {}", phi_args.join(", ")));
        t
    }

    /// Switch on print tag → member index in the union.
    fn emit_union_print_tag_to_index(&mut self, print_tag: &str, members: &[Ty]) -> String {
        let end_l = self.fresh_block("uidx.end");
        let default_l = self.fresh_block("uidx.def");
        let mut cases = String::new();
        let mut preds = Vec::new();
        for (i, m) in members.iter().enumerate() {
            let bl = self.fresh_block(&format!("uidx.{i}"));
            let ptag = member_print_tag(*m);
            cases.push_str(&format!(" i32 {ptag}, label %{bl}"));
            preds.push((bl, i as i32));
        }
        self.line(format!(
            "switch i32 {print_tag}, label %{default_l} [{cases} ]"
        ));
        let mut phi_args = Vec::new();
        for (bl, idx) in &preds {
            self.start_block(bl);
            self.line(format!("br label %{end_l}"));
            phi_args.push(format!("[ {idx}, %{bl} ]"));
        }
        self.start_block(&default_l);
        self.line(format!("br label %{end_l}"));
        phi_args.push(format!("[ 0, %{default_l} ]"));
        self.start_block(&end_l);
        let t = self.tmp();
        self.line(format!("{t} = phi i32 {}", phi_args.join(", ")));
        t
    }

    /// Build a union SSA value from a concrete (or sub-union) value.
    fn emit_to_union(&mut self, value: &str, value_ty: Ty, union: Ty) -> String {
        let Ty::Union(members) = union else {
            unreachable!("emit_to_union on non-union");
        };
        if let Ty::Union(src_members) = value_ty {
            // Re-tag each possible source member into the destination union.
            let src_tag = self.tmp();
            self.line(format!(
                "{src_tag} = extractvalue {{ i32, i64 }} {value}, 0"
            ));
            let src_payload = self.tmp();
            self.line(format!(
                "{src_payload} = extractvalue {{ i32, i64 }} {value}, 1"
            ));
            let end_l = self.fresh_block("tounion.end");
            let default_l = self.fresh_block("tounion.def");
            let mut cases = String::new();
            let mut blocks = Vec::new();
            for (i, m) in src_members.iter().enumerate() {
                let bl = self.fresh_block(&format!("tounion.{i}"));
                cases.push_str(&format!(" i32 {i}, label %{bl}"));
                let dst_idx = members
                    .iter()
                    .position(|d| d == m)
                    .expect("sub-union member missing from target")
                    as i32;
                blocks.push((bl, dst_idx));
            }
            self.line(format!(
                "switch i32 {src_tag}, label %{default_l} [{cases} ]"
            ));
            let mut phi_args = Vec::new();
            for (bl, dst_idx) in &blocks {
                self.start_block(bl);
                let u0 = self.tmp();
                self.line(format!(
                    "{u0} = insertvalue {{ i32, i64 }} undef, i32 {dst_idx}, 0"
                ));
                let u1 = self.tmp();
                self.line(format!(
                    "{u1} = insertvalue {{ i32, i64 }} {u0}, i64 {src_payload}, 1"
                ));
                let pred = self.cur_block.clone();
                self.line(format!("br label %{end_l}"));
                phi_args.push((u1, pred));
            }
            self.start_block(&default_l);
            let u_def = self.tmp();
            self.line(format!(
                "{u_def} = insertvalue {{ i32, i64 }} undef, i32 0, 0"
            ));
            let u_def2 = self.tmp();
            self.line(format!(
                "{u_def2} = insertvalue {{ i32, i64 }} {u_def}, i64 0, 1"
            ));
            let def_pred = self.cur_block.clone();
            self.line(format!("br label %{end_l}"));
            phi_args.push((u_def2, def_pred));
            self.start_block(&end_l);
            let t = self.tmp();
            let parts: Vec<String> = phi_args
                .iter()
                .map(|(v, b)| format!("[ {v}, %{b} ]"))
                .collect();
            self.line(format!("{t} = phi {{ i32, i64 }} {}", parts.join(", ")));
            return t;
        }
        // Concrete member (including None)
        let idx = members
            .iter()
            .position(|m| *m == value_ty)
            .expect("value type not in union") as i32;
        let payload = self.slot_from_value(value, value_ty);
        let u0 = self.tmp();
        self.line(format!(
            "{u0} = insertvalue {{ i32, i64 }} undef, i32 {idx}, 0"
        ));
        let u1 = self.tmp();
        self.line(format!(
            "{u1} = insertvalue {{ i32, i64 }} {u0}, i64 {payload}, 1"
        ));
        u1
    }

    /// Box a concrete/union value into `Ty::Any` (heap `{print_tag, payload}` as i64).
    fn emit_to_any(&mut self, value: &str, value_ty: Ty) -> String {
        if value_ty == Ty::Any {
            return value.to_string();
        }
        // Reuse container-union boxing: slot_from_value for Union already builds
        // a heap box; for concrete types, build the box with print_tag + payload.
        if let Ty::Union(members) = value_ty {
            return self.slot_from_value(value, Ty::Union(members));
        }
        let print_tag = member_print_tag(value_ty);
        let payload = self.slot_from_value(value, value_ty);
        let box_p = self.tmp();
        self.line(format!("{box_p} = call ptr @malloc(i64 16)"));
        let tag_p = self.tmp();
        self.line(format!(
            "{tag_p} = getelementptr inbounds {{ i32, i64 }}, ptr {box_p}, i32 0, i32 0"
        ));
        self.line(format!("store i32 {print_tag}, ptr {tag_p}"));
        let pay_p = self.tmp();
        self.line(format!(
            "{pay_p} = getelementptr inbounds {{ i32, i64 }}, ptr {box_p}, i32 0, i32 1"
        ));
        self.line(format!("store i64 {payload}, ptr {pay_p}"));
        let slot = self.tmp();
        self.line(format!("{slot} = ptrtoint ptr {box_p} to i64"));
        slot
    }

    /// Load print_tag + payload from an Any i64 slot. Null slot → tag -1 (None).
    fn emit_any_unpack(&mut self, any_slot: &str) -> (String, String) {
        let is_null = self.tmp();
        self.line(format!("{is_null} = icmp eq i64 {any_slot}, 0"));
        let end_l = self.fresh_block("any.unpack.end");
        let null_l = self.fresh_block("any.unpack.null");
        let box_l = self.fresh_block("any.unpack.box");
        self.line(format!("br i1 {is_null}, label %{null_l}, label %{box_l}"));
        self.start_block(&null_l);
        let npred = self.cur_block.clone();
        self.line(format!("br label %{end_l}"));
        self.start_block(&box_l);
        let box_p = self.tmp();
        self.line(format!("{box_p} = inttoptr i64 {any_slot} to ptr"));
        let tag_p = self.tmp();
        self.line(format!(
            "{tag_p} = getelementptr inbounds {{ i32, i64 }}, ptr {box_p}, i32 0, i32 0"
        ));
        let tag_v = self.tmp();
        self.line(format!("{tag_v} = load i32, ptr {tag_p}"));
        let pay_p = self.tmp();
        self.line(format!(
            "{pay_p} = getelementptr inbounds {{ i32, i64 }}, ptr {box_p}, i32 0, i32 1"
        ));
        let pay_v = self.tmp();
        self.line(format!("{pay_v} = load i64, ptr {pay_p}"));
        let bpred = self.cur_block.clone();
        self.line(format!("br label %{end_l}"));
        self.start_block(&end_l);
        let print_tag = self.tmp();
        self.line(format!(
            "{print_tag} = phi i32 [ -1, %{npred} ], [ {tag_v}, %{bpred} ]"
        ));
        let payload = self.tmp();
        self.line(format!(
            "{payload} = phi i64 [ 0, %{npred} ], [ {pay_v}, %{bpred} ]"
        ));
        (print_tag, payload)
    }

    /// Convert a boxed payload (slot bits) from `src_tag` into `target` value.
    /// Caller has already checked that the tag is acceptable for `target`.
    fn emit_payload_as(&mut self, payload: &str, src_tag: i32, target: Ty) -> String {
        match target {
            Ty::Int => {
                if src_tag == 2 {
                    // bool payload 0/1 → tagged small int
                    let t = self.tmp();
                    self.line(format!("{t} = icmp ne i64 {payload}, 0"));
                    let r = self.tmp();
                    self.line(format!("{r} = select i1 {t}, i64 3, i64 1"));
                    r
                } else {
                    payload.to_string()
                }
            }
            Ty::Float => {
                if src_tag == 1 {
                    self.value_from_slot(payload, Ty::Float)
                } else if src_tag == 2 {
                    let as_int = self.tmp();
                    self.line(format!("{as_int} = icmp ne i64 {payload}, 0"));
                    let tagged = self.tmp();
                    self.line(format!("{tagged} = select i1 {as_int}, i64 3, i64 1"));
                    let t = self.tmp();
                    self.line(format!(
                        "{t} = call double @pyrs_int_to_float(i64 {tagged})"
                    ));
                    t
                } else {
                    // int payload
                    let t = self.tmp();
                    self.line(format!(
                        "{t} = call double @pyrs_int_to_float(i64 {payload})"
                    ));
                    t
                }
            }
            Ty::Bool => {
                let t = self.tmp();
                self.line(format!("{t} = trunc i64 {payload} to i1"));
                t
            }
            other => self.value_from_slot(payload, other),
        }
    }

    /// Whether print_tag `src` can satisfy target type (with promotions).
    fn from_any_tag_ok(src: i32, target: Ty) -> bool {
        match target {
            Ty::Int => src == 0 || src == 2, // int or bool
            Ty::Float => src == 0 || src == 1 || src == 2,
            Ty::Bool => src == 2,
            Ty::None => src == -1,
            Ty::Str => src == 3,
            Ty::List(_) => src == member_print_tag(target),
            Ty::Tuple(_) => src == 5,
            Ty::Dict { .. } => src == 6,
            Ty::Set(_) => src == 7,
            Ty::Closure { .. } | Ty::BoundMethod { .. } => src == 9,
            Ty::Generator { .. } => src == 10,
            Ty::Exception => src == 11,
            Ty::Class(_) => src >= 13 && (src - 13) % 8 == 0,
            Ty::Any => true,
            Ty::Union(_) | Ty::File | Ty::Cell(_) => false,
        }
    }

    /// Unbox `Ty::Any` to a concrete target type with a runtime tag check.
    fn emit_from_any(&mut self, any_slot: &str, target: Ty) -> String {
        if target == Ty::Any {
            return any_slot.to_string();
        }
        let (print_tag, payload) = self.emit_any_unpack(any_slot);

        // Target is a union: match active print_tag to a member (with promotions).
        if let Ty::Union(members) = target {
            let end_l = self.fresh_block("fromany.union.end");
            let bad_l = self.fresh_block("fromany.union.bad");
            let mut cases = String::new();
            let mut blocks: Vec<(String, usize, i32)> = Vec::new();
            // Collect all acceptable (print_tag → member_idx, src_tag) pairs.
            // Prefer exact member tags; also allow bool→int / int→float into members.
            let mut seen_tags: std::collections::HashSet<i32> = std::collections::HashSet::new();
            for (mi, m) in members.iter().enumerate() {
                if let Ty::Class(want) = *m {
                    for sid in 0..self.classes.len() as u32 {
                        let mut cur = sid;
                        let mut is_sub = cur == want;
                        while !is_sub {
                            let Some(info) = self.classes.get(cur as usize) else {
                                break;
                            };
                            match info.parent {
                                Some(p) if p == want => {
                                    is_sub = true;
                                    break;
                                }
                                Some(p) => cur = p,
                                None => break,
                            }
                        }
                        if !is_sub {
                            continue;
                        }
                        let ptag = member_print_tag(Ty::Class(sid));
                        if seen_tags.insert(ptag) {
                            let bl = self.fresh_block(&format!("fromany.u.{mi}.c{sid}"));
                            cases.push_str(&format!(" i32 {ptag}, label %{bl}"));
                            blocks.push((bl, mi, ptag));
                        }
                    }
                    continue;
                }
                // Exact and promoted tags for this member.
                let candidates: Vec<i32> = match *m {
                    Ty::Int => vec![0, 2],
                    Ty::Float => vec![0, 1, 2],
                    Ty::Bool => vec![2],
                    Ty::None => vec![-1],
                    other => vec![member_print_tag(other)],
                };
                for ptag in candidates {
                    if !Self::from_any_tag_ok(ptag, *m) {
                        continue;
                    }
                    if seen_tags.insert(ptag) {
                        let bl = self.fresh_block(&format!("fromany.u.{mi}.t{ptag}"));
                        cases.push_str(&format!(" i32 {ptag}, label %{bl}"));
                        blocks.push((bl, mi, ptag));
                    }
                }
            }
            self.line(format!("switch i32 {print_tag}, label %{bad_l} [{cases} ]"));
            let mut phi_args = Vec::new();
            for (bl, mi, src_tag) in &blocks {
                self.start_block(bl);
                let m = members[*mi];
                let val = self.emit_payload_as(&payload, *src_tag, m);
                let slot = self.slot_from_value(&val, m);
                let u0 = self.tmp();
                self.line(format!(
                    "{u0} = insertvalue {{ i32, i64 }} undef, i32 {}, 0",
                    *mi as i32
                ));
                let u1 = self.tmp();
                self.line(format!(
                    "{u1} = insertvalue {{ i32, i64 }} {u0}, i64 {slot}, 1"
                ));
                let pred = self.cur_block.clone();
                self.line(format!("br label %{end_l}"));
                phi_args.push((u1, pred));
            }
            self.start_block(&bad_l);
            self.emit_die(&format!(
                "TypeError: expected {target}, got incompatible dynamic value"
            ));
            self.start_block(&end_l);
            let t = self.tmp();
            let parts: Vec<String> = phi_args
                .iter()
                .map(|(v, b)| format!("[ {v}, %{b} ]"))
                .collect();
            self.line(format!("{t} = phi {{ i32, i64 }} {}", parts.join(", ")));
            return t;
        }

        // Class targets: accept base and any subclass print tags (13+8*id).
        if let Ty::Class(want) = target {
            let end_l = self.fresh_block("fromany.class.end");
            let bad_l = self.fresh_block("fromany.class.bad");
            let mut ok_blocks = Vec::new();
            let mut cases = String::new();
            let class_name = self
                .classes
                .get(want as usize)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| format!("class#{want}"));
            for sid in 0..self.classes.len() as u32 {
                let mut cur = sid;
                let mut is_sub = cur == want;
                while !is_sub {
                    let Some(info) = self.classes.get(cur as usize) else {
                        break;
                    };
                    match info.parent {
                        Some(p) if p == want => {
                            is_sub = true;
                            break;
                        }
                        Some(p) => cur = p,
                        None => break,
                    }
                }
                if !is_sub {
                    continue;
                }
                let ptag = member_print_tag(Ty::Class(sid));
                let bl = self.fresh_block(&format!("fromany.class.{sid}"));
                cases.push_str(&format!(" i32 {ptag}, label %{bl}"));
                ok_blocks.push(bl);
            }
            self.line(format!("switch i32 {print_tag}, label %{bad_l} [{cases} ]"));
            for bl in &ok_blocks {
                self.start_block(bl);
                self.line(format!("br label %{end_l}"));
            }
            self.start_block(&bad_l);
            self.emit_die(&format!(
                "TypeError: expected {class_name}, got incompatible dynamic value"
            ));
            self.start_block(&end_l);
            return self.value_from_slot(&payload, target);
        }

        // Scalar / container: exact or promoted tags.
        let end_l = self.fresh_block("fromany.end");
        let bad_l = self.fresh_block("fromany.bad");
        let mut cases = String::new();
        let mut ok_blocks: Vec<(String, i32)> = Vec::new();
        let accept: Vec<i32> = match target {
            Ty::Int => vec![0, 2],
            Ty::Float => vec![0, 1, 2],
            Ty::Bool => vec![2],
            Ty::None => vec![-1],
            // list[Any] only: exact TAG_UNION list encoding (elem_tag Any = 8).
            // Monomorphic lists keep raw slots and must not be retyped as list[Any].
            other => vec![member_print_tag(other)],
        };
        for ptag in &accept {
            let bl = self.fresh_block(&format!("fromany.ok.{ptag}"));
            cases.push_str(&format!(" i32 {ptag}, label %{bl}"));
            ok_blocks.push((bl, *ptag));
        }
        self.line(format!("switch i32 {print_tag}, label %{bad_l} [{cases} ]"));
        let mut phi_args = Vec::new();
        for (bl, src_tag) in &ok_blocks {
            self.start_block(bl);
            let val = self.emit_payload_as(&payload, *src_tag, target);
            let pred = self.cur_block.clone();
            self.line(format!("br label %{end_l}"));
            phi_args.push((val, pred));
        }
        self.start_block(&bad_l);
        self.emit_die(&format!(
            "TypeError: expected {target}, got incompatible dynamic value"
        ));
        self.start_block(&end_l);
        let t = self.tmp();
        let parts: Vec<String> = phi_args
            .iter()
            .map(|(v, b)| format!("[ {v}, %{b} ]"))
            .collect();
        self.line(format!("{t} = phi {} {}", lty(target), parts.join(", ")));
        t
    }

    /// `isinstance` on a dynamic Any value: match box print_tag / class payload.
    fn emit_isinstance_any(
        &mut self,
        any_slot: &str,
        type_tags: &[i32],
        bool_is_int: bool,
        exc_filters: &[i32],
        class_filters: &[u32],
    ) -> String {
        let (print_tag, payload) = self.emit_any_unpack(any_slot);
        let mut acc: Option<String> = None;
        for &want in type_tags {
            let cmp = self.tmp();
            if want == 4 {
                // any list: (tag % 8) == 4 && tag >= 4
                let ge = self.tmp();
                self.line(format!("{ge} = icmp sge i32 {print_tag}, 4"));
                let rem = self.tmp();
                self.line(format!("{rem} = srem i32 {print_tag}, 8"));
                let eq4 = self.tmp();
                self.line(format!("{eq4} = icmp eq i32 {rem}, 4"));
                self.line(format!("{cmp} = and i1 {ge}, {eq4}"));
            } else if want == 0 && bool_is_int {
                let is_int = self.tmp();
                let is_bool = self.tmp();
                self.line(format!("{is_int} = icmp eq i32 {print_tag}, 0"));
                self.line(format!("{is_bool} = icmp eq i32 {print_tag}, 2"));
                self.line(format!("{cmp} = or i1 {is_int}, {is_bool}"));
            } else {
                self.line(format!("{cmp} = icmp eq i32 {print_tag}, {want}"));
            }
            acc = Some(match acc {
                None => cmp,
                Some(prev) => {
                    let or = self.tmp();
                    self.line(format!("{or} = or i1 {prev}, {cmp}"));
                    or
                }
            });
        }
        // Class filters: if print_tag is a class tag, check hierarchy on payload ptr.
        if !class_filters.is_empty() {
            let is_class = self.tmp();
            // tag >= 13 && (tag - 13) % 8 == 0
            let ge = self.tmp();
            self.line(format!("{ge} = icmp sge i32 {print_tag}, 13"));
            let sub = self.tmp();
            self.line(format!("{sub} = sub i32 {print_tag}, 13"));
            let rem = self.tmp();
            self.line(format!("{rem} = srem i32 {sub}, 8"));
            let eq0 = self.tmp();
            self.line(format!("{eq0} = icmp eq i32 {rem}, 0"));
            self.line(format!("{is_class} = and i1 {ge}, {eq0}"));
            let yes_l = self.fresh_block("isinstance.any.class.yes");
            let no_l = self.fresh_block("isinstance.any.class.no");
            let end_l = self.fresh_block("isinstance.any.class.end");
            self.line(format!("br i1 {is_class}, label %{yes_l}, label %{no_l}"));
            self.start_block(&yes_l);
            let obj = self.tmp();
            self.line(format!("{obj} = inttoptr i64 {payload} to ptr"));
            let n = self.classes.len() as i64;
            let mut class_hit: Option<String> = None;
            for &cid in class_filters {
                let m = self.tmp();
                self.line(format!(
                    "{m} = call i32 @pyrs_isinstance_class(ptr {obj}, i64 {}, ptr @pyrs_class_parents, i64 {n})",
                    cid as i64
                ));
                let cmp = self.tmp();
                self.line(format!("{cmp} = icmp ne i32 {m}, 0"));
                class_hit = Some(match class_hit {
                    None => cmp,
                    Some(prev) => {
                        let or = self.tmp();
                        self.line(format!("{or} = or i1 {prev}, {cmp}"));
                        or
                    }
                });
            }
            let ch = class_hit.unwrap_or_else(|| "false".to_string());
            let ypred = self.cur_block.clone();
            self.line(format!("br label %{end_l}"));
            self.start_block(&no_l);
            let npred = self.cur_block.clone();
            self.line(format!("br label %{end_l}"));
            self.start_block(&end_l);
            let class_res = self.tmp();
            self.line(format!(
                "{class_res} = phi i1 [ {ch}, %{ypred} ], [ false, %{npred} ]"
            ));
            acc = Some(match acc {
                None => class_res,
                Some(prev) => {
                    let or = self.tmp();
                    self.line(format!("{or} = or i1 {prev}, {class_res}"));
                    or
                }
            });
        }
        // Exception filters on Any: if tag 11, check hierarchy.
        if !exc_filters.is_empty() {
            let is_exc = self.tmp();
            self.line(format!("{is_exc} = icmp eq i32 {print_tag}, 11"));
            let yes_l = self.fresh_block("isinstance.any.exc.yes");
            let no_l = self.fresh_block("isinstance.any.exc.no");
            let end_l = self.fresh_block("isinstance.any.exc.end");
            self.line(format!("br i1 {is_exc}, label %{yes_l}, label %{no_l}"));
            self.start_block(&yes_l);
            let exc_ptr = self.tmp();
            self.line(format!("{exc_ptr} = inttoptr i64 {payload} to ptr"));
            let mut exc_hit: Option<String> = None;
            for &f in exc_filters {
                let m = self.tmp();
                self.line(format!(
                    "{m} = call i32 @pyrs_exc_isinstance(ptr {exc_ptr}, i32 {f})"
                ));
                let cmp = self.tmp();
                self.line(format!("{cmp} = icmp ne i32 {m}, 0"));
                exc_hit = Some(match exc_hit {
                    None => cmp,
                    Some(prev) => {
                        let or = self.tmp();
                        self.line(format!("{or} = or i1 {prev}, {cmp}"));
                        or
                    }
                });
            }
            let eh = exc_hit.unwrap_or_else(|| "false".to_string());
            let ypred = self.cur_block.clone();
            self.line(format!("br label %{end_l}"));
            self.start_block(&no_l);
            let npred = self.cur_block.clone();
            self.line(format!("br label %{end_l}"));
            self.start_block(&end_l);
            let exc_res = self.tmp();
            self.line(format!(
                "{exc_res} = phi i1 [ {eh}, %{ypred} ], [ false, %{npred} ]"
            ));
            acc = Some(match acc {
                None => exc_res,
                Some(prev) => {
                    let or = self.tmp();
                    self.line(format!("{or} = or i1 {prev}, {exc_res}"));
                    or
                }
            });
        }
        acc.unwrap_or_else(|| "false".to_string())
    }

    /// Runtime type_id switch for exclusive subclass fields.
    fn emit_get_field_partial(
        &mut self,
        obj: &str,
        candidates: &[(u32, u32)],
        attr: &str,
        field_ty: Ty,
    ) -> String {
        let tid_p = self.tmp();
        self.line(format!(
            "{tid_p} = getelementptr inbounds {{ i64 }}, ptr {obj}, i32 0, i32 0"
        ));
        let tid = self.tmp();
        self.line(format!("{tid} = load i64, ptr {tid_p}"));
        let end_l = self.fresh_block("gfp.end");
        let default_l = self.fresh_block("gfp.def");
        let cand_set: std::collections::HashSet<u32> = candidates.iter().map(|(c, _)| *c).collect();
        let mut cases = String::new();
        let mut ok_blocks: Vec<(String, u32, u32)> = Vec::new();
        let mut err_blocks: Vec<(String, String)> = Vec::new();
        for (cid, fidx) in candidates {
            let bl = self.fresh_block(&format!("gfp.{cid}"));
            cases.push_str(&format!(" i64 {}, label %{bl}", *cid as i64));
            ok_blocks.push((bl, *cid, *fidx));
        }
        let err_classes: Vec<(u32, String)> = self
            .classes
            .iter()
            .filter(|c| !cand_set.contains(&c.id))
            .map(|c| (c.id, c.name.clone()))
            .collect();
        for (cid, name) in err_classes {
            let bl = self.fresh_block(&format!("gfp.err.{cid}"));
            cases.push_str(&format!(" i64 {}, label %{bl}", cid as i64));
            err_blocks.push((bl, name));
        }
        self.line(format!("switch i64 {tid}, label %{default_l} [{cases} ]"));
        let mut phi_args = Vec::new();
        for (bl, cid, fidx) in &ok_blocks {
            self.start_block(bl);
            let info = self
                .classes
                .get(*cid as usize)
                .expect("GetFieldPartial class");
            let sty = class_struct_ty(info);
            let idx = *fidx as i32 + 1;
            let fp = self.tmp();
            self.line(format!(
                "{fp} = getelementptr inbounds {sty}, ptr {obj}, i32 0, i32 {idx}"
            ));
            let val = self.tmp();
            self.line(format!("{val} = load {}, ptr {fp}", lty(field_ty)));
            let pred = self.cur_block.clone();
            self.line(format!("br label %{end_l}"));
            phi_args.push((val, pred));
        }
        for (bl, name) in &err_blocks {
            self.start_block(bl);
            self.emit_die(&format!(
                "AttributeError: '{name}' object has no attribute '{attr}'"
            ));
        }
        self.start_block(&default_l);
        self.emit_die(&format!("AttributeError: object has no attribute '{attr}'"));
        self.start_block(&end_l);
        let t = self.tmp();
        let parts: Vec<String> = phi_args
            .iter()
            .map(|(v, b)| format!("[ {v}, %{b} ]"))
            .collect();
        self.line(format!("{t} = phi {} {}", lty(field_ty), parts.join(", ")));
        t
    }

    /// Call a mangled method/function with already-emitted arg values.
    fn emit_direct_method_call(
        &mut self,
        func: &str,
        arg_vals: &[(String, Ty)],
        ret: Ty,
    ) -> String {
        let args_str = arg_vals
            .iter()
            .map(|(v, ty)| format!("{} {v}", lty(*ty)))
            .collect::<Vec<_>>()
            .join(", ");
        let callee = mangle(func);
        if ret == Ty::None {
            self.line(format!("call void @{callee}({args_str})"));
            "0".to_string()
        } else {
            let t = self.tmp();
            self.line(format!("{t} = call {} @{callee}({args_str})", lty_ret(ret)));
            t
        }
    }

    /// Print a value of any supported type (including None and unions).
    fn emit_print_value(&mut self, v: &str, ty: Ty) {
        match ty {
            Ty::Int => self.line(format!("call void @pyrs_print_int(i64 {v})")),
            Ty::Float => self.line(format!("call void @pyrs_print_float(double {v})")),
            Ty::Bool => {
                let ext = self.tmp();
                self.line(format!("{ext} = zext i1 {v} to i32"));
                self.line(format!("call void @pyrs_print_bool(i32 {ext})"));
            }
            Ty::Str => self.line(format!("call void @pyrs_print_str(ptr {v})")),
            Ty::Exception => self.line(format!("call void @pyrs_print_exc(ptr {v})")),
            Ty::Class(_) => {
                // Runtime type_id → class display name (needs pyrs_set_class_names).
                self.line(format!("call void @pyrs_print_class_instance(ptr {v})"));
            }
            Ty::List(elem) => self.line(format!(
                "call void @pyrs_print_list(ptr {v}, i32 {})",
                elem_tag(elem)
            )),
            Ty::Tuple(_) => self.line(format!("call void @pyrs_print_tuple(ptr {v})")),
            Ty::Dict { .. } => self.line(format!("call void @pyrs_print_dict(ptr {v})")),
            Ty::Set(_) => self.line(format!("call void @pyrs_print_set(ptr {v})")),
            Ty::Closure { .. } | Ty::BoundMethod { .. } => {
                let s = self.intern_string("<function>");
                self.line(format!("call void @pyrs_print_str(ptr {s})"));
            }
            Ty::Cell(_) | Ty::Generator { .. } => {
                let s = self.intern_string("<object>");
                self.line(format!("call void @pyrs_print_str(ptr {s})"));
            }
            Ty::None => {
                let s = self.intern_string("None");
                self.line(format!("call void @pyrs_print_str(ptr {s})"));
            }
            Ty::Union(members) => {
                let tag = self.tmp();
                self.line(format!("{tag} = extractvalue {{ i32, i64 }} {v}, 0"));
                let payload = self.tmp();
                self.line(format!("{payload} = extractvalue {{ i32, i64 }} {v}, 1"));
                let end_l = self.fresh_block("uprint.end");
                let default_l = self.fresh_block("uprint.def");
                let mut cases = String::new();
                let mut blocks = Vec::new();
                for (i, m) in members.iter().enumerate() {
                    let bl = self.fresh_block(&format!("uprint.{i}"));
                    cases.push_str(&format!(" i32 {i}, label %{bl}"));
                    blocks.push((bl, *m));
                }
                self.line(format!("switch i32 {tag}, label %{default_l} [{cases} ]"));
                for (bl, m) in &blocks {
                    self.start_block(bl);
                    let val = self.value_from_slot(&payload, *m);
                    self.emit_print_value(&val, *m);
                    self.line(format!("br label %{end_l}"));
                }
                self.start_block(&default_l);
                self.line(format!("br label %{end_l}"));
                self.start_block(&end_l);
            }
            Ty::Any => {
                // Any is a heap box {print_tag, payload}; print via TAG_UNION path.
                self.line(format!("call void @pyrs_print_any(i64 {v})"));
            }
            Ty::File => unreachable!("semantic rejects file print"),
        }
    }

    // ---- functions ----

    fn emit_function(&mut self, func: &Function) {
        if func.is_generator {
            self.emit_generator_function(func);
            return;
        }
        self.body.clear();
        self.tmp = 0;
        self.blk = 0;
        self.loops.clear();
        self.tries.clear();
        self.fn_ret = func.ret;
        self.try_ret_ptr = None;
        self.local_storage.clear();
        for (name, ty) in &func.params {
            self.local_storage.insert(name.clone(), *ty);
        }
        for (name, ty) in &func.locals {
            self.local_storage.insert(name.clone(), *ty);
        }

        self.start_block("entry");
        // spill params into allocas so assignment to params just works
        for (name, ty) in &func.params {
            self.line(format!("%v.{name} = alloca {}", lty(*ty)));
            self.line(format!("store {} %p.{name}, ptr %v.{name}", lty(*ty)));
        }
        // all locals up front, zero/null-initialized (a conditionally
        // assigned variable reads as 0/0.0/False/null instead of being UB;
        // the runtime traps on null str/list use)
        for (name, ty) in &func.locals {
            self.line(format!("%v.{name} = alloca {}", lty(*ty)));
            let zero = match ty {
                Ty::Float => fconst(0.0),
                Ty::Int => "1".to_string(), // tagged small 0
                Ty::Str
                | Ty::List(_)
                | Ty::Tuple(_)
                | Ty::Dict { .. }
                | Ty::Set(_)
                | Ty::File
                | Ty::Closure { .. }
                | Ty::BoundMethod { .. }
                | Ty::Cell(_)
                | Ty::Generator { .. }
                | Ty::Exception
                | Ty::Class(_) => "null".to_string(),
                Ty::Union(_) => "zeroinitializer".to_string(),
                _ => "0".to_string(),
            };
            self.line(format!("store {} {zero}, ptr %v.{name}", lty(*ty)));
        }
        // pending return value for try/finally (only if the function returns)
        if func.ret != Ty::None {
            let p = "%try.retval".to_string();
            self.line(format!("{p} = alloca {}", lty(func.ret)));
            self.try_ret_ptr = Some(p);
        }

        self.emit_block(&func.body);

        if !self.terminated {
            if func.ret == Ty::None {
                self.line("ret void");
            } else {
                // semantic proved all paths return; this block is unreachable
                self.line("unreachable");
            }
        }

        let params = func
            .params
            .iter()
            .map(|(name, ty)| format!("{} %p.{name}", lty(*ty)))
            .collect::<Vec<_>>()
            .join(", ");
        self.funcs.push_str(&format!(
            "define {} @{}({params}) {{\n{}}}\n\n",
            lty_ret(func.ret),
            mangle(&func.name),
            self.body
        ));
    }

    /// Emit a generator resume function: `i32 @name(ptr %gen)`.
    /// Locals live in the gen frame; Yield stores value+state and returns 0;
    /// falling off the end returns 1 (done).
    fn emit_generator_function(&mut self, func: &Function) {
        self.body.clear();
        self.tmp = 0;
        self.blk = 0;
        self.loops.clear();
        self.tries.clear();
        self.fn_ret = Ty::Int;
        self.try_ret_ptr = None;
        self.gen_frame = Some("%gen".to_string());
        self.gen_local_index.clear();
        self.gen_fin_stack.clear();
        self.gen_yield_ty = func.yield_ty.unwrap_or(Ty::Int);
        self.local_storage.clear();
        for (name, ty) in &func.params {
            self.local_storage.insert(name.clone(), *ty);
        }
        for (name, ty) in &func.locals {
            self.local_storage.insert(name.clone(), *ty);
        }

        // Frame layout: params first, then locals
        let mut idx = 0i64;
        for (name, _) in &func.params {
            self.gen_local_index.insert(name.clone(), idx);
            idx += 1;
        }
        for (name, _) in &func.locals {
            self.gen_local_index.insert(name.clone(), idx);
            idx += 1;
        }
        // Count yields for switch
        let yield_count = count_yields_in_stmts(&func.body);
        self.start_block("entry");
        // Preallocate try control allocas so they dominate every gstate
        // (needed when yield resumes re-arm setjmp using the same slots).
        let try_depth = max_try_depth_in_stmts(&func.body).max(1);
        self.gen_try_pool.clear();
        self.gen_try_pool_next = 0;
        for i in 0..try_depth {
            let e = format!("%gen.try.exit.{i}");
            let l = format!("%gen.try.live.{i}");
            let p = format!("%gen.try.phase.{i}");
            self.line(format!("{e} = alloca i32, align 4"));
            self.line(format!("{l} = alloca i32, align 4"));
            self.line(format!("{p} = alloca i32, align 4"));
            self.gen_try_pool.push((e, l, p));
        }
        // Already finished → never re-enter body (post-exhaust / post-close /
        // uncaught throw). Close() and send/next rely on this.
        let already = self.tmp();
        self.line(format!("{already} = call i32 @pyrs_gen_done(ptr %gen)"));
        let is_done0 = self.tmp();
        self.line(format!("{is_done0} = icmp ne i32 {already}, 0"));
        let go_switch = self.fresh_block("gswitch");
        let done_early = self.fresh_block("gdone.early");
        self.line(format!(
            "br i1 {is_done0}, label %{done_early}, label %{go_switch}"
        ));
        self.start_block(&done_early);
        self.line("ret i32 1");
        self.start_block(&go_switch);
        // switch on state
        let st = self.tmp();
        self.line(format!("{st} = call i64 @pyrs_gen_state(ptr %gen)"));
        let mut cases = String::new();
        for i in 0..=yield_count {
            let lab = format!("gstate{i}");
            cases.push_str(&format!(" i64 {i}, label %{lab}"));
        }
        let bad = self.fresh_block("gbad");
        self.line(format!("switch i64 {st}, label %{bad} [{cases} ]"));
        self.start_block(&bad);
        self.line("call void @pyrs_gen_set_done(ptr %gen)");
        self.line("ret i32 1");
        // state 0 = start
        self.start_block("gstate0");
        self.gen_next_state = 1;
        self.gen_try_pool_next = 0;
        self.emit_block(&func.body);
        if !self.terminated {
            self.line("call void @pyrs_gen_set_done(ptr %gen)");
            self.line("ret i32 1");
            self.terminated = true;
        }
        // Ensure all gstateN labels exist (empty ones fall through to done)
        for i in 1..=yield_count {
            let lab = format!("gstate{i}");
            // if never started (no yield reached this state as resume), still define
            if !self.body.contains(&format!("\n{lab}:\n"))
                && !self.body.starts_with(&format!("{lab}:\n"))
            {
                self.start_block(&lab);
                self.line("call void @pyrs_gen_set_done(ptr %gen)");
                self.line("ret i32 1");
            }
        }
        self.gen_frame = None;
        self.funcs.push_str(&format!(
            "define i32 @{}(ptr %gen) {{\n{}}}\n\n",
            mangle(&func.name),
            self.body
        ));
    }

    fn emit_block(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            if self.terminated {
                // code after return/break/continue is unreachable; drop it
                break;
            }
            self.emit_stmt(stmt);
        }
    }

    fn emit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Assign { name, value } => {
                let v = self.emit_expr(value);
                if let (Some(frame), Some(idx)) = (
                    self.gen_frame.clone(),
                    self.gen_local_index.get(name).copied(),
                ) {
                    let slot = self.slot_from_value(&v, value.ty);
                    self.line(format!(
                        "call void @pyrs_gen_set_local(ptr {frame}, i64 {idx}, i64 {slot})"
                    ));
                } else {
                    self.line(format!("store {} {v}, ptr %v.{name}", lty(value.ty)));
                }
            }
            Stmt::GlobalAssign { name, value } => {
                let v = self.emit_expr(value);
                self.line(format!("store {} {v}, ptr @g.{name}", lty(value.ty)));
            }
            Stmt::IndexAssign { base, index, value } => {
                let b = self.emit_expr(base);
                let i_tagged = self.emit_expr(index);
                let v = self.emit_expr(value);
                let slot = self.slot_from_value(&v, value.ty);
                match base.ty {
                    Ty::List(_) => {
                        let i = self.emit_unbox_i64(&i_tagged);
                        let addr = self.emit_list_elem_addr(
                            &b,
                            &i,
                            "IndexError: list assignment index out of range",
                        );
                        self.line(format!("store i64 {slot}, ptr {addr}"));
                    }
                    Ty::Dict { key, value: val } => {
                        let kslot = self.slot_from_value(&i_tagged, *key);
                        let vslot = slot;
                        self.line(format!(
                            "call void @pyrs_dict_set(ptr {b}, i64 {kslot}, i32 {}, i64 {vslot}, i32 {})",
                            elem_tag(key),
                            elem_tag(val)
                        ));
                    }
                    other => unreachable!("IndexAssign on {other:?}"),
                }
            }
            Stmt::IndexDelete { base, index } => {
                let b = self.emit_expr(base);
                let i = self.emit_expr(index);
                let Ty::Dict { key, .. } = base.ty else {
                    unreachable!("IndexDelete on non-dict");
                };
                let kslot = self.slot_from_value(&i, *key);
                self.line(format!(
                    "call void @pyrs_dict_del(ptr {b}, i64 {kslot}, i32 {})",
                    elem_tag(key)
                ));
            }
            Stmt::DictClear { dict } => {
                let d = self.emit_expr(dict);
                self.line(format!("call void @pyrs_dict_clear(ptr {d})"));
            }
            Stmt::DictUpdate { dict, other } => {
                let d = self.emit_expr(dict);
                let o = self.emit_expr(other);
                self.line(format!("call void @pyrs_dict_update(ptr {d}, ptr {o})"));
            }
            Stmt::SetAdd { set, value } => {
                let s = self.emit_expr(set);
                let v = self.emit_expr(value);
                let slot = self.slot_from_value(&v, value.ty);
                self.line(format!(
                    "call void @pyrs_set_add(ptr {s}, i64 {slot}, i32 {})",
                    elem_tag(&value.ty)
                ));
            }
            Stmt::SetRemove { set, value } => {
                let s = self.emit_expr(set);
                let v = self.emit_expr(value);
                let slot = self.slot_from_value(&v, value.ty);
                self.line(format!(
                    "call void @pyrs_set_remove(ptr {s}, i64 {slot}, i32 {})",
                    elem_tag(&value.ty)
                ));
            }
            Stmt::SetDiscard { set, value } => {
                let s = self.emit_expr(set);
                let v = self.emit_expr(value);
                let slot = self.slot_from_value(&v, value.ty);
                self.line(format!(
                    "call void @pyrs_set_discard(ptr {s}, i64 {slot}, i32 {})",
                    elem_tag(&value.ty)
                ));
            }
            Stmt::SetClear { set } => {
                let s = self.emit_expr(set);
                self.line(format!("call void @pyrs_set_clear(ptr {s})"));
            }
            Stmt::SetUpdate { set, other } => {
                let s = self.emit_expr(set);
                let o = self.emit_expr(other);
                self.line(format!("call void @pyrs_set_update(ptr {s}, ptr {o})"));
            }
            Stmt::UnpackCheck { len, expected } => {
                let n_t = self.emit_expr(len);
                let n = self.emit_unbox_i64(&n_t);
                self.line(format!(
                    "call void @pyrs_unpack_check(i64 {n}, i64 {expected})"
                ));
            }
            Stmt::UnpackCheckMin { len, minimum } => {
                let n_t = self.emit_expr(len);
                let n = self.emit_unbox_i64(&n_t);
                self.line(format!(
                    "call void @pyrs_unpack_check_min(i64 {n}, i64 {minimum})"
                ));
            }
            Stmt::CellStore { cell, value } => {
                let c = self.emit_expr(cell);
                let v = self.emit_expr(value);
                let slot = self.slot_from_value(&v, value.ty);
                self.line(format!("call void @pyrs_cell_store(ptr {c}, i64 {slot})"));
            }
            Stmt::SetField {
                object,
                class_id,
                field_index,
                value,
            } => {
                let obj = self.emit_expr(object);
                let v = self.emit_expr(value);
                let info = self
                    .classes
                    .get(*class_id as usize)
                    .expect("SetField class_id");
                let sty = class_struct_ty(info);
                // field_index is 0-based into fields; LLVM index is 1+field (0=type_id)
                let idx = *field_index as i32 + 1;
                let fp = self.tmp();
                self.line(format!(
                    "{fp} = getelementptr inbounds {sty}, ptr {obj}, i32 0, i32 {idx}"
                ));
                self.line(format!("store {} {v}, ptr {fp}", lty(value.ty)));
            }
            Stmt::GenClose { generator } => {
                let g = self.emit_expr(generator);
                // CPython close(): inject GeneratorExit, run finally, swallow GenExit.
                self.line(format!("call void @pyrs_gen_close(ptr {g})"));
            }
            Stmt::Yield(value) => {
                let Some(frame) = self.gen_frame.clone() else {
                    panic!("Yield outside generator function");
                };
                let v = self.emit_expr(value);
                let slot = self.slot_from_value(&v, value.ty);
                self.line(format!(
                    "call void @pyrs_gen_set_yield(ptr {frame}, i64 {slot})"
                ));
                let next = self.gen_next_state;
                self.gen_next_state += 1;
                self.line(format!(
                    "call void @pyrs_gen_set_state(ptr {frame}, i64 {next})"
                ));
                // Persist try phase + exit across suspend (allocas do not
                // survive the resume call). Pop setjmp frames so the C stack
                // is clean. Mid-finally tries only need exit_kind restored.
                let scopes: Vec<TryScope> = self.tries.clone();
                let fins: Vec<FinallyScope> = self.gen_fin_stack.clone();
                let ntries = scopes.len();
                for scope in &scopes {
                    let i = scope.pool_idx as i64;
                    let ph = self.load_try_i32(&scope.phase_ptr);
                    let ph64 = self.tmp();
                    self.line(format!("{ph64} = zext i32 {ph} to i64"));
                    self.line(format!(
                        "call void @pyrs_gen_save_try_phase(ptr {frame}, i64 {i}, i64 {ph64})"
                    ));
                    let ex = self.load_try_i32(&scope.exit_ptr);
                    let ex64 = self.tmp();
                    self.line(format!("{ex64} = zext i32 {ex} to i64"));
                    self.line(format!(
                        "call void @pyrs_gen_save_try_exit(ptr {frame}, i64 {i}, i64 {ex64})"
                    ));
                }
                for fin in &fins {
                    let i = fin.pool_idx as i64;
                    let ex = self.load_try_i32(&fin.exit_ptr);
                    let ex64 = self.tmp();
                    self.line(format!("{ex64} = zext i32 {ex} to i64"));
                    self.line(format!(
                        "call void @pyrs_gen_save_try_exit(ptr {frame}, i64 {i}, i64 {ex64})"
                    ));
                }
                for _ in 0..ntries {
                    self.line("call void @pyrs_try_pop()");
                }
                self.line("ret i32 0");
                self.terminated = true;
                // Resume label for this yield
                let lab = format!("gstate{next}");
                self.start_block(&lab);
                // Restore mid-finally exit kinds (no setjmp — frame already dead).
                for fin in &fins {
                    let i = fin.pool_idx as i64;
                    let ex64 = self.tmp();
                    self.line(format!(
                        "{ex64} = call i64 @pyrs_gen_load_try_exit(ptr {frame}, i64 {i})"
                    ));
                    let ex = self.tmp();
                    self.line(format!("{ex} = trunc i64 {ex64} to i32"));
                    self.store_try_i32(&ex, &fin.exit_ptr);
                }
                // Re-establish active try frames (new setjmp on this call),
                // restoring the phase that was active at suspend.
                for scope in &scopes {
                    let i = scope.pool_idx as i64;
                    let ex64 = self.tmp();
                    self.line(format!(
                        "{ex64} = call i64 @pyrs_gen_load_try_exit(ptr {frame}, i64 {i})"
                    ));
                    let ex = self.tmp();
                    self.line(format!("{ex} = trunc i64 {ex64} to i32"));
                    self.store_try_i32(&ex, &scope.exit_ptr);
                    self.store_try_i32(1, &scope.live_ptr);
                    let ph64 = self.tmp();
                    self.line(format!(
                        "{ph64} = call i64 @pyrs_gen_load_try_phase(ptr {frame}, i64 {i})"
                    ));
                    let ph = self.tmp();
                    self.line(format!("{ph} = trunc i64 {ph64} to i32"));
                    self.store_try_i32(&ph, &scope.phase_ptr);
                    let tframe = self.tmp();
                    self.line(format!("{tframe} = call ptr @pyrs_try_push()"));
                    let jc = self.tmp();
                    self.line(format!(
                        "{jc} = call i32 @setjmp(ptr {tframe}) returns_twice"
                    ));
                    let ok = self.tmp();
                    self.line(format!("{ok} = icmp eq i32 {jc}, 0"));
                    let cont = self.fresh_block("try.yreenter");
                    self.line(format!("br i1 {ok}, label %{cont}, label %{}", scope.exc_l));
                    self.start_block(&cont);
                }
                // close() injects GeneratorExit; throw() injects a pending exc.
                let closing = self.tmp();
                self.line(format!(
                    "{closing} = call i32 @pyrs_gen_closing(ptr {frame})"
                ));
                let is_closing = self.tmp();
                self.line(format!("{is_closing} = icmp ne i32 {closing}, 0"));
                let close_l = self.fresh_block("gen.closing");
                let throw_chk = self.fresh_block("gen.throwchk");
                self.line(format!(
                    "br i1 {is_closing}, label %{close_l}, label %{throw_chk}"
                ));
                self.start_block(&close_l);
                if ntries > 0 {
                    // PYRS_EXC_GENEXIT = 7 — live try will run finally; close()
                    // swallows uncaught GE at the generator boundary.
                    self.line("call void @pyrs_raise(i32 7, ptr null)");
                    self.line("unreachable");
                } else {
                    // No live try frames: GenExit is immediately uncaught → done
                    // (including yield suspended inside finally with no inner try).
                    self.line(format!("call void @pyrs_gen_set_done(ptr {frame})"));
                    self.line("ret i32 1");
                }
                self.terminated = true;
                self.start_block(&throw_chk);
                let throwing = self.tmp();
                self.line(format!(
                    "{throwing} = call i32 @pyrs_gen_throwing(ptr {frame})"
                ));
                let is_throw = self.tmp();
                self.line(format!("{is_throw} = icmp ne i32 {throwing}, 0"));
                let throw_l = self.fresh_block("gen.throwing");
                let cont_l = self.fresh_block("gen.resume");
                self.line(format!(
                    "br i1 {is_throw}, label %{throw_l}, label %{cont_l}"
                ));
                self.start_block(&throw_l);
                let ttype = self.tmp();
                self.line(format!(
                    "{ttype} = call i64 @pyrs_gen_throw_type(ptr {frame})"
                ));
                let tmsg = self.tmp();
                self.line(format!(
                    "{tmsg} = call ptr @pyrs_gen_throw_msg(ptr {frame})"
                ));
                self.line(format!("call void @pyrs_gen_clear_throw(ptr {frame})"));
                // Message is a pyrs str*; pyrs_raise wants the data pointer.
                let has_msg = self.tmp();
                self.line(format!("{has_msg} = icmp ne ptr {tmsg}, null"));
                let msg_some = self.fresh_block("gen.throw.msg");
                let msg_none = self.fresh_block("gen.throw.nomsg");
                let msg_join = self.fresh_block("gen.throw.raise");
                self.line(format!(
                    "br i1 {has_msg}, label %{msg_some}, label %{msg_none}"
                ));
                self.start_block(&msg_some);
                let data = self.tmp();
                self.line(format!(
                    "{data} = getelementptr inbounds i8, ptr {tmsg}, i64 8"
                ));
                let data_blk = self.cur_block.clone();
                self.line(format!("br label %{msg_join}"));
                self.start_block(&msg_none);
                let none_blk = self.cur_block.clone();
                self.line(format!("br label %{msg_join}"));
                self.start_block(&msg_join);
                let msg_arg = self.tmp();
                self.line(format!(
                    "{msg_arg} = phi ptr [ {data}, %{data_blk} ], [ null, %{none_blk} ]"
                ));
                let t32 = self.tmp();
                self.line(format!("{t32} = trunc i64 {ttype} to i32"));
                // No live try: gen is finished when the exception leaves it.
                // (With live tries, finally-dispatch / handlers set done on
                // uncaught paths.)
                if ntries == 0 {
                    self.line(format!("call void @pyrs_gen_set_done(ptr {frame})"));
                }
                self.line(format!("call void @pyrs_raise(i32 {t32}, ptr {msg_arg})"));
                self.line("unreachable");
                self.terminated = true;
                self.start_block(&cont_l);
            }
            Stmt::Raise { exc, message } => {
                let m = self.emit_expr(message);
                // pyrs_raise wants a C string: data pointer after length header
                let data = self.tmp();
                self.line(format!(
                    "{data} = getelementptr inbounds i8, ptr {m}, i64 8"
                ));
                // Uncaught raise in a generator (no active try) finishes it.
                if self.gen_frame.is_some() && self.tries.is_empty() {
                    let frame = self.gen_frame.clone().unwrap();
                    self.line(format!("call void @pyrs_gen_set_done(ptr {frame})"));
                }
                // Always longjmp: try frames stay live through handlers, so a
                // raise in except re-enters this try's setjmp with phase=handler
                // and then runs finally (see emit_try).
                self.line(format!(
                    "call void @pyrs_raise(i32 {}, ptr {data})",
                    exc.tag()
                ));
                self.line("unreachable");
                self.terminated = true;
            }
            Stmt::Try {
                body,
                handlers,
                orelse,
                finally,
            } => {
                self.emit_try(body, handlers, orelse, finally);
            }
            Stmt::ListAppend { list, value } => {
                let l = self.emit_expr(list);
                let v = self.emit_expr(value);
                let slot = self.slot_from_value(&v, value.ty);
                self.line(format!("call void @pyrs_list_push(ptr {l}, i64 {slot})"));
            }
            Stmt::ListInsert { list, index, value } => {
                let l = self.emit_expr(list);
                let i_t = self.emit_expr(index);
                let i = self.emit_unbox_i64(&i_t);
                let v = self.emit_expr(value);
                let slot = self.slot_from_value(&v, value.ty);
                self.line(format!(
                    "call void @pyrs_list_insert(ptr {l}, i64 {i}, i64 {slot})"
                ));
            }
            Stmt::ListRemove { list, value } => {
                let l = self.emit_expr(list);
                let v = self.emit_expr(value);
                let slot = self.slot_from_value(&v, value.ty);
                self.line(format!(
                    "call void @pyrs_list_remove(ptr {l}, i64 {slot}, i32 {})",
                    elem_tag(&value.ty)
                ));
            }
            Stmt::ListClear { list } => {
                let l = self.emit_expr(list);
                self.line(format!("call void @pyrs_list_clear(ptr {l})"));
            }
            Stmt::ListSort { list } => {
                let l = self.emit_expr(list);
                let Ty::List(elem) = list.ty else {
                    unreachable!("ListSort on non-list");
                };
                self.line(format!(
                    "call void @pyrs_list_sort(ptr {l}, i32 {})",
                    elem_tag(elem)
                ));
            }
            Stmt::ListExtend { list, other } => {
                let l = self.emit_expr(list);
                let o = self.emit_expr(other);
                self.line(format!("call void @pyrs_list_extend(ptr {l}, ptr {o})"));
            }
            Stmt::ListAppendUnchecked { list, value } => {
                // capacity was guaranteed at allocation: store the slot at
                // data[len] and bump len, no call, no check
                let l = self.emit_expr(list);
                let v = self.emit_expr(value);
                let slot = self.slot_from_value(&v, value.ty);
                let len = self.tmp();
                self.line(format!("{len} = load i64, ptr {l}"));
                let data_pp = self.tmp();
                self.line(format!(
                    "{data_pp} = getelementptr inbounds i8, ptr {l}, i64 16"
                ));
                let data_p = self.tmp();
                self.line(format!("{data_p} = load ptr, ptr {data_pp}"));
                let addr = self.tmp();
                self.line(format!(
                    "{addr} = getelementptr inbounds i64, ptr {data_p}, i64 {len}"
                ));
                self.line(format!("store i64 {slot}, ptr {addr}"));
                let newlen = self.tmp();
                self.line(format!("{newlen} = add i64 {len}, 1"));
                self.line(format!("store i64 {newlen}, ptr {l}"));
            }
            Stmt::Return(None) => {
                if self.gen_frame.is_some() {
                    // Generator stop — route through finally if inside try.
                    if !self.tries.is_empty() {
                        self.emit_try_exit(TRY_EXIT_RETURN, None);
                    } else {
                        let frame = self.gen_frame.clone().unwrap();
                        self.line(format!("call void @pyrs_gen_set_done(ptr {frame})"));
                        self.line("ret i32 1");
                        self.terminated = true;
                    }
                } else if !self.tries.is_empty() {
                    self.emit_try_exit(TRY_EXIT_RETURN, None);
                } else {
                    self.line("ret void");
                    self.terminated = true;
                }
            }
            Stmt::Return(Some(value)) => {
                if self.gen_frame.is_some() {
                    // Store StopIteration.value for yield-from, then stop
                    // (through finally when inside try).
                    let frame = self.gen_frame.clone().unwrap();
                    let v = self.emit_expr(value);
                    let slot = self.slot_from_value(&v, value.ty);
                    self.line(format!(
                        "call void @pyrs_gen_set_return(ptr {frame}, i64 {slot})"
                    ));
                    if !self.tries.is_empty() {
                        self.emit_try_exit(TRY_EXIT_RETURN, None);
                    } else {
                        self.line(format!("call void @pyrs_gen_set_done(ptr {frame})"));
                        self.line("ret i32 1");
                        self.terminated = true;
                    }
                } else {
                    let v = self.emit_expr(value);
                    if !self.tries.is_empty() {
                        self.emit_try_exit(TRY_EXIT_RETURN, Some((v, value.ty)));
                    } else {
                        self.line(format!("ret {} {v}", lty(value.ty)));
                        self.terminated = true;
                    }
                }
            }
            Stmt::ExprStmt(expr) => {
                self.emit_expr(expr);
            }
            Stmt::Die(message) => {
                // Frame stays live through handlers; longjmp re-enters setjmp
                // with phase=handler and runs finally.
                self.emit_die(message);
            }
            Stmt::Print(args) => {
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        self.line("call void @pyrs_print_sep()");
                    }
                    let v = self.emit_expr(arg);
                    self.emit_print_value(&v, arg.ty);
                }
                self.line("call void @pyrs_print_end()");
            }
            Stmt::Break => {
                // Only run try finally when the try is nested *inside* the loop
                // being broken (break leaves the try). If the loop is nested
                // inside the try, break stays in the try (CPython).
                if self.break_exits_innermost_try() {
                    self.emit_try_exit(TRY_EXIT_BREAK, None);
                } else {
                    let (_, end) = self.loops.last().expect("break outside loop").clone();
                    self.line(format!("br label %{end}"));
                    self.terminated = true;
                }
            }
            Stmt::Continue => {
                if self.break_exits_innermost_try() {
                    self.emit_try_exit(TRY_EXIT_CONTINUE, None);
                } else {
                    let (cont, _) = self.loops.last().expect("continue outside loop").clone();
                    self.line(format!("br label %{cont}"));
                    self.terminated = true;
                }
            }
            Stmt::While { cond, body, step } => {
                let cond_l = self.fresh_block("while.cond");
                let body_l = self.fresh_block("while.body");
                let step_l = self.fresh_block("while.step");
                let end_l = self.fresh_block("while.end");

                self.line(format!("br label %{cond_l}"));
                self.start_block(&cond_l);
                let c = self.emit_expr(cond);
                self.line(format!("br i1 {c}, label %{body_l}, label %{end_l}"));

                self.start_block(&body_l);
                // continue jumps to the step block (for-loop increment)
                self.loops.push((step_l.clone(), end_l.clone()));
                self.emit_block(body);
                self.loops.pop();
                if !self.terminated {
                    self.line(format!("br label %{step_l}"));
                }

                self.start_block(&step_l);
                self.emit_block(step);
                self.line(format!("br label %{cond_l}"));

                self.start_block(&end_l);
            }
            Stmt::If { branches, orelse } => {
                let end_l = self.fresh_block("if.end");
                let mut else_l = String::new();

                for (i, (cond, body)) in branches.iter().enumerate() {
                    let then_l = self.fresh_block("if.then");
                    let last = i + 1 == branches.len();
                    let next_l = if last && orelse.is_empty() {
                        end_l.clone()
                    } else {
                        self.fresh_block("if.else")
                    };

                    let c = self.emit_expr(cond);
                    self.line(format!("br i1 {c}, label %{then_l}, label %{next_l}"));

                    self.start_block(&then_l);
                    self.emit_block(body);
                    if !self.terminated {
                        self.line(format!("br label %{end_l}"));
                    }

                    self.start_block(&next_l);
                    else_l = next_l;
                }

                if !orelse.is_empty() {
                    // current block is the final else
                    debug_assert_eq!(self.cur_block, else_l);
                    self.emit_block(orelse);
                    if !self.terminated {
                        self.line(format!("br label %{end_l}"));
                    }
                    self.start_block(&end_l);
                }
                // when orelse is empty the last `next_l` *is* end_l and is
                // already started
            }
        }
    }

    // ---- expressions ----

    /// Emit code for `expr`; returns the LLVM value (a temp or a constant).
    fn emit_expr(&mut self, expr: &Expr) -> String {
        match &expr.kind {
            ExprKind::ConstInt(v) => self.emit_const_int(*v),
            ExprKind::ConstIntDigits(s) => {
                // Intern decimal digits as a string blob and parse at runtime.
                let p = self.intern_string(s);
                let t = self.tmp();
                let len = s.len() as i64;
                // pyrs_int_from_str wants a C pointer to the byte data (skip i64 len header)
                self.line(format!(
                    "{t} = call i64 @pyrs_int_from_str(ptr getelementptr inbounds (i8, ptr {p}, i64 8), i64 {len})"
                ));
                t
            }
            ExprKind::ConstFloat(v) => fconst(*v),
            ExprKind::ConstBool(v) => v.to_string(),
            ExprKind::ConstStr(s) => self.intern_string(s),
            ExprKind::ConstNone => "0".to_string(),
            ExprKind::FromUnion { value } => {
                let v = self.emit_expr(value);
                let payload = self.tmp();
                self.line(format!("{payload} = extractvalue {{ i32, i64 }} {v}, 1"));
                self.value_from_slot(&payload, expr.ty)
            }
            ExprKind::ToUnion { value } => {
                let v = self.emit_expr(value);
                self.emit_to_union(&v, value.ty, expr.ty)
            }
            ExprKind::ToAny { value } => {
                let v = self.emit_expr(value);
                self.emit_to_any(&v, value.ty)
            }
            ExprKind::FromAny { value } => {
                let v = self.emit_expr(value);
                self.emit_from_any(&v, expr.ty)
            }
            ExprKind::IsNone { value, not } => self.emit_is_none(value, *not),
            ExprKind::IsIdentity { left, right, not } => self.emit_is_identity(left, right, *not),
            ExprKind::Local(name) => {
                // Load the alloca/frame storage type. Semantic peels may retype
                // `expr.ty` (e.g. Class multi-subclass peel) without a different
                // LLVM ABI for class pointers.
                let storage = self.local_storage.get(name).copied().unwrap_or(expr.ty);
                if let (Some(frame), Some(idx)) = (
                    self.gen_frame.clone(),
                    self.gen_local_index.get(name).copied(),
                ) {
                    let slot = self.tmp();
                    self.line(format!(
                        "{slot} = call i64 @pyrs_gen_get_local(ptr {frame}, i64 {idx})"
                    ));
                    self.value_from_slot(&slot, storage)
                } else {
                    let t = self.tmp();
                    self.line(format!("{t} = load {}, ptr %v.{name}", lty(storage)));
                    t
                }
            }
            ExprKind::GlobalLoad(name) => {
                let t = self.tmp();
                self.line(format!("{t} = load {}, ptr @g.{name}", lty(expr.ty)));
                t
            }
            ExprKind::Input { prompt } => {
                let p = match prompt {
                    Some(e) => self.emit_expr(e),
                    Option::None => "null".to_string(),
                };
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_input(ptr {p})"));
                t
            }
            ExprKind::Argv => {
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_argv()"));
                t
            }
            ExprKind::Open { path, mode } => {
                let p = self.emit_expr(path);
                let m = self.emit_expr(mode);
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_open(ptr {p}, ptr {m})"));
                t
            }
            ExprKind::FileCall { func, args } => {
                let vals: Vec<String> = args.iter().map(|a| self.emit_expr(a)).collect();
                let args_str = vals
                    .iter()
                    .map(|v| format!("ptr {v}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                match func {
                    FileFn::Read | FileFn::ReadLine | FileFn::ReadLines => {
                        let callee = match func {
                            FileFn::Read => "pyrs_file_read",
                            FileFn::ReadLine => "pyrs_file_readline",
                            _ => "pyrs_file_readlines",
                        };
                        let t = self.tmp();
                        self.line(format!("{t} = call ptr @{callee}({args_str})"));
                        t
                    }
                    FileFn::Write => {
                        let t = self.tmp();
                        self.line(format!("{t} = call i64 @pyrs_file_write({args_str})"));
                        self.emit_box_i64(&t)
                    }
                    FileFn::Close => {
                        self.line(format!("call void @pyrs_file_close({args_str})"));
                        String::new()
                    }
                }
            }
            ExprKind::Let { name, value, body } => {
                let v = self.emit_expr(value);
                self.line(format!("store {} {v}, ptr %v.{name}", lty(value.ty)));
                self.emit_expr(body)
            }
            ExprKind::Call { func, args } => {
                let mut arg_list = Vec::new();
                for arg in args {
                    let v = self.emit_expr(arg);
                    arg_list.push(format!("{} {v}", lty(arg.ty)));
                }
                let args_str = arg_list.join(", ");
                let callee = mangle(func);
                if expr.ty == Ty::None {
                    // Function `-> None` is void (not a value-level None return).
                    self.line(format!("call void @{callee}({args_str})"));
                    "0".to_string()
                } else {
                    let t = self.tmp();
                    self.line(format!(
                        "{t} = call {} @{callee}({args_str})",
                        lty_ret(expr.ty)
                    ));
                    t
                }
            }
            ExprKind::Index { base, index } => {
                let b = self.emit_expr(base);
                let i_tagged = self.emit_expr(index);
                match base.ty {
                    Ty::Str => {
                        let i = self.emit_unbox_i64(&i_tagged);
                        let t = self.tmp();
                        self.line(format!("{t} = call ptr @pyrs_str_index(ptr {b}, i64 {i})"));
                        t
                    }
                    Ty::List(elem) => {
                        let i = self.emit_unbox_i64(&i_tagged);
                        let addr =
                            self.emit_list_elem_addr(&b, &i, "IndexError: list index out of range");
                        let slot = self.tmp();
                        self.line(format!("{slot} = load i64, ptr {addr}"));
                        self.value_from_slot(&slot, *elem)
                    }
                    Ty::Tuple(_) => {
                        let i = self.emit_unbox_i64(&i_tagged);
                        let slot = self.tmp();
                        self.line(format!(
                            "{slot} = call i64 @pyrs_tuple_get(ptr {b}, i64 {i})"
                        ));
                        self.value_from_slot(&slot, expr.ty)
                    }
                    Ty::Dict { key, value } => {
                        let kslot = self.slot_from_value(&i_tagged, *key);
                        let slot = self.tmp();
                        self.line(format!(
                            "{slot} = call i64 @pyrs_dict_get(ptr {b}, i64 {kslot}, i32 {})",
                            elem_tag(key)
                        ));
                        self.value_from_slot(&slot, *value)
                    }
                    other => unreachable!("index on {other:?}"),
                }
            }
            ExprKind::Slice { base, lo, hi, step } => {
                let b = self.emit_expr(base);
                let lo_v = self.emit_slice_bound(lo);
                let hi_v = self.emit_slice_bound(hi);
                let step_t = self.emit_expr(step);
                let step_v = self.emit_unbox_i64(&step_t);
                let callee = match base.ty {
                    Ty::Str => "pyrs_str_slice",
                    Ty::List(_) => "pyrs_list_slice",
                    other => unreachable!("slice on {other:?}"),
                };
                let t = self.tmp();
                self.line(format!(
                    "{t} = call ptr @{callee}(ptr {b}, i64 {lo_v}, i64 {hi_v}, i64 {step_v})"
                ));
                t
            }
            ExprKind::StrCall { func, args } => {
                let vals: Vec<String> = args.iter().map(|a| self.emit_expr(a)).collect();
                // (runtime symbol, returns i1-via-i32, returns i64)
                let (callee, is_bool, is_int) = match func {
                    StrFn::Upper => ("pyrs_str_upper", false, false),
                    StrFn::Lower => ("pyrs_str_lower", false, false),
                    StrFn::Strip => ("pyrs_str_strip", false, false),
                    StrFn::Lstrip => ("pyrs_str_lstrip", false, false),
                    StrFn::Rstrip => ("pyrs_str_rstrip", false, false),
                    StrFn::StartsWith => ("pyrs_str_startswith", true, false),
                    StrFn::EndsWith => ("pyrs_str_endswith", true, false),
                    StrFn::Find => ("pyrs_str_find", false, true),
                    StrFn::RFind => ("pyrs_str_rfind", false, true),
                    StrFn::RIndex => ("pyrs_str_rindex", false, true),
                    StrFn::Count => ("pyrs_str_count", false, true),
                    StrFn::Replace => ("pyrs_str_replace", false, false),
                    StrFn::SplitWs => ("pyrs_str_split_ws", false, false),
                    StrFn::Split => ("pyrs_str_split", false, false),
                    StrFn::Join => ("pyrs_str_join", false, false),
                    StrFn::IsDigit => ("pyrs_str_isdigit", true, false),
                    StrFn::IsAlpha => ("pyrs_str_isalpha", true, false),
                    StrFn::IsSpace => ("pyrs_str_isspace", true, false),
                    StrFn::IsUpper => ("pyrs_str_isupper", true, false),
                    StrFn::IsLower => ("pyrs_str_islower", true, false),
                };
                let args_str = vals
                    .iter()
                    .map(|v| format!("ptr {v}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let t = self.tmp();
                if is_bool {
                    self.line(format!("{t} = call i32 @{callee}({args_str})"));
                    let b = self.tmp();
                    self.line(format!("{b} = icmp ne i32 {t}, 0"));
                    b
                } else if is_int {
                    self.line(format!("{t} = call i64 @{callee}({args_str})"));
                    self.emit_box_i64(&t)
                } else {
                    self.line(format!("{t} = call ptr @{callee}({args_str})"));
                    t
                }
            }
            ExprKind::Contains { needle, haystack } => {
                let n = self.emit_expr(needle);
                let h = self.emit_expr(haystack);
                let c = self.tmp();
                match haystack.ty {
                    Ty::Dict { key, .. } => {
                        let kslot = self.slot_from_value(&n, *key);
                        self.line(format!(
                            "{c} = call i32 @pyrs_dict_contains(ptr {h}, i64 {kslot}, i32 {})",
                            elem_tag(key)
                        ));
                        let b = self.tmp();
                        self.line(format!("{b} = icmp ne i32 {c}, 0"));
                        return b;
                    }
                    Ty::Set(elem) => {
                        let slot = self.slot_from_value(&n, *elem);
                        self.line(format!(
                            "{c} = call i32 @pyrs_set_contains(ptr {h}, i64 {slot}, i32 {})",
                            elem_tag(elem)
                        ));
                        let b = self.tmp();
                        self.line(format!("{b} = icmp ne i32 {c}, 0"));
                        return b;
                    }
                    Ty::Str => {
                        self.line(format!(
                            "{c} = call i32 @pyrs_str_contains(ptr {h}, ptr {n})"
                        ));
                    }
                    Ty::List(elem) => {
                        let slot = self.slot_from_value(&n, needle.ty);
                        self.line(format!(
                            "{c} = call i32 @pyrs_list_contains(ptr {h}, i64 {slot}, i32 {})",
                            elem_tag(elem)
                        ));
                    }
                    Ty::Tuple(_) => {
                        // Heterogeneous: compare only when element tag matches needle.
                        let slot = self.slot_from_value(&n, needle.ty);
                        self.line(format!(
                            "{c} = call i32 @pyrs_tuple_contains(ptr {h}, i64 {slot}, i32 {})",
                            elem_tag(&needle.ty)
                        ));
                    }
                    other => unreachable!("contains on {other:?}"),
                }
                let t = self.tmp();
                self.line(format!("{t} = icmp ne i32 {c}, 0"));
                t
            }
            ExprKind::IsInstance {
                value,
                type_tags,
                bool_is_int,
                exc_filters,
                class_filters,
            } => {
                let v = self.emit_expr(value);
                // Dynamic Any: match box print_tag (and class hierarchy on payload).
                if value.ty == Ty::Any {
                    return self.emit_isinstance_any(
                        &v,
                        type_tags,
                        *bool_is_int,
                        exc_filters,
                        class_filters,
                    );
                }
                // Locals/unions store `{ i32 member_index, i64 payload }` by value
                // (not a heap box). Match member_index against members whose
                // print-tag satisfies any of `type_tags` (list=4 means any list).
                // Exception/Class members use hierarchy filters on the payload.
                let Ty::Union(members) = value.ty else {
                    // Monomorphic path should have been folded in semantic.
                    let t = self.tmp();
                    self.line(format!("{t} = add i1 0, 0"));
                    return t;
                };
                let idx = self.tmp();
                self.line(format!("{idx} = extractvalue {{ i32, i64 }} {v}, 0"));
                let payload = self.tmp();
                self.line(format!("{payload} = extractvalue {{ i32, i64 }} {v}, 1"));
                let mut matching: Vec<usize> = Vec::new();
                let mut exc_member: Option<usize> = None;
                let mut class_members: Vec<(usize, u32)> = Vec::new(); // (member_idx, class_id)
                for (i, m) in members.iter().enumerate() {
                    if *m == Ty::Exception {
                        exc_member = Some(i);
                        continue; // never match Exception via print tags alone
                    }
                    if let Ty::Class(cid) = *m {
                        class_members.push((i, cid));
                        continue; // never match Class via print tags alone
                    }
                    let ptag = member_print_tag(*m);
                    let hit = type_tags.iter().any(|&want| {
                        if want == 4 {
                            // any list: (ptag % 8) == 4 && ptag >= 0
                            ptag >= 0 && ptag % 8 == 4
                        } else if want == 0 && *bool_is_int {
                            ptag == 0 || ptag == 2 // int or bool
                        } else {
                            ptag == want
                        }
                    });
                    if hit {
                        matching.push(i);
                    }
                }
                let mut acc: Option<String> = None;
                for i in matching {
                    let cmp = self.tmp();
                    self.line(format!("{cmp} = icmp eq i32 {idx}, {i}"));
                    acc = Some(match acc {
                        None => cmp,
                        Some(prev) => {
                            let or = self.tmp();
                            self.line(format!("{or} = or i1 {prev}, {cmp}"));
                            or
                        }
                    });
                }
                // Exception member: only load/check hierarchy when active.
                if let (Some(ei), true) = (exc_member, !exc_filters.is_empty()) {
                    let is_exc = self.tmp();
                    self.line(format!("{is_exc} = icmp eq i32 {idx}, {ei}"));
                    let yes_l = self.fresh_block("isinstance.exc.yes");
                    let no_l = self.fresh_block("isinstance.exc.no");
                    let end_l = self.fresh_block("isinstance.exc.end");
                    self.line(format!("br i1 {is_exc}, label %{yes_l}, label %{no_l}"));

                    self.start_block(&yes_l);
                    let exc_ptr = self.value_from_slot(&payload, Ty::Exception);
                    let mut exc_hit: Option<String> = None;
                    for &f in exc_filters {
                        let m = self.tmp();
                        self.line(format!(
                            "{m} = call i32 @pyrs_exc_isinstance(ptr {exc_ptr}, i32 {f})"
                        ));
                        let cmp = self.tmp();
                        self.line(format!("{cmp} = icmp ne i32 {m}, 0"));
                        exc_hit = Some(match exc_hit {
                            None => cmp,
                            Some(prev) => {
                                let or = self.tmp();
                                self.line(format!("{or} = or i1 {prev}, {cmp}"));
                                or
                            }
                        });
                    }
                    let yes_v = exc_hit.unwrap();
                    let yes_pred = self.cur_block.clone();
                    self.line(format!("br label %{end_l}"));

                    self.start_block(&no_l);
                    let no_pred = self.cur_block.clone();
                    self.line(format!("br label %{end_l}"));

                    self.start_block(&end_l);
                    let exc_ok = self.tmp();
                    self.line(format!(
                        "{exc_ok} = phi i1 [ {yes_v}, %{yes_pred} ], [ false, %{no_pred} ]"
                    ));
                    acc = Some(match acc {
                        None => exc_ok,
                        Some(prev) => {
                            let or = self.tmp();
                            self.line(format!("{or} = or i1 {prev}, {exc_ok}"));
                            or
                        }
                    });
                }
                // Class members: hierarchy check when active.
                if !class_filters.is_empty() && !class_members.is_empty() {
                    let n = self.classes.len() as i64;
                    for (mi, _cid) in &class_members {
                        let is_cls = self.tmp();
                        self.line(format!("{is_cls} = icmp eq i32 {idx}, {mi}"));
                        let yes_l = self.fresh_block("isinstance.cls.yes");
                        let no_l = self.fresh_block("isinstance.cls.no");
                        let end_l = self.fresh_block("isinstance.cls.end");
                        self.line(format!("br i1 {is_cls}, label %{yes_l}, label %{no_l}"));

                        self.start_block(&yes_l);
                        let obj = self.tmp();
                        self.line(format!("{obj} = inttoptr i64 {payload} to ptr"));
                        let mut cls_hit: Option<String> = None;
                        for &want in class_filters {
                            let m = self.tmp();
                            self.line(format!(
                                "{m} = call i32 @pyrs_isinstance_class(ptr {obj}, i64 {}, ptr @pyrs_class_parents, i64 {n})",
                                want as i64
                            ));
                            let cmp = self.tmp();
                            self.line(format!("{cmp} = icmp ne i32 {m}, 0"));
                            cls_hit = Some(match cls_hit {
                                None => cmp,
                                Some(prev) => {
                                    let or = self.tmp();
                                    self.line(format!("{or} = or i1 {prev}, {cmp}"));
                                    or
                                }
                            });
                        }
                        let yes_v = cls_hit.unwrap();
                        let yes_pred = self.cur_block.clone();
                        self.line(format!("br label %{end_l}"));

                        self.start_block(&no_l);
                        let no_pred = self.cur_block.clone();
                        self.line(format!("br label %{end_l}"));

                        self.start_block(&end_l);
                        let cls_ok = self.tmp();
                        self.line(format!(
                            "{cls_ok} = phi i1 [ {yes_v}, %{yes_pred} ], [ false, %{no_pred} ]"
                        ));
                        acc = Some(match acc {
                            None => cls_ok,
                            Some(prev) => {
                                let or = self.tmp();
                                self.line(format!("{or} = or i1 {prev}, {cls_ok}"));
                                or
                            }
                        });
                    }
                }
                match acc {
                    Some(a) => a,
                    None => {
                        let f = self.tmp();
                        self.line(format!("{f} = add i1 0, 0"));
                        f
                    }
                }
            }
            ExprKind::SetUnion { left, right } => {
                let l = self.emit_expr(left);
                let r = self.emit_expr(right);
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_set_union(ptr {l}, ptr {r})"));
                t
            }
            ExprKind::SetIntersect { left, right } => {
                let l = self.emit_expr(left);
                let r = self.emit_expr(right);
                let t = self.tmp();
                self.line(format!(
                    "{t} = call ptr @pyrs_set_intersect(ptr {l}, ptr {r})"
                ));
                t
            }
            ExprKind::SetDiff { left, right } => {
                let l = self.emit_expr(left);
                let r = self.emit_expr(right);
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_set_diff(ptr {l}, ptr {r})"));
                t
            }
            ExprKind::SetSymDiff { left, right } => {
                let l = self.emit_expr(left);
                let r = self.emit_expr(right);
                let t = self.tmp();
                self.line(format!(
                    "{t} = call ptr @pyrs_set_symdiff(ptr {l}, ptr {r})"
                ));
                t
            }
            ExprKind::ListPop { list, index } => {
                let l = self.emit_expr(list);
                let i_t = self.emit_expr(index);
                let i = self.emit_unbox_i64(&i_t);
                let slot = self.tmp();
                self.line(format!(
                    "{slot} = call i64 @pyrs_list_pop(ptr {l}, i64 {i})"
                ));
                self.value_from_slot(&slot, expr.ty)
            }
            ExprKind::ListIndexOf { list, value } => {
                let l = self.emit_expr(list);
                let v = self.emit_expr(value);
                let slot = self.slot_from_value(&v, value.ty);
                let machine = self.tmp();
                self.line(format!(
                    "{machine} = call i64 @pyrs_list_index(ptr {l}, i64 {slot}, i32 {})",
                    elem_tag(&value.ty)
                ));
                self.emit_box_i64(&machine)
            }
            ExprKind::ListLit(items) => {
                let l = self.tmp();
                self.line(format!(
                    "{l} = call ptr @pyrs_list_new(i64 {})",
                    items.len()
                ));
                for item in items {
                    let v = self.emit_expr(item);
                    let slot = self.slot_from_value(&v, item.ty);
                    self.line(format!("call void @pyrs_list_push(ptr {l}, i64 {slot})"));
                }
                l
            }
            ExprKind::ListNew { cap } => {
                let c_t = self.emit_expr(cap);
                let c = self.emit_unbox_i64(&c_t);
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_list_new(i64 {c})"));
                t
            }
            ExprKind::Block { stmts, result } => {
                for stmt in stmts {
                    self.emit_stmt(stmt);
                }
                self.emit_expr(result)
            }
            ExprKind::Len(inner) => {
                let v = self.emit_expr(inner);
                let machine = self.emit_len(&v);
                self.emit_box_i64(&machine)
            }
            ExprKind::Abs(inner) => {
                let v = self.emit_expr(inner);
                let t = self.tmp();
                match inner.ty {
                    Ty::Int => self.line(format!("{t} = call i64 @pyrs_int_abs(i64 {v})")),
                    Ty::Float => self.line(format!("{t} = call double @llvm.fabs.f64(double {v})")),
                    other => unreachable!("Abs on {other:?}"),
                }
                t
            }
            // Python min/max: if right is strictly less/greater, take right;
            // otherwise left (ties and NaN comparisons keep the left operand).
            ExprKind::Min { left, right } => self.emit_min_max(false, left, right),
            ExprKind::Max { left, right } => self.emit_min_max(true, left, right),
            ExprKind::MinList(list) => self.emit_min_max_list(false, list),
            ExprKind::MaxList(list) => self.emit_min_max_list(true, list),
            ExprKind::Sum(list) => self.emit_sum(list),
            ExprKind::MathCall { op, arg } => self.emit_math_call(*op, arg),
            ExprKind::OsGetcwd => {
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_os_getcwd()"));
                t
            }
            ExprKind::JsonDumps(arg) => {
                let v = self.emit_expr(arg);
                let slot = self.slot_from_value(&v, arg.ty);
                let tag = elem_tag(&arg.ty);
                let t = self.tmp();
                self.line(format!(
                    "{t} = call ptr @pyrs_json_dumps(i64 {slot}, i32 {tag})"
                ));
                t
            }
            ExprKind::JsonLoads { kind, arg } => {
                let s = self.emit_expr(arg);
                let t = self.tmp();
                match kind {
                    JsonLoadsKind::Int => {
                        self.line(format!("{t} = call i64 @pyrs_json_loads_int(ptr {s})"));
                    }
                    JsonLoadsKind::Float => {
                        self.line(format!("{t} = call double @pyrs_json_loads_float(ptr {s})"));
                    }
                    JsonLoadsKind::Bool => {
                        let i = self.tmp();
                        self.line(format!("{i} = call i32 @pyrs_json_loads_bool(ptr {s})"));
                        self.line(format!("{t} = trunc i32 {i} to i1"));
                    }
                    JsonLoadsKind::Str => {
                        self.line(format!("{t} = call ptr @pyrs_json_loads_str(ptr {s})"));
                    }
                    JsonLoadsKind::ListInt => {
                        self.line(format!("{t} = call ptr @pyrs_json_loads_list_int(ptr {s})"));
                    }
                    JsonLoadsKind::ListFloat => {
                        self.line(format!(
                            "{t} = call ptr @pyrs_json_loads_list_float(ptr {s})"
                        ));
                    }
                    JsonLoadsKind::ListStr => {
                        self.line(format!("{t} = call ptr @pyrs_json_loads_list_str(ptr {s})"));
                    }
                    JsonLoadsKind::ListBool => {
                        self.line(format!(
                            "{t} = call ptr @pyrs_json_loads_list_bool(ptr {s})"
                        ));
                    }
                    JsonLoadsKind::DictStrInt => {
                        self.line(format!(
                            "{t} = call ptr @pyrs_json_loads_dict_str_int(ptr {s})"
                        ));
                    }
                    JsonLoadsKind::DictStrFloat => {
                        self.line(format!(
                            "{t} = call ptr @pyrs_json_loads_dict_str_float(ptr {s})"
                        ));
                    }
                    JsonLoadsKind::DictStrStr => {
                        self.line(format!(
                            "{t} = call ptr @pyrs_json_loads_dict_str_str(ptr {s})"
                        ));
                    }
                    JsonLoadsKind::DictStrBool => {
                        self.line(format!(
                            "{t} = call ptr @pyrs_json_loads_dict_str_bool(ptr {s})"
                        ));
                    }
                }
                t
            }
            ExprKind::IntToFloat(inner) => {
                let v = self.emit_expr(inner);
                let t = self.tmp();
                self.line(format!("{t} = call double @pyrs_int_to_float(i64 {v})"));
                t
            }
            ExprKind::FloatToInt(inner) => {
                let v = self.emit_expr(inner);
                let t = self.tmp();
                self.line(format!("{t} = call i64 @pyrs_int_from_float(double {v})"));
                t
            }
            ExprKind::BoolToInt(inner) => {
                let v = self.emit_expr(inner);
                // tagged small: false → 1 (0<<1|1), true → 3 (1<<1|1)
                let t = self.tmp();
                self.line(format!("{t} = select i1 {v}, i64 3, i64 1"));
                t
            }
            ExprKind::IntToStr(inner) => {
                let v = self.emit_expr(inner);
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_str_from_int(i64 {v})"));
                t
            }
            ExprKind::FloatToStr(inner) => {
                let v = self.emit_expr(inner);
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_str_from_float(double {v})"));
                t
            }
            ExprKind::StrRepr(inner) => {
                let v = self.emit_expr(inner);
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_str_repr(ptr {v})"));
                t
            }
            ExprKind::StrAscii(inner) => {
                let v = self.emit_expr(inner);
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_str_ascii(ptr {v})"));
                t
            }
            ExprKind::FormatValue { value, spec } => {
                let spec_v = self.emit_expr(spec);
                let t = self.tmp();
                match value.ty {
                    Ty::Int => {
                        let v = self.emit_expr(value);
                        self.line(format!(
                            "{t} = call ptr @pyrs_format_int(i64 {v}, ptr {spec_v})"
                        ));
                    }
                    Ty::Float => {
                        let v = self.emit_expr(value);
                        self.line(format!(
                            "{t} = call ptr @pyrs_format_float(double {v}, ptr {spec_v})"
                        ));
                    }
                    Ty::Bool => {
                        let v = self.emit_expr(value);
                        let ext = self.tmp();
                        self.line(format!("{ext} = zext i1 {v} to i32"));
                        self.line(format!(
                            "{t} = call ptr @pyrs_format_bool(i32 {ext}, ptr {spec_v})"
                        ));
                    }
                    Ty::Str => {
                        let v = self.emit_expr(value);
                        self.line(format!(
                            "{t} = call ptr @pyrs_format_str(ptr {v}, ptr {spec_v})"
                        ));
                    }
                    other => panic!("FormatValue on unsupported type {other:?}"),
                }
                t
            }
            ExprKind::BoolToStr(inner) => {
                let v = self.emit_expr(inner);
                let ext = self.tmp();
                self.line(format!("{ext} = zext i1 {v} to i32"));
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_str_from_bool(i32 {ext})"));
                t
            }
            ExprKind::ExcToStr(inner) => {
                let v = self.emit_expr(inner);
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_str_from_exc(ptr {v})"));
                t
            }
            ExprKind::ExcIsInstance { value, filters } => {
                let v = self.emit_expr(value);
                if filters.is_empty() {
                    let t = self.tmp();
                    self.line(format!("{t} = add i1 0, 0"));
                    return t;
                }
                let mut acc = String::new();
                for (j, &f) in filters.iter().enumerate() {
                    let m = self.tmp();
                    self.line(format!(
                        "{m} = call i32 @pyrs_exc_isinstance(ptr {v}, i32 {f})"
                    ));
                    let cmp = self.tmp();
                    self.line(format!("{cmp} = icmp ne i32 {m}, 0"));
                    if j == 0 {
                        acc = cmp;
                    } else {
                        let or = self.tmp();
                        self.line(format!("{or} = or i1 {acc}, {cmp}"));
                        acc = or;
                    }
                }
                acc
            }
            ExprKind::ToBool(inner) => {
                let v = self.emit_expr(inner);
                self.emit_truthiness(&v, inner.ty)
            }
            ExprKind::Unary { op, operand } => {
                let v = self.emit_expr(operand);
                let t = self.tmp();
                match (op, operand.ty) {
                    (UnOp::Neg, Ty::Int) => {
                        self.line(format!("{t} = call i64 @pyrs_int_neg(i64 {v})"))
                    }
                    (UnOp::Neg, Ty::Float) => self.line(format!("{t} = fneg double {v}")),
                    (UnOp::Not, Ty::Bool) => self.line(format!("{t} = xor i1 {v}, true")),
                    (UnOp::Invert, Ty::Int) => {
                        self.line(format!("{t} = call i64 @pyrs_int_invert(i64 {v})"));
                    }
                    other => unreachable!("bad unary op {other:?}"),
                }
                t
            }
            ExprKind::MakeClosure {
                func,
                captures,
                capture_is_cell,
            } => {
                let n = captures.len() as i64;
                // code pointer: bitcast of the nested function
                let code = format!("@{}", mangle(func));
                let t = self.tmp();
                self.line(format!(
                    "{t} = call ptr @pyrs_closure_new(ptr {code}, i64 {n})"
                ));
                for (i, (cap, is_cell)) in captures.iter().zip(capture_is_cell.iter()).enumerate() {
                    let v = self.emit_expr(cap);
                    let slot = if *is_cell || matches!(cap.ty, Ty::Cell(_)) {
                        // cell pointer as i64
                        let s = self.tmp();
                        self.line(format!("{s} = ptrtoint ptr {v} to i64"));
                        s
                    } else {
                        self.slot_from_value(&v, cap.ty)
                    };
                    self.line(format!(
                        "call void @pyrs_closure_set(ptr {t}, i64 {i}, i64 {slot})"
                    ));
                }
                t
            }
            ExprKind::CallClosure {
                closure,
                args,
                capture_tys,
                func,
            } => {
                let c = self.emit_expr(closure);
                let mut arg_parts: Vec<String> = Vec::new();
                for (i, cty) in capture_tys.iter().enumerate() {
                    let slot = self.tmp();
                    self.line(format!(
                        "{slot} = call i64 @pyrs_closure_get(ptr {c}, i64 {i})"
                    ));
                    let v = self.value_from_slot(&slot, *cty);
                    arg_parts.push(format!("{} {v}", lty(*cty)));
                }
                for a in args {
                    let v = self.emit_expr(a);
                    arg_parts.push(format!("{} {v}", lty(a.ty)));
                }
                let args_s = arg_parts.join(", ");
                let Ty::Closure { ret, .. } = closure.ty else {
                    unreachable!("CallClosure on non-closure");
                };
                let ret_ty = *ret;
                let callee = if func.is_empty() {
                    let code = self.tmp();
                    self.line(format!("{code} = call ptr @pyrs_closure_code(ptr {c})"));
                    code
                } else {
                    format!("@{}", mangle(func))
                };
                if ret_ty == Ty::None {
                    self.line(format!("call void {callee}({args_s})"));
                    "0".to_string()
                } else {
                    let t = self.tmp();
                    self.line(format!("{t} = call {} {callee}({args_s})", lty(ret_ty)));
                    t
                }
            }
            ExprKind::ClosureCap {
                closure,
                index,
                cap_ty,
            } => {
                let c = self.emit_expr(closure);
                let slot = self.tmp();
                self.line(format!(
                    "{slot} = call i64 @pyrs_closure_get(ptr {c}, i64 {index})"
                ));
                self.value_from_slot(&slot, *cap_ty)
            }
            ExprKind::CellNew(inner) => {
                let v = self.emit_expr(inner);
                let slot = self.slot_from_value(&v, inner.ty);
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_cell_new(i64 {slot})"));
                t
            }
            ExprKind::CellNewUnbound => {
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_cell_new_unbound()"));
                t
            }
            ExprKind::CellLoad(cell) => {
                let c = self.emit_expr(cell);
                let slot = self.tmp();
                self.line(format!("{slot} = call i64 @pyrs_cell_load(ptr {c})"));
                let inner_ty = match cell.ty {
                    Ty::Cell(inner) => *inner,
                    _ => expr.ty,
                };
                self.value_from_slot(&slot, inner_ty)
            }
            ExprKind::MakeGenerator {
                func,
                code_from,
                args,
                nlocals,
            } => {
                let code = if let Some(clos) = code_from {
                    let c = self.emit_expr(clos);
                    let code = self.tmp();
                    self.line(format!("{code} = call ptr @pyrs_closure_code(ptr {c})"));
                    code
                } else if func.is_empty() {
                    panic!("MakeGenerator with empty func and no code_from");
                } else {
                    format!("@{}", mangle(func))
                };
                let t = self.tmp();
                self.line(format!(
                    "{t} = call ptr @pyrs_gen_new(ptr {code}, i64 {nlocals})"
                ));
                for (i, a) in args.iter().enumerate() {
                    let v = self.emit_expr(a);
                    let slot = self.slot_from_value(&v, a.ty);
                    self.line(format!(
                        "call void @pyrs_gen_set_local(ptr {t}, i64 {i}, i64 {slot})"
                    ));
                }
                t
            }
            ExprKind::GeneratorNext { generator, send } => {
                self.emit_generator_advance(generator, Some(send), None, expr.ty)
            }
            ExprKind::GeneratorThrow {
                generator,
                exc,
                message,
            } => self.emit_generator_advance(generator, None, Some((*exc, message)), expr.ty),
            ExprKind::GenSentValue => {
                // Load send value delivered at the resume of the preceding Yield.
                let Some(frame) = self.gen_frame.clone() else {
                    panic!("GenSentValue outside generator function");
                };
                let is_none_i = self.tmp();
                self.line(format!(
                    "{is_none_i} = call i32 @pyrs_gen_send_is_none(ptr {frame})"
                ));
                let is_none = self.tmp();
                self.line(format!("{is_none} = icmp ne i32 {is_none_i}, 0"));
                let none_l = self.fresh_block("gsent.none");
                let some_l = self.fresh_block("gsent.some");
                let end_l = self.fresh_block("gsent.end");
                self.line(format!("br i1 {is_none}, label %{none_l}, label %{some_l}"));
                self.start_block(&none_l);
                let none_u = self.emit_to_union("0", Ty::None, expr.ty);
                let nblk = self.cur_block.clone();
                self.line(format!("br label %{end_l}"));
                self.start_block(&some_l);
                let slot = self.tmp();
                self.line(format!(
                    "{slot} = call i64 @pyrs_gen_send_slot(ptr {frame})"
                ));
                // Non-None send payload is the non-None member of Optional[Y].
                let payload_ty = match expr.ty {
                    Ty::Union(ms) => {
                        let non_none: Vec<Ty> =
                            ms.iter().copied().filter(|m| *m != Ty::None).collect();
                        match non_none.len() {
                            1 => non_none[0],
                            _ => self.gen_yield_ty,
                        }
                    }
                    other => other,
                };
                let yval = self.value_from_slot(&slot, payload_ty);
                let some_u = self.emit_to_union(&yval, payload_ty, expr.ty);
                let sblk = self.cur_block.clone();
                self.line(format!("br label %{end_l}"));
                self.start_block(&end_l);
                let t = self.tmp();
                self.line(format!(
                    "{t} = phi {{ i32, i64 }} [ {none_u}, %{nblk} ], [ {some_u}, %{sblk} ]"
                ));
                t
            }
            ExprKind::GeneratorReturnValue(gexpr) => {
                // CPython: bare return / fall-off → StopIteration.value is None;
                // `return N` → value is N. Result type is Optional[payload].
                let g = self.emit_expr(gexpr);
                let has = self.tmp();
                self.line(format!("{has} = call i32 @pyrs_gen_has_return(ptr {g})"));
                let is_set = self.tmp();
                self.line(format!("{is_set} = icmp ne i32 {has}, 0"));
                let some_l = self.fresh_block("gret.some");
                let none_l = self.fresh_block("gret.none");
                let end_l = self.fresh_block("gret.end");
                self.line(format!("br i1 {is_set}, label %{some_l}, label %{none_l}"));
                self.start_block(&some_l);
                let slot = self.tmp();
                self.line(format!("{slot} = call i64 @pyrs_gen_return_value(ptr {g})"));
                // Payload is the non-None member of the Optional result type.
                let payload_ty = match expr.ty {
                    Ty::Union(ms) => {
                        let non_none: Vec<Ty> =
                            ms.iter().copied().filter(|m| *m != Ty::None).collect();
                        match non_none.len() {
                            0 => Ty::None,
                            1 => non_none[0],
                            _ => expr.ty, // unexpected multi-member; use as-is
                        }
                    }
                    other => other,
                };
                let val = self.value_from_slot(&slot, payload_ty);
                let some_u = self.emit_to_union(&val, payload_ty, expr.ty);
                let sblk = self.cur_block.clone();
                self.line(format!("br label %{end_l}"));
                self.start_block(&none_l);
                let none_u = self.emit_to_union("0", Ty::None, expr.ty);
                let nblk = self.cur_block.clone();
                self.line(format!("br label %{end_l}"));
                self.start_block(&end_l);
                let t = self.tmp();
                self.line(format!(
                    "{t} = phi {{ i32, i64 }} [ {some_u}, %{sblk} ], [ {none_u}, %{nblk} ]"
                ));
                t
            }
            ExprKind::NewObject { class_id } => {
                let info = self
                    .classes
                    .get(*class_id as usize)
                    .expect("NewObject class_id")
                    .clone();
                let sty = class_struct_ty(&info);
                // sizeof via GEP: ptrtoint of getelementptr one past
                let size_p = self.tmp();
                self.line(format!("{size_p} = getelementptr {sty}, ptr null, i32 1"));
                let nbytes = self.tmp();
                self.line(format!("{nbytes} = ptrtoint ptr {size_p} to i64"));
                let t = self.tmp();
                self.line(format!(
                    "{t} = call ptr @pyrs_object_new(i64 {}, i64 {nbytes})",
                    *class_id as i64
                ));
                // Typed zero-init for every field (raw memset leaves int as
                // untagged 0 → SEGV on arithmetic/print).
                for (fi, (_, fty)) in info.fields.iter().enumerate() {
                    let idx = fi as i32 + 1;
                    let fp = self.tmp();
                    self.line(format!(
                        "{fp} = getelementptr inbounds {sty}, ptr {t}, i32 0, i32 {idx}"
                    ));
                    let zero = match fty {
                        Ty::Int => "1".to_string(), // tagged small 0
                        Ty::Float => fconst(0.0),
                        Ty::Bool => "false".to_string(),
                        Ty::None | Ty::Any => "0".to_string(),
                        Ty::Union(_) => "zeroinitializer".to_string(),
                        Ty::Str
                        | Ty::List(_)
                        | Ty::Tuple(_)
                        | Ty::Dict { .. }
                        | Ty::Set(_)
                        | Ty::File
                        | Ty::Closure { .. }
                        | Ty::BoundMethod { .. }
                        | Ty::Cell(_)
                        | Ty::Generator { .. }
                        | Ty::Exception
                        | Ty::Class(_) => "null".to_string(),
                    };
                    self.line(format!("store {} {zero}, ptr {fp}", lty(*fty)));
                }
                t
            }
            ExprKind::GetField {
                object,
                class_id,
                field_index,
            } => {
                let obj = self.emit_expr(object);
                let info = self
                    .classes
                    .get(*class_id as usize)
                    .expect("GetField class_id");
                let sty = class_struct_ty(info);
                let fty = info.fields[*field_index as usize].1;
                let idx = *field_index as i32 + 1;
                let fp = self.tmp();
                self.line(format!(
                    "{fp} = getelementptr inbounds {sty}, ptr {obj}, i32 0, i32 {idx}"
                ));
                let t = self.tmp();
                self.line(format!("{t} = load {}, ptr {fp}", lty(fty)));
                t
            }
            ExprKind::GetFieldPartial {
                object,
                candidates,
                attr,
            } => {
                let obj = self.emit_expr(object);
                self.emit_get_field_partial(&obj, candidates, attr, expr.ty)
            }
            ExprKind::BindMethod { object, .. } => {
                // Heap box: { ptr object } (method resolved from type at call).
                let obj = self.emit_expr(object);
                let box_p = self.tmp();
                self.line(format!("{box_p} = call ptr @malloc(i64 8)"));
                let fp = self.tmp();
                self.line(format!(
                    "{fp} = getelementptr inbounds {{ ptr }}, ptr {box_p}, i32 0, i32 0"
                ));
                self.line(format!("store ptr {obj}, ptr {fp}"));
                box_p
            }
            ExprKind::CallBoundMethod {
                bound,
                args,
                direct_func,
                candidates,
                virtual_dispatch,
            } => {
                let b = self.emit_expr(bound);
                let fp = self.tmp();
                self.line(format!(
                    "{fp} = getelementptr inbounds {{ ptr }}, ptr {b}, i32 0, i32 0"
                ));
                let self_ptr = self.tmp();
                self.line(format!("{self_ptr} = load ptr, ptr {fp}"));
                let mut arg_vals = vec![(self_ptr, Ty::Class(0))]; // class id unused for lty
                // Prefer object's actual class type from bound ty when available.
                if let Ty::BoundMethod { class_id, .. } = bound.ty {
                    arg_vals[0].1 = Ty::Class(class_id);
                }
                for a in args {
                    arg_vals.push((self.emit_expr(a), a.ty));
                }
                if !*virtual_dispatch || candidates.len() <= 1 {
                    return self.emit_direct_method_call(direct_func, &arg_vals, expr.ty);
                }
                let tid_p = self.tmp();
                let self_obj = &arg_vals[0].0;
                self.line(format!(
                    "{tid_p} = getelementptr inbounds {{ i64 }}, ptr {self_obj}, i32 0, i32 0"
                ));
                let tid = self.tmp();
                self.line(format!("{tid} = load i64, ptr {tid_p}"));
                let end_l = self.fresh_block("bm.end");
                let default_l = self.fresh_block("bm.def");
                let mut cases = String::new();
                let mut blocks = Vec::new();
                for (cid, func) in candidates {
                    let bl = self.fresh_block(&format!("bm.{}", cid));
                    cases.push_str(&format!(" i64 {}, label %{bl}", *cid as i64));
                    blocks.push((bl, func.clone()));
                }
                self.line(format!("switch i64 {tid}, label %{default_l} [{cases} ]"));
                let ret_void = expr.ty == Ty::None;
                let mut phi_preds = Vec::new();
                for (bl, func) in &blocks {
                    self.start_block(bl);
                    let r = self.emit_direct_method_call(func, &arg_vals, expr.ty);
                    let cblk = self.cur_block.clone();
                    if !ret_void {
                        phi_preds.push((r, cblk));
                    }
                    self.line(format!("br label %{end_l}"));
                }
                self.start_block(&default_l);
                let rdef = self.emit_direct_method_call(direct_func, &arg_vals, expr.ty);
                let dblk = self.cur_block.clone();
                if !ret_void {
                    phi_preds.push((rdef, dblk));
                }
                self.line(format!("br label %{end_l}"));
                self.start_block(&end_l);
                if ret_void {
                    return "0".to_string();
                }
                let t = self.tmp();
                let mut phi = format!("{t} = phi {} ", lty(expr.ty));
                for (i, (v, b)) in phi_preds.iter().enumerate() {
                    if i > 0 {
                        phi.push_str(", ");
                    }
                    phi.push_str(&format!("[ {v}, %{b} ]"));
                }
                self.line(phi);
                return t;
            }
            ExprKind::CallMethod {
                direct_func,
                candidates,
                args,
                virtual_dispatch,
            } => {
                let mut arg_vals = Vec::new();
                for a in args {
                    arg_vals.push((self.emit_expr(a), a.ty));
                }
                if !*virtual_dispatch || candidates.len() <= 1 {
                    return self.emit_direct_method_call(direct_func, &arg_vals, expr.ty);
                }
                // Virtual: switch on type_id of self (args[0]).
                let self_ptr = &arg_vals[0].0;
                let tid_p = self.tmp();
                self.line(format!(
                    "{tid_p} = getelementptr inbounds {{ i64 }}, ptr {self_ptr}, i32 0, i32 0"
                ));
                let tid = self.tmp();
                self.line(format!("{tid} = load i64, ptr {tid_p}"));
                let end_l = self.fresh_block("vcall.end");
                let default_l = self.fresh_block("vcall.def");
                let mut cases = String::new();
                let mut blocks = Vec::new();
                for (cid, func) in candidates {
                    let bl = self.fresh_block(&format!("vcall.{}", cid));
                    cases.push_str(&format!(" i64 {}, label %{bl}", *cid as i64));
                    blocks.push((bl, func.clone()));
                }
                self.line(format!("switch i64 {tid}, label %{default_l} [{cases} ]"));
                let ret_void = expr.ty == Ty::None;
                let mut phi_preds = Vec::new();
                for (bl, func) in &blocks {
                    self.start_block(bl);
                    let r = self.emit_direct_method_call(func, &arg_vals, expr.ty);
                    let cblk = self.cur_block.clone();
                    if !ret_void {
                        phi_preds.push((r, cblk));
                    }
                    self.line(format!("br label %{end_l}"));
                }
                self.start_block(&default_l);
                // Fall back to direct_func for unknown type_ids.
                let rdef = self.emit_direct_method_call(direct_func, &arg_vals, expr.ty);
                let dblk = self.cur_block.clone();
                if !ret_void {
                    phi_preds.push((rdef, dblk));
                }
                self.line(format!("br label %{end_l}"));
                self.start_block(&end_l);
                if ret_void {
                    "0".to_string()
                } else {
                    let t = self.tmp();
                    let mut phi = format!("{t} = phi {} ", lty(expr.ty));
                    for (i, (v, b)) in phi_preds.iter().enumerate() {
                        if i > 0 {
                            phi.push_str(", ");
                        }
                        phi.push_str(&format!("[ {v}, %{b} ]"));
                    }
                    self.line(phi);
                    t
                }
            }
            ExprKind::ClassIsInstance { value, class_id } => {
                let v = self.emit_expr(value);
                let n = self.classes.len() as i64;
                let t = self.tmp();
                self.line(format!(
                    "{t} = call i32 @pyrs_isinstance_class(ptr {v}, i64 {}, ptr @pyrs_class_parents, i64 {n})",
                    *class_id as i64
                ));
                let b = self.tmp();
                self.line(format!("{b} = icmp ne i32 {t}, 0"));
                b
            }
            ExprKind::ObjectToStr(obj) => {
                let v = self.emit_expr(obj);
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_str_from_object(ptr {v})"));
                t
            }
            ExprKind::Binary { op, left, right } => self.emit_binary(*op, left, right),
            ExprKind::TupleLit(items) => {
                let n = items.len() as i64;
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_tuple_new(i64 {n})"));
                for (i, item) in items.iter().enumerate() {
                    let v = self.emit_expr(item);
                    let slot = self.slot_from_value(&v, item.ty);
                    self.line(format!(
                        "call void @pyrs_tuple_set(ptr {t}, i64 {i}, i64 {slot}, i32 {})",
                        elem_tag(&item.ty)
                    ));
                }
                t
            }
            ExprKind::DictLit(pairs) => {
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_dict_new()"));
                let Ty::Dict { key, value } = expr.ty else {
                    unreachable!("DictLit type");
                };
                for (k, v) in pairs {
                    let kv = self.emit_expr(k);
                    let vv = self.emit_expr(v);
                    let kslot = self.slot_from_value(&kv, k.ty);
                    let vslot = self.slot_from_value(&vv, v.ty);
                    self.line(format!(
                        "call void @pyrs_dict_set(ptr {t}, i64 {kslot}, i32 {}, i64 {vslot}, i32 {})",
                        elem_tag(key),
                        elem_tag(value)
                    ));
                }
                t
            }
            ExprKind::DictNew => {
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_dict_new()"));
                t
            }
            ExprKind::SetLit(items) => {
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_set_new()"));
                for item in items {
                    let v = self.emit_expr(item);
                    let slot = self.slot_from_value(&v, item.ty);
                    self.line(format!(
                        "call void @pyrs_set_add(ptr {t}, i64 {slot}, i32 {})",
                        elem_tag(&item.ty)
                    ));
                }
                t
            }
            ExprKind::SetNew => {
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_set_new()"));
                t
            }
            ExprKind::DictGet { dict, key, default } => {
                let d = self.emit_expr(dict);
                let k = self.emit_expr(key);
                let Ty::Dict { key: kt, value: vt } = dict.ty else {
                    unreachable!();
                };
                let kslot = self.slot_from_value(&k, *kt);
                let out_p = self.tmp();
                self.line(format!("{out_p} = alloca i64, align 8"));
                let found = self.tmp();
                self.line(format!(
                    "{found} = call i32 @pyrs_dict_get_default(ptr {d}, i64 {kslot}, i32 {}, ptr {out_p})",
                    elem_tag(kt)
                ));
                let hit = self.tmp();
                self.line(format!("{hit} = icmp ne i32 {found}, 0"));
                let then_l = self.fresh_block("dget.then");
                let else_l = self.fresh_block("dget.else");
                let end_l = self.fresh_block("dget.end");
                self.line(format!("br i1 {hit}, label %{then_l}, label %{else_l}"));
                self.start_block(&then_l);
                let slot_hit = self.tmp();
                self.line(format!("{slot_hit} = load i64, ptr {out_p}"));
                let raw_hit = self.value_from_slot(&slot_hit, *vt);
                // Bare get: result is optional(val) — wrap on hit.
                let val_hit = if expr.ty != *vt {
                    self.emit_to_union(&raw_hit, *vt, expr.ty)
                } else {
                    raw_hit
                };
                let then_pred = self.cur_block.clone();
                self.line(format!("br label %{end_l}"));
                self.start_block(&else_l);
                let val_miss = self.emit_expr(default);
                let else_pred = self.cur_block.clone();
                self.line(format!("br label %{end_l}"));
                self.start_block(&end_l);
                let phi = self.tmp();
                self.line(format!(
                    "{phi} = phi {} [ {val_hit}, %{then_pred} ], [ {val_miss}, %{else_pred} ]",
                    lty(expr.ty)
                ));
                phi
            }
            ExprKind::DictPop { dict, key, default } => {
                let d = self.emit_expr(dict);
                let k = self.emit_expr(key);
                let Ty::Dict { key: kt, value: vt } = dict.ty else {
                    unreachable!();
                };
                let kslot = self.slot_from_value(&k, *kt);
                let out_p = self.tmp();
                self.line(format!("{out_p} = alloca i64, align 8"));
                let has_def = if default.is_some() { 1 } else { 0 };
                let def_slot = if let Some(def) = default {
                    let dv = self.emit_expr(def);
                    self.slot_from_value(&dv, def.ty)
                } else {
                    "0".to_string()
                };
                let slot = self.tmp();
                self.line(format!(
                    "{slot} = call i64 @pyrs_dict_pop(ptr {d}, i64 {kslot}, i32 {}, i32 {has_def}, i64 {def_slot}, ptr {out_p})",
                    elem_tag(kt)
                ));
                // pop writes result to *out
                let loaded = self.tmp();
                self.line(format!("{loaded} = load i64, ptr {out_p}"));
                let _ = slot;
                self.value_from_slot(&loaded, *vt)
            }
            ExprKind::DictKeys(d) => {
                let v = self.emit_expr(d);
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_dict_keys(ptr {v})"));
                t
            }
            ExprKind::DictValues(d) => {
                let v = self.emit_expr(d);
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_dict_values(ptr {v})"));
                t
            }
            ExprKind::DictItems(d) => {
                let v = self.emit_expr(d);
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_dict_items(ptr {v})"));
                t
            }
            ExprKind::SetToList(s) => {
                let v = self.emit_expr(s);
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_set_elements(ptr {v})"));
                t
            }
            ExprKind::ListCopy(list) => {
                let v = self.emit_expr(list);
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_list_copy(ptr {v})"));
                t
            }
            ExprKind::ListFromStr(s) => {
                let v = self.emit_expr(s);
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_list_from_str(ptr {v})"));
                t
            }
            ExprKind::DictCopy(d) => {
                let v = self.emit_expr(d);
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_dict_copy(ptr {v})"));
                t
            }
            ExprKind::SetFromList { list, elem } => {
                let v = self.emit_expr(list);
                let t = self.tmp();
                self.line(format!(
                    "{t} = call ptr @pyrs_set_from_list(ptr {v}, i32 {})",
                    elem_tag(elem)
                ));
                t
            }
            ExprKind::SetFromStr(s) => {
                let v = self.emit_expr(s);
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_set_from_str(ptr {v})"));
                t
            }
            ExprKind::DictFromPairs { pairs, key, value } => {
                let v = self.emit_expr(pairs);
                let t = self.tmp();
                self.line(format!(
                    "{t} = call ptr @pyrs_dict_from_pairs(ptr {v}, i32 {}, i32 {})",
                    elem_tag(key),
                    elem_tag(value)
                ));
                t
            }
        }
    }

    /// Resume a generator via `next`/`send` or `throw`. Returns Optional[Y].
    /// `send` and `throw` are mutually exclusive (throw path arms exception).
    fn emit_generator_advance(
        &mut self,
        generator: &Expr,
        send: Option<&Expr>,
        throw: Option<(ir::ExcType, &Expr)>,
        result_ty: Ty,
    ) -> String {
        let g = self.emit_expr(generator);
        let Ty::Generator { yield_ty } = generator.ty else {
            unreachable!("generator advance on non-generator");
        };
        let yty = *yield_ty;
        let end_l = self.fresh_block("gend");
        let none_u_early: String;
        let early_blk: String;
        let resume_l = self.fresh_block("gadv.resume");

        if throw.is_none() {
            // send/next on a finished generator → Optional None (no re-entry).
            let alrd = self.tmp();
            self.line(format!("{alrd} = call i32 @pyrs_gen_done(ptr {g})"));
            let is_alrd = self.tmp();
            self.line(format!("{is_alrd} = icmp ne i32 {alrd}, 0"));
            let early_l = self.fresh_block("gadv.early");
            self.line(format!(
                "br i1 {is_alrd}, label %{early_l}, label %{resume_l}"
            ));
            self.start_block(&early_l);
            none_u_early = self.emit_to_union("0", Ty::None, result_ty);
            early_blk = self.cur_block.clone();
            self.line(format!("br label %{end_l}"));
            self.start_block(&resume_l);
        } else {
            // throw always arms (raises immediately if already done / not started).
            none_u_early = String::new();
            early_blk = String::new();
            self.line(format!("br label %{resume_l}"));
            self.start_block(&resume_l);
        }

        if let Some((exc, message)) = throw {
            let m = self.emit_expr(message);
            self.line(format!(
                "call void @pyrs_gen_set_throw(ptr {g}, i64 {}, ptr {m})",
                exc.tag()
            ));
        } else {
            let send_expr = send.expect("send or throw required");
            let is_none = matches!(send_expr.kind, ExprKind::ConstNone) || send_expr.ty == Ty::None;
            if is_none {
                self.line(format!(
                    "call void @pyrs_gen_set_send(ptr {g}, i64 0, i64 1)"
                ));
            } else {
                let v = self.emit_expr(send_expr);
                let slot = self.slot_from_value(&v, send_expr.ty);
                self.line(format!(
                    "call void @pyrs_gen_set_send(ptr {g}, i64 {slot}, i64 0)"
                ));
            }
        }
        let code = self.tmp();
        // PyrsGen and PyrsClosure both start with void* code
        self.line(format!("{code} = call ptr @pyrs_closure_code(ptr {g})"));
        let done = self.tmp();
        self.line(format!("{done} = call i32 {code}(ptr {g})"));
        let is_done = self.tmp();
        self.line(format!("{is_done} = icmp ne i32 {done}, 0"));
        let done_l = self.fresh_block("gdone");
        let yield_l = self.fresh_block("gyield");
        self.line(format!(
            "br i1 {is_done}, label %{done_l}, label %{yield_l}"
        ));
        self.start_block(&yield_l);
        let yslot = self.tmp();
        self.line(format!("{yslot} = call i64 @pyrs_gen_yield_value(ptr {g})"));
        let yval = self.value_from_slot(&yslot, yty);
        let yunion = self.emit_to_union(&yval, yty, result_ty);
        let yblk = self.cur_block.clone();
        self.line(format!("br label %{end_l}"));
        self.start_block(&done_l);
        let none_u = self.emit_to_union("0", Ty::None, result_ty);
        let dblk = self.cur_block.clone();
        self.line(format!("br label %{end_l}"));
        self.start_block(&end_l);
        let t = self.tmp();
        if throw.is_none() {
            self.line(format!(
                "{t} = phi {{ i32, i64 }} [ {none_u_early}, %{early_blk} ], \
                 [ {yunion}, %{yblk} ], [ {none_u}, %{dblk} ]"
            ));
        } else {
            self.line(format!(
                "{t} = phi {{ i32, i64 }} [ {yunion}, %{yblk} ], [ {none_u}, %{dblk} ]"
            ));
        }
        t
    }

    /// Whether the current outermost remaining try is left by break/continue
    /// of the innermost loop (same rule as `break_exits_innermost_try`).
    fn outer_exited_by_break(&self) -> bool {
        let Some(outer) = self.tries.last() else {
            return false;
        };
        if self.loops.is_empty() {
            return false;
        }
        outer.loops_at_entry > self.loops.len() - 1
    }

    /// Propagate exit kind to the enclosing try: mark its frame dead, pop,
    /// and jump to its finally (used after an inner finally completes).
    fn emit_chain_to_outer(&mut self, kind: i32) {
        let outer = self.tries.last().expect("chain without outer try");
        let o_exit = outer.exit_ptr.clone();
        let o_live = outer.live_ptr.clone();
        let o_fin = outer.fin_l.clone();
        self.store_try_i32(kind, &o_exit);
        // outer frame is still live until we leave it
        let was = self.load_try_i32(&o_live);
        self.store_try_i32(0, &o_live);
        let need = self.tmp();
        self.line(format!("{need} = icmp ne i32 {was}, 0"));
        let pop_l = self.fresh_block("try.chain.pop");
        let go_l = self.fresh_block("try.chain.go");
        self.line(format!("br i1 {need}, label %{pop_l}, label %{go_l}"));
        self.start_block(&pop_l);
        self.line("call void @pyrs_try_pop()");
        self.line(format!("br label %{go_l}"));
        self.start_block(&go_l);
        self.line(format!("br label %{o_fin}"));
    }

    /// True when break/continue of the innermost loop should leave the
    /// innermost try (try is nested inside that loop). False when the loop
    /// is nested inside the try (break stays in the try body).
    fn break_exits_innermost_try(&self) -> bool {
        let Some(try_scope) = self.tries.last() else {
            return false;
        };
        if self.loops.is_empty() {
            return false;
        }
        // loops_at_entry == loops.len() when try started inside the current
        // loop depth; then loops_at_entry > loop_index (loops.len()-1).
        try_scope.loops_at_entry > self.loops.len() - 1
    }

    /// Leave the protected try region: pop the setjmp frame at most once
    /// (runtime live flag), record exit kind, jump to finally.
    fn emit_try_exit(&mut self, kind: i32, ret: Option<(String, Ty)>) {
        let scope = self.tries.last().expect("emit_try_exit without active try");
        let exit_ptr = scope.exit_ptr.clone();
        let live_ptr = scope.live_ptr.clone();
        let fin_l = scope.fin_l.clone();
        if let Some((v, ty)) = ret {
            let ptr = self
                .try_ret_ptr
                .clone()
                .expect("return value in try without ret slot");
            self.line(format!("store {} {v}, ptr {ptr}", lty(ty)));
        }
        self.store_try_i32(kind, &exit_ptr);
        // Runtime live flag: every exit edge may reach here; pop at most once.
        let was = self.load_try_i32(&live_ptr);
        self.store_try_i32(0, &live_ptr);
        let need = self.tmp();
        self.line(format!("{need} = icmp ne i32 {was}, 0"));
        let pop_l = self.fresh_block("try.pop");
        let go_l = self.fresh_block("try.tofin");
        self.line(format!("br i1 {need}, label %{pop_l}, label %{go_l}"));
        self.start_block(&pop_l);
        self.line("call void @pyrs_try_pop()");
        self.line(format!("br label %{go_l}"));
        self.start_block(&go_l);
        self.line(format!("br label %{fin_l}"));
        self.terminated = true;
    }

    /// After finally body: dispatch on exit kind (normal / return / break /
    /// continue / reraise), chaining into an outer try's finally when needed.
    fn emit_finally_dispatch(&mut self, exit_ptr: &str, end_l: &str) {
        let kind = self.load_try_i32(exit_ptr);

        let normal_l = self.fresh_block("try.x.normal");
        let ret_l = self.fresh_block("try.x.ret");
        let brk_l = self.fresh_block("try.x.brk");
        let cont_l = self.fresh_block("try.x.cont");
        let re_l = self.fresh_block("try.x.re");
        let def_l = self.fresh_block("try.x.bad");

        self.line(format!(
            "switch i32 {kind}, label %{def_l} [ \
             i32 {TRY_EXIT_NORMAL}, label %{normal_l} \
             i32 {TRY_EXIT_RETURN}, label %{ret_l} \
             i32 {TRY_EXIT_BREAK}, label %{brk_l} \
             i32 {TRY_EXIT_CONTINUE}, label %{cont_l} \
             i32 {TRY_EXIT_RERAISE}, label %{re_l} ]"
        ));

        // fall through after try
        self.start_block(&normal_l);
        self.line(format!("br label %{end_l}"));

        // return — chain to outer finally or ret (generators: mark done)
        self.start_block(&ret_l);
        if self.tries.last().is_some() {
            self.emit_chain_to_outer(TRY_EXIT_RETURN);
        } else if self.gen_frame.is_some() {
            let frame = self.gen_frame.clone().unwrap();
            self.line(format!("call void @pyrs_gen_set_done(ptr {frame})"));
            self.line("ret i32 1");
        } else if self.fn_ret == Ty::None {
            self.line("ret void");
        } else {
            let ptr = self.try_ret_ptr.clone().expect("ret slot");
            let v = self.tmp();
            self.line(format!("{v} = load {}, ptr {ptr}", lty(self.fn_ret)));
            self.line(format!("ret {} {v}", lty(self.fn_ret)));
        }
        self.terminated = true;

        // break (path only taken if a break ran inside this try)
        self.start_block(&brk_l);
        if self.outer_exited_by_break() {
            self.emit_chain_to_outer(TRY_EXIT_BREAK);
        } else if let Some((_, end)) = self.loops.last() {
            let end = end.clone();
            self.line(format!("br label %{end}"));
        } else {
            self.line("unreachable");
        }
        self.terminated = true;

        // continue
        self.start_block(&cont_l);
        if self.outer_exited_by_break() {
            self.emit_chain_to_outer(TRY_EXIT_CONTINUE);
        } else if let Some((cont, _)) = self.loops.last() {
            let cont = cont.clone();
            self.line(format!("br label %{cont}"));
        } else {
            self.line("unreachable");
        }
        self.terminated = true;

        // reraise: longjmp into an outer try's setjmp (frame must still be live),
        // or print+exit if nothing catches. Do not jump to outer finally — CPython
        // runs outer handlers first, then outer finally.
        // At generator boundary: always mark done. close() swallows uncaught
        // GeneratorExit; throw(GeneratorExit) propagates like any other exc.
        self.start_block(&re_l);
        if self.gen_frame.is_some() && self.tries.is_empty() {
            let frame = self.gen_frame.clone().unwrap();
            self.line(format!("call void @pyrs_gen_set_done(ptr {frame})"));
            let is_ge = self.tmp();
            self.line(format!("{is_ge} = call i32 @pyrs_gen_is_genexit()"));
            let closing = self.tmp();
            self.line(format!(
                "{closing} = call i32 @pyrs_gen_closing(ptr {frame})"
            ));
            let swallow = self.tmp();
            // swallow only when close() is injecting GE (not throw(GE))
            self.line(format!("{swallow} = and i32 {is_ge}, {closing}"));
            let swallow_b = self.tmp();
            self.line(format!("{swallow_b} = icmp ne i32 {swallow}, 0"));
            let ge_l = self.fresh_block("try.x.genexit");
            let die_l = self.fresh_block("try.x.reraise");
            self.line(format!("br i1 {swallow_b}, label %{ge_l}, label %{die_l}"));
            self.start_block(&ge_l);
            self.line("ret i32 1");
            self.terminated = true;
            self.start_block(&die_l);
            self.line("call void @pyrs_reraise()");
            self.line("unreachable");
            self.terminated = true;
        } else {
            self.line("call void @pyrs_reraise()");
            self.line("unreachable");
            self.terminated = true;
        }

        self.start_block(&def_l);
        self.line("unreachable");
    }

    fn emit_try(
        &mut self,
        body: &[Stmt],
        handlers: &[ir::ExceptHandler],
        orelse: &[Stmt],
        finally: &[Stmt],
    ) {
        // Generators: use preallocated pool slots (dominate yield resume).
        // Ordinary functions: fresh allocas.
        let (exit_ptr, live_ptr, phase_ptr, pool_idx) = if self.gen_frame.is_some() {
            let i = self.gen_try_pool_next;
            self.gen_try_pool_next += 1;
            if i >= self.gen_try_pool.len() {
                // Over-deep nesting vs pre-scan — allocate anyway (may not
                // dominate resume, but rare; grow pool defensively).
                let e = self.tmp();
                let l = self.tmp();
                let p = self.tmp();
                self.line(format!("{e} = alloca i32, align 4"));
                self.line(format!("{l} = alloca i32, align 4"));
                self.line(format!("{p} = alloca i32, align 4"));
                (e, l, p, i)
            } else {
                let (e, l, p) = self.gen_try_pool[i].clone();
                (e, l, p, i)
            }
        } else {
            let e = self.tmp();
            let l = self.tmp();
            let p = self.tmp();
            self.line(format!("{e} = alloca i32, align 4"));
            self.line(format!("{l} = alloca i32, align 4"));
            self.line(format!("{p} = alloca i32, align 4"));
            (e, l, p, 0usize)
        };
        self.store_try_i32(TRY_EXIT_NORMAL, &exit_ptr);
        self.store_try_i32(1, &live_ptr);
        // 0 = try body, 1 = except handler, 2 = else
        // phase != 0 on longjmp → finally/reraise (skip re-dispatch to handlers)
        self.store_try_i32(0, &phase_ptr);

        let fin_l = self.fresh_block("try.finally");
        let end_l = self.fresh_block("try.end");
        let exc_l = self.fresh_block("try.exc");
        self.tries.push(TryScope {
            fin_l: fin_l.clone(),
            end_l: end_l.clone(),
            exc_l: exc_l.clone(),
            exit_ptr: exit_ptr.clone(),
            live_ptr: live_ptr.clone(),
            phase_ptr: phase_ptr.clone(),
            loops_at_entry: self.loops.len(),
            pool_idx,
        });

        let frame = self.tmp();
        self.line(format!("{frame} = call ptr @pyrs_try_push()"));
        let jc = self.tmp();
        // setjmp on the frame pointer — jmp_buf is the first field
        self.line(format!(
            "{jc} = call i32 @setjmp(ptr {frame}) returns_twice"
        ));
        let ok = self.tmp();
        self.line(format!("{ok} = icmp eq i32 {jc}, 0"));
        let body_l = self.fresh_block("try.body");
        self.line(format!("br i1 {ok}, label %{body_l}, label %{exc_l}"));

        // ---- try body ----
        self.start_block(&body_l);
        self.emit_block(body);
        if !self.terminated {
            if orelse.is_empty() {
                self.emit_try_exit(TRY_EXIT_NORMAL, None);
            } else {
                // Normal completion: run else before finally. phase=2 so
                // exceptions in else skip this try's handlers (CPython).
                self.store_try_i32(2, &phase_ptr);
                let else_l = self.fresh_block("try.else");
                self.line(format!("br label %{else_l}"));
                self.start_block(&else_l);
                self.emit_block(orelse);
                if !self.terminated {
                    self.emit_try_exit(TRY_EXIT_NORMAL, None);
                }
            }
        }

        // ---- exception path (frame still live until structured exit) ----
        self.start_block(&exc_l);
        let phase = self.load_try_i32(&phase_ptr);
        let in_handler = self.tmp();
        self.line(format!("{in_handler} = icmp ne i32 {phase}, 0"));
        let hraise_l = self.fresh_block("try.hreraise");
        let dispatch_l = self.fresh_block("try.hdispatch");
        self.line(format!(
            "br i1 {in_handler}, label %{hraise_l}, label %{dispatch_l}"
        ));

        // Second longjmp: raise/trap while running a handler or else → finally/reraise
        self.start_block(&hraise_l);
        self.emit_try_exit(TRY_EXIT_RERAISE, None);

        // First longjmp: body exception → match handlers (do not pop yet)
        self.start_block(&dispatch_l);
        self.store_try_i32(1, &phase_ptr);
        let ety = self.tmp();
        self.line(format!("{ety} = call i32 @pyrs_exc_type()"));

        let mut next_check = String::new();
        for (i, (filter, bind, hbody)) in handlers.iter().enumerate() {
            let match_l = self.fresh_block(&format!("try.h{i}"));
            let nomatch_l = self.fresh_block(&format!("try.h{i}.no"));
            if i > 0 {
                self.start_block(&next_check);
            }
            match filter {
                None => {
                    self.line(format!("br label %{match_l}"));
                }
                Some(excs) if excs.is_empty() => {
                    self.line(format!("br label %{match_l}"));
                }
                Some(excs) if excs.len() == 1 => {
                    // Hierarchy: OSError catches FileNotFoundError, etc.
                    let m = self.tmp();
                    self.line(format!(
                        "{m} = call i32 @pyrs_exc_matches(i32 {}, i32 {ety})",
                        excs[0].tag()
                    ));
                    let cmp = self.tmp();
                    self.line(format!("{cmp} = icmp ne i32 {m}, 0"));
                    self.line(format!("br i1 {cmp}, label %{match_l}, label %{nomatch_l}"));
                }
                Some(excs) => {
                    // Multi-type `except (A, B, …):` — OR of hierarchy matches.
                    let mut acc = String::new();
                    for (j, exc) in excs.iter().enumerate() {
                        let m = self.tmp();
                        self.line(format!(
                            "{m} = call i32 @pyrs_exc_matches(i32 {}, i32 {ety})",
                            exc.tag()
                        ));
                        let cmp = self.tmp();
                        self.line(format!("{cmp} = icmp ne i32 {m}, 0"));
                        if j == 0 {
                            acc = cmp;
                        } else {
                            let or = self.tmp();
                            self.line(format!("{or} = or i1 {acc}, {cmp}"));
                            acc = or;
                        }
                    }
                    self.line(format!("br i1 {acc}, label %{match_l}, label %{nomatch_l}"));
                }
            }
            self.start_block(&match_l);
            if let Some(name) = bind {
                // Bind a first-class exception object (type tag + message).
                let obj = self.tmp();
                self.line(format!("{obj} = call ptr @pyrs_exc_object()"));
                // Generators store locals in the frame, not `%v.*` allocas.
                if let (Some(frame), Some(idx)) = (
                    self.gen_frame.clone(),
                    self.gen_local_index.get(name).copied(),
                ) {
                    let slot = self.tmp();
                    self.line(format!("{slot} = ptrtoint ptr {obj} to i64"));
                    self.line(format!(
                        "call void @pyrs_gen_set_local(ptr {frame}, i64 {idx}, i64 {slot})"
                    ));
                } else {
                    self.line(format!("store ptr {obj}, ptr %v.{name}"));
                }
            }
            self.line("call void @pyrs_exc_clear()");
            // Frame remains live so traps/raises in the handler longjmp here
            // with phase=1 and take hraise_l → finally.
            self.emit_block(hbody);
            if !self.terminated {
                self.emit_try_exit(TRY_EXIT_NORMAL, None);
            }
            next_check = nomatch_l;
            if filter.is_none() {
                self.start_block(&next_check);
                self.line("unreachable");
                next_check.clear();
                break;
            }
        }
        // unmatched (or bare try/finally): finally then reraise
        if handlers.is_empty() {
            self.emit_try_exit(TRY_EXIT_RERAISE, None);
        } else if !next_check.is_empty() {
            self.start_block(&next_check);
            self.emit_try_exit(TRY_EXIT_RERAISE, None);
        }

        // ---- finally (only reached via emit_try_exit, which pops once) ----
        let scope = self.tries.pop().expect("try scope");
        // Keep this try's pool slot reserved while finally runs so a nested
        // try inside finally does not reuse exit_ptr/phase and clobber the
        // outer exit kind (needed after yield resume in finally).
        if self.gen_frame.is_some() {
            self.gen_fin_stack.push(FinallyScope {
                exit_ptr: scope.exit_ptr.clone(),
                pool_idx: scope.pool_idx,
            });
        }
        self.start_block(&fin_l);
        self.emit_block(finally);
        if self.gen_frame.is_some() {
            self.gen_fin_stack.pop();
            // Free this slot (and anything nested that completed under it).
            if self.gen_try_pool_next > scope.pool_idx {
                self.gen_try_pool_next = scope.pool_idx;
            }
        }
        if !self.terminated {
            self.emit_finally_dispatch(&scope.exit_ptr, &scope.end_l);
        }

        self.start_block(&end_l);
    }

    fn emit_binary(&mut self, op: BinOp, left: &Expr, right: &Expr) -> String {
        // short-circuit and/or need control flow, not plain instructions
        if matches!(op, BinOp::And | BinOp::Or) {
            return self.emit_short_circuit(op, left, right);
        }

        // string / list / tuple operations dispatch to the runtime
        if left.ty == Ty::Str {
            return self.emit_str_binary(op, left, right);
        }
        if matches!(left.ty, Ty::List(_)) {
            return self.emit_list_binary(op, left, right);
        }
        if matches!(left.ty, Ty::Tuple(_)) {
            return self.emit_tuple_binary(op, left, right);
        }

        let l = self.emit_expr(left);
        let r = self.emit_expr(right);
        let ty = left.ty; // semantic guarantees both sides match

        match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul => {
                if ty == Ty::Int {
                    let callee = match op {
                        BinOp::Add => "pyrs_int_add",
                        BinOp::Sub => "pyrs_int_sub",
                        BinOp::Mul => "pyrs_int_mul",
                        _ => unreachable!(),
                    };
                    let t = self.tmp();
                    self.line(format!("{t} = call i64 @{callee}(i64 {l}, i64 {r})"));
                    return t;
                }
                let instr = match (op, ty) {
                    (BinOp::Add, Ty::Float) => "fadd",
                    (BinOp::Sub, Ty::Float) => "fsub",
                    (BinOp::Mul, Ty::Float) => "fmul",
                    other => unreachable!("bad arith {other:?}"),
                };
                let t = self.tmp();
                self.line(format!("{t} = {instr} {} {l}, {r}", lty(ty)));
                t
            }
            BinOp::Pow => match ty {
                // repeated squaring in the runtime; negative exponent traps
                Ty::Int => {
                    let t = self.tmp();
                    self.line(format!("{t} = call i64 @pyrs_ipow(i64 {l}, i64 {r})"));
                    t
                }
                Ty::Float => {
                    // Python: 0.0 ** negative raises instead of returning inf
                    let zb = self.tmp();
                    self.line(format!("{zb} = fcmp oeq double {l}, {}", fconst(0.0)));
                    let ne = self.tmp();
                    self.line(format!("{ne} = fcmp olt double {r}, {}", fconst(0.0)));
                    let bad = self.tmp();
                    self.line(format!("{bad} = and i1 {zb}, {ne}"));
                    let trap_l = self.fresh_block("pow.trap");
                    let ok_l = self.fresh_block("pow.ok");
                    self.line(format!("br i1 {bad}, label %{trap_l}, label %{ok_l}"));
                    self.start_block(&trap_l);
                    self.emit_die("ZeroDivisionError: 0.0 cannot be raised to a negative power");
                    self.start_block(&ok_l);
                    let t = self.tmp();
                    self.line(format!(
                        "{t} = call double @llvm.pow.f64(double {l}, double {r})"
                    ));
                    t
                }
                other => unreachable!("pow on {other:?}"),
            },
            // true division: always float (semantic inserted the casts)
            BinOp::Div => {
                self.guard_zero(&r, Ty::Float, "ZeroDivisionError: division by zero");
                let t = self.tmp();
                self.line(format!("{t} = fdiv double {l}, {r}"));
                t
            }
            BinOp::FloorDiv => match ty {
                Ty::Int => {
                    let t = self.tmp();
                    self.line(format!(
                        "{t} = call i64 @pyrs_int_floordiv(i64 {l}, i64 {r})"
                    ));
                    t
                }
                Ty::Float => {
                    self.guard_zero(&r, Ty::Float, "ZeroDivisionError: division by zero");
                    // CPython float_divmod semantics live in the runtime
                    let t = self.tmp();
                    self.line(format!(
                        "{t} = call double @pyrs_ffloordiv(double {l}, double {r})"
                    ));
                    t
                }
                other => unreachable!("floordiv on {other:?}"),
            },
            BinOp::Mod => match ty {
                Ty::Int => {
                    let t = self.tmp();
                    self.line(format!("{t} = call i64 @pyrs_int_mod(i64 {l}, i64 {r})"));
                    t
                }
                Ty::Float => {
                    self.guard_zero(&r, Ty::Float, "ZeroDivisionError: division by zero");
                    // CPython float_divmod semantics live in the runtime
                    let t = self.tmp();
                    self.line(format!(
                        "{t} = call double @pyrs_fmod_floored(double {l}, double {r})"
                    ));
                    t
                }
                other => unreachable!("mod on {other:?}"),
            },
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let t = self.tmp();
                match ty {
                    Ty::Int => {
                        if matches!(op, BinOp::Eq | BinOp::Ne) {
                            let c = self.tmp();
                            self.line(format!("{c} = call i32 @pyrs_int_eq(i64 {l}, i64 {r})"));
                            let eq = self.tmp();
                            self.line(format!("{eq} = icmp ne i32 {c}, 0"));
                            if matches!(op, BinOp::Eq) {
                                // t is outer; reassign via move — emit into t
                                self.line(format!("{t} = or i1 {eq}, false"));
                            } else {
                                self.line(format!("{t} = xor i1 {eq}, true"));
                            }
                        } else {
                            let c = self.tmp();
                            self.line(format!("{c} = call i32 @pyrs_int_cmp(i64 {l}, i64 {r})"));
                            let cc = match op {
                                BinOp::Lt => "slt",
                                BinOp::Le => "sle",
                                BinOp::Gt => "sgt",
                                BinOp::Ge => "sge",
                                _ => unreachable!(),
                            };
                            self.line(format!("{t} = icmp {cc} i32 {c}, 0"));
                        }
                    }
                    // Bool is i1; used by match `case True`/`case False` and bool == bool.
                    Ty::Bool => {
                        let cc = match op {
                            BinOp::Eq => "eq",
                            BinOp::Ne => "ne",
                            BinOp::Lt => "ult",
                            BinOp::Le => "ule",
                            BinOp::Gt => "ugt",
                            BinOp::Ge => "uge",
                            _ => unreachable!(),
                        };
                        self.line(format!("{t} = icmp {cc} i1 {l}, {r}"));
                    }
                    Ty::Float => {
                        // ordered except Ne: Python nan != x is True
                        let cc = match op {
                            BinOp::Eq => "oeq",
                            BinOp::Ne => "une",
                            BinOp::Lt => "olt",
                            BinOp::Le => "ole",
                            BinOp::Gt => "ogt",
                            BinOp::Ge => "oge",
                            _ => unreachable!(),
                        };
                        self.line(format!("{t} = fcmp {cc} double {l}, {r}"));
                    }
                    other => unreachable!("comparison on {other:?}"),
                }
                t
            }
            BinOp::And | BinOp::Or => unreachable!("handled above"),
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                let callee = match op {
                    BinOp::BitAnd => "pyrs_int_and",
                    BinOp::BitOr => "pyrs_int_or",
                    BinOp::BitXor => "pyrs_int_xor",
                    _ => unreachable!(),
                };
                let t = self.tmp();
                self.line(format!("{t} = call i64 @{callee}(i64 {l}, i64 {r})"));
                t
            }
            BinOp::LShift | BinOp::RShift => {
                let callee = if matches!(op, BinOp::LShift) {
                    "pyrs_int_lshift"
                } else {
                    "pyrs_int_rshift"
                };
                let t = self.tmp();
                self.line(format!("{t} = call i64 @{callee}(i64 {l}, i64 {r})"));
                t
            }
        }
    }

    /// Binary ops whose left operand is a list (`+`/`*`/`==`/`!=`).
    fn emit_list_binary(&mut self, op: BinOp, left: &Expr, right: &Expr) -> String {
        let l = self.emit_expr(left);
        let r = self.emit_expr(right);
        match op {
            BinOp::Add => {
                let t = self.tmp();
                self.line(format!(
                    "{t} = call ptr @pyrs_list_concat(ptr {l}, ptr {r})"
                ));
                t
            }
            // semantic normalizes the int count to the right operand
            BinOp::Mul => {
                let n = self.emit_unbox_i64(&r);
                let t = self.tmp();
                self.line(format!(
                    "{t} = call ptr @pyrs_list_repeat(ptr {l}, i64 {n})"
                ));
                t
            }
            BinOp::Eq | BinOp::Ne => {
                let Ty::List(elem) = left.ty else {
                    unreachable!("list eq without list type");
                };
                let c = self.tmp();
                self.line(format!(
                    "{c} = call i32 @pyrs_list_eq(ptr {l}, ptr {r}, i32 {})",
                    elem_tag(elem)
                ));
                let eq = self.tmp();
                self.line(format!("{eq} = icmp ne i32 {c}, 0"));
                if matches!(op, BinOp::Eq) {
                    eq
                } else {
                    let t = self.tmp();
                    self.line(format!("{t} = xor i1 {eq}, true"));
                    t
                }
            }
            other => unreachable!("bad list op {other:?}"),
        }
    }

    fn emit_tuple_binary(&mut self, op: BinOp, left: &Expr, right: &Expr) -> String {
        let l = self.emit_expr(left);
        let r = self.emit_expr(right);
        match op {
            BinOp::Eq | BinOp::Ne => {
                let c = self.tmp();
                self.line(format!("{c} = call i32 @pyrs_tuple_eq(ptr {l}, ptr {r})"));
                let eq = self.tmp();
                self.line(format!("{eq} = icmp ne i32 {c}, 0"));
                if matches!(op, BinOp::Eq) {
                    eq
                } else {
                    let t = self.tmp();
                    self.line(format!("{t} = xor i1 {eq}, true"));
                    t
                }
            }
            other => unreachable!("bad tuple op {other:?}"),
        }
    }

    /// Binary ops whose left operand is a string.
    fn emit_str_binary(&mut self, op: BinOp, left: &Expr, right: &Expr) -> String {
        let l = self.emit_expr(left);
        let r = self.emit_expr(right);
        match op {
            BinOp::Add => {
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_str_concat(ptr {l}, ptr {r})"));
                t
            }
            // semantic normalizes the int count to the right operand
            BinOp::Mul => {
                let n = self.emit_unbox_i64(&r);
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_str_repeat(ptr {l}, i64 {n})"));
                t
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let c = self.tmp();
                self.line(format!("{c} = call i32 @pyrs_str_cmp(ptr {l}, ptr {r})"));
                let cc = match op {
                    BinOp::Eq => "eq",
                    BinOp::Ne => "ne",
                    BinOp::Lt => "slt",
                    BinOp::Le => "sle",
                    BinOp::Gt => "sgt",
                    BinOp::Ge => "sge",
                    _ => unreachable!(),
                };
                let t = self.tmp();
                self.line(format!("{t} = icmp {cc} i32 {c}, 0"));
                t
            }
            other => unreachable!("bad str op {other:?}"),
        }
    }

    fn emit_short_circuit(&mut self, op: BinOp, left: &Expr, right: &Expr) -> String {
        let rhs_l = self.fresh_block("sc.rhs");
        let end_l = self.fresh_block("sc.end");

        let l = self.emit_expr(left);
        // Branch on truthiness; yield the left or right value (Python and/or).
        // Truthiness may insert null-check blocks — record the predecessor
        // for the phi *after* that, immediately before the branch.
        let cond = self.emit_truthiness(&l, left.ty);
        let lhs_block = self.cur_block.clone();
        let (on_true, on_false) = match op {
            // a and b: if a is false, skip rhs and produce a
            BinOp::And => (rhs_l.clone(), end_l.clone()),
            // a or b: if a is true, skip rhs and produce a
            BinOp::Or => (end_l.clone(), rhs_l.clone()),
            _ => unreachable!(),
        };
        self.line(format!("br i1 {cond}, label %{on_true}, label %{on_false}"));

        self.start_block(&rhs_l);
        let r = self.emit_expr(right);
        let rhs_block = self.cur_block.clone();
        self.line(format!("br label %{end_l}"));

        self.start_block(&end_l);
        let t = self.tmp();
        let rty = lty(left.ty);
        self.line(format!(
            "{t} = phi {rty} [ {l}, %{lhs_block} ], [ {r}, %{rhs_block} ]"
        ));
        t
    }

    /// `value is None` / `is not None`.
    fn emit_is_none(&mut self, value: &Expr, not: bool) -> String {
        match value.ty {
            Ty::None => {
                // pure None: is None → true, is not None → false
                if not { "false" } else { "true" }.to_string()
            }
            Ty::Union(members) => {
                let v = self.emit_expr(value);
                if let Some(none_idx) = members.iter().position(|m| *m == Ty::None) {
                    let tag = self.tmp();
                    self.line(format!("{tag} = extractvalue {{ i32, i64 }} {v}, 0"));
                    let t = self.tmp();
                    let pred = if not { "ne" } else { "eq" };
                    self.line(format!("{t} = icmp {pred} i32 {tag}, {none_idx}"));
                    t
                } else {
                    // union without None: is None → false
                    if not { "true" } else { "false" }.to_string()
                }
            }
            // concrete non-optional: is None → false
            _ => {
                // still evaluate for side effects
                let _ = self.emit_expr(value);
                if not { "true" } else { "false" }.to_string()
            }
        }
    }

    /// Pointer/slot identity for `is` / `is not` (non-None).
    fn emit_is_identity(&mut self, left: &Expr, right: &Expr, not: bool) -> String {
        let l = self.emit_expr(left);
        let r = self.emit_expr(right);
        let t = self.tmp();
        match left.ty {
            Ty::Int | Ty::Bool | Ty::Any => {
                let pred = if not { "ne" } else { "eq" };
                // bool is i1; zext both for a uniform compare when mixed — types match.
                if left.ty == Ty::Bool {
                    self.line(format!("{t} = icmp {pred} i1 {l}, {r}"));
                } else {
                    // Int and Any are both i64 slots (Any is boxed heap ptr bits).
                    self.line(format!("{t} = icmp {pred} i64 {l}, {r}"));
                }
            }
            Ty::Float => {
                // Bit-identity (NaN is NaN); not float equality.
                let lb = self.tmp();
                let rb = self.tmp();
                self.line(format!("{lb} = bitcast double {l} to i64"));
                self.line(format!("{rb} = bitcast double {r} to i64"));
                let pred = if not { "ne" } else { "eq" };
                self.line(format!("{t} = icmp {pred} i64 {lb}, {rb}"));
            }
            Ty::Union(_) => {
                // Compare tag and payload bits.
                let lt = self.tmp();
                let lp = self.tmp();
                let rt = self.tmp();
                let rp = self.tmp();
                self.line(format!("{lt} = extractvalue {{ i32, i64 }} {l}, 0"));
                self.line(format!("{lp} = extractvalue {{ i32, i64 }} {l}, 1"));
                self.line(format!("{rt} = extractvalue {{ i32, i64 }} {r}, 0"));
                self.line(format!("{rp} = extractvalue {{ i32, i64 }} {r}, 1"));
                let teq = self.tmp();
                let peq = self.tmp();
                self.line(format!("{teq} = icmp eq i32 {lt}, {rt}"));
                self.line(format!("{peq} = icmp eq i64 {lp}, {rp}"));
                let both = self.tmp();
                self.line(format!("{both} = and i1 {teq}, {peq}"));
                if not {
                    self.line(format!("{t} = xor i1 {both}, true"));
                } else {
                    return both;
                }
            }
            _ => {
                // Heap pointers (str/list/dict/set/tuple/file/closure/gen/cell)
                let pred = if not { "ne" } else { "eq" };
                self.line(format!("{t} = icmp {pred} ptr {l}, {r}"));
            }
        }
        t
    }

    /// Truthiness test for short-circuit `and`/`or` (same rules as ToBool).
    fn emit_truthiness(&mut self, v: &str, ty: Ty) -> String {
        match ty {
            Ty::Bool => v.to_string(),
            Ty::None => {
                // None is always falsy
                "false".to_string()
            }
            Ty::Int => {
                let c = self.tmp();
                self.line(format!("{c} = call i32 @pyrs_int_truth(i64 {v})"));
                let t = self.tmp();
                self.line(format!("{t} = icmp ne i32 {c}, 0"));
                t
            }
            Ty::Float => {
                let t = self.tmp();
                // une: NaN is truthy, like Python
                self.line(format!("{t} = fcmp une double {v}, {}", fconst(0.0)));
                t
            }
            Ty::Str | Ty::List(_) | Ty::Tuple(_) | Ty::Dict { .. } | Ty::Set(_) => {
                let len = self.emit_len(v);
                let t = self.tmp();
                self.line(format!("{t} = icmp ne i64 {len}, 0"));
                t
            }
            // Exception instances are always truthy (CPython BaseException).
            // User class instances are always truthy when non-null (we never
            // produce null instances after construction).
            Ty::Exception | Ty::Class(_) => {
                let _ = v;
                "true".to_string()
            }
            // Any: runtime covers all list tags (4+8*k including list[str]/list[Any]),
            // empty containers, null/None, nested union boxes.
            Ty::Any => {
                let c = self.tmp();
                self.line(format!("{c} = call i32 @pyrs_any_truth(i64 {v})"));
                let t = self.tmp();
                self.line(format!("{t} = icmp ne i32 {c}, 0"));
                t
            }
            Ty::Union(members) => {
                // If tag is None → false; else truthiness of active member payload.
                let tag = self.tmp();
                self.line(format!("{tag} = extractvalue {{ i32, i64 }} {v}, 0"));
                let payload = self.tmp();
                self.line(format!("{payload} = extractvalue {{ i32, i64 }} {v}, 1"));
                let end_l = self.fresh_block("truth.end");
                let default_l = self.fresh_block("truth.def");
                let mut cases = String::new();
                let mut blocks = Vec::new();
                for (i, m) in members.iter().enumerate() {
                    let bl = self.fresh_block(&format!("truth.{i}"));
                    cases.push_str(&format!(" i32 {i}, label %{bl}"));
                    blocks.push((bl, *m));
                }
                self.line(format!("switch i32 {tag}, label %{default_l} [{cases} ]"));
                let mut phi_args = Vec::new();
                for (bl, m) in &blocks {
                    self.start_block(bl);
                    let cond = if *m == Ty::None {
                        "false".to_string()
                    } else {
                        let val = self.value_from_slot(&payload, *m);
                        self.emit_truthiness(&val, *m)
                    };
                    let pred = self.cur_block.clone();
                    self.line(format!("br label %{end_l}"));
                    phi_args.push((cond, pred));
                }
                self.start_block(&default_l);
                let def_pred = self.cur_block.clone();
                self.line(format!("br label %{end_l}"));
                phi_args.push(("false".to_string(), def_pred));
                self.start_block(&end_l);
                let t = self.tmp();
                let parts: Vec<String> = phi_args
                    .iter()
                    .map(|(c, b)| format!("[ {c}, %{b} ]"))
                    .collect();
                self.line(format!("{t} = phi i1 {}", parts.join(", ")));
                t
            }
            other => unreachable!("truthiness on {other:?}"),
        }
    }

    /// Trap with a ZeroDivisionError when the divisor is zero (float path).
    fn guard_zero(&mut self, divisor: &str, ty: Ty, message: &str) {
        let is_zero = self.tmp();
        match ty {
            Ty::Int => self.line(format!("{is_zero} = icmp eq i64 {divisor}, 0")),
            Ty::Float => self.line(format!(
                "{is_zero} = fcmp oeq double {divisor}, {}",
                fconst(0.0)
            )),
            other => unreachable!("guard_zero on {other:?}"),
        }
        let trap_l = self.fresh_block("div.trap");
        let ok_l = self.fresh_block("div.ok");
        self.line(format!("br i1 {is_zero}, label %{trap_l}, label %{ok_l}"));
        self.start_block(&trap_l);
        self.emit_die(message);
        self.start_block(&ok_l);
    }
}
