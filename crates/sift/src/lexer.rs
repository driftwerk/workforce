//! Source text to tokens.
//!
//! String interpolation holes and the holes of `sql(db) { ... }` blocks are
//! lexed here, into nested token lists the parser reads as expressions. The
//! SQL between the braces is kept verbatim. Lexing never stops at an error:
//! it reports it and carries on so the parser can report independent errors.

use crate::diagnostic::{code, Diagnostic, Pos};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    Ident(String),
    Int(i64),
    Float(f64),
    Str(Vec<Part>),
    Sql(Vec<Part>),
    Let,
    If,
    Else,
    For,
    In,
    Match,
    Return,
    True,
    False,
    Null,
    SqlKeyword,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Colon,
    Semi,
    Dot,
    Spread,
    QuestionDot,
    Question,
    QuestionQuestion,
    Arrow,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Bang,
    Assign,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    OrOr,
    Pipe,
    Eof,
}

/// A piece of a string literal or SQL block: literal text, or the tokens of
/// one `${...}` hole ending in an `Eof` at the hole's closing brace.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Part {
    Text(String),
    Hole(Vec<Token>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Token {
    pub tok: Tok,
    pub pos: Pos,
    /// A line break separates this token from the one before it.
    pub newline_before: bool,
}

impl Tok {
    /// How the token reads in a diagnostic's "found ...".
    pub(crate) fn describe(&self) -> String {
        match self {
            Tok::Ident(name) => format!("identifier `{name}`"),
            Tok::Int(n) => format!("number `{n}`"),
            Tok::Float(n) => format!("number `{n}`"),
            Tok::Str(_) => "a string".to_string(),
            Tok::Sql(_) => "a SQL block".to_string(),
            Tok::Eof => "the end of the program".to_string(),
            other => format!("`{}`", other.text()),
        }
    }

    /// The source spelling of keywords and punctuation.
    pub(crate) fn text(&self) -> &'static str {
        match self {
            Tok::Let => "let",
            Tok::If => "if",
            Tok::Else => "else",
            Tok::For => "for",
            Tok::In => "in",
            Tok::Match => "match",
            Tok::Return => "return",
            Tok::True => "true",
            Tok::False => "false",
            Tok::Null => "null",
            Tok::SqlKeyword => "sql",
            Tok::LParen => "(",
            Tok::RParen => ")",
            Tok::LBrace => "{",
            Tok::RBrace => "}",
            Tok::LBracket => "[",
            Tok::RBracket => "]",
            Tok::Comma => ",",
            Tok::Colon => ":",
            Tok::Semi => ";",
            Tok::Dot => ".",
            Tok::Spread => "...",
            Tok::QuestionDot => "?.",
            Tok::Question => "?",
            Tok::QuestionQuestion => "??",
            Tok::Arrow => "=>",
            Tok::Plus => "+",
            Tok::Minus => "-",
            Tok::Star => "*",
            Tok::Slash => "/",
            Tok::Percent => "%",
            Tok::Bang => "!",
            Tok::Assign => "=",
            Tok::EqEq => "==",
            Tok::NotEq => "!=",
            Tok::Lt => "<",
            Tok::Le => "<=",
            Tok::Gt => ">",
            Tok::Ge => ">=",
            Tok::AndAnd => "&&",
            Tok::OrOr => "||",
            Tok::Pipe => "|",
            Tok::Ident(_) | Tok::Int(_) | Tok::Float(_) | Tok::Str(_) | Tok::Sql(_) | Tok::Eof => {
                ""
            }
        }
    }
}

/// Lex a whole program. The token list always ends with `Eof`.
pub(crate) fn lex(source: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    let chars: Vec<char> = source.chars().collect();
    let mut lexer = Lexer::new(&chars, 0, chars.len(), Pos::START);
    lexer.run();
    (lexer.tokens, lexer.diagnostics)
}

/// Tracks `sql ( ... )` so the brace that follows is read as raw SQL.
#[derive(Clone, Copy)]
enum SqlWatch {
    Idle,
    AwaitParen,
    Depth(u32),
}

struct Lexer<'a> {
    chars: &'a [char],
    i: usize,
    end: usize,
    line: u32,
    column: u32,
    newline: bool,
    watch: SqlWatch,
    tokens: Vec<Token>,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Lexer<'a> {
    fn new(chars: &'a [char], start: usize, end: usize, pos: Pos) -> Self {
        Self {
            chars,
            i: start,
            end,
            line: pos.line,
            column: pos.column,
            newline: false,
            watch: SqlWatch::Idle,
            tokens: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    fn peek(&self, ahead: usize) -> Option<char> {
        let at = self.i + ahead;
        (at < self.end).then(|| self.chars[at])
    }

    fn pos(&self) -> Pos {
        Pos {
            line: self.line,
            column: self.column,
        }
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek(0)?;
        self.i += 1;
        if c == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(c)
    }

    fn bump_to(&mut self, index: usize) {
        while self.i < index && self.bump().is_some() {}
    }

    fn report(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }

    fn run(&mut self) {
        loop {
            self.skip_trivia();
            let pos = self.pos();
            let Some(c) = self.peek(0) else {
                self.push(Tok::Eof, pos);
                return;
            };
            if let Some(tok) = self.token(c, pos) {
                let closes_sql_head = self.push(tok, pos);
                if closes_sql_head {
                    self.sql_body();
                }
            }
        }
    }

    /// Record a token; true when it is the `)` that ends `sql(...)`.
    fn push(&mut self, tok: Tok, pos: Pos) -> bool {
        let mut closes_sql_head = false;
        self.watch = match (self.watch, &tok) {
            (_, Tok::SqlKeyword) => SqlWatch::AwaitParen,
            (SqlWatch::AwaitParen, Tok::LParen) => SqlWatch::Depth(1),
            (SqlWatch::Depth(d), Tok::LParen) => SqlWatch::Depth(d + 1),
            (SqlWatch::Depth(1), Tok::RParen) => {
                closes_sql_head = true;
                SqlWatch::Idle
            }
            (SqlWatch::Depth(d), Tok::RParen) => SqlWatch::Depth(d - 1),
            (SqlWatch::Depth(d), _) => SqlWatch::Depth(d),
            (SqlWatch::AwaitParen | SqlWatch::Idle, _) => SqlWatch::Idle,
        };
        self.tokens.push(Token {
            tok,
            pos,
            newline_before: std::mem::take(&mut self.newline),
        });
        closes_sql_head
    }

    fn skip_trivia(&mut self) {
        while let Some(c) = self.peek(0) {
            match c {
                '\n' => {
                    self.newline = true;
                    self.bump();
                }
                c if c.is_whitespace() => {
                    self.bump();
                }
                '/' if self.peek(1) == Some('/') => {
                    while self.peek(0).is_some_and(|c| c != '\n') {
                        self.bump();
                    }
                }
                _ => return,
            }
        }
    }

    fn token(&mut self, c: char, pos: Pos) -> Option<Tok> {
        if c.is_ascii_alphabetic() || c == '_' {
            return Some(self.word());
        }
        if c.is_ascii_digit() {
            return Some(self.number(pos));
        }
        match c {
            '"' => return Some(self.string(pos)),
            '\'' => return Some(self.misquoted(pos, '\'', "strings use double quotes")),
            '`' => {
                return Some(self.misquoted(
                    pos,
                    '`',
                    "template literals are written with double quotes; `${}` works inside them",
                ))
            }
            _ => {}
        }
        self.bump();
        let next = self.peek(0);
        let tok = match c {
            '(' => Tok::LParen,
            ')' => Tok::RParen,
            '{' => Tok::LBrace,
            '}' => Tok::RBrace,
            '[' => Tok::LBracket,
            ']' => Tok::RBracket,
            ',' => Tok::Comma,
            ':' => Tok::Colon,
            ';' => Tok::Semi,
            '+' => Tok::Plus,
            '-' => Tok::Minus,
            '*' => Tok::Star,
            '/' => Tok::Slash,
            '%' => Tok::Percent,
            '.' if next == Some('.') && self.peek(1) == Some('.') => {
                self.bump();
                self.bump();
                Tok::Spread
            }
            '.' => Tok::Dot,
            '?' => match next {
                Some('.') if !self.peek(1).is_some_and(|c| c.is_ascii_digit()) => {
                    self.bump();
                    Tok::QuestionDot
                }
                Some('?') => {
                    self.bump();
                    Tok::QuestionQuestion
                }
                _ => Tok::Question,
            },
            '=' => match next {
                Some('=') => {
                    self.bump();
                    if self.peek(0) == Some('=') {
                        self.bump();
                    }
                    Tok::EqEq
                }
                Some('>') => {
                    self.bump();
                    Tok::Arrow
                }
                _ => Tok::Assign,
            },
            '!' => match next {
                Some('=') => {
                    self.bump();
                    if self.peek(0) == Some('=') {
                        self.bump();
                    }
                    Tok::NotEq
                }
                _ => Tok::Bang,
            },
            '<' | '>' => {
                let or_equal = next == Some('=');
                if or_equal {
                    self.bump();
                }
                match (c, or_equal) {
                    ('<', false) => Tok::Lt,
                    ('<', true) => Tok::Le,
                    (_, false) => Tok::Gt,
                    (_, true) => Tok::Ge,
                }
            }
            '&' => {
                if next == Some('&') {
                    self.bump();
                } else {
                    self.report(
                        Diagnostic::new(code::LEX, pos, "`&` is not an operator").with_fix("`&&`"),
                    );
                }
                Tok::AndAnd
            }
            '|' if next == Some('|') => {
                self.bump();
                Tok::OrOr
            }
            '|' => Tok::Pipe,
            other => {
                self.report(Diagnostic::new(
                    code::LEX,
                    pos,
                    format!("unexpected character `{other}`"),
                ));
                return None;
            }
        };
        Some(tok)
    }

    fn word(&mut self) -> Tok {
        let mut word = String::new();
        while let Some(c) = self
            .peek(0)
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        {
            word.push(c);
            self.bump();
        }
        match word.as_str() {
            "let" | "const" | "var" => Tok::Let,
            "if" => Tok::If,
            "else" => Tok::Else,
            "for" => Tok::For,
            "in" => Tok::In,
            "match" => Tok::Match,
            "return" => Tok::Return,
            "true" => Tok::True,
            "false" => Tok::False,
            "null" => Tok::Null,
            "sql" => Tok::SqlKeyword,
            _ => Tok::Ident(word),
        }
    }

    fn number(&mut self, pos: Pos) -> Tok {
        let digits = |lexer: &mut Self, text: &mut String| {
            while let Some(c) = lexer.peek(0).filter(char::is_ascii_digit) {
                text.push(c);
                lexer.bump();
            }
        };
        let mut text = String::new();
        digits(self, &mut text);
        let mut float = false;
        if self.peek(0) == Some('.') && self.peek(1).is_some_and(|c| c.is_ascii_digit()) {
            float = true;
            text.push('.');
            self.bump();
            digits(self, &mut text);
        }
        if matches!(self.peek(0), Some('e' | 'E')) {
            let signed = matches!(self.peek(1), Some('+' | '-'));
            let first_digit = self.peek(if signed { 2 } else { 1 });
            if first_digit.is_some_and(|c| c.is_ascii_digit()) {
                float = true;
                text.push('e');
                self.bump();
                if signed {
                    text.extend(self.bump());
                }
                digits(self, &mut text);
            }
        }
        if float {
            match text.parse::<f64>() {
                Ok(n) if n.is_finite() => Tok::Float(n),
                _ => {
                    self.report(Diagnostic::new(
                        code::LEX,
                        pos,
                        format!("number `{text}` is out of range"),
                    ));
                    Tok::Float(0.0)
                }
            }
        } else {
            text.parse::<i64>().map(Tok::Int).unwrap_or_else(|_| {
                self.report(
                    Diagnostic::new(
                        code::LEX,
                        pos,
                        format!("integer `{text}` does not fit in a 64-bit int"),
                    )
                    .with_help("write it as a float, e.g. `1.0e20`"),
                );
                Tok::Int(0)
            })
        }
    }

    fn string(&mut self, start: Pos) -> Tok {
        self.bump();
        let mut parts = Vec::new();
        let mut text = String::new();
        loop {
            let pos = self.pos();
            match self.peek(0) {
                None | Some('\n') => {
                    self.report(
                        Diagnostic::new(code::LEX, start, "unterminated string").with_help(
                            "close it with `\"` on the same line; write a line break as `\\n`",
                        ),
                    );
                    break;
                }
                Some('"') => {
                    self.bump();
                    break;
                }
                Some('\\') => {
                    self.bump();
                    match self.bump() {
                        Some('"') => text.push('"'),
                        Some('\\') => text.push('\\'),
                        Some('n') => text.push('\n'),
                        Some('t') => text.push('\t'),
                        Some('$') => text.push('$'),
                        other => {
                            let shown = other.map(String::from).unwrap_or_default();
                            self.report(
                                Diagnostic::new(
                                    code::LEX,
                                    pos,
                                    format!("unknown escape `\\{shown}`"),
                                )
                                .with_help(
                                    "the escapes are `\\\"`, `\\\\`, `\\n`, `\\t` and `\\$`",
                                ),
                            );
                        }
                    }
                }
                Some('$') if self.peek(1) == Some('{') => {
                    if !text.is_empty() {
                        parts.push(Part::Text(std::mem::take(&mut text)));
                    }
                    match self.hole(pos) {
                        Some(hole) => parts.push(hole),
                        None => break,
                    }
                }
                Some(c) => {
                    text.push(c);
                    self.bump();
                }
            }
        }
        if !text.is_empty() || parts.is_empty() {
            parts.push(Part::Text(text));
        }
        Tok::Str(parts)
    }

    /// A string in `'...'` or `` `...` ``: report it with the fixed spelling
    /// and lex its content as plain text so parsing continues.
    fn misquoted(&mut self, start: Pos, quote: char, message: &str) -> Tok {
        self.bump();
        let mut text = String::new();
        while let Some(c) = self.peek(0) {
            if c == quote || (c == '\n' && quote == '\'') {
                break;
            }
            text.push(c);
            self.bump();
        }
        if self.peek(0) == Some(quote) {
            self.bump();
        }
        let fixed = text
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n");
        self.report(Diagnostic::new(code::LEX, start, message).with_fix(format!("\"{fixed}\"")));
        Tok::Str(vec![Part::Text(text)])
    }

    /// Lex `${ ... }` starting at the `$`. None when it never closes; the rest
    /// of the line is then skipped.
    fn hole(&mut self, dollar: Pos) -> Option<Part> {
        let open = self.i + 2;
        let Some(close) = scan_hole_end(self.chars, open, self.end) else {
            self.report(
                Diagnostic::new(code::LEX, dollar, "`${` is never closed")
                    .with_fix("close the hole with `}` on the same line"),
            );
            while self.peek(0).is_some_and(|c| c != '\n') {
                self.bump();
            }
            return None;
        };
        self.bump();
        self.bump();
        let mut inner = Lexer::new(self.chars, open, close, self.pos());
        inner.run();
        self.diagnostics.append(&mut inner.diagnostics);
        self.bump_to(close + 1);
        Some(Part::Hole(inner.tokens))
    }

    /// After `sql(...)`: when a `{` follows, lex the raw SQL up to its
    /// balancing `}` as one token.
    fn sql_body(&mut self) {
        let saved = (self.i, self.line, self.column, self.newline);
        self.skip_trivia();
        if self.peek(0) != Some('{') {
            (self.i, self.line, self.column, self.newline) = saved;
            return;
        }
        let open = self.pos();
        self.bump();
        let mut parts = Vec::new();
        let mut text = String::new();
        let mut depth = 1u32;
        loop {
            let pos = self.pos();
            let Some(c) = self.peek(0) else {
                self.report(
                    Diagnostic::new(code::LEX, open, "the SQL block is never closed")
                        .with_help("end it with a `}` that balances the `{` after `sql(db)`"),
                );
                break;
            };
            match c {
                '\'' | '"' => self.sql_quoted(c, &mut text),
                '-' if self.peek(1) == Some('-') => {
                    while let Some(c) = self.peek(0).filter(|c| *c != '\n') {
                        text.push(c);
                        self.bump();
                    }
                }
                '/' if self.peek(1) == Some('*') => {
                    while self.peek(0).is_some() {
                        if self.peek(0) == Some('*') && self.peek(1) == Some('/') {
                            text.push_str("*/");
                            self.bump();
                            self.bump();
                            break;
                        }
                        text.extend(self.bump());
                    }
                }
                '$' if self.peek(1) == Some('{') => {
                    if !text.is_empty() {
                        parts.push(Part::Text(std::mem::take(&mut text)));
                    }
                    match self.hole(pos) {
                        Some(hole) => parts.push(hole),
                        None => break,
                    }
                }
                '{' => {
                    depth += 1;
                    text.push(c);
                    self.bump();
                }
                '}' => {
                    self.bump();
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    text.push(c);
                }
                _ => {
                    text.push(c);
                    self.bump();
                }
            }
        }
        if !text.is_empty() {
            parts.push(Part::Text(text));
        }
        self.push(Tok::Sql(parts), open);
    }

    /// A quoted SQL string or identifier, copied verbatim. A `${` inside one
    /// is reported: holes are parameters, never text inside a literal.
    fn sql_quoted(&mut self, quote: char, text: &mut String) {
        text.extend(self.bump());
        while let Some(c) = self.peek(0) {
            if c == '$' && self.peek(1) == Some('{') {
                let pos = self.pos();
                let hole = scan_hole_end(self.chars, self.i + 2, self.end)
                    .map(|close| self.chars[self.i..=close].iter().collect::<String>());
                let mut diagnostic = Diagnostic::new(
                    code::SQL_QUOTED_HOLE,
                    pos,
                    "a `${}` hole inside a quoted SQL string is not a parameter",
                )
                .with_help(
                    "holes are passed as query parameters; write them where a value goes, without quotes",
                );
                diagnostic.fix = hole.map(|hole| format!("{hole} without the surrounding quotes"));
                self.report(diagnostic);
            }
            text.push(c);
            self.bump();
            if c == quote {
                return;
            }
        }
    }
}

/// Index of the `}` that closes a hole whose content starts at `from`,
/// skipping nested braces and double-quoted strings.
fn scan_hole_end(chars: &[char], from: usize, end: usize) -> Option<usize> {
    let mut depth = 0u32;
    let mut j = from;
    while j < end {
        match chars[j] {
            '{' => depth += 1,
            '}' if depth == 0 => return Some(j),
            '\n' => return None,
            '}' => depth -= 1,
            '"' => {
                j = skip_string(chars, j, end)?;
                continue;
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// Index just past the double-quoted string opening at `start`.
fn skip_string(chars: &[char], start: usize, end: usize) -> Option<usize> {
    let mut j = start + 1;
    while j < end {
        match chars[j] {
            '\\' => j += 2,
            '"' => return Some(j + 1),
            '\n' => return None,
            '$' if chars.get(j + 1) == Some(&'{') => j = scan_hole_end(chars, j + 2, end)? + 1,
            _ => j += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(source: &str) -> Vec<Tok> {
        let (tokens, diagnostics) = lex(source);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        tokens.into_iter().map(|t| t.tok).collect()
    }

    fn at(line: u32, column: u32) -> Pos {
        Pos { line, column }
    }

    #[test]
    fn operators_and_accepted_spellings() {
        assert_eq!(
            kinds("a === b !== c == d != e => ?. ?? ? <= >= < > && || |"),
            vec![
                Tok::Ident("a".into()),
                Tok::EqEq,
                Tok::Ident("b".into()),
                Tok::NotEq,
                Tok::Ident("c".into()),
                Tok::EqEq,
                Tok::Ident("d".into()),
                Tok::NotEq,
                Tok::Ident("e".into()),
                Tok::Arrow,
                Tok::QuestionDot,
                Tok::QuestionQuestion,
                Tok::Question,
                Tok::Le,
                Tok::Ge,
                Tok::Lt,
                Tok::Gt,
                Tok::AndAnd,
                Tok::OrOr,
                Tok::Pipe,
                Tok::Eof,
            ]
        );
        assert_eq!(
            kinds("const var let"),
            vec![Tok::Let, Tok::Let, Tok::Let, Tok::Eof]
        );
    }

    #[test]
    fn numbers() {
        assert_eq!(
            kinds("0 42 1.5 2e3 1.25E-2 3.x"),
            vec![
                Tok::Int(0),
                Tok::Int(42),
                Tok::Float(1.5),
                Tok::Float(2000.0),
                Tok::Float(0.0125),
                Tok::Int(3),
                Tok::Dot,
                Tok::Ident("x".into()),
                Tok::Eof,
            ]
        );
        let (_, diagnostics) = lex("99999999999999999999");
        assert_eq!(diagnostics[0].code, code::LEX);
        assert!(diagnostics[0].message.contains("64-bit"));
    }

    #[test]
    fn positions_count_characters_and_lines() {
        let (tokens, diagnostics) = lex("let s = \"é\" + t\n  // note\n  return s");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let positions: Vec<_> = tokens.iter().map(|t| (t.pos, t.newline_before)).collect();
        assert_eq!(positions[5], (at(1, 15), false));
        assert_eq!(positions[6], (at(3, 3), true));
        assert_eq!(positions[7], (at(3, 10), false));
    }

    #[test]
    fn strings_with_escapes_and_holes() {
        let (tokens, diagnostics) = lex(r#""a\"b\\c\n\t\$ ${x + 1} and ${r["}"]}""#);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Tok::Str(parts) = &tokens[0].tok else {
            panic!("{tokens:?}")
        };
        assert_eq!(parts[0], Part::Text("a\"b\\c\n\t$ ".into()));
        let Part::Hole(hole) = &parts[1] else {
            panic!("{parts:?}")
        };
        let hole_kinds: Vec<_> = hole.iter().map(|t| t.tok.clone()).collect();
        assert_eq!(
            hole_kinds,
            vec![Tok::Ident("x".into()), Tok::Plus, Tok::Int(1), Tok::Eof]
        );
        assert_eq!(hole[0].pos, at(1, 18));
        assert_eq!(parts[2], Part::Text(" and ".into()));
        assert!(matches!(&parts[3], Part::Hole(tokens) if tokens.len() == 5));
    }

    #[test]
    fn misquoted_strings_suggest_double_quotes() {
        let (tokens, diagnostics) = lex("let a = 'it'\nlet b = `x ${y}`");
        assert_eq!(tokens[3].tok, Tok::Str(vec![Part::Text("it".into())]));
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0].pos(), at(1, 9));
        assert_eq!(diagnostics[0].fix.as_deref(), Some("\"it\""));
        assert_eq!(diagnostics[1].pos(), at(2, 9));
        assert_eq!(diagnostics[1].fix.as_deref(), Some("\"x ${y}\""));
    }

    #[test]
    fn unterminated_string_and_bad_escape() {
        let (_, diagnostics) = lex("let a = \"abc\nlet b = \"\\q\"");
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0].pos(), at(1, 9));
        assert!(diagnostics[0].message.contains("unterminated"));
        assert_eq!(diagnostics[1].pos(), at(2, 10));
        assert!(diagnostics[1].message.contains("\\q"));
    }

    #[test]
    fn sql_block_is_verbatim_with_holes() {
        let source = "sql(db) {\n  SELECT '{x}' AS \"a}\" -- don't }\n  FROM t WHERE id = ${org.id} AND n IN (${ids})\n}";
        let (tokens, diagnostics) = lex(source);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let kinds: Vec<_> = tokens.iter().map(|t| &t.tok).collect();
        assert_eq!(
            kinds[..4],
            [
                &Tok::SqlKeyword,
                &Tok::LParen,
                &Tok::Ident("db".into()),
                &Tok::RParen
            ]
        );
        let Tok::Sql(parts) = &tokens[4].tok else {
            panic!("{tokens:?}")
        };
        assert_eq!(tokens[4].pos, at(1, 9));
        assert_eq!(
            parts[0],
            Part::Text("\n  SELECT '{x}' AS \"a}\" -- don't }\n  FROM t WHERE id = ".into())
        );
        assert!(matches!(&parts[1], Part::Hole(t) if t.len() == 4 && t[0].pos == at(3, 23)));
        assert_eq!(parts[2], Part::Text(" AND n IN (".into()));
        assert_eq!(parts[4], Part::Text(")\n".into()));
        assert_eq!(tokens[5].tok, Tok::Eof);
    }

    #[test]
    fn sql_nested_braces_and_nested_parens_in_db() {
        let (tokens, diagnostics) =
            lex("sql(f(a)) { SELECT '{' || x->>'{a,b}' FROM {t} }\nreturn 1");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let Tok::Sql(parts) = &tokens[7].tok else {
            panic!("{tokens:?}")
        };
        assert_eq!(
            parts,
            &[Part::Text(" SELECT '{' || x->>'{a,b}' FROM {t} ".into())]
        );
        assert_eq!(tokens[8].tok, Tok::Return);
        assert!(tokens[8].newline_before);
    }

    #[test]
    fn sql_quoted_hole_and_unterminated_block() {
        let (_, diagnostics) = lex("sql(db) { SELECT 1 WHERE a = '${name}' }");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, code::SQL_QUOTED_HOLE);
        assert_eq!(diagnostics[0].pos(), at(1, 31));
        assert_eq!(
            diagnostics[0].fix.as_deref(),
            Some("${name} without the surrounding quotes")
        );
        let (_, diagnostics) = lex("sql(db) { SELECT (\nreturn 1");
        assert_eq!(diagnostics[0].pos(), at(1, 9));
        assert!(diagnostics[0].message.contains("never closed"));
    }

    #[test]
    fn sql_without_braces_is_ordinary_tokens() {
        assert_eq!(
            kinds("sql(db) + 1"),
            vec![
                Tok::SqlKeyword,
                Tok::LParen,
                Tok::Ident("db".into()),
                Tok::RParen,
                Tok::Plus,
                Tok::Int(1),
                Tok::Eof
            ]
        );
    }

    #[test]
    fn stray_characters_are_reported_and_skipped() {
        let (tokens, diagnostics) = lex("a # b & c [...xs]");
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0].pos(), at(1, 3));
        assert_eq!(diagnostics[1].fix.as_deref(), Some("`&&`"));
        assert_eq!(tokens.len(), 9);
        assert!(tokens.iter().any(|t| t.tok == Tok::Spread));
    }

    #[test]
    fn unclosed_hole() {
        let (_, diagnostics) = lex("\"a ${b\nreturn 1");
        assert_eq!(diagnostics[0].pos(), at(1, 4));
        assert!(diagnostics[0].message.contains("never closed"));
    }
}
