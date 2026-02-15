use crate::lexer::lex;
use std::{env, fs, path};

mod lexer;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Invalid usage");
        eprintln!("Usage:");
        eprintln!("    pyrsc <input.py>");
        return;
    }

    let content = fs::read_to_string(path::Path::new(&args[1])).unwrap();
    let tokens = lex(&content);

    for (token, span) in tokens {
        println!("{:?}: {:?}", token.unwrap(), span)
    }
}
