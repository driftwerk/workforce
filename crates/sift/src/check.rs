//! Static rules over a parsed program: where `return` may stand, and that no
//! name is bound twice in one scope.

use crate::ast::Stmt;
use crate::diagnostic::{code, Diagnostic, Pos};

/// Check a program. `end` is where a missing top-level `return` is reported;
/// the return rules run only on a program that parsed cleanly, since a
/// broken statement may have been the `return`.
pub(crate) fn program(body: &[Stmt], end: Pos, returns: bool) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    block(body, &[], &mut diagnostics, returns);
    if returns && !body.iter().any(|s| matches!(s, Stmt::Return { .. })) {
        diagnostics.push(
            Diagnostic::new(
                code::RETURN_MISSING,
                end,
                "the program has no top-level `return`",
            )
            .with_help("a `return` inside `if` or `for` covers only that branch")
            .with_fix("end the program with `return <value>`"),
        );
    }
    diagnostics
}

/// Check a lambda's `{ ... }` body. `end` is its closing `}`; `returns` as
/// for [`program`].
pub(crate) fn function(body: &[Stmt], end: Pos, returns: bool) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    block(body, &[], &mut diagnostics, returns);
    if returns && !body.iter().any(|s| matches!(s, Stmt::Return { .. })) {
        diagnostics.push(
            Diagnostic::new(
                code::RETURN_MISSING,
                end,
                "the function body has no `return` at its end",
            )
            .with_help("a `return` inside `if` or `for` covers only that branch")
            .with_fix("end the body with `return <value>`"),
        );
    }
    diagnostics
}

/// Check one block whose scope starts with `bound` (a `for` variable).
fn block(stmts: &[Stmt], bound: &[(&str, Pos)], out: &mut Vec<Diagnostic>, returns: bool) {
    let mut scope: Vec<(&str, Pos)> = bound.to_vec();
    for (index, stmt) in stmts.iter().enumerate() {
        match stmt {
            Stmt::Let { name, pos, .. } => {
                if let Some((_, first)) = scope.iter().find(|(bound, _)| bound == name) {
                    out.push(
                        Diagnostic::new(
                            code::LET_REBIND,
                            *pos,
                            format!(
                                "`{name}` is already bound at line {}:{}; bindings are immutable",
                                first.line, first.column
                            ),
                        )
                        .with_fix(format!("let {name}_2 = ...")),
                    );
                } else {
                    scope.push((name, *pos));
                }
            }
            Stmt::If {
                branches,
                otherwise,
            } => {
                for body in branches.iter().map(|(_, body)| body).chain(otherwise) {
                    block(body, &[], out, returns);
                }
            }
            Stmt::For { var, body, pos, .. } => block(body, &[(var, *pos)], out, returns),
            Stmt::Return { pos, .. } => {
                if returns && index + 1 < stmts.len() {
                    out.push(
                        Diagnostic::new(
                            code::RETURN_NOT_LAST,
                            *pos,
                            "`return` must be the last statement of its block",
                        )
                        .with_help("the statements after it would never run"),
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::diagnostic::code;
    use crate::parser::parse;

    fn codes(source: &str) -> Vec<(&'static str, u32, u32)> {
        parse(source)
            .err()
            .unwrap_or_default()
            .into_iter()
            .map(|d| (d.code, d.line, d.column))
            .collect()
    }

    #[test]
    fn return_rules() {
        assert_eq!(codes("let a = 1\n"), [(code::RETURN_MISSING, 2, 1)]);
        assert_eq!(codes(""), [(code::RETURN_MISSING, 1, 1)]);
        assert_eq!(
            codes("if a { return 1 }\nlet b = 2"),
            [(code::RETURN_MISSING, 2, 10)]
        );
        assert_eq!(
            codes("return 1\nlet a = 2"),
            [(code::RETURN_NOT_LAST, 1, 1)]
        );
        assert_eq!(codes("return 1\nreturn 2"), [(code::RETURN_NOT_LAST, 1, 1)]);
        assert_eq!(
            codes("for x in xs { return x\nlet y = 1 }\nreturn 0"),
            [(code::RETURN_NOT_LAST, 1, 15)]
        );
        assert!(codes("if a { return 1 } else { return 2 }\nreturn 3").is_empty());
        assert!(codes("return 1;").is_empty());
    }

    #[test]
    fn rebinding() {
        assert_eq!(
            codes("let a = 1\nlet a = 2\nreturn a"),
            [(code::LET_REBIND, 2, 5)]
        );
        let d = parse("let a = 1\nlet a = 2\nreturn a")
            .err()
            .unwrap_or_default();
        assert_eq!(
            d[0].message,
            "`a` is already bound at line 1:5; bindings are immutable"
        );
        assert!(
            codes("let a = 1\nif a { let a = 2 }\nfor b in [a] { let c = b }\nreturn a").is_empty()
        );
        assert_eq!(
            codes("for x in xs { let x = 1 }\nreturn 1"),
            [(code::LET_REBIND, 1, 19)]
        );
    }

    #[test]
    fn a_broken_return_is_not_reported_missing() {
        assert_eq!(codes("return (1"), [(code::SYNTAX, 1, 10)]);
    }
}
