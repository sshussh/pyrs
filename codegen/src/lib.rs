//! LLVM bridge.
//!
//! The Rust side lowers the typed IR to LLVM IR text ([`emit_llvm_ir`]); the
//! C++ shim (built via CMake, see `shim/`) parses that text with LLVM's
//! IRReader, verifies it, runs the standard optimization pipeline, and
//! writes a native object file ([`compile_ir_to_object`]).
//!
//! Compiled programs also link the C runtime ([`RUNTIME_C`]) and its
//! nonmoving collector ([`GC_C`], [`GC_H`]); the driver compiles them with
//! the system C compiler at link time.

pub mod emit;
pub mod intfast;
pub mod strfast;

use std::ffi::{CStr, CString, c_char};
use std::path::Path;

pub use emit::{emit_library_ir, emit_llvm_ir};

/// Source of the C runtime linked into every compiled program.
pub const RUNTIME_C: &str = include_str!("../runtime/runtime.c");

/// Source of the collector linked into every compiled program.
pub const GC_C: &str = include_str!("../runtime/gc.c");

/// Shared collector/runtime declarations written beside the embedded C files.
pub const GC_H: &str = include_str!("../runtime/gc.h");

/// Unicode 16.0.0 property and case tables, generated from the CPython oracle
/// by `scripts/gen_unicode_tables.py`.
pub const UNICODE_DATA_C: &str = include_str!("../runtime/unicode_data.c");

/// Declarations and lookup inlines for [`UNICODE_DATA_C`].
pub const UNICODE_DATA_H: &str = include_str!("../runtime/unicode_data.h");

/// CPython version the Unicode tables and the differential oracle were
/// generated against, read from the stamp in the generated header.
///
/// Tooling needs this to keep a project's interpreter aligned with the one
/// PyRs was built for: uv picks its own default otherwise, and a mismatch
/// only shows up when something Unicode- or compatibility-shaped disagrees.
pub fn oracle_python_version() -> &'static str {
    const KEY: &str = "#define PYRS_UNIDATA_CPYTHON \"";
    match UNICODE_DATA_H.find(KEY) {
        Some(at) => {
            let rest = &UNICODE_DATA_H[at + KEY.len()..];
            match rest.find('"') {
                Some(end) => &rest[..end],
                None => "3",
            }
        }
        None => "3",
    }
}

/// Major.minor of [`oracle_python_version`].
pub fn oracle_python_minor() -> &'static str {
    let v = oracle_python_version();
    match v.match_indices('.').nth(1) {
        Some((at, _)) => &v[..at],
        None => v,
    }
}

pub fn ping() -> String {
    String::from("pong")
}

#[link(name = "codegen_shim", kind = "static")]
unsafe extern "C" {
    fn pyrs_compile_ir(
        ir_data: *const u8,
        ir_len: usize,
        out_path: *const c_char,
        opt_level: i32,
        cpu: *const c_char,
        err_buf: *mut c_char,
        err_buf_len: usize,
    ) -> i32;

    fn pyrs_target_identity(cpu: *const c_char, out: *mut c_char, out_len: usize) -> i32;
}

/// The portable baseline: whatever the target triple guarantees, with no
/// optional ISA extensions. What `pyrs compile` produces unless asked
/// otherwise, so an artifact runs wherever its architecture does.
pub const CPU_GENERIC: &str = "generic";

/// This host's own CPU model and features. Faster — the baseline has no AVX2,
/// BMI2 or FMA — but the binary may fault on an older machine, so it is the
/// default only for `pyrs run` and `pyrs test`, which build for this machine
/// and throw the binary away.
pub const CPU_NATIVE: &str = "native";

/// The `<model>|<features>` a CPU request resolves to on this host.
///
/// Belongs in a compile cache key: two hosts both asking for `native` resolve
/// it differently, and a cache shared between them would otherwise hand one
/// machine the other's illegal instructions.
pub fn target_identity(cpu: &str) -> String {
    let Ok(c_cpu) = CString::new(cpu) else {
        return cpu.to_string();
    };
    let mut buf = vec![0u8; 4096];
    let rc =
        unsafe { pyrs_target_identity(c_cpu.as_ptr(), buf.as_mut_ptr() as *mut c_char, buf.len()) };
    if rc != 0 {
        return cpu.to_string();
    }
    unsafe { CStr::from_ptr(buf.as_ptr() as *const c_char) }
        .to_string_lossy()
        .into_owned()
}

/// Compile LLVM IR text into a native object file at `out_path`.
///
/// `opt_level` is clamped to 0..=3 and reaches both the IR pass pipeline and
/// the backend. `cpu` is [`CPU_GENERIC`], [`CPU_NATIVE`], or a model name.
pub fn compile_ir_to_object(
    ir_text: &str,
    out_path: &Path,
    opt_level: u8,
    cpu: &str,
) -> Result<(), String> {
    let path_str = out_path
        .to_str()
        .ok_or_else(|| "output path is not valid UTF-8".to_string())?;
    let c_path =
        CString::new(path_str).map_err(|_| "output path contains a NUL byte".to_string())?;
    let c_cpu = CString::new(cpu).map_err(|_| "target CPU contains a NUL byte".to_string())?;

    let mut err_buf = vec![0u8; 4096];
    let rc = unsafe {
        pyrs_compile_ir(
            ir_text.as_ptr(),
            ir_text.len(),
            c_path.as_ptr(),
            opt_level.min(3) as i32,
            c_cpu.as_ptr(),
            err_buf.as_mut_ptr() as *mut c_char,
            err_buf.len(),
        )
    };

    if rc == 0 {
        Ok(())
    } else {
        let msg = unsafe { CStr::from_ptr(err_buf.as_ptr() as *const c_char) }
            .to_string_lossy()
            .into_owned();
        Err(if msg.is_empty() {
            format!("LLVM backend failed with code {rc}")
        } else {
            msg
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lower(src: &str) -> String {
        let module = parser::parse(src).expect("parse failed");
        let ir_module = semantic::analyze(&module).expect("semantic failed");
        emit_llvm_ir(&ir_module)
    }

    #[test]
    fn library_has_no_c_entry_or_implicit_main_call() {
        let ast = parser::parse("def main(value: int) -> int:\n    return value + 1\n").unwrap();
        // Executable convenience entry points cannot have parameters. Library
        // exports named main must behave like any other exported function.
        assert!(semantic::analyze(&ast).is_err());
        let module = semantic::analyze_library(&ast).unwrap();
        let ll = emit_library_ir(&module);
        assert!(ll.contains("define i64 @pyrs_main(i64 %p.value)"), "{ll}");
        assert!(!ll.contains("define i32 @main("), "{ll}");
        assert!(!ll.contains("call i64 @pyrs_main("), "{ll}");
    }

    #[test]
    fn emits_function_and_main() {
        let ll = lower("def add(a: int, b: int) -> int:\n    return a + b\n\nprint(add(1, 2))\n");
        assert!(
            ll.contains("define i64 @pyrs_add(i64 %p.a, i64 %p.b)"),
            "{ll}"
        );
        assert!(
            ll.contains("define i32 @main(i32 %argc, ptr %argv)"),
            "{ll}"
        );
        assert!(ll.contains("call void @pyrs___main__()"), "{ll}");
        assert!(ll.contains("call void @pyrs_print_int"), "{ll}");
    }

    #[test]
    fn initializes_gc_and_registers_managed_globals() {
        let ll = lower(
            "text: str = \"root\"\n\
             number: int = 42\n\
             dynamic: Any = text\n\
             choice: int | str = text\n\
             flag: bool = True\n",
        );

        let init = ll
            .find("call void @pyrs_gc_init(ptr %gc.stack.anchor)")
            .expect("missing GC initialization");
        let roots = ll
            .find("call void @pyrs_gc_add_root_range(ptr @g.text, i64 8)")
            .expect("missing pointer-global root");
        let set_args = ll
            .find("call void @pyrs_set_args(i32 %argc, ptr %argv)")
            .expect("missing argv initialization");
        assert!(init < roots && roots < set_args, "{ll}");
        assert!(
            ll.contains("call void @pyrs_gc_add_root_range(ptr @g.number, i64 8)"),
            "{ll}"
        );
        assert!(
            ll.contains("call void @pyrs_gc_add_root_range(ptr @g.dynamic, i64 8)"),
            "{ll}"
        );
        assert!(
            ll.contains("call void @pyrs_gc_add_root_range(ptr @g.choice, i64 16)"),
            "{ll}"
        );
        assert!(
            !ll.contains("pyrs_gc_add_root_range(ptr @g.flag"),
            "scalar-only global was registered: {ll}"
        );
    }

    #[test]
    fn emits_managed_box_constructors_without_raw_malloc() {
        let ll = lower(
            "class Greeter:\n    def message(self) -> str:\n        return \"hi\"\n\ngreeter = Greeter()\ncallback = greeter.message\ndynamic: Any = callback\n",
        );

        assert!(
            ll.contains(" = call ptr @pyrs_bound_method_new(ptr "),
            "{ll}"
        );
        assert!(ll.contains(" = call ptr @pyrs_union_box_new(i32 "), "{ll}");
        assert!(!ll.contains("declare ptr @malloc"), "{ll}");
        assert!(!ll.contains("call ptr @malloc"), "{ll}");
    }

    #[test]
    fn try_functions_reserve_the_native_frame_pointer_for_gc_roots() {
        let ll = lower(
            "def guarded() -> str:\n    value = \"kept\"\n    try:\n        raise ValueError(\"boom\")\n    except ValueError:\n        return value\n\ndef guarded_gen():\n    try:\n        yield 1\n    finally:\n        pass\n\ndef plain() -> str:\n    return \"plain\"\n\nprint(guarded())\nprint(next(guarded_gen()))\n",
        );

        assert!(
            ll.contains("attributes #0 = { \"frame-pointer\"=\"all\" }"),
            "{ll}"
        );
        assert!(
            ll.contains("define ptr @pyrs_guarded() #0 {"),
            "try function did not reserve the frame pointer: {ll}"
        );
        assert!(
            ll.contains("define ptr @pyrs_plain() {"),
            "plain function unnecessarily reserved the frame pointer: {ll}"
        );
        assert!(
            ll.contains("define i32 @pyrs_guarded_gen(ptr %gen) #0 {"),
            "generator try function did not reserve the frame pointer: {ll}"
        );
    }

    #[test]
    fn synthesized_expression_tries_reserve_the_native_frame_pointer() {
        let ll = lower(
            "class ListIter:\n    def __next__(self) -> list[int]:\n        raise StopIteration(\"\")\n\ndef hidden_gen():\n    fallback: list[int] = []\n    next(ListIter(), fallback).append(7)\n    yield 1\n\nfallback: list[int] = []\nnext(ListIter(), fallback).append(7)\nprint(next(hidden_gen()))\n",
        );

        assert!(
            ll.contains("define void @pyrs___main__() #0 {"),
            "top-level synthesized try did not reserve the frame pointer: {ll}"
        );
        assert!(
            ll.contains("define i32 @pyrs_hidden_gen(ptr %gen) #0 {"),
            "generator synthesized try did not reserve the frame pointer: {ll}"
        );
        assert!(
            ll.matches("call i32 @_setjmp(").count() >= 2,
            "expected synthesized setjmp calls: {ll}"
        );
    }

    #[test]
    fn emits_float_hex_constants() {
        let ll = lower("x = 1.5\nprint(x)\n");
        // 1.5 as raw IEEE-754 bits
        assert!(ll.contains("0x3FF8000000000000"), "{ll}");
    }

    #[test]
    fn division_guards_against_zero() {
        let ll = lower("def f(a: int, b: int) -> int:\n    return a // b\nprint(f(7, 2))\n");
        // Floor division is handled in the bigint runtime (zero-div trap included).
        assert!(ll.contains("pyrs_int_floordiv"), "{ll}");
    }

    #[test]
    fn short_circuit_uses_phi() {
        let ll =
            lower("def f(a: bool, b: bool) -> bool:\n    return a and b\nprint(f(True, False))\n");
        assert!(ll.contains("phi i1"), "{ll}");
    }

    #[test]
    fn strings_are_interned_globals() {
        let ll = lower("print(\"hello\", \"hello\", \"world\")\n");
        assert_eq!(ll.matches("c\"hello\\00\"").count(), 1, "{ll}");
        assert!(ll.contains("c\"world\\00\""), "{ll}");
    }

    #[test]
    fn compiles_object_file_through_llvm() {
        let ll = lower("def sq(x: int) -> int:\n    return x * x\n\nprint(sq(12))\n");
        let dir = std::env::temp_dir();
        let obj = dir.join(format!("pyrs-test-{}.o", std::process::id()));
        let result = compile_ir_to_object(&ll, &obj, 2, CPU_GENERIC);
        assert!(result.is_ok(), "shim failed: {:?}", result.err());
        let meta = std::fs::metadata(&obj).expect("object file missing");
        assert!(meta.len() > 0, "object file is empty");
        let _ = std::fs::remove_file(&obj);
    }

    #[test]
    fn invalid_ir_reports_error() {
        let err = compile_ir_to_object(
            "this is not llvm ir",
            &std::env::temp_dir().join("pyrs-test-invalid.o"),
            0,
            CPU_GENERIC,
        )
        .expect_err("expected parse failure");
        assert!(!err.is_empty());
    }

    /// The identity goes into a compile cache key, so `generic` must be
    /// stable, and `native` must differ from it on any host with optional ISA
    /// extensions — otherwise a cache shared between two machines could serve
    /// one of them the other's instructions.
    #[test]
    fn target_identity_separates_generic_from_native() {
        let generic = target_identity(CPU_GENERIC);
        assert_eq!(generic, target_identity(CPU_GENERIC), "generic is stable");
        assert!(generic.starts_with("generic|"), "got {generic}");

        let native = target_identity(CPU_NATIVE);
        assert_eq!(native, target_identity(CPU_NATIVE), "native is stable");
        assert!(
            native.contains('|'),
            "identity is <model>|<features>, got {native}"
        );
        assert_ne!(
            native, generic,
            "a host with no distinguishing model or features would make the \
             cache key unable to tell the two apart"
        );
    }

    /// An empty request is the portable baseline rather than an error, so a
    /// caller that has nothing to say does not accidentally get `native`.
    #[test]
    fn an_empty_cpu_request_is_the_baseline() {
        assert_eq!(target_identity(""), target_identity(CPU_GENERIC));
    }

    /// A named model is passed through, so `--target-cpu x86-64-v3` reaches
    /// LLVM rather than being silently reinterpreted.
    #[test]
    fn a_named_model_is_passed_through() {
        let named = target_identity("x86-64-v3");
        assert!(named.starts_with("x86-64-v3|"), "got {named}");
    }
}
