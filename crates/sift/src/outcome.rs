//! The result of a run, and the text the model reads about it.

use crate::diagnostic::Diagnostic;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The program ran to its `return`.
    Returned {
        value: serde_json::Value,
        stats: Stats,
    },
    /// The program did not parse or broke a static rule; nothing ran.
    Rejected { diagnostics: Vec<Diagnostic> },
    /// The program started and stopped at an error or a budget.
    Failed { error: Diagnostic, stats: Stats },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Stats {
    pub steps: u64,
    pub calls: u32,
    pub elapsed: Duration,
    /// Every host call that ran, in the order they finished.
    pub journal: Vec<JournalEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    /// The tool's model-facing name, or `sql`.
    pub tool: String,
    pub ok: bool,
    pub duration: Duration,
}

impl Outcome {
    /// Compact text for the model: the value as pretty JSON, or each
    /// diagnostic with its source line and caret, plus for a failed run a
    /// line on what had run.
    pub fn render(&self, source: &str) -> String {
        match self {
            Outcome::Returned { value, .. } => format!("{value:#}"),
            Outcome::Rejected { diagnostics } => {
                let mut out = String::new();
                for diagnostic in diagnostics {
                    diagnostic.render_into(source, &mut out);
                }
                out
            }
            Outcome::Failed { error, stats } => {
                let mut out = String::new();
                error.render_into(source, &mut out);
                out.push_str(&stats.summary());
                out
            }
        }
    }
}

impl Stats {
    /// One line: how many calls ran, of which tools, and which failed.
    pub fn summary(&self) -> String {
        if self.journal.is_empty() {
            return "ran no tool or SQL calls".to_string();
        }
        let mut tally: Vec<(&str, usize)> = Vec::new();
        let mut failed: Vec<&str> = Vec::new();
        for entry in &self.journal {
            match tally.iter_mut().find(|(tool, _)| *tool == entry.tool) {
                Some((_, n)) => *n += 1,
                None => tally.push((&entry.tool, 1)),
            }
            if !entry.ok && !failed.contains(&entry.tool.as_str()) {
                failed.push(&entry.tool);
            }
        }
        let tools: Vec<String> = tally
            .into_iter()
            .map(|(tool, n)| {
                if n == 1 {
                    tool.to_string()
                } else {
                    format!("{tool} ×{n}")
                }
            })
            .collect();
        let n = self.journal.len();
        let mut line = format!(
            "ran {n} call{}: {}",
            if n == 1 { "" } else { "s" },
            tools.join(", ")
        );
        if !failed.is_empty() {
            line.push_str("; failed: ");
            line.push_str(&failed.join(", "));
        }
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::{code, Pos};

    fn entry(tool: &str, ok: bool) -> JournalEntry {
        JournalEntry {
            tool: tool.to_string(),
            ok,
            duration: Duration::ZERO,
        }
    }

    #[test]
    fn summary_counts_and_names_failures() {
        let stats = Stats {
            journal: vec![
                entry("jira_search", true),
                entry("sql", true),
                entry("jira_search", false),
            ],
            ..Stats::default()
        };
        assert_eq!(
            stats.summary(),
            "ran 3 calls: jira_search ×2, sql; failed: jira_search"
        );
        assert_eq!(Stats::default().summary(), "ran no tool or SQL calls");
    }

    #[test]
    fn renders_each_outcome() {
        let returned = Outcome::Returned {
            value: serde_json::json!({ "n": 1 }),
            stats: Stats::default(),
        };
        assert_eq!(returned.render(""), "{\n  \"n\": 1\n}");
        let source = "let a = f()\nreturn a";
        let error = Diagnostic::new(code::TOOL, Pos { line: 1, column: 9 }, "`f` failed: nope");
        let failed = Outcome::Failed {
            error,
            stats: Stats {
                journal: vec![entry("f", false)],
                ..Stats::default()
            },
        };
        assert_eq!(
            failed.render(source),
            "error[E-TOOL] line 1:9: `f` failed: nope\n1 | let a = f()\n  |         ^\nran 1 call: f; failed: f"
        );
    }
}
