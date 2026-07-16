//! Scanner: wraps `logos` with a state machine for Python's semantic
//! whitespace. Emits `(Token, Span)` pairs; `Indent`/`Dedent` tokens are
//! synthesized from a visual-width indent stack, blank/comment-only lines
//! are insignificant, and newlines inside parentheses are suppressed
//! (implicit line joining).

use std::collections::VecDeque;

use common::{Diagnostic, Phase, Span};
use logos::Logos;

pub fn ping() -> String {
    String::from("pong")
}

/// Process escape sequences in the interior of a string literal (no
/// surrounding quotes). Same rules for single-line and triple-quoted forms.
///
/// Physical newlines are normalized like CPython's tokenizer: `\r\n` and
/// lone `\r` become `\n`. A backslash immediately before a physical newline
/// is a line continuation and contributes no characters.
fn unescape_contents(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\r' {
            // universal-newline: \r\n and \r → \n
            if chars.as_str().starts_with('\n') {
                chars.next();
            }
            out.push('\n');
            continue;
        }
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('\\') => out.push('\\'),
            Some('\'') => out.push('\''),
            Some('"') => out.push('"'),
            // line continuation inside the literal: consume the newline
            Some('\n') => {}
            Some('\r') => {
                if chars.as_str().starts_with('\n') {
                    chars.next();
                }
            }
            // unknown escape: keep it verbatim, like CPython
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Strip one surrounding quote character from each end, then unescape.
fn unescape(slice: &str) -> String {
    unescape_contents(&slice[1..slice.len() - 1])
}

/// After matching the opening `"""`, scan for the closing delimiter,
/// apply escapes, and extend the logos span via `bump`. Returns `None`
/// (unterminated) after consuming the rest of the input so the error
/// span covers the whole open string.
fn lex_triple_double(lex: &mut logos::Lexer<Token>) -> Option<String> {
    lex_triple(lex, b'"')
}

/// After matching the opening `'''`, scan for the closing delimiter.
fn lex_triple_single(lex: &mut logos::Lexer<Token>) -> Option<String> {
    lex_triple(lex, b'\'')
}

fn lex_triple(lex: &mut logos::Lexer<Token>, quote: u8) -> Option<String> {
    let rem = lex.remainder();
    let bytes = rem.as_bytes();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == b'\\' {
            // skip the backslash and the escaped unit; for line
            // continuation also skip a full \r\n pair so the closer is
            // not mis-scanned. Content decoding still runs in unescape.
            i += 1;
            if i < bytes.len() {
                if bytes[i] == b'\r' {
                    i += 1;
                    if i < bytes.len() && bytes[i] == b'\n' {
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if bytes[i] == quote && bytes[i + 1] == quote && bytes[i + 2] == quote {
            let inner = &rem[..i];
            lex.bump(i + 3);
            return Some(unescape_contents(inner));
        }
        i += 1;
    }
    // unterminated: consume remainder so diagnostics cover the open text
    lex.bump(rem.len());
    None
}

/// After matching the opening `f"""` / `f'''`, scan for the matching
/// closing triple quotes (same rules as plain triples), unescape the
/// interior, and return content only (no leading `f`, no surrounding
/// quotes) for the parser's `parse_fstring`.
fn lex_triple_fstring(lex: &mut logos::Lexer<Token>) -> Option<String> {
    // logos has already matched the opener; slice is `f"""` or `f'''`.
    let quote = match lex.slice().as_bytes().last() {
        Some(b'"') => b'"',
        Some(b'\'') => b'\'',
        _ => return None,
    };
    lex_triple(lex, quote)
}

#[derive(Logos, Debug, Clone, PartialEq)]
#[logos(skip r"[ \t\f]+")] // skip basic horizontal whitespace
#[logos(skip(r"#[^\n]*", allow_greedy = true))] // skip comments
#[logos(skip r"\\\r?\n")] // explicit line joining with backslash
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
    Yield,

    // builtin type names (reserved so annotations and casts are unambiguous)
    #[token("int")]
    Int,
    #[token("float")]
    Float,
    #[token("bool")]
    Bool,
    #[token("str")]
    Str,
    #[token("file")]
    File,
    #[token("list")]
    List,
    #[token("tuple")]
    Tuple,
    #[token("dict")]
    Dict,
    #[token("set")]
    Set,

    // Literals
    #[regex("[a-zA-Z_][a-zA-Z0-9_]*", |lex| lex.slice().to_string())]
    Ident(String),
    /// Decimal digits only (underscores stripped). May exceed i64.
    #[regex(r"[0-9][0-9_]*", |lex| Some(lex.slice().replace('_', "")))]
    Intlit(String),
    #[regex(r"([0-9][0-9_]*\.[0-9]*|\.[0-9]+)([eE][+-]?[0-9]+)?", |lex| lex.slice().replace('_', "").parse::<f64>().ok())]
    #[regex(r"[0-9][0-9_]*[eE][+-]?[0-9]+", |lex| lex.slice().replace('_', "").parse::<f64>().ok())]
    Floatlit(f64),
    // Triple-quoted forms first: the opening delimiter is longer than a
    // single quote, so logos prefers them over empty `""` / `''`. The
    // callback extends the span through the closing `"""` / `'''` and
    // preserves interior newlines as part of the string value.
    #[token("\"\"\"", lex_triple_double)]
    #[token("'''", lex_triple_single)]
    #[regex(r#""([^"\\\n]|\\.)*""#, |lex| unescape(lex.slice()))]
    #[regex(r#"'([^'\\\n]|\\.)*'"#, |lex| unescape(lex.slice()))]
    Strlit(String),
    /// f-string raw content, escapes processed but `{`/`}` preserved for
    /// the parser to split into literal and expression parts.
    /// Triple-quoted openers first so `f"""` / `f'''` are not misparsed
    /// as empty `f""`/`f''` plus junk.
    #[token("f\"\"\"", lex_triple_fstring)]
    #[token("f'''", lex_triple_fstring)]
    #[regex(r#"f"([^"\\\n]|\\.)*""#, |lex| unescape(&lex.slice()[1..]))]
    #[regex(r#"f'([^'\\\n]|\\.)*'"#, |lex| unescape(&lex.slice()[1..]))]
    FStrlit(String),

    // operators
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("**")]
    DoubleStar,
    #[token("/")]
    Slash,
    #[token("//")]
    DoubleSlash,
    #[token("%")]
    Percent,
    #[token("=")]
    Eq,
    #[token("==")]
    EqEq,
    #[token("!=")]
    NotEq,
    #[token("<")]
    Lt,
    #[token("<=")]
    LtEq,
    #[token(">")]
    Gt,
    #[token(">=")]
    GtEq,
    #[token("+=")]
    PlusEq,
    #[token("-=")]
    MinusEq,
    #[token("*=")]
    StarEq,
    #[token("/=")]
    SlashEq,
    #[token("//=")]
    DoubleSlashEq,
    #[token("%=")]
    PercentEq,
    #[token("**=")]
    DoubleStarEq,
    #[token("&=")]
    AmpEq,
    #[token("|=")]
    PipeEq,
    #[token("^=")]
    CaretEq,
    #[token("<<=")]
    LShiftEq,
    #[token(">>=")]
    RShiftEq,

    // bitwise operators (longer forms before shorter where needed)
    #[token("<<")]
    LShift,
    #[token(">>")]
    RShift,
    #[token("&")]
    Amp,
    #[token("^")]
    Caret,
    #[token("~")]
    Tilde,

    // symbols
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("[")]
    LBracket,
    #[token("]")]
    RBracket,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token(":=")]
    ColonEqual,
    #[token(":")]
    Colon,
    #[token(",")]
    Comma,
    #[token(".")]
    Dot,
    #[token("@")]
    At,
    #[token("->")]
    Arrow,
    /// `|` — type unions (`int | None`) and bitwise-or expressions.
    #[token("|")]
    Pipe,

    // indentation
    #[regex(r"\r?\n")]
    Newline,
    Indent,
    Dedent,

    EOF,
}

impl Token {
    /// Human-readable name for error messages.
    pub fn describe(&self) -> String {
        match self {
            Token::Ident(name) => format!("identifier '{name}'"),
            Token::Intlit(v) => format!("integer literal {v}"),
            // v is String digits
            Token::Floatlit(v) => format!("float literal {v}"),
            Token::Strlit(_) => "string literal".to_string(),
            Token::FStrlit(_) => "f-string literal".to_string(),
            Token::Newline => "end of line".to_string(),
            Token::Indent => "indent".to_string(),
            Token::Dedent => "dedent".to_string(),
            Token::EOF => "end of file".to_string(),
            other => format!("'{}'", token_text(other)),
        }
    }
}

fn token_text(token: &Token) -> &'static str {
    match token {
        Token::And => "and",
        Token::As => "as",
        Token::Assert => "assert",
        Token::Async => "async",
        Token::Await => "await",
        Token::Break => "break",
        Token::Case => "case",
        Token::Class => "class",
        Token::Continue => "continue",
        Token::Def => "def",
        Token::Del => "del",
        Token::Elif => "elif",
        Token::Else => "else",
        Token::Except => "except",
        Token::False => "False",
        Token::Finally => "finally",
        Token::For => "for",
        Token::From => "from",
        Token::Global => "global",
        Token::If => "if",
        Token::Import => "import",
        Token::In => "in",
        Token::Is => "is",
        Token::Lambda => "lambda",
        Token::Match => "match",
        Token::None => "None",
        Token::Nonlocal => "nonlocal",
        Token::Not => "not",
        Token::Or => "or",
        Token::Pass => "pass",
        Token::Raise => "raise",
        Token::Return => "return",
        Token::True => "True",
        Token::Try => "try",
        Token::While => "while",
        Token::With => "with",
        Token::Yield => "yield",
        Token::Int => "int",
        Token::Float => "float",
        Token::Bool => "bool",
        Token::Str => "str",
        Token::File => "file",
        Token::List => "list",
        Token::Tuple => "tuple",
        Token::Dict => "dict",
        Token::Set => "set",
        Token::Plus => "+",
        Token::Minus => "-",
        Token::Star => "*",
        Token::DoubleStar => "**",
        Token::Slash => "/",
        Token::DoubleSlash => "//",
        Token::Percent => "%",
        Token::Eq => "=",
        Token::EqEq => "==",
        Token::NotEq => "!=",
        Token::Lt => "<",
        Token::LtEq => "<=",
        Token::Gt => ">",
        Token::GtEq => ">=",
        Token::PlusEq => "+=",
        Token::MinusEq => "-=",
        Token::StarEq => "*=",
        Token::SlashEq => "/=",
        Token::DoubleSlashEq => "//=",
        Token::PercentEq => "%=",
        Token::DoubleStarEq => "**=",
        Token::AmpEq => "&=",
        Token::PipeEq => "|=",
        Token::CaretEq => "^=",
        Token::LShiftEq => "<<=",
        Token::RShiftEq => ">>=",
        Token::LShift => "<<",
        Token::RShift => ">>",
        Token::Amp => "&",
        Token::Caret => "^",
        Token::Tilde => "~",
        Token::LParen => "(",
        Token::RParen => ")",
        Token::LBracket => "[",
        Token::RBracket => "]",
        Token::LBrace => "{",
        Token::RBrace => "}",
        Token::Colon => ":",
        Token::ColonEqual => ":=",
        Token::Comma => ",",
        Token::Dot => ".",
        Token::At => "@",
        Token::Arrow => "->",
        Token::Pipe => "|",
        _ => "?",
    }
}

pub struct Lexer<'a> {
    inner: logos::Lexer<'a, Token>,
    indent_stack: Vec<usize>,
    pending: VecDeque<(Token, Span)>,
    paren_depth: usize,
    emitted_eof: bool,
    last_was_newline: bool,
}

impl<'a> Lexer<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            inner: Token::lexer(source),
            indent_stack: vec![0],
            pending: VecDeque::new(),
            paren_depth: 0,
            emitted_eof: false,
            // suppress newlines at the very start of the file
            last_was_newline: true,
        }
    }
}

/// Visual width (spaces = 1, tabs = 8) and bytes consumed of the leading
/// indentation of `input`.
fn calc_indent(input: &str) -> (usize, usize) {
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

impl<'a> Iterator for Lexer<'a> {
    type Item = Result<(Token, Span), Diagnostic>;

    fn next(&mut self) -> Option<Self::Item> {
        // drain any queued Indents/Dedents first
        if let Some((token, span)) = self.pending.pop_front() {
            self.last_was_newline = token == Token::Newline;
            return Some(Ok((token, span)));
        }

        loop {
            let result = self.inner.next();
            let span = Span::from(self.inner.span());

            match result {
                Some(Ok(Token::Newline)) => {
                    // implicit line joining: newlines inside brackets are
                    // insignificant
                    if self.paren_depth > 0 {
                        continue;
                    }

                    // measure the indentation of the next line
                    let (visual_width, consumed) = calc_indent(self.inner.remainder());
                    self.inner.bump(consumed);
                    let line_start = self.inner.span().end;
                    let mark = Span::new(line_start, line_start);

                    match self.inner.remainder().chars().next() {
                        // blank or comment-only line: no indent handling, no
                        // newline; the newline ending the *last* such line
                        // does the work
                        Some('\n') | Some('\r') | Some('#') => continue,
                        // end of input: emit a final newline if meaningful;
                        // EOF handling closes open blocks
                        Option::None => {
                            if self.last_was_newline {
                                continue;
                            }
                            self.last_was_newline = true;
                            return Some(Ok((Token::Newline, span)));
                        }
                        Some(_) => {}
                    }

                    let current_indent = *self.indent_stack.last().unwrap();

                    if visual_width > current_indent {
                        // moving deeper: push to stack and queue an Indent
                        self.indent_stack.push(visual_width);
                        self.pending.push_back((Token::Indent, mark));
                    } else if visual_width < current_indent {
                        // moving out: pop until we hit the matching level
                        while let Some(&top) = self.indent_stack.last() {
                            if visual_width < top {
                                self.indent_stack.pop();
                                self.pending.push_back((Token::Dedent, mark));
                            } else if visual_width > top {
                                return Some(Err(Diagnostic::new(
                                    Phase::Lex,
                                    "unindent does not match any outer indentation level",
                                    mark,
                                )));
                            } else {
                                break;
                            }
                        }
                    }

                    // a newline right after another newline (e.g. leading
                    // blank lines) is redundant; still drain any queued
                    // indent tokens
                    if self.last_was_newline {
                        match self.pending.pop_front() {
                            Some((token, span)) => {
                                self.last_was_newline = token == Token::Newline;
                                return Some(Ok((token, span)));
                            }
                            Option::None => continue,
                        }
                    }
                    self.last_was_newline = true;
                    return Some(Ok((Token::Newline, span)));
                }
                Some(Ok(token)) => {
                    match token {
                        Token::LParen | Token::LBracket | Token::LBrace => {
                            self.paren_depth += 1;
                        }
                        Token::RParen | Token::RBracket | Token::RBrace => {
                            self.paren_depth = self.paren_depth.saturating_sub(1);
                        }
                        _ => {}
                    }
                    self.last_was_newline = false;
                    return Some(Ok((token, span)));
                }
                Some(Err(())) => {
                    let slice = self.inner.slice();
                    // Callbacks return None for unclosed triple-quoted
                    // strings / f-strings (span bumped through remainder).
                    let message = if slice.starts_with("f\"\"\"") || slice.starts_with("f'''") {
                        "unterminated triple-quoted f-string literal".to_string()
                    } else if slice.starts_with("\"\"\"") || slice.starts_with("'''") {
                        "unterminated triple-quoted string literal".to_string()
                    } else {
                        format!("unexpected character {:?}", slice)
                    };
                    return Some(Err(Diagnostic::new(Phase::Lex, message, span)));
                }
                Option::None => {
                    if self.emitted_eof {
                        return Option::None;
                    }
                    self.emitted_eof = true;
                    let end = Span::new(self.inner.source().len(), self.inner.source().len());
                    // close any blocks still open at end of input
                    while self.indent_stack.len() > 1 {
                        self.indent_stack.pop();
                        self.pending.push_back((Token::Dedent, end));
                    }
                    self.pending.push_back((Token::EOF, end));
                    let (token, span) = self.pending.pop_front().unwrap();
                    return Some(Ok((token, span)));
                }
            }
        }
    }
}

/// Lex the entire source, stopping at the first error.
pub fn lex(source: &str) -> Result<Vec<(Token, Span)>, Diagnostic> {
    Lexer::new(source).collect()
}

#[cfg(test)]
mod test {
    use super::*;

    /// Token kinds only, spans stripped — keeps assertions readable.
    fn kinds(code: &str) -> Vec<Token> {
        lex(code)
            .expect("lexing failed")
            .into_iter()
            .map(|(t, _)| t)
            .collect()
    }

    #[test]
    fn test_lexer() {
        assert_eq!(
            kinds("def main():\n    return 5"),
            vec![
                Token::Def,
                Token::Ident("main".to_string()),
                Token::LParen,
                Token::RParen,
                Token::Colon,
                Token::Newline,
                Token::Indent,
                Token::Return,
                Token::Intlit("5".into()),
                Token::Dedent,
                Token::EOF
            ]
        )
    }

    #[test]
    fn test_keywords() {
        let code = "and as assert async await break case class continue def del elif else except False finally for from global if import in is lambda match None nonlocal not or raise return True try while with yield";
        assert_eq!(
            kinds(code),
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
                Token::Yield,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_types() {
        assert_eq!(
            kinds("int float bool str file list"),
            vec![
                Token::Int,
                Token::Float,
                Token::Bool,
                Token::Str,
                Token::File,
                Token::List,
                Token::EOF
            ]
        );
    }

    #[test]
    fn test_ident() {
        assert_eq!(
            kinds("foo bar _baz _abc123"),
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
        assert_eq!(
            kinds("0 42 1000 1_000_000"),
            vec![
                Token::Intlit("0".into()),
                Token::Intlit("42".into()),
                Token::Intlit("1000".into()),
                Token::Intlit("1000000".into()),
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_float_literal() {
        assert_eq!(
            kinds("1.5 0.25 2. .5 1e3 2.5e-2"),
            vec![
                Token::Floatlit(1.5),
                Token::Floatlit(0.25),
                Token::Floatlit(2.0),
                Token::Floatlit(0.5),
                Token::Floatlit(1000.0),
                Token::Floatlit(0.025),
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_string_literal() {
        assert_eq!(
            kinds(r#""hello" 'world' "a\nb" "q\"q""#),
            vec![
                Token::Strlit("hello".to_string()),
                Token::Strlit("world".to_string()),
                Token::Strlit("a\nb".to_string()),
                Token::Strlit("q\"q".to_string()),
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_triple_double_quoted_string() {
        assert_eq!(
            kinds(r#""""hello""""#),
            vec![Token::Strlit("hello".to_string()), Token::EOF]
        );
        // empty triple
        assert_eq!(
            kinds("\"\"\"\"\"\""),
            vec![Token::Strlit(String::new()), Token::EOF]
        );
        // multi-line content; interior newlines stay in the value
        assert_eq!(
            kinds("\"\"\"a\nb\"\"\""),
            vec![Token::Strlit("a\nb".to_string()), Token::EOF]
        );
        // single and double quotes inside are fine
        assert_eq!(
            kinds(r#""""he said "hi" and 'bye'""""#),
            vec![
                Token::Strlit("he said \"hi\" and 'bye'".to_string()),
                Token::EOF
            ]
        );
        // escapes match single-line strings
        assert_eq!(
            kinds(r#""""a\nb\t\"c""""#),
            vec![Token::Strlit("a\nb\t\"c".to_string()), Token::EOF]
        );
    }

    #[test]
    fn test_triple_single_quoted_string() {
        assert_eq!(
            kinds("'''hello'''"),
            vec![Token::Strlit("hello".to_string()), Token::EOF]
        );
        assert_eq!(
            kinds("'''a\nb'''"),
            vec![Token::Strlit("a\nb".to_string()), Token::EOF]
        );
        assert_eq!(
            kinds(r#"'''it\'s fine'''"#),
            vec![Token::Strlit("it's fine".to_string()), Token::EOF]
        );
    }

    #[test]
    fn test_triple_string_does_not_disturb_indent() {
        // Newlines inside a triple string are content, not NEWLINE tokens;
        // the next statement still sees the surrounding indent level.
        assert_eq!(
            kinds("def f():\n    s = \"\"\"a\nb\"\"\"\n    return s\n"),
            vec![
                Token::Def,
                Token::Ident("f".to_string()),
                Token::LParen,
                Token::RParen,
                Token::Colon,
                Token::Newline,
                Token::Indent,
                Token::Ident("s".to_string()),
                Token::Eq,
                Token::Strlit("a\nb".to_string()),
                Token::Newline,
                Token::Return,
                Token::Ident("s".to_string()),
                Token::Newline,
                Token::Dedent,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_unterminated_triple_string_is_error() {
        let err = lex("\"\"\"unclosed").unwrap_err();
        assert!(
            err.message.contains("unterminated triple-quoted"),
            "{}",
            err.message
        );
        // span covers the open text through EOF
        assert_eq!(err.span.start, 0);
        assert_eq!(err.span.end, "\"\"\"unclosed".len());

        let multi = "'''also unclosed\nmore";
        let err = lex(multi).unwrap_err();
        assert!(
            err.message.contains("unterminated triple-quoted"),
            "{}",
            err.message
        );
        assert_eq!(err.span.start, 0);
        assert_eq!(err.span.end, multi.len());
    }

    #[test]
    fn test_triple_crlf_and_cr_normalized_to_lf() {
        // CPython tokenizer: physical \r\n and lone \r inside triples → \n
        assert_eq!(
            kinds("\"\"\"a\r\nb\"\"\""),
            vec![Token::Strlit("a\nb".to_string()), Token::EOF]
        );
        assert_eq!(
            kinds("\"\"\"a\rb\"\"\""),
            vec![Token::Strlit("a\nb".to_string()), Token::EOF]
        );
        assert_eq!(
            kinds("'''a\r\nb'''"),
            vec![Token::Strlit("a\nb".to_string()), Token::EOF]
        );
    }

    #[test]
    fn test_backslash_newline_line_continuation_in_triple() {
        // """a\<newline>b""" → "ab"
        assert_eq!(
            kinds("\"\"\"a\\\nb\"\"\""),
            vec![Token::Strlit("ab".to_string()), Token::EOF]
        );
        assert_eq!(
            kinds("\"\"\"a\\\r\nb\"\"\""),
            vec![Token::Strlit("ab".to_string()), Token::EOF]
        );
        assert_eq!(
            kinds("'''a\\\nb'''"),
            vec![Token::Strlit("ab".to_string()), Token::EOF]
        );
    }

    #[test]
    fn test_triple_fstring_content() {
        assert_eq!(
            kinds(r#"f"""hello""""#),
            vec![Token::FStrlit("hello".to_string()), Token::EOF]
        );
        assert_eq!(
            kinds("f'''hello'''"),
            vec![Token::FStrlit("hello".to_string()), Token::EOF]
        );
        // empty triple f-string
        assert_eq!(
            kinds("f\"\"\"\"\"\""),
            vec![Token::FStrlit(String::new()), Token::EOF]
        );
        // multi-line; interior newlines stay; braces preserved for parser
        assert_eq!(
            kinds("f\"\"\"a\nb {x}\"\"\""),
            vec![Token::FStrlit("a\nb {x}".to_string()), Token::EOF]
        );
        assert_eq!(
            kinds("f'''a\nb {x}'''"),
            vec![Token::FStrlit("a\nb {x}".to_string()), Token::EOF]
        );
        // single/double quotes inside are fine; escapes match plain triples
        assert_eq!(
            kinds(r#"f"""he said "hi" {x}""""#),
            vec![Token::FStrlit("he said \"hi\" {x}".to_string()), Token::EOF]
        );
        assert_eq!(
            kinds(r#"f"""a\nb\t\"c""""#),
            vec![Token::FStrlit("a\nb\t\"c".to_string()), Token::EOF]
        );
        // {{ / }} brace escapes left for the parser
        assert_eq!(
            kinds(r#"f"""{{x}} is {x}""""#),
            vec![Token::FStrlit("{{x}} is {x}".to_string()), Token::EOF]
        );
    }

    #[test]
    fn test_triple_fstring_crlf_and_line_continuation() {
        assert_eq!(
            kinds("f\"\"\"a\r\nb {x}\"\"\""),
            vec![Token::FStrlit("a\nb {x}".to_string()), Token::EOF]
        );
        assert_eq!(
            kinds("f\"\"\"a\rb {x}\"\"\""),
            vec![Token::FStrlit("a\nb {x}".to_string()), Token::EOF]
        );
        assert_eq!(
            kinds("f\"\"\"a\\\nb{x}\"\"\""),
            vec![Token::FStrlit("ab{x}".to_string()), Token::EOF]
        );
        assert_eq!(
            kinds("f\"\"\"a\\\r\nb{x}\"\"\""),
            vec![Token::FStrlit("ab{x}".to_string()), Token::EOF]
        );
        assert_eq!(
            kinds("f'''a\\\nb{x}'''"),
            vec![Token::FStrlit("ab{x}".to_string()), Token::EOF]
        );
    }

    #[test]
    fn test_triple_fstring_does_not_disturb_indent() {
        assert_eq!(
            kinds("def f():\n    s = f\"\"\"a\nb {x}\"\"\"\n    return s\n"),
            vec![
                Token::Def,
                Token::Ident("f".to_string()),
                Token::LParen,
                Token::RParen,
                Token::Colon,
                Token::Newline,
                Token::Indent,
                Token::Ident("s".to_string()),
                Token::Eq,
                Token::FStrlit("a\nb {x}".to_string()),
                Token::Newline,
                Token::Return,
                Token::Ident("s".to_string()),
                Token::Newline,
                Token::Dedent,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_unterminated_triple_fstring_is_error() {
        let err = lex("f\"\"\"unclosed").unwrap_err();
        assert!(
            err.message.contains("unterminated triple-quoted f-string"),
            "{}",
            err.message
        );
        assert_eq!(err.span.start, 0);
        assert_eq!(err.span.end, "f\"\"\"unclosed".len());

        let multi = "f'''also unclosed\nmore";
        let err = lex(multi).unwrap_err();
        assert!(
            err.message.contains("unterminated triple-quoted f-string"),
            "{}",
            err.message
        );
        assert_eq!(err.span.start, 0);
        assert_eq!(err.span.end, multi.len());
    }

    #[test]
    fn test_module_docstring_tokens() {
        assert_eq!(
            kinds("\"\"\"module doc\"\"\"\nprint\n"),
            vec![
                Token::Strlit("module doc".to_string()),
                Token::Newline,
                Token::Ident("print".to_string()),
                Token::Newline,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_operators() {
        assert_eq!(
            kinds("+ - * ** / // % = == != < <= > >= += -= *= /= //= %= **="),
            vec![
                Token::Plus,
                Token::Minus,
                Token::Star,
                Token::DoubleStar,
                Token::Slash,
                Token::DoubleSlash,
                Token::Percent,
                Token::Eq,
                Token::EqEq,
                Token::NotEq,
                Token::Lt,
                Token::LtEq,
                Token::Gt,
                Token::GtEq,
                Token::PlusEq,
                Token::MinusEq,
                Token::StarEq,
                Token::SlashEq,
                Token::DoubleSlashEq,
                Token::PercentEq,
                Token::DoubleStarEq,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_symbols() {
        assert_eq!(
            kinds("()()(:)"),
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
        assert_eq!(
            kinds("def main(): # this is a comment\n    return 5"),
            vec![
                Token::Def,
                Token::Ident("main".to_string()),
                Token::LParen,
                Token::RParen,
                Token::Colon,
                Token::Newline,
                Token::Indent,
                Token::Return,
                Token::Intlit("5".into()),
                Token::Dedent,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_newline_no_indent_change() {
        assert_eq!(
            kinds("foo\nbar"),
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
        assert_eq!(
            kinds("if True:\n    pass\n"),
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
        assert_eq!(
            kinds("def foo():\n    if True:\n        return 1\n"),
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
                Token::Intlit("1".into()),
                Token::Newline,
                Token::Dedent,
                Token::Dedent,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_multiple_dedents() {
        assert_eq!(
            kinds("def foo():\n    if True:\n        return 1\n    return 2\n"),
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
                Token::Intlit("1".into()),
                Token::Newline,
                Token::Dedent,
                Token::Return,
                Token::Intlit("2".into()),
                Token::Newline,
                Token::Dedent,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_eof_closes_open_blocks() {
        assert_eq!(
            kinds("def foo():\n    return 1"),
            vec![
                Token::Def,
                Token::Ident("foo".to_string()),
                Token::LParen,
                Token::RParen,
                Token::Colon,
                Token::Newline,
                Token::Indent,
                Token::Return,
                Token::Intlit("1".into()),
                Token::Dedent,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_tab_indent() {
        assert_eq!(
            kinds("def foo():\n\treturn 1"),
            vec![
                Token::Def,
                Token::Ident("foo".to_string()),
                Token::LParen,
                Token::RParen,
                Token::Colon,
                Token::Newline,
                Token::Indent,
                Token::Return,
                Token::Intlit("1".into()),
                Token::Dedent,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_crlf_newline() {
        assert_eq!(
            kinds("foo\r\nbar"),
            vec![
                Token::Ident("foo".to_string()),
                Token::Newline,
                Token::Ident("bar".to_string()),
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_blank_lines_inside_block() {
        // blank lines must not produce indent/dedent churn
        assert_eq!(
            kinds("if True:\n    x = 1\n\n    y = 2\n"),
            vec![
                Token::If,
                Token::True,
                Token::Colon,
                Token::Newline,
                Token::Indent,
                Token::Ident("x".to_string()),
                Token::Eq,
                Token::Intlit("1".into()),
                Token::Newline,
                Token::Ident("y".to_string()),
                Token::Eq,
                Token::Intlit("2".into()),
                Token::Newline,
                Token::Dedent,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_comment_only_line() {
        assert_eq!(
            kinds("x = 1\n# a comment\ny = 2\n"),
            vec![
                Token::Ident("x".to_string()),
                Token::Eq,
                Token::Intlit("1".into()),
                Token::Newline,
                Token::Ident("y".to_string()),
                Token::Eq,
                Token::Intlit("2".into()),
                Token::Newline,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_leading_blank_lines() {
        assert_eq!(
            kinds("\n\nx = 1\n"),
            vec![
                Token::Ident("x".to_string()),
                Token::Eq,
                Token::Intlit("1".into()),
                Token::Newline,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_newline_in_parens_ignored() {
        assert_eq!(
            kinds("foo(1,\n    2)\n"),
            vec![
                Token::Ident("foo".to_string()),
                Token::LParen,
                Token::Intlit("1".into()),
                Token::Comma,
                Token::Intlit("2".into()),
                Token::RParen,
                Token::Newline,
                Token::EOF,
            ]
        );
    }

    #[test]
    fn test_invalid_dedent_is_error() {
        let code = "def foo():\n    if True:\n        return 1\n  return 2\n";
        let result = lex(code);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("unindent"));
    }

    #[test]
    fn test_unexpected_char_is_error() {
        let result = lex("x = 1 ?\n");
        assert!(result.is_err());
    }

    #[test]
    fn test_spans_point_into_source() {
        let code = "abc = 42";
        let tokens = lex(code).unwrap();
        let (token, span) = &tokens[0];
        assert_eq!(*token, Token::Ident("abc".to_string()));
        assert_eq!(&code[span.start..span.end], "abc");
        let (token, span) = &tokens[2];
        assert_eq!(*token, Token::Intlit("42".into()));
        assert_eq!(&code[span.start..span.end], "42");
    }
}
