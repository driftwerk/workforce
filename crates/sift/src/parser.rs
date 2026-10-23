//! Tokens to a syntax tree, with recovery at statement boundaries so one
//! run reports independent errors.

use crate::ast::{
    Arm, BinaryOp, Body, Expr, ExprKind, Field, Lambda, Pattern, Segment, Stmt, UnaryOp,
};
use crate::check;
use crate::diagnostic::{code, settle, Diagnostic, Pos};
use crate::lexer::{lex, Part, Tok, Token};
use std::sync::Arc;

/// At most this many diagnostics reach the model.
pub(crate) const MAX_DIAGNOSTICS: usize = 5;

/// How deeply expressions and blocks may nest. It bounds the interpreter's
/// recursion as well as the parser's.
const MAX_DEPTH: u32 = 96;

/// Parse and statically check a program.
pub(crate) fn parse(source: &str) -> Result<Vec<Stmt>, Vec<Diagnostic>> {
    let (tokens, mut diagnostics) = lex(source);
    let mut parser = Parser::new(&tokens, 0);
    let program = parser.statements(true);
    let syntax_clean = diagnostics.is_empty() && parser.diagnostics.is_empty();
    diagnostics.append(&mut parser.diagnostics);
    let end = tokens.last().map_or(Pos::START, |t| t.pos);
    diagnostics.extend(check::program(&program, end, syntax_clean));
    if diagnostics.is_empty() {
        Ok(program)
    } else {
        Err(settle(diagnostics, MAX_DIAGNOSTICS))
    }
}

type Parsed<T> = Result<T, Diagnostic>;

/// Words from other languages that a model may reach for, with what Sift
/// offers instead.
const FOREIGN_KEYWORDS: &[(&str, &str)] = &[
    ("while", "the only loop is `for x in list { ... }`"),
    ("loop", "the only loop is `for x in list { ... }`"),
    ("do", "the only loop is `for x in list { ... }`"),
    (
        "function",
        "functions are single-expression lambdas: `x => expr`",
    ),
    ("fn", "functions are single-expression lambdas: `x => expr`"),
    (
        "def",
        "functions are single-expression lambdas: `x => expr`",
    ),
    ("class", "values are records: `{ name: \"a\", n: 1 }`"),
    ("struct", "values are records: `{ name: \"a\", n: 1 }`"),
    ("type", "types are not written in Sift"),
    ("interface", "types are not written in Sift"),
    ("enum", "types are not written in Sift"),
    (
        "import",
        "tools and library functions are available by name",
    ),
    ("export", "a program answers with `return <value>`"),
    (
        "async",
        "tool calls are awaited automatically; run them concurrently with `all([...])`",
    ),
    (
        "await",
        "tool calls are awaited automatically; run them concurrently with `all([...])`",
    ),
    ("try", "a failed call ends the run with its error"),
    ("throw", "a failed call ends the run with its error"),
    ("switch", "use `match value { \"a\" => ..., _ => ... }`"),
    ("elif", "write `else if`"),
];

struct Parser<'t> {
    tokens: &'t [Token],
    i: usize,
    depth: u32,
    diagnostics: Vec<Diagnostic>,
}

impl<'t> Parser<'t> {
    fn new(tokens: &'t [Token], depth: u32) -> Self {
        Self {
            tokens,
            i: 0,
            depth,
            diagnostics: Vec::new(),
        }
    }

    fn peek(&self) -> &'t Token {
        self.nth(0)
    }

    fn nth(&self, ahead: usize) -> &'t Token {
        let last = self.tokens.len().saturating_sub(1);
        &self.tokens[(self.i + ahead).min(last)]
    }

    fn at(&self, tok: &Tok) -> bool {
        &self.peek().tok == tok
    }

    fn advance(&mut self) -> &'t Token {
        let token = self.peek();
        if token.tok != Tok::Eof {
            self.i += 1;
        }
        token
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        let found = self.at(tok);
        if found {
            self.advance();
        }
        found
    }

    fn unexpected(&self, expected: &str) -> Diagnostic {
        let found = self.peek();
        Diagnostic::new(
            code::SYNTAX,
            found.pos,
            format!("expected {expected}, found {}", found.tok.describe()),
        )
    }

    fn expect(&mut self, tok: &Tok, context: &str) -> Parsed<&'t Token> {
        if self.at(tok) {
            Ok(self.advance())
        } else {
            Err(self.unexpected(&format!("`{}` {context}", tok.text())))
        }
    }

    fn enter(&mut self, pos: Pos) -> Parsed<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            Err(Diagnostic::new(
                code::NESTING,
                pos,
                format!("the program nests deeper than {MAX_DEPTH} levels here"),
            )
            .with_help("bind intermediate values with `let`"))
        } else {
            Ok(())
        }
    }

    /// A statement list up to `}` (or the end, at top level), recovering
    /// from each broken statement at the next statement boundary.
    fn statements(&mut self, top_level: bool) -> Vec<Stmt> {
        let mut stmts = Vec::new();
        loop {
            match &self.peek().tok {
                Tok::Eof => break,
                Tok::RBrace if !top_level => break,
                _ => {}
            }
            let start = self.i;
            let depth = self.depth;
            match self.statement() {
                Ok(stmt) => stmts.push(stmt),
                Err(diagnostic) => {
                    self.diagnostics.push(diagnostic);
                    self.depth = depth;
                    self.synchronize(start);
                    if self.i == start {
                        self.advance();
                    }
                }
            }
        }
        stmts
    }

    /// Skip to the start of the next statement: past a `;`, or up to a
    /// statement keyword that begins a line, or the `}` closing this block.
    /// Braces the broken statement (from `start`) left open are its own, so
    /// their `}` is skipped too.
    fn synchronize(&mut self, start: usize) {
        let mut unclosed = self.tokens[start..self.i]
            .iter()
            .fold(0u32, |open, t| match t.tok {
                Tok::LBrace => open + 1,
                Tok::RBrace => open.saturating_sub(1),
                _ => open,
            });
        let mut nested = 0u32;
        loop {
            let token = self.peek();
            match token.tok {
                Tok::Eof => return,
                Tok::LBrace => nested += 1,
                Tok::RBrace if nested > 0 => nested -= 1,
                Tok::RBrace if unclosed > 0 => unclosed -= 1,
                Tok::RBrace => return,
                Tok::Semi if nested == 0 => {
                    self.advance();
                    return;
                }
                Tok::Let | Tok::If | Tok::For | Tok::Return
                    if nested == 0 && token.newline_before =>
                {
                    return
                }
                _ => {}
            }
            self.advance();
        }
    }

    fn block(&mut self, what: &str) -> Parsed<Vec<Stmt>> {
        let open = self.peek().pos;
        if !self.at(&Tok::LBrace) {
            let mut diagnostic = self.unexpected(&format!("`{{` to open {what}"));
            if self.at(&Tok::Assign) {
                diagnostic = diagnostic
                    .with_help("`=` binds a name; compare with `==`")
                    .with_fix("==");
            }
            return Err(diagnostic);
        }
        self.enter(open)?;
        self.advance();
        let body = self.statements(false);
        if !self.eat(&Tok::RBrace) {
            return Err(self.unexpected(&format!(
                "`}}` to close {what} opened at line {}:{}",
                open.line, open.column
            )));
        }
        self.depth -= 1;
        Ok(body)
    }

    fn end_statement(&mut self) -> Parsed<()> {
        let next = self.peek();
        match next.tok {
            Tok::Semi => {
                self.advance();
                Ok(())
            }
            Tok::RBrace | Tok::Eof => Ok(()),
            _ if next.newline_before => Ok(()),
            _ => Err(self.unexpected("a line break or `;` after the statement")),
        }
    }

    fn statement(&mut self) -> Parsed<Stmt> {
        let token = self.peek();
        match &token.tok {
            Tok::Let => {
                self.advance();
                let (name, pos) = self.binding_name("`let`")?;
                if self.at(&Tok::Colon) {
                    return Err(Diagnostic::new(
                        code::UNSUPPORTED,
                        self.peek().pos,
                        "type annotations are not written in Sift",
                    )
                    .with_fix(format!("let {name} = <value>")));
                }
                if !self.eat(&Tok::Assign) {
                    return Err(self
                        .unexpected(&format!("`=` after `let {name}`"))
                        .with_fix(format!("let {name} = <value>")));
                }
                let value = self.expr()?;
                self.end_statement()?;
                Ok(Stmt::Let { name, value, pos })
            }
            Tok::If => {
                self.advance();
                let mut branches = vec![(self.expr()?, self.block("the `if` body")?)];
                let mut otherwise = None;
                while self.eat(&Tok::Else) {
                    if self.eat(&Tok::If) {
                        branches.push((self.expr()?, self.block("the `else if` body")?));
                    } else {
                        otherwise = Some(self.block("the `else` body")?);
                        break;
                    }
                }
                Ok(Stmt::If {
                    branches,
                    otherwise,
                })
            }
            Tok::For => {
                self.advance();
                if self.at(&Tok::LParen) {
                    return Err(Diagnostic::new(
                        code::UNSUPPORTED,
                        self.peek().pos,
                        "`for` takes no parentheses and has one form",
                    )
                    .with_fix("for x in list { ... }"));
                }
                let (var, pos) = self.binding_name("`for`")?;
                if !self.eat(&Tok::In) {
                    return Err(self
                        .unexpected(&format!("`in` after `for {var}`"))
                        .with_fix(format!("for {var} in <list> {{ ... }}")));
                }
                let iterable = self.expr()?;
                let body = self.block("the `for` body")?;
                Ok(Stmt::For {
                    var,
                    iterable,
                    body,
                    pos,
                })
            }
            Tok::Return => {
                let pos = self.advance().pos;
                let next = self.peek();
                if matches!(next.tok, Tok::Semi | Tok::RBrace | Tok::Eof) || next.newline_before {
                    return Err(
                        Diagnostic::new(code::SYNTAX, next.pos, "`return` needs a value")
                            .with_fix("return null"),
                    );
                }
                let value = self.expr()?;
                self.end_statement()?;
                Ok(Stmt::Return { value, pos })
            }
            Tok::Ident(word) => {
                if let Some((_, help)) = FOREIGN_KEYWORDS.iter().find(|(w, _)| w == word) {
                    return Err(Diagnostic::new(
                        code::UNSUPPORTED,
                        token.pos,
                        format!("`{word}` is not in Sift"),
                    )
                    .with_help(*help));
                }
                let next = &self.nth(1).tok;
                let compound = matches!(next, Tok::Plus | Tok::Minus | Tok::Star | Tok::Slash)
                    && self.nth(2).tok == Tok::Assign;
                if *next == Tok::Assign || compound {
                    return Err(Diagnostic::new(
                        code::UNSUPPORTED,
                        token.pos,
                        format!("`{word}` cannot be reassigned; bindings are immutable"),
                    )
                    .with_help("bind the new value to a new name")
                    .with_fix(format!("let {word}_2 = <new value>")));
                }
                Err(self.not_a_statement())
            }
            _ => Err(self.not_a_statement()),
        }
    }

    fn not_a_statement(&self) -> Diagnostic {
        let diagnostic = self.unexpected("a statement (`let`, `if`, `for` or `return`)");
        if matches!(self.peek().tok, Tok::RBrace) {
            diagnostic.with_help("this `}` closes nothing")
        } else {
            diagnostic
                .with_help("a value on its own does nothing; bind it with `let` or `return` it")
        }
    }

    fn binding_name(&mut self, after: &str) -> Parsed<(String, Pos)> {
        let token = self.peek();
        match &token.tok {
            Tok::Ident(name) => {
                self.advance();
                Ok((name.clone(), token.pos))
            }
            tok if !tok.text().is_empty() && tok.text().chars().all(char::is_alphabetic) => {
                Err(Diagnostic::new(
                    code::SYNTAX,
                    token.pos,
                    format!("`{}` is a reserved word and cannot be a name", tok.text()),
                ))
            }
            _ => Err(self.unexpected(&format!("a name after {after}"))),
        }
    }

    fn expr(&mut self) -> Parsed<Expr> {
        let start = self.peek().pos;
        self.enter(start)?;
        let mut expr = self.binary(1)?;
        if self.eat(&Tok::Question) {
            let then = self.expr()?;
            self.expect(&Tok::Colon, "between the branches of `cond ? a : b`")?;
            let otherwise = self.expr()?;
            expr = Expr {
                pos: expr.pos,
                kind: ExprKind::Cond {
                    cond: Box::new(expr),
                    then: Box::new(then),
                    otherwise: Box::new(otherwise),
                },
            };
        }
        self.depth -= 1;
        Ok(expr)
    }

    fn binary(&mut self, min_precedence: u8) -> Parsed<Expr> {
        let base = self.depth;
        let mut left = self.unary()?;
        while let Some((op, precedence)) = binary_op(&self.peek().tok) {
            if precedence < min_precedence {
                break;
            }
            let pos = self.advance().pos;
            self.enter(pos)?;
            let right = self.binary(precedence + 1)?;
            left = Expr {
                kind: ExprKind::Binary {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                pos,
            };
        }
        self.depth = base;
        Ok(left)
    }

    fn unary(&mut self) -> Parsed<Expr> {
        let token = self.peek();
        let op = match token.tok {
            Tok::Minus => UnaryOp::Neg,
            Tok::Bang => UnaryOp::Not,
            _ => return self.postfix(),
        };
        self.advance();
        self.enter(token.pos)?;
        let operand = self.unary()?;
        self.depth -= 1;
        Ok(Expr {
            kind: ExprKind::Unary {
                op,
                operand: Box::new(operand),
            },
            pos: token.pos,
        })
    }

    fn postfix(&mut self) -> Parsed<Expr> {
        let base = self.depth;
        let mut expr = self.primary()?;
        loop {
            let token = self.peek();
            let optional = match token.tok {
                Tok::Dot => false,
                Tok::QuestionDot => true,
                Tok::LBracket if !token.newline_before => {
                    self.advance();
                    self.enter(token.pos)?;
                    let index = self.expr()?;
                    self.expect(&Tok::RBracket, "to close the index")?;
                    expr = Expr {
                        kind: ExprKind::Index {
                            object: Box::new(expr),
                            index: Box::new(index),
                        },
                        pos: token.pos,
                    };
                    continue;
                }
                Tok::LParen if !token.newline_before => {
                    return Err(Diagnostic::new(
                        code::SYNTAX,
                        token.pos,
                        "only functions called by name can be called",
                    )
                    .with_help("bind the function with `let f = x => ...` and call `f(...)`"));
                }
                _ => break,
            };
            self.advance();
            let field_token = self.peek();
            let Some(field) = ident_like(&field_token.tok) else {
                return Err(self.unexpected(&format!("a field name after `{}`", token.tok.text())));
            };
            self.advance();
            self.enter(field_token.pos)?;
            let next = self.peek();
            expr = if next.tok == Tok::LParen && !next.newline_before {
                let args = self.arguments()?;
                Expr {
                    kind: ExprKind::Method {
                        receiver: Box::new(expr),
                        name: field.to_string(),
                        args,
                        optional,
                    },
                    pos: field_token.pos,
                }
            } else {
                Expr {
                    kind: ExprKind::Member {
                        object: Box::new(expr),
                        field: field.to_string(),
                        optional,
                    },
                    pos: field_token.pos,
                }
            };
        }
        self.depth = base;
        Ok(expr)
    }

    fn arguments(&mut self) -> Parsed<Vec<Expr>> {
        let open = self.expect(&Tok::LParen, "to start the arguments")?.pos;
        self.comma_list(&Tok::RParen, "argument list", open)
    }

    /// Comma-separated expressions up to `close`, which it consumes. A
    /// trailing comma is allowed.
    fn comma_list(&mut self, close: &Tok, what: &str, open: Pos) -> Parsed<Vec<Expr>> {
        let mut items = Vec::new();
        while !self.eat(close) {
            items.push(self.expr()?);
            if !self.eat(&Tok::Comma) && !self.at(close) {
                return Err(self.unexpected(&format!(
                    "`,` or `{}` in the {what} opened at line {}:{}",
                    close.text(),
                    open.line,
                    open.column
                )));
            }
        }
        Ok(items)
    }

    fn primary(&mut self) -> Parsed<Expr> {
        let token = self.advance();
        let pos = token.pos;
        let kind = match &token.tok {
            Tok::Null => ExprKind::Null,
            Tok::True => ExprKind::Bool(true),
            Tok::False => ExprKind::Bool(false),
            Tok::Int(n) => ExprKind::Int(*n),
            Tok::Float(n) => ExprKind::Float(*n),
            Tok::Str(parts) => match parts.as_slice() {
                [Part::Text(text)] => ExprKind::Str(text.clone()),
                _ => ExprKind::Template(self.segments(parts)?),
            },
            Tok::Spread => {
                return Err(Diagnostic::new(
                    code::UNSUPPORTED,
                    pos,
                    "spread `...` works only inside a record, as `{ ...r, key: value }`",
                )
                .with_help("concatenate lists with `+`"))
            }
            Tok::LBracket => ExprKind::List(self.comma_list(&Tok::RBracket, "list", pos)?),
            Tok::LBrace => self.record(pos)?,
            Tok::LParen if self.lambda_ahead() => self.lambda(pos)?,
            Tok::LParen => {
                let inner = self.expr()?;
                self.expect(&Tok::RParen, "to close the parenthesis")?;
                return Ok(inner);
            }
            Tok::Ident(name) if self.at(&Tok::Arrow) => {
                self.advance();
                let body = self.lambda_body()?;
                ExprKind::Lambda(Arc::new(Lambda {
                    params: vec![name.clone()],
                    body,
                }))
            }
            Tok::Ident(name) if self.at(&Tok::LParen) && !self.peek().newline_before => {
                ExprKind::Call {
                    name: name.clone(),
                    args: self.arguments()?,
                }
            }
            Tok::Ident(name) => ExprKind::Name(name.clone()),
            Tok::Match => self.match_expr(pos)?,
            Tok::SqlKeyword => self.sql(pos)?,
            Tok::If => {
                return Err(Diagnostic::new(
                    code::UNSUPPORTED,
                    pos,
                    "`if` is a statement, not an expression",
                )
                .with_help("choose a value with `match cond { true => a, _ => b }`"))
            }
            _ => {
                self.i -= usize::from(token.tok != Tok::Eof);
                return Err(self.unexpected("an expression"));
            }
        };
        Ok(Expr { kind, pos })
    }

    fn segments(&mut self, parts: &[Part]) -> Parsed<Vec<Segment>> {
        parts
            .iter()
            .map(|part| match part {
                Part::Text(text) => Ok(Segment::Text(text.clone())),
                Part::Hole(tokens) => {
                    let mut inner = Parser::new(tokens, self.depth);
                    let expr = inner.expr();
                    let expr = expr.and_then(|expr| {
                        if inner.at(&Tok::Eof) {
                            Ok(expr)
                        } else {
                            Err(inner.unexpected("`}` to close the `${` hole"))
                        }
                    });
                    self.diagnostics.append(&mut inner.diagnostics);
                    expr.map(Segment::Hole)
                }
            })
            .collect()
    }

    /// After `(`: is this the parameter list of a lambda?
    fn lambda_ahead(&self) -> bool {
        let mut ahead = 0;
        if self.nth(0).tok != Tok::RParen {
            loop {
                if !matches!(self.nth(ahead).tok, Tok::Ident(_)) {
                    return false;
                }
                ahead += 1;
                match self.nth(ahead).tok {
                    Tok::Comma => ahead += 1,
                    Tok::RParen => break,
                    _ => return false,
                }
            }
        }
        self.nth(ahead + 1).tok == Tok::Arrow
    }

    fn lambda(&mut self, open: Pos) -> Parsed<ExprKind> {
        let mut params: Vec<String> = Vec::new();
        while let Tok::Ident(name) = &self.peek().tok {
            let token = self.advance();
            if params.contains(name) {
                self.diagnostics.push(Diagnostic::new(
                    code::LET_REBIND,
                    token.pos,
                    format!("parameter `{name}` is named twice"),
                ));
            }
            params.push(name.clone());
            self.eat(&Tok::Comma);
        }
        self.expect(
            &Tok::RParen,
            &format!(
                "to close the parameters opened at line {}:{}",
                open.line, open.column
            ),
        )?;
        self.expect(&Tok::Arrow, "after the parameters")?;
        let body = self.lambda_body()?;
        Ok(ExprKind::Lambda(Arc::new(Lambda { params, body })))
    }

    /// After `=>`: a value, or `{ statements }` ending in `return`. A `{`
    /// opening a statement is a body; any other `{` opens a record.
    fn lambda_body(&mut self) -> Parsed<Body> {
        let statement = matches!(self.nth(1).tok, Tok::Let | Tok::If | Tok::For | Tok::Return)
            && self.nth(2).tok != Tok::Colon;
        if !(self.at(&Tok::LBrace) && statement) {
            return Ok(Body::Expr(self.expr()?));
        }
        let before = self.diagnostics.len();
        let body = self.block("the function body")?;
        let end = self.tokens[self.i - 1].pos;
        let clean = self.diagnostics.len() == before;
        self.diagnostics.extend(check::function(&body, end, clean));
        Ok(Body::Block(body))
    }

    fn record(&mut self, open: Pos) -> Parsed<ExprKind> {
        let mut fields: Vec<Field> = Vec::new();
        while !self.eat(&Tok::RBrace) {
            if self.eat(&Tok::Spread) {
                fields.push(Field::Spread(self.expr()?));
                self.record_separator(open)?;
                continue;
            }
            let token = self.advance();
            let (key, shorthand) = match &token.tok {
                Tok::Ident(name) => (name.clone(), true),
                Tok::Str(parts) => match parts.as_slice() {
                    [Part::Text(text)] => (text.clone(), false),
                    _ => {
                        return Err(Diagnostic::new(
                            code::SYNTAX,
                            token.pos,
                            "a record key cannot be interpolated",
                        ))
                    }
                },
                tok => match ident_like(tok) {
                    Some(word) => (word.to_string(), false),
                    None => {
                        self.i -= usize::from(token.tok != Tok::Eof);
                        return Err(self.unexpected(&format!(
                            "a field name or `}}` in the record opened at line {}:{}",
                            open.line, open.column
                        )));
                    }
                },
            };
            let value = if self.eat(&Tok::Colon) {
                self.expr()?
            } else if shorthand {
                Expr {
                    kind: ExprKind::Name(key.clone()),
                    pos: token.pos,
                }
            } else {
                return Err(self.unexpected(&format!("`:` after the key `{key}`")));
            };
            let repeated = fields
                .iter()
                .any(|field| matches!(field, Field::Pair(existing, _) if *existing == key));
            if repeated {
                self.diagnostics.push(Diagnostic::new(
                    code::DUPLICATE_KEY,
                    token.pos,
                    format!("the record already has a field `{key}`"),
                ));
            }
            fields.push(Field::Pair(key, value));
            self.record_separator(open)?;
        }
        Ok(ExprKind::Record(fields))
    }

    /// The `,` after a record entry, or the record's closing `}` ahead.
    fn record_separator(&mut self, open: Pos) -> Parsed<()> {
        if self.eat(&Tok::Comma) || self.at(&Tok::RBrace) {
            return Ok(());
        }
        let diagnostic = self.unexpected(&format!(
            "`,` or `}}` in the record opened at line {}:{}",
            open.line, open.column
        ));
        Err(if self.peek().newline_before {
            diagnostic.with_fix("add `,` after the previous field")
        } else {
            diagnostic
        })
    }

    fn match_expr(&mut self, pos: Pos) -> Parsed<ExprKind> {
        let scrutinee = self.expr()?;
        let open = self.expect(&Tok::LBrace, "to open the `match` arms")?.pos;
        let mut arms = Vec::new();
        let mut default = None;
        while !self.eat(&Tok::RBrace) {
            let arm_start = self.peek().pos;
            let mut patterns = Vec::new();
            let mut wildcard = false;
            loop {
                match self.pattern()? {
                    Some(pattern) => patterns.push(pattern),
                    None => wildcard = true,
                }
                if !self.eat(&Tok::Pipe) {
                    break;
                }
            }
            self.expect(&Tok::Arrow, "after the `match` pattern")?;
            let value = self.expr()?;
            if default.is_some() {
                self.diagnostics.push(
                    Diagnostic::new(
                        code::MATCH_DEFAULT,
                        arm_start,
                        "this arm follows `_` and never matches",
                    )
                    .with_help("`_` must be the last arm"),
                );
            } else if wildcard {
                if !patterns.is_empty() {
                    self.diagnostics.push(Diagnostic::new(
                        code::MATCH_DEFAULT,
                        arm_start,
                        "`_` must stand alone in its arm",
                    ));
                }
                default = Some(value);
            } else {
                arms.push(Arm { patterns, value });
            }
            if !self.eat(&Tok::Comma) && !self.at(&Tok::RBrace) {
                return Err(self.unexpected(&format!(
                    "`,` or `}}` in the `match` opened at line {}:{}",
                    open.line, open.column
                )));
            }
        }
        let default = default.unwrap_or_else(|| {
            self.diagnostics.push(
                Diagnostic::new(
                    code::MATCH_DEFAULT,
                    pos,
                    "`match` needs a final `_ => ...` arm",
                )
                .with_fix("add `_ => null` as the last arm"),
            );
            Expr {
                kind: ExprKind::Null,
                pos,
            }
        });
        Ok(ExprKind::Match {
            scrutinee: Box::new(scrutinee),
            arms,
            default: Box::new(default),
        })
    }

    /// One literal pattern, or None for `_`.
    fn pattern(&mut self) -> Parsed<Option<Pattern>> {
        let token = self.advance();
        Ok(Some(match &token.tok {
            Tok::Ident(name) if name == "_" => return Ok(None),
            Tok::Str(parts) => match parts.as_slice() {
                [Part::Text(text)] => Pattern::Str(text.clone()),
                _ => {
                    return Err(Diagnostic::new(
                        code::SYNTAX,
                        token.pos,
                        "a `match` pattern cannot be interpolated",
                    ))
                }
            },
            Tok::Int(n) => Pattern::Int(*n),
            Tok::Minus => match self.advance().tok {
                Tok::Int(n) => Pattern::Int(-n),
                _ => return Err(self.unexpected("an integer after `-`")),
            },
            Tok::True => Pattern::Bool(true),
            Tok::False => Pattern::Bool(false),
            _ => {
                self.i -= usize::from(token.tok != Tok::Eof);
                return Err(self
                    .unexpected("a string, integer or bool pattern, or `_`")
                    .with_help("patterns are literals; `_` matches anything else"));
            }
        }))
    }

    fn sql(&mut self, pos: Pos) -> Parsed<ExprKind> {
        self.expect(&Tok::LParen, "after `sql`: `sql(db) { ... }`")?;
        let db = self.expr()?;
        self.expect(&Tok::RParen, "to close `sql(`")?;
        let token = self.peek();
        let Tok::Sql(parts) = &token.tok else {
            return Err(self
                .unexpected("`{` and the SQL query after `sql(db)`")
                .with_fix("sql(db) { SELECT ... }"));
        };
        self.advance();
        let parts = self.segments(parts)?;
        if parts
            .iter()
            .all(|p| matches!(p, Segment::Text(t) if t.trim().is_empty()))
        {
            return Err(Diagnostic::new(code::SYNTAX, pos, "the SQL block is empty"));
        }
        Ok(ExprKind::Sql {
            db: Box::new(db),
            parts,
        })
    }
}

fn binary_op(tok: &Tok) -> Option<(BinaryOp, u8)> {
    Some(match tok {
        Tok::QuestionQuestion => (BinaryOp::Coalesce, 1),
        Tok::OrOr => (BinaryOp::Or, 2),
        Tok::AndAnd => (BinaryOp::And, 3),
        Tok::EqEq => (BinaryOp::Eq, 4),
        Tok::NotEq => (BinaryOp::Ne, 4),
        Tok::Lt => (BinaryOp::Lt, 5),
        Tok::Le => (BinaryOp::Le, 5),
        Tok::Gt => (BinaryOp::Gt, 5),
        Tok::Ge => (BinaryOp::Ge, 5),
        Tok::Plus => (BinaryOp::Add, 6),
        Tok::Minus => (BinaryOp::Sub, 6),
        Tok::Star => (BinaryOp::Mul, 7),
        Tok::Slash => (BinaryOp::Div, 7),
        Tok::Percent => (BinaryOp::Rem, 7),
        _ => return None,
    })
}

/// Names usable as record keys and fields, including keywords (`r.in`).
fn ident_like(tok: &Tok) -> Option<&str> {
    match tok {
        Tok::Ident(name) => Some(name),
        other => {
            let text = other.text();
            (!text.is_empty() && text.chars().all(char::is_alphabetic)).then_some(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(source: &str) -> Vec<Stmt> {
        parse(source).unwrap_or_else(|d| panic!("{source}\n{d:#?}"))
    }

    fn errors(source: &str) -> Vec<Diagnostic> {
        match parse(source) {
            Ok(_) => panic!("parsed: {source}"),
            Err(diagnostics) => diagnostics,
        }
    }

    fn returned(source: &str) -> Expr {
        match ok(source).pop() {
            Some(Stmt::Return { value, .. }) => value,
            other => panic!("{other:?}"),
        }
    }

    fn at(d: &Diagnostic) -> (u32, u32) {
        (d.line, d.column)
    }

    #[test]
    fn precedence() {
        let e = returned("return a ?? b || c && d == e < f + g * -h");
        let ExprKind::Binary { op, right, .. } = e.kind else {
            panic!()
        };
        assert_eq!(op, BinaryOp::Coalesce);
        let ExprKind::Binary { op, right, .. } = right.kind else {
            panic!()
        };
        assert_eq!(op, BinaryOp::Or);
        let ExprKind::Binary { op, right, .. } = right.kind else {
            panic!()
        };
        assert_eq!(op, BinaryOp::And);
        let ExprKind::Binary { op, right, .. } = right.kind else {
            panic!()
        };
        assert_eq!(op, BinaryOp::Eq);
        let ExprKind::Binary { op, right, .. } = right.kind else {
            panic!()
        };
        assert_eq!(op, BinaryOp::Lt);
        let ExprKind::Binary { op, right, .. } = right.kind else {
            panic!()
        };
        assert_eq!(op, BinaryOp::Add);
        let ExprKind::Binary { op, right, .. } = right.kind else {
            panic!()
        };
        assert_eq!(op, BinaryOp::Mul);
        assert!(matches!(
            right.kind,
            ExprKind::Unary {
                op: UnaryOp::Neg,
                ..
            }
        ));
    }

    #[test]
    fn left_associative() {
        let e = returned("return 1 - 2 - 3");
        let ExprKind::Binary { left, right, .. } = e.kind else {
            panic!()
        };
        assert!(matches!(left.kind, ExprKind::Binary { .. }));
        assert!(matches!(right.kind, ExprKind::Int(3)));
    }

    #[test]
    fn every_construct_parses() {
        ok(r#"
            let a = null; let b = true; let c = false
            let d = [1, 2.5, "s", [],]
            let e = { x: 1, "y z": 2, d, in: 3, }
            let f = e.x + e?.x + d[0] + e["y z"]
            let g = d.map(x => x * 2).filter((x) => x > 1)
            let h = ((a, b) => a + b)
            let i = () => 1
            let j = "n=${f} and ${ e["y z"] }"
            let k = match j { "a" | "b" => 1, 3 => 2, -4 => 3, true => 4, _ => 5 }
            let l = sql(e.x) { SELECT ${a} }
            if b { let m = 1 } else if c { let m = 2 } else { let m = 3 }
            for x in d { if x == 1 { return x } }
            return !b
        "#);
    }

    #[test]
    fn accepted_deviations() {
        ok("const a = 1;\nvar b = a === 1;\nlet c = a !== 2;\nreturn [a, b, c];");
    }

    #[test]
    fn member_call_and_lambda_shapes() {
        let e = returned("return xs?.map(x => x.a)");
        let ExprKind::Method {
            name,
            args,
            optional,
            ..
        } = e.kind
        else {
            panic!()
        };
        assert_eq!(name, "map");
        assert!(optional);
        assert!(matches!(&args[0].kind, ExprKind::Lambda(l) if l.params == ["x"]));
        let e = returned("return f((a, b) => a, (c))");
        let ExprKind::Call { args, .. } = e.kind else {
            panic!()
        };
        assert!(matches!(&args[0].kind, ExprKind::Lambda(l) if l.params == ["a", "b"]));
        assert!(matches!(&args[1].kind, ExprKind::Name(n) if n == "c"));
    }

    #[test]
    fn record_shorthand_and_keyword_keys() {
        let e = returned("return { d, match: 1, \"a b\": 2 }");
        let ExprKind::Record(fields) = e.kind else {
            panic!()
        };
        let pairs: Vec<_> = fields
            .iter()
            .map(|field| match field {
                Field::Pair(k, v) => (k.as_str(), v),
                Field::Spread(_) => panic!("no spread here"),
            })
            .collect();
        let keys: Vec<_> = pairs.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, ["d", "match", "a b"]);
        assert!(matches!(&pairs[0].1.kind, ExprKind::Name(n) if n == "d"));
    }

    #[test]
    fn a_brace_after_an_arrow_opens_a_body_only_before_a_statement() {
        ok("let f = x => { let y = x * 2\n return y }\nreturn f(1)");
        ok("let f = (a, b) => { if a > b { return a }\n return b }\nreturn f(1, 2)");
        let e = returned("return x => { return: 1, let: 2 }");
        let ExprKind::Lambda(lambda) = e.kind else {
            panic!()
        };
        assert!(matches!(
            &lambda.body,
            Body::Expr(Expr {
                kind: ExprKind::Record(_),
                ..
            })
        ));
    }

    #[test]
    fn recovers_to_report_independent_errors() {
        let d = errors("let a = (1 + \nlet b = 2\nlet c 3\nreturn a");
        assert_eq!(d.len(), 2, "{d:#?}");
        assert_eq!(at(&d[0]), (2, 1));
        assert!(d[0].message.contains("found `let`"), "{}", d[0].message);
        assert_eq!(at(&d[1]), (3, 7));
        assert_eq!(d[1].message, "expected `=` after `let c`, found number `3`");
        assert_eq!(d[1].fix.as_deref(), Some("let c = <value>"));
    }

    #[test]
    fn recovers_inside_blocks() {
        let d = errors("if x {\n  let = 1\n  let y = [1 2]\n}\nlet z = }\nreturn 1");
        assert_eq!(d.len(), 3, "{d:#?}");
        assert_eq!(at(&d[0]), (2, 7));
        assert_eq!(at(&d[1]), (3, 14));
        assert!(d[1]
            .message
            .starts_with("expected `,` or `]` in the list opened at line 3:11"));
        assert_eq!(at(&d[2]), (5, 9));
    }

    #[test]
    fn caps_at_five() {
        let d = errors("let a =\nlet b =\nlet c =\nlet d =\nlet e =\nlet f =\nreturn 1");
        assert_eq!(d.len(), 5);
        assert_eq!(at(&d[4]), (6, 1));
    }

    #[test]
    fn lexer_and_parser_errors_merge_in_order() {
        let d = errors("let a = 'x'\nlet b c\nreturn a");
        assert_eq!(d.iter().map(at).collect::<Vec<_>>(), [(1, 9), (2, 7)]);
    }

    #[test]
    fn statement_end_is_required() {
        let d = errors("let a = b c\nreturn a");
        assert_eq!(at(&d[0]), (1, 11));
        assert_eq!(
            d[0].message,
            "expected a line break or `;` after the statement, found identifier `c`"
        );
    }

    #[test]
    fn helpful_rejections() {
        let cases: &[(&str, &str, (u32, u32))] = &[
            ("while x { }\nreturn 1", code::UNSUPPORTED, (1, 1)),
            ("x = 2\nreturn 1", code::UNSUPPORTED, (1, 1)),
            ("x += 2\nreturn 1", code::UNSUPPORTED, (1, 1)),
            ("return a ? b", code::SYNTAX, (1, 13)),
            ("return [...xs]", code::UNSUPPORTED, (1, 9)),
            (
                "let f = x => { let y = x }\nreturn f(1)",
                code::RETURN_MISSING,
                (1, 26),
            ),
            (
                "let f = x => { return x\nlet y = 1 }\nreturn 1",
                code::RETURN_NOT_LAST,
                (1, 16),
            ),
            (
                "let a = if b { 1 } else { 2 }\nreturn a",
                code::UNSUPPORTED,
                (1, 9),
            ),
            ("for (x of xs) { }\nreturn 1", code::UNSUPPORTED, (1, 5)),
            ("for x of xs { }\nreturn 1", code::SYNTAX, (1, 7)),
            ("if a = 1 { }\nreturn 1", code::SYNTAX, (1, 6)),
            ("let x: int = 1\nreturn x", code::UNSUPPORTED, (1, 6)),
            ("return", code::SYNTAX, (1, 7)),
            ("let in = 1\nreturn 1", code::SYNTAX, (1, 5)),
            ("return f(1)(2)", code::SYNTAX, (1, 12)),
            ("return sql(db) + 1", code::SYNTAX, (1, 16)),
            ("return match x { \"a\" => 1 }", code::MATCH_DEFAULT, (1, 8)),
            (
                "return match x { _ => 1, \"a\" => 2 }",
                code::MATCH_DEFAULT,
                (1, 26),
            ),
            ("return { a: 1, a: 2 }", code::DUPLICATE_KEY, (1, 16)),
            ("return (a, a) => a", code::LET_REBIND, (1, 12)),
            ("return {\n a: 1\n b: 2 }", code::SYNTAX, (3, 2)),
            ("return \"${a b}\"", code::SYNTAX, (1, 13)),
        ];
        for (source, expected, pos) in cases {
            let d = errors(source);
            assert_eq!(
                (d[0].code, at(&d[0])),
                (*expected, *pos),
                "{source}: {d:#?}"
            );
        }
        assert_eq!(
            errors("if a = 1 { }\nreturn 1")[0].fix.as_deref(),
            Some("==")
        );
        assert_eq!(
            errors("return {\n a: 1\n b: 2 }")[0].fix.as_deref(),
            Some("add `,` after the previous field")
        );
    }

    #[test]
    fn nesting_is_bounded() {
        let deep = format!("return {}1{}", "(".repeat(200), ")".repeat(200));
        assert_eq!(errors(&deep)[0].code, code::NESTING);
        let chain = format!("return a{}", ".b".repeat(200));
        assert_eq!(errors(&chain)[0].code, code::NESTING);
        let sum = format!("return 1{}", " + 1".repeat(200));
        assert_eq!(errors(&sum)[0].code, code::NESTING);
        ok(&format!("return 1{}", " + 1".repeat(50)));
    }

    #[test]
    fn newline_ends_a_call_or_index() {
        let d = errors("let a = b\n(1)\nreturn a");
        assert_eq!(at(&d[0]), (2, 1));
    }
}
