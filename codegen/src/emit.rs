//! Lowers the typed IR to LLVM IR in its textual format.
//!
//! The textual IR is the serialization boundary handed to the C++ shim,
//! which parses, verifies, optimizes and emits native object code.
//!
//! Semantics preserved from Python:
//! - `//` and `%` use floored division (result sign follows the divisor)
//! - division by zero traps with a ZeroDivisionError message instead of UB
//! - `int(float)` is a saturating conversion (no UB on NaN/overflow)
//! - `i64::MIN // -1` wraps instead of being UB (select on the divisor)
//! - `**` on ints uses runtime repeated squaring (negative exponent traps);
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

use ir::{BinOp, Expr, ExprKind, FileFn, Function, Module, Stmt, StrFn, Ty, UnOp};

pub fn emit_llvm_ir(module: &Module) -> String {
    let mut e = Emitter::default();
    e.emit_module(module);
    e.finish()
}

fn lty(ty: Ty) -> &'static str {
    match ty {
        Ty::Int => "i64",
        Ty::Float => "double",
        Ty::Bool => "i1",
        Ty::Str => "ptr",
        Ty::List(_) | Ty::Tuple(_) | Ty::Dict { .. } | Ty::Set(_) | Ty::File => "ptr",
        Ty::None => "void",
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
/// Scalars 0-3; nested list `4 + 8 * inner`; tuple=5, dict=6, set=7.
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
        Ty::File | Ty::None => unreachable!("no print tag for {ty:?}"),
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
struct TryScope {
    fin_l: String,
    end_l: String,
    /// `alloca i32` holding TRY_EXIT_*
    exit_ptr: String,
    /// Runtime flag (`alloca i32`, 1=live): pop at most once on structured exit.
    live_ptr: String,
    /// `loops.len()` when this try was entered.
    loops_at_entry: usize,
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
        }
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
        out.push_str("declare void @pyrs_exc_clear()\n");
        out.push_str("declare ptr @pyrs_tuple_new(i64)\n");
        out.push_str("declare void @pyrs_tuple_set(ptr, i64, i64, i32)\n");
        out.push_str("declare i64 @pyrs_tuple_get(ptr, i64)\n");
        out.push_str("declare i32 @pyrs_tuple_eq(ptr, ptr)\n");
        out.push_str("declare void @pyrs_unpack_check(i64, i64)\n");
        out.push_str("declare ptr @pyrs_dict_new()\n");
        out.push_str("declare void @pyrs_dict_set(ptr, i64, i32, i64, i32)\n");
        out.push_str("declare i64 @pyrs_dict_get(ptr, i64, i32)\n");
        out.push_str("declare i32 @pyrs_dict_get_default(ptr, i64, i32, ptr)\n");
        out.push_str("declare void @pyrs_dict_del(ptr, i64, i32)\n");
        out.push_str("declare i32 @pyrs_dict_contains(ptr, i64, i32)\n");
        out.push_str("declare void @pyrs_dict_clear(ptr)\n");
        out.push_str("declare i64 @pyrs_dict_pop(ptr, i64, i32, i32, i64, ptr)\n");
        out.push_str("declare ptr @pyrs_dict_keys(ptr)\n");
        out.push_str("declare ptr @pyrs_dict_values(ptr)\n");
        out.push_str("declare ptr @pyrs_dict_items(ptr)\n");
        out.push_str("declare ptr @pyrs_set_new()\n");
        out.push_str("declare void @pyrs_set_add(ptr, i64, i32)\n");
        out.push_str("declare void @pyrs_set_remove(ptr, i64, i32)\n");
        out.push_str("declare void @pyrs_set_discard(ptr, i64, i32)\n");
        out.push_str("declare i32 @pyrs_set_contains(ptr, i64, i32)\n");
        out.push_str("declare void @pyrs_set_clear(ptr)\n");
        out.push_str("declare ptr @pyrs_set_elements(ptr)\n");
        out.push_str("declare ptr @pyrs_str_concat(ptr, ptr)\n");
        out.push_str("declare ptr @pyrs_str_repeat(ptr, i64)\n");
        out.push_str("declare i32 @pyrs_str_cmp(ptr, ptr)\n");
        out.push_str("declare ptr @pyrs_str_index(ptr, i64)\n");
        out.push_str("declare ptr @pyrs_str_from_int(i64)\n");
        out.push_str("declare ptr @pyrs_str_from_float(double)\n");
        out.push_str("declare ptr @pyrs_str_format_float(double, i64)\n");
        out.push_str("declare ptr @pyrs_str_from_bool(i32)\n");
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
        out.push_str("declare double @pyrs_ffloordiv(double, double)\n");
        out.push_str("declare double @pyrs_fmod_floored(double, double)\n");
        out.push_str("declare double @llvm.fabs.f64(double)\n");
        out.push_str("declare double @llvm.floor.f64(double)\n");
        out.push_str("declare double @llvm.pow.f64(double, double)\n");
        out.push_str("declare i64 @llvm.abs.i64(i64, i1)\n");
        out.push_str("declare i64 @llvm.fptosi.sat.i64.f64(double)\n\n");
        out.push_str(&self.global_defs);
        out.push_str(&self.string_defs);
        out.push('\n');
        out.push_str(&self.funcs);
        out
    }

    fn emit_module(&mut self, module: &Module) {
        // module globals, zero/null-initialized; assigned when the entry
        // function runs its top-level statements
        for (name, ty) in &module.globals {
            let init = match ty {
                Ty::Float => fconst(0.0),
                Ty::Bool => "false".to_string(),
                Ty::Str | Ty::List(_) | Ty::Tuple(_) | Ty::Dict { .. } | Ty::Set(_) | Ty::File => {
                    "null".to_string()
                }
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
        self.funcs.push_str(&format!(
            "define i32 @main(i32 %argc, ptr %argv) {{\nentry:\n  \
             call void @pyrs_set_args(i32 %argc, ptr %argv)\n  \
             call void @{entry}()\n  ret i32 0\n}}\n\n"
        ));
    }

    // ---- low-level helpers ----

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
                let pred = if is_max { "sgt" } else { "slt" };
                self.line(format!("{pick_r} = icmp {pred} i64 {r}, {l}"));
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
            Ty::Int => "0".to_string(),
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
                self.line(format!("{acc_next} = add i64 {acc}, {slot}"));
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
            Ty::Int => value.to_string(),
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
            Ty::Str | Ty::List(_) | Ty::Tuple(_) | Ty::Dict { .. } | Ty::Set(_) => {
                let t = self.tmp();
                self.line(format!("{t} = ptrtoint ptr {value} to i64"));
                t
            }
            other => unreachable!("no slot representation for {other:?}"),
        }
    }

    fn value_from_slot(&mut self, slot: &str, ty: Ty) -> String {
        match ty {
            Ty::Int => slot.to_string(),
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
            Ty::Str | Ty::List(_) | Ty::Tuple(_) | Ty::Dict { .. } | Ty::Set(_) => {
                let t = self.tmp();
                self.line(format!("{t} = inttoptr i64 {slot} to ptr"));
                t
            }
            other => unreachable!("no slot representation for {other:?}"),
        }
    }

    // ---- functions ----

    fn emit_function(&mut self, func: &Function) {
        self.body.clear();
        self.tmp = 0;
        self.blk = 0;
        self.loops.clear();
        self.tries.clear();
        self.fn_ret = func.ret;
        self.try_ret_ptr = None;

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
                Ty::Str | Ty::List(_) | Ty::Tuple(_) | Ty::Dict { .. } | Ty::Set(_) | Ty::File => {
                    "null".to_string()
                }
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
            lty(func.ret),
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
                self.line(format!("store {} {v}, ptr %v.{name}", lty(value.ty)));
            }
            Stmt::GlobalAssign { name, value } => {
                let v = self.emit_expr(value);
                self.line(format!("store {} {v}, ptr @g.{name}", lty(value.ty)));
            }
            Stmt::IndexAssign { base, index, value } => {
                let b = self.emit_expr(base);
                let i = self.emit_expr(index);
                let v = self.emit_expr(value);
                let slot = self.slot_from_value(&v, value.ty);
                match base.ty {
                    Ty::List(_) => {
                        let addr = self.emit_list_elem_addr(
                            &b,
                            &i,
                            "IndexError: list assignment index out of range",
                        );
                        self.line(format!("store i64 {slot}, ptr {addr}"));
                    }
                    Ty::Dict { key, value: val } => {
                        let kslot = self.slot_from_value(&i, *key);
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
            Stmt::UnpackCheck { len, expected } => {
                let n = self.emit_expr(len);
                self.line(format!(
                    "call void @pyrs_unpack_check(i64 {n}, i64 {expected})"
                ));
            }
            Stmt::Raise { exc, message } => {
                let m = self.emit_expr(message);
                // pyrs_raise wants a C string: data pointer after length header
                let data = self.tmp();
                self.line(format!(
                    "{data} = getelementptr inbounds i8, ptr {m}, i64 8"
                ));
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
                finally,
            } => {
                self.emit_try(body, handlers, finally);
            }
            Stmt::ListAppend { list, value } => {
                let l = self.emit_expr(list);
                let v = self.emit_expr(value);
                let slot = self.slot_from_value(&v, value.ty);
                self.line(format!("call void @pyrs_list_push(ptr {l}, i64 {slot})"));
            }
            Stmt::ListInsert { list, index, value } => {
                let l = self.emit_expr(list);
                let i = self.emit_expr(index);
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
                if !self.tries.is_empty() {
                    self.emit_try_exit(TRY_EXIT_RETURN, None);
                } else {
                    self.line("ret void");
                    self.terminated = true;
                }
            }
            Stmt::Return(Some(value)) => {
                let v = self.emit_expr(value);
                if !self.tries.is_empty() {
                    self.emit_try_exit(TRY_EXIT_RETURN, Some((v, value.ty)));
                } else {
                    self.line(format!("ret {} {v}", lty(value.ty)));
                    self.terminated = true;
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
                    match arg.ty {
                        Ty::Int => self.line(format!("call void @pyrs_print_int(i64 {v})")),
                        Ty::Float => self.line(format!("call void @pyrs_print_float(double {v})")),
                        Ty::Bool => {
                            let ext = self.tmp();
                            self.line(format!("{ext} = zext i1 {v} to i32"));
                            self.line(format!("call void @pyrs_print_bool(i32 {ext})"));
                        }
                        Ty::Str => self.line(format!("call void @pyrs_print_str(ptr {v})")),
                        Ty::List(elem) => self.line(format!(
                            "call void @pyrs_print_list(ptr {v}, i32 {})",
                            elem_tag(elem)
                        )),
                        Ty::Tuple(_) => self.line(format!("call void @pyrs_print_tuple(ptr {v})")),
                        Ty::Dict { .. } => {
                            self.line(format!("call void @pyrs_print_dict(ptr {v})"))
                        }
                        Ty::Set(_) => self.line(format!("call void @pyrs_print_set(ptr {v})")),
                        Ty::File | Ty::None => {
                            unreachable!("semantic rejects {:?} print args", arg.ty)
                        }
                    }
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
            ExprKind::ConstInt(v) => v.to_string(),
            ExprKind::ConstFloat(v) => fconst(*v),
            ExprKind::ConstBool(v) => v.to_string(),
            ExprKind::ConstStr(s) => self.intern_string(s),
            ExprKind::Local(name) => {
                let t = self.tmp();
                self.line(format!("{t} = load {}, ptr %v.{name}", lty(expr.ty)));
                t
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
                        t
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
                    self.line(format!("call void @{callee}({args_str})"));
                    String::new()
                } else {
                    let t = self.tmp();
                    self.line(format!("{t} = call {} @{callee}({args_str})", lty(expr.ty)));
                    t
                }
            }
            ExprKind::Index { base, index } => {
                let b = self.emit_expr(base);
                let i = self.emit_expr(index);
                match base.ty {
                    Ty::Str => {
                        let t = self.tmp();
                        self.line(format!("{t} = call ptr @pyrs_str_index(ptr {b}, i64 {i})"));
                        t
                    }
                    Ty::List(elem) => {
                        let addr =
                            self.emit_list_elem_addr(&b, &i, "IndexError: list index out of range");
                        let slot = self.tmp();
                        self.line(format!("{slot} = load i64, ptr {addr}"));
                        self.value_from_slot(&slot, *elem)
                    }
                    Ty::Tuple(_) => {
                        let slot = self.tmp();
                        self.line(format!(
                            "{slot} = call i64 @pyrs_tuple_get(ptr {b}, i64 {i})"
                        ));
                        self.value_from_slot(&slot, expr.ty)
                    }
                    Ty::Dict { key, value } => {
                        let kslot = self.slot_from_value(&i, *key);
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
                let lo_v = self.emit_expr(lo);
                let hi_v = self.emit_expr(hi);
                let step_v = self.emit_expr(step);
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
                    t
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
                    other => unreachable!("contains on {other:?}"),
                }
                let t = self.tmp();
                self.line(format!("{t} = icmp ne i32 {c}, 0"));
                t
            }
            ExprKind::ListPop { list, index } => {
                let l = self.emit_expr(list);
                let i = self.emit_expr(index);
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
                let t = self.tmp();
                self.line(format!(
                    "{t} = call i64 @pyrs_list_index(ptr {l}, i64 {slot}, i32 {})",
                    elem_tag(&value.ty)
                ));
                t
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
                let c = self.emit_expr(cap);
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
                self.emit_len(&v)
            }
            ExprKind::Abs(inner) => {
                let v = self.emit_expr(inner);
                let t = self.tmp();
                match inner.ty {
                    // is_int_min_poison=false: abs(i64::MIN) wraps (no bigints)
                    Ty::Int => {
                        self.line(format!("{t} = call i64 @llvm.abs.i64(i64 {v}, i1 false)"))
                    }
                    Ty::Float => self.line(format!("{t} = call double @llvm.fabs.f64(double {v})")),
                    other => unreachable!("Abs on {other:?}"),
                }
                t
            }
            // Python min/max: if right is strictly less/greater, take right;
            // otherwise left (ties and NaN comparisons keep the left operand).
            ExprKind::Min { left, right } => self.emit_min_max(false, left, right),
            ExprKind::Max { left, right } => self.emit_min_max(true, left, right),
            ExprKind::Sum(list) => self.emit_sum(list),
            ExprKind::IntToFloat(inner) => {
                let v = self.emit_expr(inner);
                let t = self.tmp();
                self.line(format!("{t} = sitofp i64 {v} to double"));
                t
            }
            ExprKind::FloatToInt(inner) => {
                let v = self.emit_expr(inner);
                // Python raises on nan/inf instead of converting
                let is_nan = self.tmp();
                self.line(format!("{is_nan} = fcmp uno double {v}, {}", fconst(0.0)));
                let nan_l = self.fresh_block("toint.nan");
                let ok1_l = self.fresh_block("toint.num");
                self.line(format!("br i1 {is_nan}, label %{nan_l}, label %{ok1_l}"));
                self.start_block(&nan_l);
                self.emit_die("ValueError: cannot convert float NaN to integer");
                self.start_block(&ok1_l);
                let abs = self.tmp();
                self.line(format!("{abs} = call double @llvm.fabs.f64(double {v})"));
                let is_inf = self.tmp();
                self.line(format!(
                    "{is_inf} = fcmp oeq double {abs}, {}",
                    fconst(f64::INFINITY)
                ));
                let inf_l = self.fresh_block("toint.inf");
                let ok2_l = self.fresh_block("toint.ok");
                self.line(format!("br i1 {is_inf}, label %{inf_l}, label %{ok2_l}"));
                self.start_block(&inf_l);
                self.emit_die("OverflowError: cannot convert float infinity to integer");
                self.start_block(&ok2_l);
                let t = self.tmp();
                // saturating: still defined if the finite value exceeds i64
                self.line(format!(
                    "{t} = call i64 @llvm.fptosi.sat.i64.f64(double {v})"
                ));
                t
            }
            ExprKind::BoolToInt(inner) => {
                let v = self.emit_expr(inner);
                let t = self.tmp();
                self.line(format!("{t} = zext i1 {v} to i64"));
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
            ExprKind::FloatFormat { value, precision } => {
                let v = self.emit_expr(value);
                let t = self.tmp();
                self.line(format!(
                    "{t} = call ptr @pyrs_str_format_float(double {v}, i64 {precision})"
                ));
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
            ExprKind::ToBool(inner) => {
                let v = self.emit_expr(inner);
                match inner.ty {
                    Ty::Int => {
                        let t = self.tmp();
                        self.line(format!("{t} = icmp ne i64 {v}, 0"));
                        t
                    }
                    // une: NaN is truthy, like Python
                    Ty::Float => {
                        let t = self.tmp();
                        self.line(format!("{t} = fcmp une double {v}, {}", fconst(0.0)));
                        t
                    }
                    // containers share the leading i64 length field
                    Ty::Str | Ty::List(_) | Ty::Tuple(_) | Ty::Dict { .. } | Ty::Set(_) => {
                        let len = self.emit_len(&v);
                        let t = self.tmp();
                        self.line(format!("{t} = icmp ne i64 {len}, 0"));
                        t
                    }
                    other => unreachable!("ToBool on {other:?}"),
                }
            }
            ExprKind::Unary { op, operand } => {
                let v = self.emit_expr(operand);
                let t = self.tmp();
                match (op, operand.ty) {
                    (UnOp::Neg, Ty::Int) => self.line(format!("{t} = sub i64 0, {v}")),
                    (UnOp::Neg, Ty::Float) => self.line(format!("{t} = fneg double {v}")),
                    (UnOp::Not, Ty::Bool) => self.line(format!("{t} = xor i1 {v}, true")),
                    other => unreachable!("bad unary op {other:?}"),
                }
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
                let val_hit = self.value_from_slot(&slot_hit, *vt);
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
                    lty(*vt)
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
        }
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
        self.line(format!("store i32 {kind}, ptr {o_exit}"));
        // outer frame is still live until we leave it
        let was = self.tmp();
        self.line(format!("{was} = load i32, ptr {o_live}"));
        self.line(format!("store i32 0, ptr {o_live}"));
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
        self.line(format!("store i32 {kind}, ptr {exit_ptr}"));
        // Runtime live flag: every exit edge may reach here; pop at most once.
        let was = self.tmp();
        self.line(format!("{was} = load i32, ptr {live_ptr}"));
        self.line(format!("store i32 0, ptr {live_ptr}"));
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
        let kind = self.tmp();
        self.line(format!("{kind} = load i32, ptr {exit_ptr}"));

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

        // return — chain to outer finally or ret
        self.start_block(&ret_l);
        if self.tries.last().is_some() {
            self.emit_chain_to_outer(TRY_EXIT_RETURN);
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
        self.start_block(&re_l);
        self.line("call void @pyrs_reraise()");
        self.line("unreachable");
        self.terminated = true;

        self.start_block(&def_l);
        self.line("unreachable");
    }

    fn emit_try(
        &mut self,
        body: &[Stmt],
        handlers: &[(Option<ir::ExcType>, Option<String>, Vec<Stmt>)],
        finally: &[Stmt],
    ) {
        let exit_ptr = self.tmp();
        self.line(format!("{exit_ptr} = alloca i32, align 4"));
        self.line(format!("store i32 {TRY_EXIT_NORMAL}, ptr {exit_ptr}"));
        let live_ptr = self.tmp();
        self.line(format!("{live_ptr} = alloca i32, align 4"));
        self.line(format!("store i32 1, ptr {live_ptr}"));
        // 0 = try body, 1 = except handler (second longjmp → finally/reraise)
        let phase_ptr = self.tmp();
        self.line(format!("{phase_ptr} = alloca i32, align 4"));
        self.line(format!("store i32 0, ptr {phase_ptr}"));

        let fin_l = self.fresh_block("try.finally");
        let end_l = self.fresh_block("try.end");
        self.tries.push(TryScope {
            fin_l: fin_l.clone(),
            end_l: end_l.clone(),
            exit_ptr: exit_ptr.clone(),
            live_ptr: live_ptr.clone(),
            loops_at_entry: self.loops.len(),
        });

        let frame = self.tmp();
        self.line(format!("{frame} = call ptr @pyrs_try_push()"));
        let jc = self.tmp();
        // setjmp on the frame pointer — jmp_buf is the first field
        self.line(format!("{jc} = call i32 @setjmp(ptr {frame})"));
        let ok = self.tmp();
        self.line(format!("{ok} = icmp eq i32 {jc}, 0"));
        let body_l = self.fresh_block("try.body");
        let exc_l = self.fresh_block("try.exc");
        self.line(format!("br i1 {ok}, label %{body_l}, label %{exc_l}"));

        // ---- try body ----
        self.start_block(&body_l);
        self.emit_block(body);
        if !self.terminated {
            self.emit_try_exit(TRY_EXIT_NORMAL, None);
        }

        // ---- exception path (frame still live until structured exit) ----
        self.start_block(&exc_l);
        let phase = self.tmp();
        self.line(format!("{phase} = load i32, ptr {phase_ptr}"));
        let in_handler = self.tmp();
        self.line(format!("{in_handler} = icmp ne i32 {phase}, 0"));
        let hraise_l = self.fresh_block("try.hreraise");
        let dispatch_l = self.fresh_block("try.hdispatch");
        self.line(format!(
            "br i1 {in_handler}, label %{hraise_l}, label %{dispatch_l}"
        ));

        // Second longjmp: raise/trap while running a handler → finally then reraise
        self.start_block(&hraise_l);
        self.emit_try_exit(TRY_EXIT_RERAISE, None);

        // First longjmp: body exception → match handlers (do not pop yet)
        self.start_block(&dispatch_l);
        self.line(format!("store i32 1, ptr {phase_ptr}"));
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
                Some(exc) => {
                    let cmp = self.tmp();
                    self.line(format!("{cmp} = icmp eq i32 {ety}, {}", exc.tag()));
                    self.line(format!("br i1 {cmp}, label %{match_l}, label %{nomatch_l}"));
                }
            }
            self.start_block(&match_l);
            if let Some(name) = bind {
                let msg = self.tmp();
                self.line(format!("{msg} = call ptr @pyrs_exc_message()"));
                self.line(format!("store ptr {msg}, ptr %v.{name}"));
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
        self.start_block(&fin_l);
        self.emit_block(finally);
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
                let instr = match (op, ty) {
                    (BinOp::Add, Ty::Int) => "add",
                    (BinOp::Sub, Ty::Int) => "sub",
                    (BinOp::Mul, Ty::Int) => "mul",
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
                    self.guard_zero(&r, Ty::Int, "ZeroDivisionError: division by zero");
                    let safe_r = self.guard_int_min(&l, &r);
                    let (q, r0) = self.emit_divmod(&l, &safe_r);
                    let adj = self.emit_floor_adjust(&r0, &safe_r);
                    let adj64 = self.tmp();
                    self.line(format!("{adj64} = zext i1 {adj} to i64"));
                    let t = self.tmp();
                    self.line(format!("{t} = sub i64 {q}, {adj64}"));
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
                    self.guard_zero(&r, Ty::Int, "ZeroDivisionError: division by zero");
                    let safe_r = self.guard_int_min(&l, &r);
                    let r0 = self.tmp();
                    self.line(format!("{r0} = srem i64 {l}, {safe_r}"));
                    let adj = self.emit_floor_adjust(&r0, &safe_r);
                    let sel = self.tmp();
                    self.line(format!("{sel} = select i1 {adj}, i64 {safe_r}, i64 0"));
                    let t = self.tmp();
                    self.line(format!("{t} = add i64 {r0}, {sel}"));
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
                        let cc = match op {
                            BinOp::Eq => "eq",
                            BinOp::Ne => "ne",
                            BinOp::Lt => "slt",
                            BinOp::Le => "sle",
                            BinOp::Gt => "sgt",
                            BinOp::Ge => "sge",
                            _ => unreachable!(),
                        };
                        self.line(format!("{t} = icmp {cc} i64 {l}, {r}"));
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
                let t = self.tmp();
                self.line(format!(
                    "{t} = call ptr @pyrs_list_repeat(ptr {l}, i64 {r})"
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
                let t = self.tmp();
                self.line(format!("{t} = call ptr @pyrs_str_repeat(ptr {l}, i64 {r})"));
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
        let lhs_block = self.cur_block.clone();
        let (skip_value, on_true, on_false) = match op {
            // a and b: if a is false, skip rhs and produce false
            BinOp::And => ("false", rhs_l.clone(), end_l.clone()),
            // a or b: if a is true, skip rhs and produce true
            BinOp::Or => ("true", end_l.clone(), rhs_l.clone()),
            _ => unreachable!(),
        };
        self.line(format!("br i1 {l}, label %{on_true}, label %{on_false}"));

        self.start_block(&rhs_l);
        let r = self.emit_expr(right);
        let rhs_block = self.cur_block.clone();
        self.line(format!("br label %{end_l}"));

        self.start_block(&end_l);
        let t = self.tmp();
        self.line(format!(
            "{t} = phi i1 [ {skip_value}, %{lhs_block} ], [ {r}, %{rhs_block} ]"
        ));
        t
    }

    /// Trap with a ZeroDivisionError when the divisor is zero.
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

    /// `i64::MIN / -1` overflows (UB in sdiv/srem). Substitute divisor 1 in
    /// exactly that case: MIN/1 = MIN matches two's-complement wrapping and
    /// the remainder is 0, branch-free.
    fn guard_int_min(&mut self, dividend: &str, divisor: &str) -> String {
        let is_min = self.tmp();
        self.line(format!(
            "{is_min} = icmp eq i64 {dividend}, -9223372036854775808"
        ));
        let is_neg1 = self.tmp();
        self.line(format!("{is_neg1} = icmp eq i64 {divisor}, -1"));
        let overflow = self.tmp();
        self.line(format!("{overflow} = and i1 {is_min}, {is_neg1}"));
        let safe = self.tmp();
        self.line(format!(
            "{safe} = select i1 {overflow}, i64 1, i64 {divisor}"
        ));
        safe
    }

    fn emit_divmod(&mut self, l: &str, r: &str) -> (String, String) {
        let q = self.tmp();
        self.line(format!("{q} = sdiv i64 {l}, {r}"));
        let rem = self.tmp();
        self.line(format!("{rem} = srem i64 {l}, {r}"));
        (q, rem)
    }

    /// True when the truncated result must be adjusted for floored
    /// semantics: remainder nonzero and its sign differs from the divisor's.
    fn emit_floor_adjust(&mut self, rem: &str, divisor: &str) -> String {
        let rnz = self.tmp();
        self.line(format!("{rnz} = icmp ne i64 {rem}, 0"));
        let x = self.tmp();
        self.line(format!("{x} = xor i64 {rem}, {divisor}"));
        let sgn = self.tmp();
        self.line(format!("{sgn} = icmp slt i64 {x}, 0"));
        let adj = self.tmp();
        self.line(format!("{adj} = and i1 {rnz}, {sgn}"));
        adj
    }
}
