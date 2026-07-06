//! Syntax analysis: consumes the token stream and builds the AST.
//!
//! Hand-written recursive descent with layered precedence for expressions.
//! Stops at the first syntax error and reports it as a [`Diagnostic`].

pub mod ast;

use ast::*;
use common::{Diagnostic, Phase, Span};
use lexer::Token;

pub fn ping() -> String {
    String::from("pong")
}

type PResult<T> = Result<T, Diagnostic>;

/// Parse a full module from source text (lexes internally).
pub fn parse(source: &str) -> PResult<Module> {
    let tokens = lexer::lex(source)?;
    Parser::new(tokens).parse_module()
}

pub struct Parser {
    tokens: Vec<(Token, Span)>,
    pos: usize,
}

impl Parser {
    pub fn new(tokens: Vec<(Token, Span)>) -> Self {
        Self { tokens, pos: 0 }
    }

    // ---- cursor helpers ----

    fn peek(&self) -> &Token {
        self.tokens
            .get(self.pos)
            .map(|(t, _)| t)
            .unwrap_or(&Token::EOF)
    }

    fn peek2(&self) -> &Token {
        self.tokens
            .get(self.pos + 1)
            .map(|(t, _)| t)
            .unwrap_or(&Token::EOF)
    }

    fn peek_span(&self) -> Span {
        self.tokens
            .get(self.pos)
            .map(|(_, s)| *s)
            .unwrap_or_else(|| {
                self.tokens
                    .last()
                    .map(|(_, s)| *s)
                    .unwrap_or(Span::new(0, 0))
            })
    }

    fn advance(&mut self) -> (Token, Span) {
        let item = self
            .tokens
            .get(self.pos)
            .cloned()
            .unwrap_or((Token::EOF, self.peek_span()));
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        item
    }

    fn eat(&mut self, token: &Token) -> bool {
        if self.peek() == token {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: Token, context: &str) -> PResult<Span> {
        if self.peek() == &token {
            let span = self.peek_span();
            self.pos += 1;
            Ok(span)
        } else {
            Err(self.error(format!(
                "expected {} {}, found {}",
                token.describe(),
                context,
                self.peek().describe()
            )))
        }
    }

    fn error(&self, message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(Phase::Parse, message, self.peek_span())
    }

    fn skip_newlines(&mut self) {
        while self.peek() == &Token::Newline {
            self.pos += 1;
        }
    }

    /// A simple statement ends at a newline; dedent/EOF also close it.
    fn expect_stmt_end(&mut self) -> PResult<()> {
        match self.peek() {
            Token::Newline => {
                self.pos += 1;
                Ok(())
            }
            Token::Dedent | Token::EOF => Ok(()),
            other => Err(self.error(format!(
                "expected end of line after statement, found {}",
                other.describe()
            ))),
        }
    }

    // ---- module & statements ----

    pub fn parse_module(&mut self) -> PResult<Module> {
        let mut body = Vec::new();
        loop {
            self.skip_newlines();
            if self.peek() == &Token::EOF {
                break;
            }
            body.push(self.parse_stmt()?);
        }
        Ok(Module { body })
    }

    fn parse_stmt(&mut self) -> PResult<Stmt> {
        match self.peek() {
            Token::Def => self.parse_funcdef(),
            Token::If => self.parse_if(),
            Token::While => self.parse_while(),
            Token::For => Err(self.error("'for' loops are not supported yet; use 'while'")),
            Token::Class => Err(self.error("classes are not supported yet")),
            Token::Import | Token::From => Err(self.error("imports are not supported yet")),
            Token::Try | Token::Raise => Err(self.error("exceptions are not supported yet")),
            Token::Match => Err(self.error("'match' statements are not supported yet")),
            Token::With => Err(self.error("'with' statements are not supported yet")),
            Token::Indent => Err(self.error("unexpected indent")),
            _ => {
                let stmt = self.parse_simple_stmt()?;
                self.expect_stmt_end()?;
                Ok(stmt)
            }
        }
    }

    fn parse_simple_stmt(&mut self) -> PResult<Stmt> {
        let start = self.peek_span();
        match self.peek() {
            Token::Return => {
                self.advance();
                let value = match self.peek() {
                    Token::Newline | Token::Dedent | Token::EOF => None,
                    _ => Some(self.parse_expr()?),
                };
                let end = value.as_ref().map(|e| e.span).unwrap_or(start);
                Ok(Stmt {
                    kind: StmtKind::Return(value),
                    span: start.to(end),
                })
            }
            Token::Pass => {
                self.advance();
                Ok(Stmt {
                    kind: StmtKind::Pass,
                    span: start,
                })
            }
            Token::Break => {
                self.advance();
                Ok(Stmt {
                    kind: StmtKind::Break,
                    span: start,
                })
            }
            Token::Continue => {
                self.advance();
                Ok(Stmt {
                    kind: StmtKind::Continue,
                    span: start,
                })
            }
            Token::Ident(_) => self.parse_assign_or_expr(),
            _ => {
                let expr = self.parse_expr()?;
                let span = expr.span;
                Ok(Stmt {
                    kind: StmtKind::ExprStmt(expr),
                    span,
                })
            }
        }
    }

    /// Statements starting with an identifier: assignment, augmented
    /// assignment, annotated assignment, or a plain expression statement.
    fn parse_assign_or_expr(&mut self) -> PResult<Stmt> {
        let aug_op = match self.peek2() {
            Token::PlusEq => Some(BinOp::Add),
            Token::MinusEq => Some(BinOp::Sub),
            Token::StarEq => Some(BinOp::Mul),
            Token::SlashEq => Some(BinOp::Div),
            Token::DoubleSlashEq => Some(BinOp::FloorDiv),
            Token::PercentEq => Some(BinOp::Mod),
            _ => None,
        };

        match (self.peek2(), aug_op) {
            (Token::Eq, _) => {
                let (name, name_span) = self.expect_ident("in assignment")?;
                self.advance(); // '='
                let value = self.parse_expr()?;
                let span = name_span.to(value.span);
                Ok(Stmt {
                    kind: StmtKind::Assign {
                        name,
                        name_span,
                        annotation: None,
                        value,
                    },
                    span,
                })
            }
            (Token::Colon, _) => {
                let (name, name_span) = self.expect_ident("in assignment")?;
                self.advance(); // ':'
                let annotation = self.parse_type_name("after ':' in annotated assignment")?;
                self.expect(Token::Eq, "after type annotation (declarations require a value)")?;
                let value = self.parse_expr()?;
                let span = name_span.to(value.span);
                Ok(Stmt {
                    kind: StmtKind::Assign {
                        name,
                        name_span,
                        annotation: Some(annotation),
                        value,
                    },
                    span,
                })
            }
            (_, Some(op)) => {
                let (name, name_span) = self.expect_ident("in assignment")?;
                self.advance(); // the augmented-assignment operator
                let value = self.parse_expr()?;
                let span = name_span.to(value.span);
                Ok(Stmt {
                    kind: StmtKind::AugAssign {
                        name,
                        name_span,
                        op,
                        value,
                    },
                    span,
                })
            }
            _ => {
                let expr = self.parse_expr()?;
                if self.peek() == &Token::Eq {
                    return Err(self.error("cannot assign to this expression"));
                }
                let span = expr.span;
                Ok(Stmt {
                    kind: StmtKind::ExprStmt(expr),
                    span,
                })
            }
        }
    }

    fn expect_ident(&mut self, context: &str) -> PResult<(String, Span)> {
        match self.advance() {
            (Token::Ident(name), span) => Ok((name, span)),
            (other, span) => Err(Diagnostic::new(
                Phase::Parse,
                format!("expected identifier {}, found {}", context, other.describe()),
                span,
            )),
        }
    }

    fn parse_type_name(&mut self, context: &str) -> PResult<TypeName> {
        match self.peek() {
            Token::Int => {
                self.advance();
                Ok(TypeName::Int)
            }
            Token::Float => {
                self.advance();
                Ok(TypeName::Float)
            }
            Token::Bool => {
                self.advance();
                Ok(TypeName::Bool)
            }
            Token::None => {
                self.advance();
                Ok(TypeName::None)
            }
            other => Err(self.error(format!(
                "expected a type ('int', 'float', 'bool' or 'None') {}, found {}",
                context,
                other.describe()
            ))),
        }
    }

    fn parse_funcdef(&mut self) -> PResult<Stmt> {
        let def_span = self.expect(Token::Def, "")?;
        let (name, _) = self.expect_ident("after 'def'")?;
        self.expect(Token::LParen, "after function name")?;

        let mut params = Vec::new();
        if self.peek() != &Token::RParen {
            loop {
                let (pname, pspan) = self.expect_ident("in parameter list")?;
                if !self.eat(&Token::Colon) {
                    return Err(Diagnostic::new(
                        Phase::Parse,
                        format!(
                            "parameter '{pname}' is missing a type annotation \
                             (e.g. '{pname}: int')"
                        ),
                        pspan,
                    ));
                }
                let ty = self.parse_type_name("in parameter annotation")?;
                params.push(Param {
                    name: pname,
                    ty,
                    span: pspan,
                });
                if !self.eat(&Token::Comma) {
                    break;
                }
                // allow trailing comma
                if self.peek() == &Token::RParen {
                    break;
                }
            }
        }
        let close = self.expect(Token::RParen, "after parameter list")?;

        let ret = if self.eat(&Token::Arrow) {
            Some(self.parse_type_name("after '->'")?)
        } else {
            None
        };

        let header_span = def_span.to(close);
        let body = self.parse_block("function body")?;
        Ok(Stmt {
            kind: StmtKind::FuncDef(FuncDef {
                name,
                params,
                ret,
                body,
                span: header_span,
            }),
            span: header_span,
        })
    }

    fn parse_if(&mut self) -> PResult<Stmt> {
        let start = self.expect(Token::If, "")?;
        let mut branches = Vec::new();
        let cond = self.parse_expr()?;
        let body = self.parse_block("'if' body")?;
        branches.push((cond, body));

        let mut orelse = Vec::new();
        loop {
            // `elif`/`else` sit at the same indentation as the `if`; the
            // Dedent closing the previous block was consumed by parse_block
            if self.peek() == &Token::Elif {
                self.advance();
                let cond = self.parse_expr()?;
                let body = self.parse_block("'elif' body")?;
                branches.push((cond, body));
            } else if self.peek() == &Token::Else {
                self.advance();
                orelse = self.parse_block("'else' body")?;
                break;
            } else {
                break;
            }
        }

        let end = self.peek_span();
        Ok(Stmt {
            kind: StmtKind::If { branches, orelse },
            span: start.to(end),
        })
    }

    fn parse_while(&mut self) -> PResult<Stmt> {
        let start = self.expect(Token::While, "")?;
        let cond = self.parse_expr()?;
        let body = self.parse_block("'while' body")?;
        let end = self.peek_span();
        Ok(Stmt {
            kind: StmtKind::While { cond, body },
            span: start.to(end),
        })
    }

    /// `: NEWLINE INDENT stmt+ DEDENT` or a single-line suite `: simple_stmt`.
    fn parse_block(&mut self, what: &str) -> PResult<Vec<Stmt>> {
        self.expect(Token::Colon, &format!("before {what}"))?;

        if self.eat(&Token::Newline) {
            self.expect(Token::Indent, &format!("(an indented block) for {what}"))?;
            let mut body = Vec::new();
            loop {
                self.skip_newlines();
                match self.peek() {
                    Token::Dedent => {
                        self.advance();
                        break;
                    }
                    Token::EOF => break,
                    _ => body.push(self.parse_stmt()?),
                }
            }
            if body.is_empty() {
                return Err(self.error(format!("empty {what}")));
            }
            Ok(body)
        } else {
            // single-line suite: `if x: return 1`
            let stmt = self.parse_simple_stmt()?;
            self.expect_stmt_end()?;
            Ok(vec![stmt])
        }
    }

    // ---- expressions (precedence climbing, lowest first) ----

    pub fn parse_expr(&mut self) -> PResult<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> PResult<Expr> {
        let mut left = self.parse_and()?;
        while self.peek() == &Token::Or {
            self.advance();
            let right = self.parse_and()?;
            let span = left.span.to(right.span);
            left = Expr {
                kind: ExprKind::Binary {
                    op: BinOp::Or,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                span,
            };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> PResult<Expr> {
        let mut left = self.parse_not()?;
        while self.peek() == &Token::And {
            self.advance();
            let right = self.parse_not()?;
            let span = left.span.to(right.span);
            left = Expr {
                kind: ExprKind::Binary {
                    op: BinOp::And,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                span,
            };
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> PResult<Expr> {
        if self.peek() == &Token::Not {
            let start = self.peek_span();
            self.advance();
            let operand = self.parse_not()?;
            let span = start.to(operand.span);
            return Ok(Expr {
                kind: ExprKind::Unary {
                    op: UnaryOp::Not,
                    operand: Box::new(operand),
                },
                span,
            });
        }
        self.parse_comparison()
    }

    fn comparison_op(&self) -> Option<BinOp> {
        match self.peek() {
            Token::EqEq => Some(BinOp::Eq),
            Token::NotEq => Some(BinOp::NotEq),
            Token::Lt => Some(BinOp::Lt),
            Token::LtEq => Some(BinOp::LtEq),
            Token::Gt => Some(BinOp::Gt),
            Token::GtEq => Some(BinOp::GtEq),
            _ => None,
        }
    }

    fn parse_comparison(&mut self) -> PResult<Expr> {
        let left = self.parse_arith()?;
        if let Some(op) = self.comparison_op() {
            self.advance();
            let right = self.parse_arith()?;
            if self.comparison_op().is_some() {
                return Err(self.error(
                    "comparison chaining (a < b < c) is not supported yet; \
                     use 'and' to combine comparisons",
                ));
            }
            let span = left.span.to(right.span);
            return Ok(Expr {
                kind: ExprKind::Binary {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                span,
            });
        }
        Ok(left)
    }

    fn parse_arith(&mut self) -> PResult<Expr> {
        let mut left = self.parse_term()?;
        loop {
            let op = match self.peek() {
                Token::Plus => BinOp::Add,
                Token::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let right = self.parse_term()?;
            let span = left.span.to(right.span);
            left = Expr {
                kind: ExprKind::Binary {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                span,
            };
        }
        Ok(left)
    }

    fn parse_term(&mut self) -> PResult<Expr> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Token::Star => BinOp::Mul,
                Token::Slash => BinOp::Div,
                Token::DoubleSlash => BinOp::FloorDiv,
                Token::Percent => BinOp::Mod,
                _ => break,
            };
            self.advance();
            let right = self.parse_unary()?;
            let span = left.span.to(right.span);
            left = Expr {
                kind: ExprKind::Binary {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                span,
            };
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        match self.peek() {
            Token::Minus => {
                let start = self.peek_span();
                self.advance();
                let operand = self.parse_unary()?;
                let span = start.to(operand.span);
                Ok(Expr {
                    kind: ExprKind::Unary {
                        op: UnaryOp::Neg,
                        operand: Box::new(operand),
                    },
                    span,
                })
            }
            // unary plus is a no-op on numbers
            Token::Plus => {
                self.advance();
                self.parse_unary()
            }
            _ => self.parse_power(),
        }
    }

    fn parse_power(&mut self) -> PResult<Expr> {
        let base = self.parse_primary()?;
        if self.peek() == &Token::DoubleStar {
            return Err(self.error("the power operator '**' is not supported yet"));
        }
        Ok(base)
    }

    fn parse_primary(&mut self) -> PResult<Expr> {
        let span = self.peek_span();
        match self.peek().clone() {
            Token::Intlit(v) => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Int(v),
                    span,
                })
            }
            Token::Floatlit(v) => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Float(v),
                    span,
                })
            }
            Token::Strlit(s) => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Str(s),
                    span,
                })
            }
            Token::True => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Bool(true),
                    span,
                })
            }
            Token::False => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Bool(false),
                    span,
                })
            }
            Token::None => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::NoneLit,
                    span,
                })
            }
            // int(x) / float(x) / bool(x) casts
            Token::Int | Token::Float | Token::Bool => {
                let ty = self.parse_type_name("")?;
                self.expect(Token::LParen, &format!("after '{ty}' (cast)"))?;
                let arg = self.parse_expr()?;
                let close = self.expect(Token::RParen, "after cast argument")?;
                Ok(Expr {
                    kind: ExprKind::Cast {
                        ty,
                        arg: Box::new(arg),
                    },
                    span: span.to(close),
                })
            }
            Token::Ident(name) => {
                self.advance();
                if self.peek() == &Token::LParen {
                    self.advance();
                    let mut args = Vec::new();
                    if self.peek() != &Token::RParen {
                        loop {
                            args.push(self.parse_expr()?);
                            if !self.eat(&Token::Comma) {
                                break;
                            }
                            if self.peek() == &Token::RParen {
                                break;
                            }
                        }
                    }
                    let close = self.expect(Token::RParen, "after call arguments")?;
                    return Ok(Expr {
                        kind: ExprKind::Call {
                            func: name,
                            func_span: span,
                            args,
                        },
                        span: span.to(close),
                    });
                }
                Ok(Expr {
                    kind: ExprKind::Name(name),
                    span,
                })
            }
            Token::LParen => {
                self.advance();
                let expr = self.parse_expr()?;
                self.expect(Token::RParen, "to close the parenthesized expression")?;
                Ok(expr)
            }
            Token::LBracket => Err(self.error("lists are not supported yet")),
            Token::LBrace => Err(self.error("dicts and sets are not supported yet")),
            Token::Lambda => Err(self.error("'lambda' is not supported yet")),
            other => Err(self.error(format!("expected an expression, found {}", other.describe()))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(src: &str) -> Module {
        match parse(src) {
            Ok(m) => m,
            Err(e) => panic!("parse failed: {}\n{}", e.message, e.render("test.py", src)),
        }
    }

    fn parse_err(src: &str) -> Diagnostic {
        parse(src).expect_err("expected a parse error")
    }

    #[test]
    fn parses_function_def() {
        let m = parse_ok("def add(a: int, b: int) -> int:\n    return a + b\n");
        assert_eq!(m.body.len(), 1);
        let StmtKind::FuncDef(f) = &m.body[0].kind else {
            panic!("expected FuncDef");
        };
        assert_eq!(f.name, "add");
        assert_eq!(f.params.len(), 2);
        assert_eq!(f.params[0].ty, TypeName::Int);
        assert_eq!(f.ret, Some(TypeName::Int));
        assert_eq!(f.body.len(), 1);
        assert!(matches!(f.body[0].kind, StmtKind::Return(Some(_))));
    }

    #[test]
    fn parses_if_elif_else() {
        let m = parse_ok(
            "if x < 1:\n    y = 1\nelif x < 2:\n    y = 2\nelse:\n    y = 3\n",
        );
        let StmtKind::If { branches, orelse } = &m.body[0].kind else {
            panic!("expected If");
        };
        assert_eq!(branches.len(), 2);
        assert_eq!(orelse.len(), 1);
    }

    #[test]
    fn parses_while_with_break_continue() {
        let m = parse_ok("while True:\n    if x:\n        break\n    continue\n");
        let StmtKind::While { body, .. } = &m.body[0].kind else {
            panic!("expected While");
        };
        assert_eq!(body.len(), 2);
        assert!(matches!(body[1].kind, StmtKind::Continue));
    }

    #[test]
    fn precedence_mul_binds_tighter_than_add() {
        let m = parse_ok("x = 1 + 2 * 3\n");
        let StmtKind::Assign { value, .. } = &m.body[0].kind else {
            panic!("expected Assign");
        };
        // 1 + (2 * 3)
        let ExprKind::Binary { op: BinOp::Add, right, .. } = &value.kind else {
            panic!("expected Add at top, got {:?}", value.kind);
        };
        assert!(matches!(
            right.kind,
            ExprKind::Binary { op: BinOp::Mul, .. }
        ));
    }

    #[test]
    fn precedence_comparison_below_arith_above_and() {
        let m = parse_ok("x = a + 1 < b and c\n");
        let StmtKind::Assign { value, .. } = &m.body[0].kind else {
            panic!("expected Assign");
        };
        // ((a + 1) < b) and c
        let ExprKind::Binary { op: BinOp::And, left, .. } = &value.kind else {
            panic!("expected And at top, got {:?}", value.kind);
        };
        assert!(matches!(left.kind, ExprKind::Binary { op: BinOp::Lt, .. }));
    }

    #[test]
    fn parses_annotated_assignment() {
        let m = parse_ok("x: float = 1.5\n");
        let StmtKind::Assign { annotation, .. } = &m.body[0].kind else {
            panic!("expected Assign");
        };
        assert_eq!(*annotation, Some(TypeName::Float));
    }

    #[test]
    fn parses_augmented_assignment() {
        let m = parse_ok("x += 2\n");
        assert!(matches!(
            m.body[0].kind,
            StmtKind::AugAssign { op: BinOp::Add, .. }
        ));
    }

    #[test]
    fn parses_call_with_args() {
        let m = parse_ok("print(1, 2.5, \"hi\")\n");
        let StmtKind::ExprStmt(e) = &m.body[0].kind else {
            panic!("expected ExprStmt");
        };
        let ExprKind::Call { func, args, .. } = &e.kind else {
            panic!("expected Call");
        };
        assert_eq!(func, "print");
        assert_eq!(args.len(), 3);
    }

    #[test]
    fn parses_cast() {
        let m = parse_ok("x = float(3)\n");
        let StmtKind::Assign { value, .. } = &m.body[0].kind else {
            panic!("expected Assign");
        };
        assert!(matches!(
            value.kind,
            ExprKind::Cast { ty: TypeName::Float, .. }
        ));
    }

    #[test]
    fn parses_single_line_suite() {
        let m = parse_ok("if x: return 1\n");
        let StmtKind::If { branches, .. } = &m.body[0].kind else {
            panic!("expected If");
        };
        assert_eq!(branches[0].1.len(), 1);
    }

    #[test]
    fn parses_nested_blocks() {
        let src = "\
def f(n: int) -> int:
    total = 0
    i = 0
    while i < n:
        if i % 2 == 0:
            total += i
        i += 1
    return total
";
        let m = parse_ok(src);
        let StmtKind::FuncDef(f) = &m.body[0].kind else {
            panic!("expected FuncDef");
        };
        assert_eq!(f.body.len(), 4);
    }

    #[test]
    fn error_missing_param_annotation() {
        let e = parse_err("def f(x):\n    return x\n");
        assert!(e.message.contains("type annotation"), "{}", e.message);
    }

    #[test]
    fn error_missing_colon() {
        let e = parse_err("if x\n    pass\n");
        assert!(e.message.contains("':'"), "{}", e.message);
    }

    #[test]
    fn error_chained_comparison() {
        let e = parse_err("x = 1 < y < 3\n");
        assert!(e.message.contains("chaining"), "{}", e.message);
    }

    #[test]
    fn error_power_operator() {
        let e = parse_err("x = 2 ** 3\n");
        assert!(e.message.contains("'**'"), "{}", e.message);
    }

    #[test]
    fn error_list_literal() {
        let e = parse_err("x = [1, 2]\n");
        assert!(e.message.contains("lists"), "{}", e.message);
    }

    #[test]
    fn error_assign_to_expression() {
        let e = parse_err("f(x) = 3\n");
        assert!(e.message.contains("cannot assign"), "{}", e.message);
    }

    #[test]
    fn error_empty_block() {
        // a def with no body at all — the lexer produces no Indent
        let e = parse_err("def f():\n");
        assert!(!e.message.is_empty());
    }

    #[test]
    fn top_level_script_statements() {
        let m = parse_ok("x = 1\ny = x + 2\nprint(y)\n");
        assert_eq!(m.body.len(), 3);
    }
}
