use clap::Parser;
use std::{
    fs, io,
    io::{Read, Write},
    path,
};

mod cli;

use lexer;

fn main() {
    let cli = cli::Cli::parse();

    match cli.command {
        cli::Command::Lex(args) => {
            let input_path = args.io.input;
            let output_path = args.io.output;
            let source = read_file(&input_path)
                .unwrap_or_else(|_| panic!("Failed to read file: {:?}", input_path));
            let lexer = lexer::Lexer::new(&source);
            write_file(lexer.collect(), &output_path)
                .unwrap_or_else(|_| panic!("Failed to write to file: {:?}", output_path));
        }
        cli::Command::Parse(_parse_command) => todo!(),
        cli::Command::Compile(_compile_command) => todo!(),
    }
}

fn read_file(path: &path::PathBuf) -> Result<String, io::Error> {
    let mut file =
        fs::File::open(&path).unwrap_or_else(|_| panic!("Failed to open file: {:?}", path));
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    Ok(content)
}

fn write_file(content: Vec<lexer::Token>, path: &path::PathBuf) -> Result<(), io::Error> {
    let mut file =
        fs::File::create(&path).unwrap_or_else(|_| panic!("Failed to create file: {:?}", path));
    file.write_all(
        content
            .into_iter()
            .map(
                |token|
                match token {
                    lexer::Token::Newline => format!("{:?}\n", token),
                    lexer::Token::EOF => format!("{:?}\n", token),
                    _ => format!("{:?} ", token),
                }
            )
            .collect::<String>()
            .as_bytes(),
    )?;
    Ok(())
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
