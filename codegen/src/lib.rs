//! LLVM bridge.
//!
//! The Rust side lowers the typed IR to LLVM IR text ([`emit_llvm_ir`]); the
//! C++ shim (built via CMake, see `shim/`) parses that text with LLVM's
//! IRReader, verifies it, runs the standard optimization pipeline, and
//! writes a native object file ([`compile_ir_to_object`]).
//!
//! Compiled programs also link a small C runtime ([`RUNTIME_C`]) providing
//! Python-faithful printing and runtime error traps; the driver compiles it
//! with the system C compiler at link time.

pub mod emit;

use std::ffi::{CStr, CString, c_char};
use std::path::Path;

pub use emit::emit_llvm_ir;

/// Source of the C runtime linked into every compiled program.
pub const RUNTIME_C: &str = include_str!("../runtime/runtime.c");

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
        err_buf: *mut c_char,
        err_buf_len: usize,
    ) -> i32;
}

/// Compile LLVM IR text into a native object file at `out_path`.
///
/// `opt_level` is clamped to 0..=3 (default pipeline levels O0–O3).
pub fn compile_ir_to_object(ir_text: &str, out_path: &Path, opt_level: u8) -> Result<(), String> {
    let path_str = out_path
        .to_str()
        .ok_or_else(|| "output path is not valid UTF-8".to_string())?;
    let c_path =
        CString::new(path_str).map_err(|_| "output path contains a NUL byte".to_string())?;

    let mut err_buf = vec![0u8; 4096];
    let rc = unsafe {
        pyrs_compile_ir(
            ir_text.as_ptr(),
            ir_text.len(),
            c_path.as_ptr(),
            opt_level.min(3) as i32,
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
        let result = compile_ir_to_object(&ll, &obj, 2);
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
        )
        .expect_err("expected parse failure");
        assert!(!err.is_empty());
    }
}
