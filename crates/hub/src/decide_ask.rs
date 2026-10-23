//! Asking recorded examples again, offline, for `workforce decide ask`: a
//! held-out file put to the configured backends exactly as a hub puts a
//! live decision — the same request builder, the same per-use budgets, the
//! same slot guard — so a baseline measures the path that will serve.
//!
//! One example is asked at a time. A backend too busy to answer within its
//! use's budget stops the run where it is, with everything answered so far
//! written: a baseline must never pile onto a model that is serving a hub.

use std::io::{BufRead, Write};
use std::time::Instant;

use eyre::{eyre, Result, WrapErr};
use serde_json::{json, Map, Value};
use workforce_decide::export::Example;

use crate::credentials::Caller;
use crate::decide::{answer_fields, explain, Decisions};

/// How a run over a file of examples ended.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Asked {
    pub answered: usize,
    /// Examples with nothing to decide about, which no hub would ask.
    pub empty: usize,
    /// The line the run stopped at because the backends were busy: nothing
    /// from it on was asked.
    pub busy_at: Option<usize>,
}

/// Put every example line of `input` to `decisions` with `caller`'s keys,
/// writing one line of picks per answered example to `out`: its id, use and
/// label, each question's pick with its probability, who answered and how
/// long it took.
pub async fn ask_examples(
    decisions: &Decisions,
    caller: &Caller,
    input: impl BufRead,
    mut out: impl Write,
) -> Result<Asked> {
    let mut asked = Asked::default();
    let result = ask_each(decisions, caller, input, &mut out, &mut asked).await;
    out.flush()?;
    result.map(|()| asked)
}

async fn ask_each(
    decisions: &Decisions,
    caller: &Caller,
    input: impl BufRead,
    out: &mut impl Write,
    asked: &mut Asked,
) -> Result<()> {
    for (number, line) in input.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let example: Example = serde_json::from_str(&line)
            .wrap_err_with(|| format!("line {}: not a recorded example", number + 1))?;
        let started = Instant::now();
        let decision = match decisions.put(caller, &example.input).await {
            Ok(Some(decision)) => decision,
            Ok(None) => {
                asked.empty += 1;
                continue;
            }
            Err(e) if e.is_overloaded() => {
                asked.busy_at = Some(number + 1);
                return Ok(());
            }
            Err(e) => return Err(eyre!("line {}: {}", number + 1, explain(&e))),
        };
        // The primary gave no answer and the fallback did: what follows
        // would measure the wrong model while pressing the one in trouble.
        if decision.fell_back {
            asked.busy_at = Some(number + 1);
            return Ok(());
        }
        let latency_ms = started.elapsed().as_secs_f64() * 1_000.0;
        let picks: Map<String, Value> = decision
            .response
            .answers
            .iter()
            .map(|(id, answer)| (id.clone(), Value::Object(answer_fields(answer, &[]))))
            .collect();
        let pick = json!({
            "id": example.tags.id,
            "use": example.input.usage().as_str(),
            "label": example.label,
            "picks": picks,
            "backend": decision.backend,
            "model": decision.response.model,
            "latency_ms": latency_ms,
        });
        writeln!(out, "{pick}")?;
        asked.answered += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::testing::{config_for, recording_backend};
    use std::time::Duration;
    use workforce_common::settings::{DecisionBackend, DecisionConfig, DecisionUses};

    fn nobody() -> Caller {
        Caller::new(None, None, None)
    }

    fn examples() -> String {
        [
            json!({"use": "routing", "message": "How many refunds last week?",
                   "label": "database", "id": "routing:1"}),
            json!({"use": "routing", "message": "  ", "label": "ambiguous", "id": "routing:2"}),
            json!({"use": "scope_gate", "message": "What's the weather?", "topic": "billing",
                   "label": false, "id": "scope_gate:3"}),
        ]
        .map(|example| format!("{example}\n\n"))
        .concat()
    }

    #[tokio::test]
    async fn each_example_is_asked_as_a_hub_asks_it_and_its_picks_are_written() {
        let (url, seen) = recording_backend("database", Duration::ZERO).await;
        let config = DecisionConfig {
            uses: DecisionUses {
                routing: toml::from_str("state_chars = 400").unwrap(),
                ..Default::default()
            },
            ..config_for(url)
        };
        let decisions = crate::decide::build(&config).unwrap().unwrap();
        let mut out = Vec::new();
        let asked = ask_examples(&decisions, &nobody(), examples().as_bytes(), &mut out)
            .await
            .unwrap();
        assert_eq!(
            asked,
            Asked {
                answered: 2,
                empty: 1,
                busy_at: None
            }
        );
        let picks: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(picks.len(), 2);
        assert_eq!(picks[0]["id"], "routing:1");
        assert_eq!(picks[0]["use"], "routing");
        assert_eq!(picks[0]["label"], "database");
        assert_eq!(picks[0]["picks"]["use_case"]["label"], "database");
        assert_eq!(picks[0]["picks"]["use_case"]["probability"], 0.9);
        assert_eq!(picks[0]["backend"], "kev");
        assert_eq!(picks[0]["model"], "fake-1");
        assert!(picks[0]["latency_ms"].as_f64().unwrap() >= 0.0);
        assert_eq!(picks[1]["picks"]["on_topic"]["label"], "no");
        assert_eq!(picks[1]["label"], false);

        let sent = seen.lock().clone();
        assert_eq!(sent.len(), 2, "the blank one is never sent");
        let routing = workforce_decide::Input::Routing {
            message: "How many refunds last week?".into(),
            agent: None,
            channel: None,
        }
        .request(workforce_decide::RequestBudget::chars(400))
        .unwrap();
        assert_eq!(sent[0]["state"], routing.state);
        assert_eq!(sent[0]["questions"], json!(routing.questions));
    }

    #[tokio::test]
    async fn a_busy_backend_stops_the_run_with_what_was_answered_written() {
        let (url, seen) = recording_backend("database", Duration::from_millis(400)).await;
        let config = DecisionConfig {
            backends: vec![DecisionBackend {
                url,
                max_concurrent: 1,
                ..crate::decide::testing::backend("kev")
            }],
            uses: DecisionUses {
                routing: toml::from_str("budget_ms = 3000").unwrap(),
                scope_gate: toml::from_str("budget_ms = 100").unwrap(),
                ..Default::default()
            },
            ..config_for(String::new())
        };
        let decisions = crate::decide::build(&config).unwrap().unwrap();
        let mut out = Vec::new();
        let asked = ask_examples(&decisions, &nobody(), examples().as_bytes(), &mut out)
            .await
            .unwrap();
        assert_eq!(
            asked,
            Asked {
                answered: 1,
                empty: 1,
                busy_at: Some(5)
            }
        );
        let written = String::from_utf8(out).unwrap();
        assert_eq!(written.lines().count(), 1, "the answered one is written");
        assert!(written.contains("routing:1"));
        assert_eq!(
            seen.lock().len(),
            2,
            "nothing more is sent to a busy backend"
        );
    }

    #[tokio::test]
    async fn a_bad_line_and_a_backend_fault_fail_and_name_the_line() {
        let (url, _) = recording_backend("database", Duration::ZERO).await;
        let decisions = crate::decide::build(&config_for(url)).unwrap().unwrap();
        let bad = format!("{}{{\"use\":\"routing\"}}\n", examples());
        let mut out = Vec::new();
        let err = ask_examples(&decisions, &nobody(), bad.as_bytes(), &mut out)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("line 7"), "{err}");
        assert_eq!(String::from_utf8(out).unwrap().lines().count(), 2);

        let down = crate::decide::build(&config_for("http://127.0.0.1:9".into()))
            .unwrap()
            .unwrap();
        let err = ask_examples(&down, &nobody(), examples().as_bytes(), Vec::new())
            .await
            .unwrap_err();
        assert!(err.to_string().starts_with("line 1: "), "{err}");
    }
}
