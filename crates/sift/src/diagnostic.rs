//! Diagnostics: what the model reads when a program is rejected or fails.

use std::fmt::Write as _;

/// Stable diagnostic codes. Hosts may match on them; the hub logs codes and
/// positions, never source or values.
pub mod code {
    /// A token the grammar does not allow at this point.
    pub const SYNTAX: &str = "E-SYNTAX";
    /// A malformed literal, character or unterminated string or block.
    pub const LEX: &str = "E-LEX";
    /// A construct that other languages have and Sift leaves out.
    pub const UNSUPPORTED: &str = "E-UNSUPPORTED";
    /// Expressions, calls or values nested deeper than allowed.
    pub const NESTING: &str = "E-NESTING";
    /// A `${}` hole written inside a quoted SQL string.
    pub const SQL_QUOTED_HOLE: &str = "E-SQL-QUOTED-HOLE";
    /// A record literal names the same key twice.
    pub const DUPLICATE_KEY: &str = "E-DUPLICATE-KEY";
    /// A `match` without a trailing `_` arm.
    pub const MATCH_DEFAULT: &str = "E-MATCH-DEFAULT";
    /// The program has no top-level `return`.
    pub const RETURN_MISSING: &str = "E-RETURN-MISSING";
    /// Statements follow a `return` in the same block.
    pub const RETURN_NOT_LAST: &str = "E-RETURN-NOT-LAST";
    /// A name bound twice in one scope.
    pub const LET_REBIND: &str = "E-LET-REBIND";
    /// A name that is not a local, a library function or a callable tool.
    pub const UNKNOWN_NAME: &str = "E-UNKNOWN-NAME";
    /// A value of the wrong type for an operator or statement.
    pub const TYPE: &str = "E-TYPE";
    /// A bad argument to a library function or tool.
    pub const ARGUMENT: &str = "E-ARG";
    /// The wrong number of arguments.
    pub const ARITY: &str = "E-ARITY";
    /// A record field that does not exist.
    pub const FIELD: &str = "E-FIELD";
    /// A list index out of range.
    pub const INDEX: &str = "E-INDEX";
    /// Field access or a method call on null.
    pub const NULL: &str = "E-NULL";
    /// Integer overflow, a non-finite float or division by zero.
    pub const ARITHMETIC: &str = "E-ARITH";
    /// A tool call the host failed.
    pub const TOOL: &str = "E-TOOL";
    /// A SQL block the host failed.
    pub const SQL: &str = "E-SQL";
    /// The step budget is spent.
    pub const BUDGET_STEPS: &str = "E-BUDGET-STEPS";
    /// The tool and SQL call budget is spent.
    pub const BUDGET_CALLS: &str = "E-BUDGET-CALLS";
    /// The run's deadline passed.
    pub const BUDGET_TIME: &str = "E-BUDGET-TIME";
    /// A value too large to walk: built from shared parts until converting,
    /// printing or comparing it would take unbounded work.
    pub const VALUE_SIZE: &str = "E-VALUE-SIZE";
    /// The return value is larger than allowed.
    pub const BUDGET_RETURN: &str = "E-BUDGET-RETURN";
}

/// A 1-based line and column; columns count characters, not bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pos {
    pub line: u32,
    pub column: u32,
}

impl Pos {
    pub const START: Pos = Pos { line: 1, column: 1 };
}

/// One problem with a program, anchored at the token or expression at fault.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("error[{code}] line {line}:{column}: {message}")]
pub struct Diagnostic {
    pub code: &'static str,
    pub message: String,
    pub line: u32,
    pub column: u32,
    /// What to consider, when the message alone does not say what to do.
    pub help: Option<String>,
    /// A concrete replacement, when one is obvious.
    pub fix: Option<String>,
}

impl Diagnostic {
    pub(crate) fn new(code: &'static str, pos: Pos, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            line: pos.line,
            column: pos.column,
            help: None,
            fix: None,
        }
    }

    pub(crate) fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub(crate) fn with_fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }

    pub(crate) fn pos(&self) -> Pos {
        Pos {
            line: self.line,
            column: self.column,
        }
    }

    /// The header line, the offending source line with a caret under the
    /// column, then `help:` and `fix:` lines.
    pub(crate) fn render_into(&self, source: &str, out: &mut String) {
        let _ = writeln!(out, "{self}");
        if let Some(text) = source.lines().nth(self.line.saturating_sub(1) as usize) {
            let gutter = self.line.to_string();
            let shown: String = text
                .chars()
                .map(|c| if c == '\t' { ' ' } else { c })
                .collect();
            let _ = writeln!(out, "{gutter} | {shown}");
            let pad = " ".repeat(self.column.saturating_sub(1) as usize);
            let _ = writeln!(out, "{} | {pad}^", " ".repeat(gutter.len()));
        }
        if let Some(help) = &self.help {
            let _ = writeln!(out, "help: {help}");
        }
        if let Some(fix) = &self.fix {
            let _ = writeln!(out, "fix: {fix}");
        }
    }
}

/// Keep at most `limit` diagnostics, in source order, one per position.
pub(crate) fn settle(mut diagnostics: Vec<Diagnostic>, limit: usize) -> Vec<Diagnostic> {
    diagnostics.sort_by_key(Diagnostic::pos);
    diagnostics.dedup_by_key(|d| d.pos());
    diagnostics.truncate(limit);
    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_caret_under_column() {
        let d = Diagnostic::new(code::SYNTAX, Pos { line: 2, column: 5 }, "expected `=`")
            .with_help("bind with `let name = value`")
            .with_fix("let x = 1");
        let mut out = String::new();
        d.render_into("let a = 1\nlet x 1\n", &mut out);
        assert_eq!(
            out,
            "error[E-SYNTAX] line 2:5: expected `=`\n\
             2 | let x 1\n  |     ^\n\
             help: bind with `let name = value`\n\
             fix: let x = 1\n"
        );
    }

    #[test]
    fn settle_sorts_dedups_and_caps() {
        let at = |line, column| Diagnostic::new(code::SYNTAX, Pos { line, column }, "x");
        let settled = settle(vec![at(3, 1), at(1, 2), at(1, 2), at(2, 1)], 2);
        assert_eq!(
            settled.iter().map(Diagnostic::pos).collect::<Vec<_>>(),
            vec![Pos { line: 1, column: 2 }, Pos { line: 2, column: 1 }]
        );
    }
}
