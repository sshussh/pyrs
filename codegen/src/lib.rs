pub fn ping() -> String {
    String::from("pong")
}

#[link(name = "codegen_shim", kind = "static")]
unsafe extern "C" {
    fn run_llvm_test(data: *const u8, len: usize) -> i32;
}

pub fn compile_to_llvm(data: &[u8]) -> i32 {
    unsafe { run_llvm_test(data.as_ptr(), data.len()) }
}
