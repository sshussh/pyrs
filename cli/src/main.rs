fn main() {
    println!("Hello, World!")
}

#[allow(unused)]
fn ping() -> String {
    String::from("pong")
}

#[cfg(test)]
mod tests {
    #[test]
    fn initialization_lexer_test() {
        assert_eq!(lexer::ping(), String::from("pong"));
    }

    #[test]
    fn initialization_parser_test() {
        assert_eq!(parser::ping(), String::from("pong"));
    }

    #[test]
    fn initialization_semantic_test() {
        assert_eq!(semantic::ping(), String::from("pong"));
    }

    #[test]
    fn initialization_ir_test() {
        assert_eq!(ir::ping(), String::from("pong"));
    }

    #[test]
    fn initialization_codegen_test() {
        assert_eq!(codegen::ping(), String::from("pong"));
    }

    #[test]
    fn initialization_common_test() {
        assert_eq!(crate::ping(), String::from("pong"));
    }

    #[test]
    fn initialization_codegen_llvm_test() {
        assert_eq!(codegen::compile_to_llvm("ffs".as_bytes()), 0);
    }
}
