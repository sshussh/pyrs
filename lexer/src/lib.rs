use std::collections::VecDeque;

use logos::Logos;

pub fn ping() -> String {
    String::from("pong")
}

#[derive(Logos, Debug, PartialEq, Eq)]
#[logos(skip r"[ \t\f]+")] // skip basic horizontal whitespace
#[logos(skip(r"#[^\n]*", allow_greedy = true))] // skip comments
pub enum Token {
    // keywords
    #[token("and")]
    And,
    #[token("as")]
    As,
    #[token("assert")]
    Assert,
    #[token("async")]
    Async,
    #[token("await")]
    Await,
    #[token("break")]
    Break,
    #[token("case")]
    Case,
    #[token("class")]
    Class,
    #[token("continue")]
    Continue,
    #[token("def")]
    Def,
    #[token("del")]
    Del,
    #[token("elif")]
    Elif,
    #[token("else")]
    Else,
    #[token("except")]
    Except,
    #[token("False")]
    False,
    #[token("finally")]
    Finally,
    #[token("for")]
    For,
    #[token("from")]
    From,
    #[token("global")]
    Global,
    #[token("if")]
    If,
    #[token("import")]
    Import,
    #[token("in")]
    In,
    #[token("is")]
    Is,
    #[token("lambda")]
    Lambda,
    #[token("match")]
    Match,
    #[token("None")]
    None,
    #[token("nonlocal")]
    Nonlocal,
    #[token("not")]
    Not,
    #[token("or")]
    Or,
    #[token("pass")]
    Pass,
    #[token("raise")]
    Raise,
    #[token("return")]
    Return,
    #[token("True")]
    True,
    #[token("try")]
    Try,
    #[token("while")]
    While,
    #[token("with")]
    With,
    #[token("yield")]
    Yeild,

    // types
    #[token("int")]
    Int,
    #[token("float")]
    Float,

    // Literals
    #[regex("[a-zA-Z_][a-zA-Z0-9_]*", |lex| lex.slice().to_string())]
    Ident(String),
    #[regex(r"[0-9]+", |lex| lex.slice().parse::<i64>().ok())]
    Intlit(i64),

    // symbols
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token(":")]
    Colon,
    #[token("->")]
    Arrow,

    // indentation
    #[regex(r"\r?\n")]
    Newline,
    Indent,
    Dedent,

    #[token("EOF")]
    EOF,
}

pub struct Lexer<'a> {
    inner: logos::Lexer<'a, Token>,
    indent_stack: Vec<usize>,
    pending: VecDeque<Token>,
    emit_eof: bool,
}

impl<'a> Lexer<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            inner: Token::lexer(source),
            indent_stack: vec![0],
            pending: VecDeque::new(),
            emit_eof: false,
        }
    }

    fn calc_indent(&self, input: &str) -> (usize, usize) {
        let mut width = 0;
        let mut consumed = 0;
        for char in input.chars() {
            match char {
                ' ' => {
                    width += 1;
                    consumed += char.len_utf8();
                }
                '\t' => {
                    width += 8;
                    consumed += char.len_utf8();
                }
                _ => break,
            }
        }
        (width, consumed)
    }
}

impl<'a> Iterator for Lexer<'a> {
    type Item = Token;

    fn next(&mut self) -> Option<Self::Item> {
        // drain any queued Indents/Dendents
        if let Some(token) = self.pending.pop_front() {
            return Some(token);
        }

        // fetch the next raw token from Logos
        let result = self.inner.next();

        match result {
            Some(Ok(Token::Newline)) => {
                let remainder = self.inner.remainder();
                let (visual_width, consumed) = self.calc_indent(remainder);

                self.inner.bump(consumed);

                let current_indent = *self.indent_stack.last().unwrap();

                if visual_width > current_indent {
                    // moving deeper, push to stack and emit Indent
                    self.indent_stack.push(visual_width);
                    self.pending.push_back(Token::Indent);
                    Some(Token::Newline)
                } else if visual_width < current_indent {
                    // moving back out, pop until we hit the matching level
                    while let Some(&top) = self.indent_stack.last() {
                        if visual_width < top {
                            self.indent_stack.pop();
                            self.pending.push_back(Token::Dedent);
                        } else if visual_width > top {
                            // ERROR: the user dedented to a level that doesn't exist
                            panic!("indentation error: dedent past current level");
                        } else {
                            break;
                        }
                    }
                    // return the first Dedent, others stay in "pending"
                    // self.pending.pop_front().or(Some(Token::Newline))
                    self.pending.push_front(Token::Newline);
                    self.pending.pop_front()
                } else {
                    Some(Token::Newline)
                }
            }
            Some(Ok(token)) => Some(token),
            Some(Err(_)) => {
                panic!(
                    "Lexer error: unexpected token {:?}",
                    self.inner.slice(),
                )
            }
            None => {
                // handle EOF
                if !self.emit_eof {
                    self.emit_eof = true;
                    // close remaining blocks
                    while self.indent_stack.len() > 1 {
                        self.indent_stack.pop();
                        self.pending.push_back(Token::Dedent);
                    }
                    self.pending.push_back(Token::EOF);
                    self.pending.pop_front()
                } else {
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_lexer() {
        let code = "def main():\n    return 5";
        let lexer = Lexer::new(code);

        assert_eq!(
            lexer.collect::<Vec<_>>(),
            vec![
                Token::Def,
                Token::Ident("main".to_string()),
                Token::LParen,
                Token::RParen,
                Token::Colon,
                Token::Newline,
                Token::Indent,
                Token::Return,
                Token::Intlit(5),
                Token::Dedent,
                Token::EOF
            ]
        )
    }

    #[test]
    fn test_keywords() {
        let code = "and as assert async await break case class continue def del elif else except False finally for from global if import in is lambda match None nonlocal not or raise return True try while with";
        let lexer = Lexer::new(code);
        assert_eq!(
            lexer.collect::<Vec<_>>(),
            vec![
                Token::And,
                Token::As,
                Token::Assert,
                Token::Async,
                Token::Await,
                Token::Break,
                Token::Case,
                Token::Class,
                Token::Continue,
                Token::Def,
                Token::Del,
                Token::Elif,
                Token::Else,
                Token::Except,
                Token::False,
                Token::Finally,
                Token::For,
                Token::From,
                Token::Global,
                Token::If,
                Token::Import,
                Token::In,
                Token::Is,
                Token::Lambda,
                Token::Match,
                Token::None,
                Token::Nonlocal,
                Token::Not,
                Token::Or,
                Token::Raise,
                Token::Return,
                Token::True,
                Token::Try,
                Token::While,
                Token::With,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_types() {
        let code = "int float";
        let lexer = Lexer::new(code);
        assert_eq!(
            lexer.collect::<Vec<_>>(),
            vec![Token::Int, Token::Float, Token::EOF]
        );
    }

    #[test]
    fn test_ident() {
        let code = "foo bar _baz _abc123";
        let lexer = Lexer::new(code);
        assert_eq!(
            lexer.collect::<Vec<_>>(),
            vec![
                Token::Ident("foo".to_string()),
                Token::Ident("bar".to_string()),
                Token::Ident("_baz".to_string()),
                Token::Ident("_abc123".to_string()),
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_int_literal() {
        let code = "0 42 1000";
        let lexer = Lexer::new(code);
        assert_eq!(
            lexer.collect::<Vec<_>>(),
            vec![
                Token::Intlit(0),
                Token::Intlit(42),
                Token::Intlit(1000),
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_symbols() {
        let code = "()()(:)";
        let lexer = Lexer::new(code);
        assert_eq!(
            lexer.collect::<Vec<_>>(),
            vec![
                Token::LParen,
                Token::RParen,
                Token::LParen,
                Token::RParen,
                Token::LParen,
                Token::Colon,
                Token::RParen,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_comment_skipped() {
        let code = "def main(): # this is a comment\n    return 5";
        let lexer = Lexer::new(code);
        assert_eq!(
            lexer.collect::<Vec<_>>(),
            vec![
                Token::Def,
                Token::Ident("main".to_string()),
                Token::LParen,
                Token::RParen,
                Token::Colon,
                Token::Newline,
                Token::Indent,
                Token::Return,
                Token::Intlit(5),
                Token::Dedent,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_newline_no_indent_change() {
        let code = "foo\nbar";
        let lexer = Lexer::new(code);
        assert_eq!(
            lexer.collect::<Vec<_>>(),
            vec![
                Token::Ident("foo".to_string()),
                Token::Newline,
                Token::Ident("bar".to_string()),
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_single_indent_dedent() {
        let code = "if True:\n    pass\n";
        let lexer = Lexer::new(code);
        assert_eq!(
            lexer.collect::<Vec<_>>(),
            vec![
                Token::If,
                Token::True,
                Token::Colon,
                Token::Newline,
                Token::Indent,
                Token::Pass,
                Token::Newline,
                Token::Dedent,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_nested_indent_dedent() {
        let code = "def foo():\n    if True:\n        return 1\n";
        let lexer = Lexer::new(code);
        assert_eq!(
            lexer.collect::<Vec<_>>(),
            vec![
                Token::Def,
                Token::Ident("foo".to_string()),
                Token::LParen,
                Token::RParen,
                Token::Colon,
                Token::Newline,
                Token::Indent,
                Token::If,
                Token::True,
                Token::Colon,
                Token::Newline,
                Token::Indent,
                Token::Return,
                Token::Intlit(1),
                Token::Newline,
                Token::Dedent,
                Token::Dedent,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_multiple_dedents() {
        let code = "def foo():\n    if True:\n        return 1\n    return 2\n";
        let lexer = Lexer::new(code);
        assert_eq!(
            lexer.collect::<Vec<_>>(),
            vec![
                Token::Def,
                Token::Ident("foo".to_string()),
                Token::LParen,
                Token::RParen,
                Token::Colon,
                Token::Newline,
                Token::Indent,
                Token::If,
                Token::True,
                Token::Colon,
                Token::Newline,
                Token::Indent,
                Token::Return,
                Token::Intlit(1),
                Token::Newline,
                Token::Dedent,
                Token::Return,
                Token::Intlit(2),
                Token::Newline,
                Token::Dedent,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_eof_closes_open_blocks() {
        let code = "def foo():\n    return 1";
        let lexer = Lexer::new(code);
        assert_eq!(
            lexer.collect::<Vec<_>>(),
            vec![
                Token::Def,
                Token::Ident("foo".to_string()),
                Token::LParen,
                Token::RParen,
                Token::Colon,
                Token::Newline,
                Token::Indent,
                Token::Return,
                Token::Intlit(1),
                Token::Dedent,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_tab_indent() {
        let code = "def foo():\n\treturn 1";
        let lexer = Lexer::new(code);
        assert_eq!(
            lexer.collect::<Vec<_>>(),
            vec![
                Token::Def,
                Token::Ident("foo".to_string()),
                Token::LParen,
                Token::RParen,
                Token::Colon,
                Token::Newline,
                Token::Indent,
                Token::Return,
                Token::Intlit(1),
                Token::Dedent,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_crlf_newline() {
        let code = "foo\r\nbar";
        let lexer = Lexer::new(code);
        assert_eq!(
            lexer.collect::<Vec<_>>(),
            vec![
                Token::Ident("foo".to_string()),
                Token::Newline,
                Token::Ident("bar".to_string()),
                Token::EOF,
            ]
        );
    }

    #[test]
    #[should_panic(expected = "indentation error: dedent past current level")]
    fn test_invalid_dedent_panics() {
        let code = "def foo():\n    if True:\n        return 1\n  return 2\n";
        let lexer = Lexer::new(code);
        let _ = lexer.collect::<Vec<_>>();
    }
}
