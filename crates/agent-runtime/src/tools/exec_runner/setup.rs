//! The outcome of a workspace's setup commands, shared by the hub-local path
//! and a worker so the report Claude Code is handed reads the same wherever
//! the commands ran.

use std::future::Future;

use eyre::Result;
use serde::{Deserialize, Serialize};

use super::run_or_dispatch::ExecOutcome;

const STDERR_TAIL_BYTES: usize = 2_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SetupOutcome {
    Ok,
    Failed { exit_code: i32, stderr_tail: String },
    TimedOut,
    Unavailable { reason: String },
}

impl SetupOutcome {
    /// Whether the command actually started, success or failure. A command
    /// that could not be started at all is retried next time.
    pub fn ran(&self) -> bool {
        !matches!(self, Self::Unavailable { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupReport {
    pub alias: String,
    pub command: String,
    #[serde(flatten)]
    pub outcome: SetupOutcome,
}

impl SetupReport {
    /// One line for Claude Code's system prompt, so it knows the state of the
    /// tree it was handed rather than rediscovering it.
    pub fn describe(&self) -> String {
        let outcome = match &self.outcome {
            SetupOutcome::Ok => "ok".to_string(),
            SetupOutcome::Failed {
                exit_code,
                stderr_tail,
            } if stderr_tail.is_empty() => format!("failed (exit {exit_code})"),
            SetupOutcome::Failed {
                exit_code,
                stderr_tail,
            } => format!("failed (exit {exit_code}): {stderr_tail}"),
            SetupOutcome::TimedOut => "timed out".to_string(),
            SetupOutcome::Unavailable { reason } => format!("could not run: {reason}"),
        };
        format!("`{}`: `{}` — {outcome}", self.alias, self.command)
    }
}

/// Run each `(alias, command)` through `exec`, in order, and report every
/// outcome. Shared by the hub-local path and a worker so the report Claude
/// Code is handed is built in exactly one place.
pub async fn run_setup<F, Fut>(commands: &[(String, String)], exec: F) -> Vec<SetupReport>
where
    F: Fn(String, String) -> Fut,
    Fut: Future<Output = Result<ExecOutcome>>,
{
    let mut reports = Vec::with_capacity(commands.len());
    for (alias, command) in commands {
        let outcome = match exec(alias.clone(), command.clone()).await {
            Ok(out) if out.summary.timed_out => SetupOutcome::TimedOut,
            Ok(out) if out.success() => SetupOutcome::Ok,
            Ok(out) => SetupOutcome::Failed {
                exit_code: out.exit_code(),
                stderr_tail: tail_utf8(out.stderr.trim_end(), STDERR_TAIL_BYTES),
            },
            Err(e) => SetupOutcome::Unavailable {
                reason: e.to_string(),
            },
        };
        reports.push(SetupReport {
            alias: alias.clone(),
            command: command.clone(),
            outcome,
        });
    }
    reports
}

/// The last `max_bytes` of `s`, prefixed with `…\n` when anything was cut. The
/// cut is snapped forward to a character boundary and then to the start of a
/// line, so a tail never splits a multi-byte sequence or opens mid-line.
pub fn tail_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let cut = s.len() - max_bytes;
    let boundary = (cut..=s.len())
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(s.len());
    let snapped = s[boundary..]
        .find('\n')
        .map(|nl| boundary + nl + 1)
        .filter(|&start| start < s.len())
        .unwrap_or(boundary);
    format!("…\n{}", &s[snapped..])
}

#[cfg(test)]
mod tests {
    use super::super::protocol::ExecSummary;
    use super::*;

    fn outcome(exit_code: Option<i32>, stderr: &str, timed_out: bool) -> ExecOutcome {
        ExecOutcome {
            stdout: String::new(),
            stderr: stderr.into(),
            summary: ExecSummary {
                exit_code,
                attempts: 1,
                truncated: false,
                timed_out,
            },
        }
    }

    #[tokio::test]
    async fn every_outcome_kind_is_reported_in_order() {
        let commands = vec![
            ("ok".to_string(), "true".to_string()),
            ("bad".to_string(), "false".to_string()),
            ("slow".to_string(), "sleep".to_string()),
            ("down".to_string(), "x".to_string()),
        ];
        let reports = run_setup(&commands, |alias, _| async move {
            match alias.as_str() {
                "ok" => Ok(outcome(Some(0), "", false)),
                "bad" => Ok(outcome(Some(2), "boom\n", false)),
                "slow" => Ok(outcome(None, "", true)),
                _ => Err(eyre::eyre!("runner unavailable")),
            }
        })
        .await;
        assert_eq!(reports[0].outcome, SetupOutcome::Ok);
        assert_eq!(
            reports[1].outcome,
            SetupOutcome::Failed {
                exit_code: 2,
                stderr_tail: "boom".into()
            }
        );
        assert_eq!(reports[2].outcome, SetupOutcome::TimedOut);
        assert!(matches!(
            reports[3].outcome,
            SetupOutcome::Unavailable { .. }
        ));
        assert_eq!(reports[3].command, "x");
    }

    #[test]
    fn tail_keeps_short_input_and_never_splits_a_character() {
        assert_eq!(tail_utf8("line 1\nline 2\n", 4_000), "line 1\nline 2\n");
        let s = "é".repeat(100);
        for max in 1..10 {
            let tail = tail_utf8(&s, max);
            let kept = tail.strip_prefix("…\n").expect("a cut tail says so");
            assert!(kept.chars().all(|c| c == 'é'), "{max}: {tail:?}");
        }
    }

    #[test]
    fn tail_opens_at_a_line_start_when_one_falls_inside_the_window() {
        let s = "first\nsecond\nthird";
        assert_eq!(tail_utf8(s, 9), "…\nthird");
        let lines: String = (0..2_000).map(|i| format!("line {i:04}\n")).collect();
        let tail = tail_utf8(&lines, 4_000);
        assert!(tail.len() <= 4_000 + "…\n".len());
        assert!(tail.contains("line 1999") && !tail.contains("line 0000"));
    }

    #[test]
    fn a_report_round_trips_with_its_outcome_flattened() {
        let report = SetupReport {
            alias: "rails".into(),
            command: "bundle install".into(),
            outcome: SetupOutcome::Unavailable {
                reason: "runner down".into(),
            },
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["status"], "unavailable");
        assert_eq!(json["reason"], "runner down");
        let back: SetupReport = serde_json::from_value(json).unwrap();
        assert_eq!(back, report);
        assert!(!back.outcome.ran());
        assert!(back.describe().contains("could not run: runner down"));
    }
}
