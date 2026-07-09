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
            Token::Comma => Err(self.error("tuples and multiple assignment are not supported yet")),
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
            Token::For => self.parse_for(),
            Token::Class => Err(self.error("classes are not supported yet")),
            Token::Import => self.parse_import(),
            Token::From => self.parse_from_import(),
            Token::Try | Token::Raise => Err(self.error("exceptions are not supported yet")),
            Token::Match => Err(self.error("'match' statements are not supported yet")),
            Token::With => self.parse_with(),
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
            Token::Global => {
                self.advance();
                let mut names = Vec::new();
                loop {
                    names.push(self.expect_ident("after 'global'")?);
                    if !self.eat(&Token::Comma) {
                        break;
                    }
                }
                let end = names.last().map(|(_, s)| *s).unwrap_or(start);
                Ok(Stmt {
                    kind: StmtKind::Global(names),
                    span: start.to(end),
                })
            }
            Token::Nonlocal => Err(self.error("'nonlocal' is not supported yet")),
            _ => self.parse_assign_or_expr(),
        }
    }

    /// Parse an expression, then decide: plain/annotated/augmented
    /// assignment (if a valid target) or an expression statement.
    fn parse_assign_or_expr(&mut self) -> PResult<Stmt> {
        let expr = self.parse_expr()?;

        let aug_op = match self.peek() {
            Token::PlusEq => Some(BinOp::Add),
            Token::MinusEq => Some(BinOp::Sub),
            Token::StarEq => Some(BinOp::Mul),
            Token::SlashEq => Some(BinOp::Div),
            Token::DoubleSlashEq => Some(BinOp::FloorDiv),
            Token::PercentEq => Some(BinOp::Mod),
            Token::DoubleStarEq => Some(BinOp::Pow),
            _ => None,
        };

        if self.peek() == &Token::Eq {
            self.advance();
            let mut targets = vec![self.expr_to_target(expr)?];
            // `a = b = c = value` — more targets while RHS is itself assigned
            let value = loop {
                let next = self.parse_expr()?;
                if self.peek() == &Token::Eq {
                    self.advance();
                    targets.push(self.expr_to_target(next)?);
                } else {
                    break next;
                }
            };
            let span = target_span(&targets[0]).to(value.span);
            return Ok(Stmt {
                kind: StmtKind::Assign {
                    targets,
                    annotation: None,
                    value,
                },
                span,
            });
        }

        if self.peek() == &Token::Colon {
            // annotated assignment: `x: ty = value` (names only; not multi-assign)
            let ExprKind::Name(_) = expr.kind else {
                return Err(self.error("type annotations are only allowed on plain variable names"));
            };
            self.advance();
            let annotation = self.parse_type_name("after ':' in annotated assignment")?;
            self.expect(
                Token::Eq,
                "after type annotation (declarations require a value)",
            )?;
            let value = self.parse_expr()?;
            if self.peek() == &Token::Eq {
                return Err(
                    self.error("type annotations are not allowed in multi-target assignment")
                );
            }
            let span = expr.span.to(value.span);
            let target = self.expr_to_target(expr)?;
            return Ok(Stmt {
                kind: StmtKind::Assign {
                    targets: vec![target],
                    annotation: Some(annotation),
                    value,
                },
                span,
            });
        }

        if let Some(op) = aug_op {
            self.advance();
            let target = self.expr_to_target(expr)?;
            let value = self.parse_expr()?;
            let span = target_span(&target).to(value.span);
            return Ok(Stmt {
                kind: StmtKind::AugAssign { target, op, value },
                span,
            });
        }

        let span = expr.span;
        Ok(Stmt {
            kind: StmtKind::ExprStmt(expr),
            span,
        })
    }

    fn expr_to_target(&self, expr: Expr) -> PResult<AssignTarget> {
        match expr.kind {
            ExprKind::Name(name) => Ok(AssignTarget::Name {
                name,
                span: expr.span,
            }),
            ExprKind::Index { base, index } => Ok(AssignTarget::Index {
                base: *base,
                index: *index,
            }),
            _ => Err(Diagnostic::new(
                Phase::Parse,
                "cannot assign to this expression",
                expr.span,
            )),
        }
    }

    fn expect_ident(&mut self, context: &str) -> PResult<(String, Span)> {
        match self.advance() {
            (Token::Ident(name), span) => Ok((name, span)),
            (other, span) => Err(Diagnostic::new(
                Phase::Parse,
                format!(
                    "expected identifier {}, found {}",
                    context,
                    other.describe()
                ),
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
            Token::Str => {
                self.advance();
                Ok(TypeName::Str)
            }
            Token::File => {
                self.advance();
                Ok(TypeName::File)
            }
            Token::List => {
                self.advance();
                self.expect(Token::LBracket, "after 'list' (e.g. 'list[int]')")?;
                let elem = self.parse_type_name("inside 'list[...]'")?;
                if elem == TypeName::None {
                    return Err(self.error("list elements cannot be None"));
                }
                if elem == TypeName::File {
                    return Err(self.error("list elements cannot be file"));
                }
                self.expect(Token::RBracket, "to close 'list[...]'")?;
                Ok(TypeName::List(Box::leak(Box::new(elem))))
            }
            Token::None => {
                self.advance();
                Ok(TypeName::None)
            }
            other => Err(self.error(format!(
                "expected a type ('int', 'float', 'bool', 'str', 'file', 'list[...]' or 'None') {}, found {}",
                context,
                other.describe()
            ))),
        }
    }

    fn parse_funcdef(&mut self) -> PResult<Stmt> {
        let def_span = self.expect(Token::Def, "")?;
        let (name, _) = self.expect_ident("after 'def'")?;
        self.expect(Token::LParen, "after function name")?;

        let mut params: Vec<Param> = Vec::new();
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
                let default = if self.eat(&Token::Eq) {
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                // no non-default after default
                if default.is_none() && params.iter().any(|p| p.default.is_some()) {
                    return Err(Diagnostic::new(
                        Phase::Parse,
                        format!("non-default argument '{pname}' follows default argument"),
                        pspan,
                    ));
                }
                params.push(Param {
                    name: pname,
                    ty,
                    span: pspan,
                    default,
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

    fn parse_for(&mut self) -> PResult<Stmt> {
        let start = self.expect(Token::For, "")?;
        let (var, var_span) = self.expect_ident("after 'for'")?;
        if self.peek() == &Token::Comma {
            return Err(self.error("unpacking multiple loop variables is not supported yet"));
        }
        self.expect(Token::In, "after the loop variable")?;
        let iter = self.parse_expr()?;
        let body = self.parse_block("'for' body")?;
        let end = self.peek_span();
        Ok(Stmt {
            kind: StmtKind::For {
                var,
                var_span,
                iter,
                body,
            },
            span: start.to(end),
        })
    }

    fn parse_import(&mut self) -> PResult<Stmt> {
        let start = self.expect(Token::Import, "")?;
        let (module, mspan) = self.expect_ident("after 'import'")?;
        if self.peek() == &Token::Dot {
            return Err(self.error("dotted module names (packages) are not supported yet"));
        }
        let alias = if self.eat(&Token::As) {
            Some(self.expect_ident("after 'as'")?.0)
        } else {
            None
        };
        if self.peek() == &Token::Comma {
            return Err(self.error(
                "importing several modules in one statement is not supported yet; \
                 use one 'import' per line",
            ));
        }
        let stmt = Stmt {
            kind: StmtKind::Import {
                module,
                alias,
                span: mspan,
            },
            span: start.to(mspan),
        };
        self.expect_stmt_end()?;
        Ok(stmt)
    }

    fn parse_from_import(&mut self) -> PResult<Stmt> {
        let start = self.expect(Token::From, "")?;
        let (module, _) = self.expect_ident("after 'from'")?;
        if self.peek() == &Token::Dot {
            return Err(self.error("dotted module names (packages) are not supported yet"));
        }
        self.expect(Token::Import, "after the module name")?;
        if self.peek() == &Token::Star {
            return Err(self.error("'from module import *' is not supported yet"));
        }
        let mut names = Vec::new();
        loop {
            let (name, name_span) = self.expect_ident("in the import list")?;
            let alias = if self.eat(&Token::As) {
                Some(self.expect_ident("after 'as'")?.0)
            } else {
                None
            };
            names.push((name, alias, name_span));
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        let end = self.peek_span();
        let stmt = Stmt {
            kind: StmtKind::FromImport {
                module,
                names,
                span: start.to(end),
            },
            span: start.to(end),
        };
        self.expect_stmt_end()?;
        Ok(stmt)
    }

    fn parse_with(&mut self) -> PResult<Stmt> {
        let start = self.expect(Token::With, "")?;
        let item = self.parse_expr()?;
        let target = if self.eat(&Token::As) {
            Some(self.expect_ident("after 'as'")?)
        } else {
            None
        };
        if self.peek() == &Token::Comma {
            return Err(self.error(
                "multiple context managers in one 'with' are not supported \
                 yet; nest them instead",
            ));
        }
        let body = self.parse_block("'with' body")?;
        let end = self.peek_span();
        Ok(Stmt {
            kind: StmtKind::With { item, target, body },
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

    /// The comparison operator at the cursor, with how many tokens it
    /// spans (`not in` is two).
    fn comparison_op(&self) -> Option<(BinOp, usize)> {
        match self.peek() {
            Token::EqEq => Some((BinOp::Eq, 1)),
            Token::NotEq => Some((BinOp::NotEq, 1)),
            Token::Lt => Some((BinOp::Lt, 1)),
            Token::LtEq => Some((BinOp::LtEq, 1)),
            Token::Gt => Some((BinOp::Gt, 1)),
            Token::GtEq => Some((BinOp::GtEq, 1)),
            Token::In => Some((BinOp::In, 1)),
            Token::Not if self.peek2() == &Token::In => Some((BinOp::NotIn, 2)),
            _ => None,
        }
    }

    /// A comparison or a chain (`a < b <= c`); chains get their own node so
    /// semantic can evaluate the middle operands exactly once.
    fn parse_comparison(&mut self) -> PResult<Expr> {
        let first = self.parse_arith()?;
        let mut rest = Vec::new();
        while let Some((op, tokens)) = self.comparison_op() {
            self.pos += tokens;
            let operand = self.parse_arith()?;
            rest.push((op, operand));
        }
        if rest.len() > 1
            && rest
                .iter()
                .any(|(op, _)| matches!(op, BinOp::In | BinOp::NotIn))
        {
            return Err(self.error("'in' cannot be combined with other comparisons in a chain"));
        }
        match rest.len() {
            0 => Ok(first),
            1 => {
                let (op, right) = rest.pop().unwrap();
                let span = first.span.to(right.span);
                Ok(Expr {
                    kind: ExprKind::Binary {
                        op,
                        left: Box::new(first),
                        right: Box::new(right),
                    },
                    span,
                })
            }
            _ => {
                let span = first.span.to(rest.last().unwrap().1.span);
                Ok(Expr {
                    kind: ExprKind::Compare {
                        first: Box::new(first),
                        rest,
                    },
                    span,
                })
            }
        }
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

    /// `**` is right-associative and, like Python, binds tighter than a
    /// unary minus on the left but looser than one on the right:
    /// `-2**2 == -4`, `2**-1 == 0.5`.
    fn parse_power(&mut self) -> PResult<Expr> {
        let base = self.parse_postfix()?;
        if self.peek() == &Token::DoubleStar {
            self.advance();
            let exp = self.parse_unary()?;
            let span = base.span.to(exp.span);
            return Ok(Expr {
                kind: ExprKind::Binary {
                    op: BinOp::Pow,
                    left: Box::new(base),
                    right: Box::new(exp),
                },
                span,
            });
        }
        Ok(base)
    }

    /// Postfix operators: subscripts `x[i]` and method calls `x.m(...)`.
    fn parse_postfix(&mut self) -> PResult<Expr> {
        let mut expr = self.parse_primary()?;
        loop {
            match self.peek() {
                Token::LBracket => {
                    self.advance();
                    // `[index]`, `[lo:hi]`, `[:hi]`, `[lo:]`, `[:]`
                    let lo = if self.peek() == &Token::Colon {
                        None
                    } else {
                        Some(self.parse_expr()?)
                    };
                    if self.eat(&Token::Colon) {
                        let hi = if matches!(self.peek(), Token::RBracket | Token::Colon) {
                            None
                        } else {
                            Some(Box::new(self.parse_expr()?))
                        };
                        // optional third component; empty (`xs[::]`) means
                        // no step, like Python
                        let step = if self.eat(&Token::Colon) && self.peek() != &Token::RBracket {
                            Some(Box::new(self.parse_expr()?))
                        } else {
                            None
                        };
                        let close = self.expect(Token::RBracket, "to close the slice")?;
                        let span = expr.span.to(close);
                        expr = Expr {
                            kind: ExprKind::Slice {
                                base: Box::new(expr),
                                lo: lo.map(Box::new),
                                hi,
                                step,
                            },
                            span,
                        };
                    } else {
                        let index = lo.expect("index expression parsed above");
                        let close = self.expect(Token::RBracket, "to close the subscript")?;
                        let span = expr.span.to(close);
                        expr = Expr {
                            kind: ExprKind::Index {
                                base: Box::new(expr),
                                index: Box::new(index),
                            },
                            span,
                        };
                    }
                }
                Token::Dot => {
                    self.advance();
                    let (method, method_span) = self.expect_ident("after '.'")?;
                    if self.peek() != &Token::LParen {
                        // bare attribute (e.g. sys.argv) — semantic validates
                        let span = expr.span.to(method_span);
                        expr = Expr {
                            kind: ExprKind::Attribute {
                                base: Box::new(expr),
                                attr: method,
                                attr_span: method_span,
                            },
                            span,
                        };
                        continue;
                    }
                    self.advance();
                    let (args, keywords) = self.parse_call_args()?;
                    let close = self.expect(Token::RParen, "after method arguments")?;
                    let span = expr.span.to(close);
                    expr = Expr {
                        kind: ExprKind::MethodCall {
                            base: Box::new(expr),
                            method,
                            method_span,
                            args,
                            keywords,
                        },
                        span,
                    };
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    /// Positional args then `name=value` keywords (no positionals after keywords).
    fn parse_call_args(&mut self) -> PResult<(Vec<Expr>, Vec<Keyword>)> {
        let mut args = Vec::new();
        let mut keywords = Vec::new();
        let mut seen_kw = false;
        if self.peek() != &Token::RParen {
            loop {
                // keyword: IDENT '=' expr (not `==`)
                if let Token::Ident(name) = self.peek().clone()
                    && *self.peek2() == Token::Eq
                {
                    let name_span = self.peek_span();
                    self.advance(); // name
                    self.advance(); // =
                    let value = self.parse_expr()?;
                    keywords.push(Keyword {
                        name,
                        name_span,
                        value,
                    });
                    seen_kw = true;
                } else {
                    if seen_kw {
                        return Err(self.error("positional argument follows keyword argument"));
                    }
                    args.push(self.parse_expr()?);
                }
                if !self.eat(&Token::Comma) {
                    break;
                }
                if self.peek() == &Token::RParen {
                    break;
                }
            }
        }
        Ok((args, keywords))
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
            Token::FStrlit(raw) => {
                self.advance();
                parse_fstring(&raw, span)
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
            // int(x) / float(x) / bool(x) / str(x) casts (file(...) is rejected in semantic)
            Token::Int | Token::Float | Token::Bool | Token::Str | Token::File => {
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
            Token::List => Err(self.error(
                "list() is not supported; use a literal like '[]' with a type \
                 annotation, e.g. 'xs: list[int] = []'",
            )),
            Token::Ident(name) => {
                self.advance();
                if self.peek() == &Token::LParen {
                    self.advance();
                    let (args, keywords) = self.parse_call_args()?;
                    let close = self.expect(Token::RParen, "after call arguments")?;
                    return Ok(Expr {
                        kind: ExprKind::Call {
                            func: name,
                            func_span: span,
                            args,
                            keywords,
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
                if self.peek() == &Token::Comma {
                    return Err(self.error("tuples are not supported yet"));
                }
                self.expect(Token::RParen, "to close the parenthesized expression")?;
                Ok(expr)
            }
            Token::LBracket => {
                self.advance();
                if self.peek() == &Token::RBracket {
                    let close = self.expect(Token::RBracket, "to close the list literal")?;
                    return Ok(Expr {
                        kind: ExprKind::ListLit(vec![]),
                        span: span.to(close),
                    });
                }
                let first = self.parse_expr()?;
                // `[elem for var in iter]` — a comprehension
                if self.peek() == &Token::For {
                    self.advance();
                    let (var, var_span) = self.expect_ident("after 'for'")?;
                    if self.peek() == &Token::Comma {
                        return Err(
                            self.error("unpacking multiple loop variables is not supported yet")
                        );
                    }
                    self.expect(Token::In, "after the comprehension variable")?;
                    let iter = self.parse_expr()?;
                    let cond = if self.eat(&Token::If) {
                        Some(Box::new(self.parse_expr()?))
                    } else {
                        None
                    };
                    if matches!(self.peek(), Token::For | Token::If) {
                        return Err(self.error(
                            "multiple comprehension clauses are not supported \
                             yet; nest comprehensions or use a loop",
                        ));
                    }
                    let close = self.expect(Token::RBracket, "to close the comprehension")?;
                    return Ok(Expr {
                        kind: ExprKind::ListComp {
                            elem: Box::new(first),
                            var,
                            var_span,
                            iter: Box::new(iter),
                            cond,
                        },
                        span: span.to(close),
                    });
                }
                let mut items = vec![first];
                while self.eat(&Token::Comma) {
                    if self.peek() == &Token::RBracket {
                        break;
                    }
                    items.push(self.parse_expr()?);
                }
                let close = self.expect(Token::RBracket, "to close the list literal")?;
                Ok(Expr {
                    kind: ExprKind::ListLit(items),
                    span: span.to(close),
                })
            }
            Token::LBrace => Err(self.error("dicts and sets are not supported yet")),
            Token::Lambda => Err(self.error("'lambda' is not supported yet")),
            other => Err(self.error(format!(
                "expected an expression, found {}",
                other.describe()
            ))),
        }
    }
}

fn target_span(target: &AssignTarget) -> Span {
    match target {
        AssignTarget::Name { span, .. } => *span,
        AssignTarget::Index { base, index } => base.span.to(index.span),
    }
}

// ---- f-strings ----

/// Split f-string content into literal chunks and `{...}` expressions.
/// `{{`/`}}` are escaped braces. Each fragment is lexed and parsed on its
/// own; fragment spans are rebased onto the whole literal's span so
/// diagnostics point at the f-string in the real source.
fn parse_fstring(raw: &str, span: Span) -> PResult<Expr> {
    let err = |msg: &str| Diagnostic::new(Phase::Parse, msg, span);

    let mut parts = Vec::new();
    let mut lit = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    lit.push('{');
                    continue;
                }
                let mut depth = 1;
                let mut frag = String::new();
                loop {
                    match chars.next() {
                        Some('{') => {
                            depth += 1;
                            frag.push('{');
                        }
                        Some('}') => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                            frag.push('}');
                        }
                        Some(ch) => frag.push(ch),
                        None => return Err(err("unterminated '{' in f-string")),
                    }
                }
                check_fragment_modifiers(&frag, span)?;
                if frag.trim().is_empty() {
                    return Err(err("empty expression in f-string"));
                }
                if !lit.is_empty() {
                    parts.push(FStringPart::Literal(std::mem::take(&mut lit)));
                }
                parts.push(FStringPart::Expr(parse_fragment(&frag, span)?));
            }
            '}' => {
                if chars.peek() == Some(&'}') {
                    chars.next();
                    lit.push('}');
                } else {
                    return Err(err("single '}' is not allowed in an f-string; use '}}'"));
                }
            }
            _ => lit.push(c),
        }
    }
    if !lit.is_empty() {
        parts.push(FStringPart::Literal(lit));
    }
    Ok(Expr {
        kind: ExprKind::JoinedStr(parts),
        span,
    })
}

/// Reject `{x:...}` format specs and `{x!r}` conversions (unsupported); a
/// ':' inside brackets (e.g. a slice) is fine.
fn check_fragment_modifiers(frag: &str, span: Span) -> PResult<()> {
    let mut depth = 0i32;
    let mut chars = frag.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ':' if depth == 0 => {
                return Err(Diagnostic::new(
                    Phase::Parse,
                    "format specifiers in f-strings are not supported yet; \
                     convert explicitly, e.g. {str(x)}",
                    span,
                ));
            }
            '!' if depth == 0 && chars.peek() != Some(&'=') => {
                return Err(Diagnostic::new(
                    Phase::Parse,
                    "conversions like '!r' in f-strings are not supported yet",
                    span,
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn parse_fragment(frag: &str, span: Span) -> PResult<Expr> {
    let in_fstring = |d: Diagnostic| {
        Diagnostic::new(
            Phase::Parse,
            format!("in f-string expression '{{{frag}}}': {}", d.message),
            span,
        )
    };
    let tokens = lexer::lex(frag).map_err(in_fstring)?;
    let mut parser = Parser::new(tokens);
    let mut expr = parser.parse_expr().map_err(in_fstring)?;
    if !matches!(parser.peek(), Token::EOF | Token::Newline) {
        return Err(Diagnostic::new(
            Phase::Parse,
            format!(
                "in f-string expression '{{{frag}}}': unexpected {}",
                parser.peek().describe()
            ),
            span,
        ));
    }
    rebase_spans(&mut expr, span);
    Ok(expr)
}

/// Point every node of a fragment expression at the f-string literal's
/// span (fragment-local offsets are meaningless in the real source).
fn rebase_spans(expr: &mut Expr, span: Span) {
    expr.span = span;
    match &mut expr.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::NoneLit
        | ExprKind::Name(_) => {}
        ExprKind::Call {
            func_span, args, ..
        } => {
            *func_span = span;
            for a in args {
                rebase_spans(a, span);
            }
        }
        ExprKind::Attribute {
            base, attr_span, ..
        } => {
            *attr_span = span;
            rebase_spans(base, span);
        }
        ExprKind::MethodCall {
            base,
            method_span,
            args,
            ..
        } => {
            *method_span = span;
            rebase_spans(base, span);
            for a in args {
                rebase_spans(a, span);
            }
        }
        ExprKind::Index { base, index } => {
            rebase_spans(base, span);
            rebase_spans(index, span);
        }
        ExprKind::Slice { base, lo, hi, step } => {
            rebase_spans(base, span);
            if let Some(lo) = lo {
                rebase_spans(lo, span);
            }
            if let Some(hi) = hi {
                rebase_spans(hi, span);
            }
            if let Some(step) = step {
                rebase_spans(step, span);
            }
        }
        ExprKind::ListLit(items) => {
            for item in items {
                rebase_spans(item, span);
            }
        }
        ExprKind::ListComp {
            elem,
            var_span,
            iter,
            cond,
            ..
        } => {
            *var_span = span;
            rebase_spans(elem, span);
            rebase_spans(iter, span);
            if let Some(c) = cond {
                rebase_spans(c, span);
            }
        }
        ExprKind::Cast { arg, .. } => rebase_spans(arg, span),
        ExprKind::Binary { left, right, .. } => {
            rebase_spans(left, span);
            rebase_spans(right, span);
        }
        ExprKind::Compare { first, rest } => {
            rebase_spans(first, span);
            for (_, e) in rest {
                rebase_spans(e, span);
            }
        }
        ExprKind::Unary { operand, .. } => rebase_spans(operand, span),
        ExprKind::JoinedStr(parts) => {
            for part in parts {
                if let FStringPart::Expr(e) = part {
                    rebase_spans(e, span);
                }
            }
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
        let m = parse_ok("if x < 1:\n    y = 1\nelif x < 2:\n    y = 2\nelse:\n    y = 3\n");
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
    fn parses_for_loop() {
        let m = parse_ok("for i in range(10):\n    print(i)\n");
        let StmtKind::For {
            var, iter, body, ..
        } = &m.body[0].kind
        else {
            panic!("expected For");
        };
        assert_eq!(var, "i");
        assert!(matches!(&iter.kind, ExprKind::Call { func, .. } if func == "range"));
        assert_eq!(body.len(), 1);
    }

    #[test]
    fn precedence_mul_binds_tighter_than_add() {
        let m = parse_ok("x = 1 + 2 * 3\n");
        let StmtKind::Assign { value, .. } = &m.body[0].kind else {
            panic!("expected Assign");
        };
        // 1 + (2 * 3)
        let ExprKind::Binary {
            op: BinOp::Add,
            right,
            ..
        } = &value.kind
        else {
            panic!("expected Add at top, got {:?}", value.kind);
        };
        assert!(matches!(
            right.kind,
            ExprKind::Binary { op: BinOp::Mul, .. }
        ));
    }

    #[test]
    fn power_is_right_associative() {
        let m = parse_ok("x = 2 ** 3 ** 2\n");
        let StmtKind::Assign { value, .. } = &m.body[0].kind else {
            panic!("expected Assign");
        };
        // 2 ** (3 ** 2)
        let ExprKind::Binary {
            op: BinOp::Pow,
            left,
            right,
        } = &value.kind
        else {
            panic!("expected Pow at top, got {:?}", value.kind);
        };
        assert!(matches!(left.kind, ExprKind::Int(2)));
        assert!(matches!(
            right.kind,
            ExprKind::Binary { op: BinOp::Pow, .. }
        ));
    }

    #[test]
    fn power_binds_tighter_than_unary_minus_on_left() {
        let m = parse_ok("x = -2 ** 2\n");
        let StmtKind::Assign { value, .. } = &m.body[0].kind else {
            panic!("expected Assign");
        };
        // -(2 ** 2)
        let ExprKind::Unary {
            op: UnaryOp::Neg,
            operand,
        } = &value.kind
        else {
            panic!("expected Neg at top, got {:?}", value.kind);
        };
        assert!(matches!(
            operand.kind,
            ExprKind::Binary { op: BinOp::Pow, .. }
        ));
    }

    #[test]
    fn power_allows_unary_minus_exponent() {
        let m = parse_ok("x = 2 ** -1\n");
        let StmtKind::Assign { value, .. } = &m.body[0].kind else {
            panic!("expected Assign");
        };
        let ExprKind::Binary {
            op: BinOp::Pow,
            right,
            ..
        } = &value.kind
        else {
            panic!("expected Pow, got {:?}", value.kind);
        };
        assert!(matches!(
            right.kind,
            ExprKind::Unary {
                op: UnaryOp::Neg,
                ..
            }
        ));
    }

    #[test]
    fn comparison_chain_gets_own_node() {
        let m = parse_ok("x = 1 < y < 3\n");
        let StmtKind::Assign { value, .. } = &m.body[0].kind else {
            panic!("expected Assign");
        };
        let ExprKind::Compare { rest, .. } = &value.kind else {
            panic!("expected Compare, got {:?}", value.kind);
        };
        assert_eq!(rest.len(), 2);
    }

    #[test]
    fn single_comparison_stays_binary() {
        let m = parse_ok("x = a < b\n");
        let StmtKind::Assign { value, .. } = &m.body[0].kind else {
            panic!("expected Assign");
        };
        assert!(matches!(value.kind, ExprKind::Binary { op: BinOp::Lt, .. }));
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
    fn parses_list_annotation() {
        let m = parse_ok("xs: list[int] = []\n");
        let StmtKind::Assign {
            annotation, value, ..
        } = &m.body[0].kind
        else {
            panic!("expected Assign");
        };
        assert_eq!(
            *annotation,
            Some(TypeName::List(Box::leak(Box::new(TypeName::Int))))
        );
        assert!(matches!(&value.kind, ExprKind::ListLit(items) if items.is_empty()));
    }

    #[test]
    fn parses_list_literal_and_index() {
        let m = parse_ok("xs = [1, 2, 3]\ny = xs[0]\n");
        let StmtKind::Assign { value, .. } = &m.body[0].kind else {
            panic!("expected Assign");
        };
        assert!(matches!(&value.kind, ExprKind::ListLit(items) if items.len() == 3));
        let StmtKind::Assign { value, .. } = &m.body[1].kind else {
            panic!("expected Assign");
        };
        assert!(matches!(value.kind, ExprKind::Index { .. }));
    }

    #[test]
    fn parses_index_assignment() {
        let m = parse_ok("xs[0] = 5\n");
        let StmtKind::Assign { targets, .. } = &m.body[0].kind else {
            panic!("expected Assign");
        };
        assert!(matches!(targets.as_slice(), [AssignTarget::Index { .. }]));
    }

    #[test]
    fn parses_multi_assign() {
        let m = parse_ok("a = b = 0\n");
        let StmtKind::Assign { targets, value, .. } = &m.body[0].kind else {
            panic!("expected Assign");
        };
        assert_eq!(targets.len(), 2);
        assert!(matches!(&targets[0], AssignTarget::Name { name, .. } if name == "a"));
        assert!(matches!(&targets[1], AssignTarget::Name { name, .. } if name == "b"));
        assert!(matches!(value.kind, ExprKind::Int(0)));
    }

    #[test]
    fn parses_method_call() {
        let m = parse_ok("xs.append(4)\n");
        let StmtKind::ExprStmt(e) = &m.body[0].kind else {
            panic!("expected ExprStmt");
        };
        let ExprKind::MethodCall { method, args, .. } = &e.kind else {
            panic!("expected MethodCall, got {:?}", e.kind);
        };
        assert_eq!(method, "append");
        assert_eq!(args.len(), 1);
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
    fn parses_augmented_index_assignment() {
        let m = parse_ok("xs[i] += 2\n");
        let StmtKind::AugAssign { target, .. } = &m.body[0].kind else {
            panic!("expected AugAssign");
        };
        assert!(matches!(target, AssignTarget::Index { .. }));
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
    fn parses_casts() {
        let m = parse_ok("x = float(3)\ns = str(42)\n");
        let StmtKind::Assign { value, .. } = &m.body[0].kind else {
            panic!("expected Assign");
        };
        assert!(matches!(
            value.kind,
            ExprKind::Cast {
                ty: TypeName::Float,
                ..
            }
        ));
        let StmtKind::Assign { value, .. } = &m.body[1].kind else {
            panic!("expected Assign");
        };
        assert!(matches!(
            value.kind,
            ExprKind::Cast {
                ty: TypeName::Str,
                ..
            }
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
    fn parses_nested_list_type() {
        let m = parse_ok("xs: list[list[int]] = []\n");
        let StmtKind::Assign { annotation, .. } = &m.body[0].kind else {
            panic!();
        };
        let Some(TypeName::List(inner)) = annotation else {
            panic!("expected list annotation, got {annotation:?}");
        };
        assert!(matches!(inner, TypeName::List(e) if **e == TypeName::Int));
    }

    #[test]
    fn parses_slices() {
        let m = parse_ok("a = xs[1:3]\nb = xs[:2]\nc = xs[1:]\nd = xs[:]\n");
        for stmt in &m.body {
            let StmtKind::Assign { value, .. } = &stmt.kind else {
                panic!("expected Assign");
            };
            assert!(matches!(value.kind, ExprKind::Slice { .. }), "{:?}", value);
        }
        let StmtKind::Assign { value, .. } = &m.body[0].kind else {
            panic!();
        };
        let ExprKind::Slice { lo, hi, .. } = &value.kind else {
            panic!();
        };
        assert!(lo.is_some() && hi.is_some());
    }

    #[test]
    fn parses_slice_steps() {
        let m = parse_ok("a = xs[1:5:2]\nb = xs[::-1]\nc = xs[::2]\nd = xs[::]\n");
        let StmtKind::Assign { value, .. } = &m.body[0].kind else {
            panic!();
        };
        let ExprKind::Slice { lo, hi, step, .. } = &value.kind else {
            panic!("expected Slice, got {:?}", value.kind);
        };
        assert!(lo.is_some() && hi.is_some() && step.is_some());
        let StmtKind::Assign { value, .. } = &m.body[1].kind else {
            panic!();
        };
        let ExprKind::Slice { lo, hi, step, .. } = &value.kind else {
            panic!();
        };
        assert!(lo.is_none() && hi.is_none() && step.is_some());
        // empty trailing step means no step
        let StmtKind::Assign { value, .. } = &m.body[3].kind else {
            panic!();
        };
        assert!(matches!(&value.kind, ExprKind::Slice { step, .. } if step.is_none()));
    }

    #[test]
    fn parses_global_statement() {
        let m = parse_ok("def f():\n    global a, b\n    a = 1\nf()\n");
        let StmtKind::FuncDef(f) = &m.body[0].kind else {
            panic!();
        };
        let StmtKind::Global(names) = &f.body[0].kind else {
            panic!("expected Global, got {:?}", f.body[0].kind);
        };
        assert_eq!(names.len(), 2);
        assert_eq!(names[0].0, "a");
    }

    #[test]
    fn parses_in_and_not_in() {
        let m = parse_ok("a = x in xs\nb = x not in xs\n");
        let StmtKind::Assign { value, .. } = &m.body[0].kind else {
            panic!();
        };
        assert!(matches!(value.kind, ExprKind::Binary { op: BinOp::In, .. }));
        let StmtKind::Assign { value, .. } = &m.body[1].kind else {
            panic!();
        };
        assert!(matches!(
            value.kind,
            ExprKind::Binary {
                op: BinOp::NotIn,
                ..
            }
        ));
    }

    #[test]
    fn error_in_inside_chain() {
        let e = parse_err("b = 1 < x in xs\n");
        assert!(e.message.contains("chain"), "{}", e.message);
    }

    #[test]
    fn parses_fstring_parts() {
        let m = parse_ok("s = f\"a{x}b{y + 1}c\"\n");
        let StmtKind::Assign { value, .. } = &m.body[0].kind else {
            panic!();
        };
        let ExprKind::JoinedStr(parts) = &value.kind else {
            panic!("expected JoinedStr, got {:?}", value.kind);
        };
        assert_eq!(parts.len(), 5);
        assert!(matches!(&parts[0], FStringPart::Literal(s) if s == "a"));
        assert!(matches!(&parts[1], FStringPart::Expr(_)));
        assert!(matches!(&parts[3], FStringPart::Expr(e)
            if matches!(e.kind, ExprKind::Binary { .. })));
    }

    #[test]
    fn fstring_escaped_braces() {
        let m = parse_ok("s = f\"{{literal}} {x}\"\n");
        let StmtKind::Assign { value, .. } = &m.body[0].kind else {
            panic!();
        };
        let ExprKind::JoinedStr(parts) = &value.kind else {
            panic!();
        };
        assert!(matches!(&parts[0], FStringPart::Literal(s) if s == "{literal} "));
    }

    #[test]
    fn error_fstring_format_spec() {
        let e = parse_err("s = f\"{x:.2f}\"\n");
        assert!(e.message.contains("format specifiers"), "{}", e.message);
    }

    #[test]
    fn error_fstring_bad_expr() {
        let e = parse_err("s = f\"{x +}\"\n");
        assert!(e.message.contains("f-string"), "{}", e.message);
    }

    #[test]
    fn parses_bare_attribute() {
        // semantic decides whether the attribute is valid (only sys.argv is)
        let m = parse_ok("y = sys.argv\n");
        let StmtKind::Assign { value, .. } = &m.body[0].kind else {
            panic!();
        };
        assert!(matches!(
            &value.kind,
            ExprKind::Attribute { attr, .. } if attr == "argv"
        ));
    }

    #[test]
    fn parses_import_sys() {
        let m = parse_ok("import sys\nprint(len(sys.argv))\n");
        assert!(matches!(
            &m.body[0].kind,
            StmtKind::Import { module, .. } if module == "sys"
        ));
    }

    #[test]
    fn error_assign_to_expression() {
        let e = parse_err("f(x) = 3\n");
        assert!(e.message.contains("cannot assign"), "{}", e.message);
    }

    #[test]
    fn error_tuple_assignment() {
        let e = parse_err("a, b = 1, 2\n");
        assert!(e.message.contains("not supported"), "{}", e.message);
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
