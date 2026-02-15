use logos::Logos;
use std::ops::Range;

#[derive(Logos, Debug, PartialEq)]
#[logos(skip r"[ \t]+")]
pub enum Token<'a> {
    #[token("def")]
    Definition,

    #[regex("[a-zA-Z]+")]
    Identifier(&'a str),

    #[token("(")]
    LeftParentheses,

    #[token(")")]
    RightParentheses,

    #[token("->")]
    Arrow,

    #[token(":")]
    Colon,

    #[regex(r"\n([ \t]*\n)*[ \t]*")]
    Newline,

    Indentation,
    Deindentation,

    #[token("return")]
    Return,

    #[regex("[0-9]+", |lex| lex.slice().parse().ok())]
    Integer(i64),
}

pub fn lex(source: &'_ str) -> Vec<(Result<Token<'_>, ()>, Range<usize>)> {
    let lexer = Token::lexer(source);
    let mut indent_stack: Vec<usize> = vec![0];
    let mut result: Vec<(Result<Token, ()>, Range<usize>)> = Vec::new();

    for (token, span) in lexer.spanned() {
        match token {
            Ok(Token::Newline) => {
                let slice = &source[span.clone()];
                let indentation = slice.rsplit('\n').next().unwrap_or("").len();

                result.push((Ok(Token::Newline), span.clone()));

                let current = *indent_stack.last().unwrap();
                if indentation > current {
                    indent_stack.push(indentation);
                    result.push((Ok(Token::Indentation), span.clone()));
                } else if indentation < current {
                    while *indent_stack.last().unwrap() > indentation {
                        indent_stack.pop();
                        result.push((Ok(Token::Deindentation), span.clone()));
                    }
                    if *indent_stack.last().unwrap() != indentation {
                        result.push((Err(()), span.clone()));
                    }
                }
            }
            other => result.push((other, span)),
        }
    }

    let eof = source.len()..source.len();
    while indent_stack.len() > 1 {
        indent_stack.pop();
        result.push((Ok(Token::Deindentation), eof.clone()));
    }
    result
}
