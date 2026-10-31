//! `crew.spend`: what the person's sessions spent, from the desk's ledger,
//! for the overseer to answer usage questions with. It reads only: no gate
//! beyond the level that lets the overseer read the crew at all.
//!
//! The dollars are API-equivalent: what the tokens cost at list prices from
//! the hub's own table. A plan's share is the person's latest reading of
//! that plan, split between the sessions on it by what each spent in the
//! window; only the windows the reading reports are shown.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::{json, Value};
use workforce_common::usage::{account_key, LoginRef, PlanKind, Reading, Window};
use workforce_desk::engine::{Engine, SignIn};
use workforce_desk::ledger::{Period, SpendBy, SpendRow, SpendView, Totals};
use workforce_desk::ledger_store::Filter;
use workforce_desk::SpendQuery;

use super::CrewHub;

pub const NAME: &str = "crew.spend";

pub const DESCRIPTION: &str =
    "What the person's Desk sessions spent: tokens (input, output, cache \
read, cache write) and API-equivalent dollars from the hub's price table, the costliest first, by \
session, project, day, engine or account, since five hours ago, today, this week, this month or a \
date. Where this computer's plan readings allow, each session's estimated share of its plan's \
windows. Use it for every question about tokens, usage, cost or spend (\"which session is eating \
my tokens?\"); never suggest an outside tool for it.";

pub fn schema() -> Value {
    json!({ "type": "object", "properties": {
        "by": { "type": "string", "enum": ["session", "project", "day", "engine", "account"],
                "description": "How to group it (default session)" },
        "since": { "type": "string", "description": "five_hours, today (default), week, month, or a date or time (2026-10-30, 2026-10-30T09:00:00Z)" },
        "limit": { "type": "integer", "minimum": 1, "maximum": 50, "description": "Rows at most (default 10)" }
    }})
}

/// Where `since` begins, as the overseer says it.
fn since_of(
    since: &str,
    period_start: impl Fn(Period) -> DateTime<Utc>,
) -> Result<DateTime<Utc>, String> {
    match since.trim() {
        "" | "today" | "day" => Ok(period_start(Period::Day)),
        "five_hours" | "5h" => Ok(period_start(Period::FiveHours)),
        "week" | "this_week" => Ok(period_start(Period::Week)),
        "month" | "this_month" => Ok(period_start(Period::Month)),
        other => DateTime::parse_from_rfc3339(other)
            .map(|t| t.to_utc())
            .or_else(|_| {
                NaiveDate::parse_from_str(other, "%Y-%m-%d")
                    .map(|d| d.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc())
            })
            .map_err(|_| {
                "Say since as five_hours, today, week, month, or a date like 2026-10-30."
                    .to_string()
            }),
    }
}

/// A window's name as the person reads it: "5-hour", "weekly", else its
/// length.
fn window_name(minutes: u32) -> String {
    match minutes {
        300 => "5-hour".into(),
        10_080 => "weekly".into(),
        m if m % 1440 == 0 => format!("{}-day", m / 1440),
        m if m % 60 == 0 => format!("{}-hour", m / 60),
        m => format!("{m}-minute"),
    }
}

/// Where a window began: its length before it resets. `None` for one with
/// no reset, or one that has reset since it was read (nothing is known of
/// its new use).
fn window_from(w: &Window, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let resets = w.resets_at.filter(|r| *r > now)?;
    Some(resets - Duration::minutes(i64::from(w.minutes)))
}

/// One plan window, and what each session on that plan spent in it.
pub(crate) struct WindowSpend {
    pub window: Window,
    pub spend: SpendView,
}

/// Each window's line for a session: how full the plan was, and the
/// session's estimated share of it (its part of what Desk's sessions on
/// that account spent in the window, times how full the window is).
fn shares(session: &str, windows: &[WindowSpend]) -> Vec<Value> {
    windows
        .iter()
        .filter_map(|ws| {
            let total = ws.spend.total.usd;
            let mine = ws
                .spend
                .rows
                .iter()
                .find(|r| r.key == session)
                .map_or(0.0, |r| r.totals.usd);
            (total > 0.0 && mine > 0.0).then(|| {
                json!({
                    "window": window_name(ws.window.minutes),
                    "plan_used_percent": round(ws.window.used_percent),
                    "resets_at": ws.window.resets_at,
                    "session_share_percent_estimate": round(ws.window.used_percent * mine / total),
                })
            })
        })
        .collect()
}

fn round(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn tokens(t: &Totals) -> Value {
    json!({
        "input": t.tokens.input,
        "output": t.tokens.output,
        "cache_read": t.tokens.cache_read,
        "cache_write": t.tokens.cache_write,
        "total": t.tokens.total(),
    })
}

fn row(r: &SpendRow, total_usd: f64, windows: &[WindowSpend]) -> Value {
    let mut v = json!({
        "name": r.label,
        "tokens": tokens(&r.totals),
        "usd_api_equivalent": (r.totals.usd * 100.0).round() / 100.0,
        "share_of_spend_percent": if total_usd > 0.0 { round(100.0 * r.totals.usd / total_usd) } else { 0.0 },
        "calls": r.totals.calls,
        "billing": r.totals.billing.iter().map(|b| b.as_str()).collect::<Vec<_>>(),
    });
    let o = v.as_object_mut().expect("an object");
    if let Some(id) = r.session {
        o.insert("session_id".into(), json!(id));
        o.insert("plan".into(), json!(shares(&id.to_string(), windows)));
    }
    if let Some(e) = r.engine {
        o.insert("engine".into(), json!(e.title()));
    }
    if let Some(f) = &r.folder {
        o.insert("folder".into(), json!(f));
    }
    if !r.totals.unpriced_models.is_empty() {
        o.insert(
            "no_price_for".into(),
            json!(r.totals.unpriced_models.iter().collect::<Vec<_>>()),
        );
    }
    v
}

/// The answer the overseer reads.
pub(crate) fn answer(view: &SpendView, windows: &[WindowSpend], plan: Option<&Reading>) -> Value {
    json!({
        "from": view.from,
        "to": view.to,
        "by": view.by.as_str(),
        "usd_means": "API-equivalent: what these tokens cost at API list prices from the hub's price table. A session on a plan pays its plan, not this.",
        "total": {
            "tokens": tokens(&view.total),
            "usd_api_equivalent": (view.total.usd * 100.0).round() / 100.0,
            "calls": view.total.calls,
            "unpriced_calls": view.total.unpriced_calls,
            "no_price_for": view.total.unpriced_models.iter().collect::<Vec<_>>(),
        },
        "rows": view.rows.iter().map(|r| row(r, view.total.usd, windows)).collect::<Vec<_>>(),
        "more_rows": view.more,
        "plan": plan.map(|p| json!({
            "plan": p.plan,
            "read_at": p.observed_at,
            "windows": p.windows.iter().map(|w| json!({
                "window": window_name(w.minutes),
                "used_percent": round(w.used_percent),
                "resets_at": w.resets_at,
            })).collect::<Vec<_>>(),
            "note": "Shares are estimates: the plan also counts use outside Desk's sessions.",
        })),
    })
}

/// The account the ledger names Claude Code's calls by, and the key the
/// plan readings know it by: the CLI's sign-in now.
fn claude_account(sign_in: &SignIn) -> Option<(String, String)> {
    let SignIn::In { who, ids, .. } = sign_in else {
        return None;
    };
    let label = who.clone().or_else(|| ids.first().cloned())?;
    let key = account_key(PlanKind::Claude, ids)?;
    Some((label, key))
}

impl CrewHub {
    pub(super) async fn spend(&self, input: &Value) -> Result<Value, String> {
        let internal = |e: workforce_desk::DeskError| {
            tracing::warn!(error = %e, "reading the ledger for crew.spend failed");
            "Spend couldn't be read just now.".to_string()
        };
        let by = SpendBy::parse(input["by"].as_str().unwrap_or("session"))
            .map_err(|_| "Say by as session, project, day, engine or account.".to_string())?;
        let now = Utc::now();
        let from = since_of(input["since"].as_str().unwrap_or("today"), |p| {
            self.desk.period_start(p, now)
        })?;
        let limit = input["limit"].as_u64().unwrap_or(10).clamp(1, 50) as usize;
        let view = self
            .desk
            .spend(&SpendQuery {
                by,
                from,
                to: now,
                limit,
                filter: Filter::default(),
            })
            .await
            .map_err(internal)?;
        let (plan, windows) = match by {
            SpendBy::Session => self.claude_windows(now).await.map_err(internal)?,
            _ => (None, Vec::new()),
        };
        Ok(answer(&view, &windows, plan.as_ref()))
    }

    /// This computer's latest reading of the Claude plan it is signed in
    /// to, and what Desk's sessions on that account spent in each window it
    /// reports. None where the account can't be matched to a reading.
    async fn claude_windows(
        &self,
        now: DateTime<Utc>,
    ) -> Result<(Option<Reading>, Vec<WindowSpend>), workforce_desk::DeskError> {
        let Some((label, key)) = claude_account(self.desk.sign_ins().of(Engine::Claude)) else {
            return Ok((None, Vec::new()));
        };
        let Some(floor) = crate::floor::installed() else {
            return Ok((None, Vec::new()));
        };
        let reading = match floor.store.readings().await {
            Ok(readings) => readings
                .into_iter()
                .filter(|r| {
                    r.kind == PlanKind::Claude
                        && r.login == LoginRef::Mac
                        && r.account.as_deref() == Some(key.as_str())
                })
                .max_by_key(|r| r.observed_at),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "reading plan readings for crew.spend failed");
                None
            }
        };
        let Some(reading) = reading else {
            return Ok((None, Vec::new()));
        };
        let mut windows = Vec::new();
        for w in &reading.windows {
            let Some(from) = window_from(w, now) else {
                continue;
            };
            let spend = self
                .desk
                .spend(&SpendQuery {
                    by: SpendBy::Session,
                    from,
                    to: now,
                    limit: usize::MAX,
                    filter: Filter {
                        engine: Some(Engine::Claude),
                        account: Some(label.clone()),
                        session: None,
                    },
                })
                .await?;
            windows.push(WindowSpend {
                window: w.clone(),
                spend,
            });
        }
        Ok((Some(reading), windows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use uuid::Uuid;
    use workforce_desk::ledger::{Billing, Tokens};

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().to_utc()
    }

    fn totals(usd: f64, output: u64) -> Totals {
        Totals {
            tokens: Tokens {
                input: 1_200,
                output,
                cache_read: 4_000_000,
                cache_write: 300_000,
                reasoning: 0,
            },
            usd,
            calls: 40,
            unpriced_calls: 0,
            unpriced_models: BTreeSet::new(),
            billing: [Billing::Plan].into(),
        }
    }

    fn view(rows: &[(Uuid, &str, f64)]) -> SpendView {
        SpendView {
            from: at("2026-10-31T00:00:00Z"),
            to: at("2026-10-31T12:00:00Z"),
            by: SpendBy::Session,
            total: totals(rows.iter().map(|r| r.2).sum(), 9_000),
            rows: rows
                .iter()
                .map(|(id, name, usd)| SpendRow {
                    key: id.to_string(),
                    label: name.to_string(),
                    session: Some(*id),
                    engine: Some(Engine::Claude),
                    folder: Some(format!("/Users/allan/dev/{name}")),
                    totals: totals(*usd, 3_000),
                })
                .collect(),
            more: 0,
        }
    }

    fn weekly(used: f64) -> Window {
        Window {
            used_percent: used,
            resets_at: Some(at("2026-11-04T09:00:00Z")),
            minutes: 10_080,
        }
    }

    #[test]
    fn a_reading_with_only_a_weekly_window_gives_a_weekly_share_and_no_five_hour_line() {
        let (web, api) = (Uuid::new_v4(), Uuid::new_v4());
        let today = view(&[(web, "acme-web", 30.0), (api, "atlas-api", 10.0)]);
        let reading = Reading {
            kind: PlanKind::Claude,
            login: LoginRef::Mac,
            account: Some("k".into()),
            plan: Some("ultra".into()),
            windows: vec![weekly(40.0)],
            observed_at: at("2026-10-31T11:55:00Z"),
        };
        let windows = [WindowSpend {
            window: weekly(40.0),
            spend: view(&[(web, "acme-web", 60.0), (api, "atlas-api", 20.0)]),
        }];
        let a = answer(&today, &windows, Some(&reading));
        let first = &a["rows"][0];
        assert_eq!(first["name"], "acme-web");
        assert_eq!(first["share_of_spend_percent"], 75.0);
        assert_eq!(first["plan"].as_array().unwrap().len(), 1);
        assert_eq!(first["plan"][0]["window"], "weekly");
        assert_eq!(first["plan"][0]["session_share_percent_estimate"], 30.0);
        assert!(!a.to_string().contains("5-hour"), "{a}");
        assert_eq!(
            a["plan"]["plan"], "ultra",
            "the plan's name is as the CLI says it"
        );
        assert_eq!(a["total"]["tokens"]["cache_read"], 4_000_000);
    }

    #[test]
    fn a_window_that_reset_since_its_reading_or_has_no_reset_gives_no_share() {
        let now = at("2026-10-31T12:00:00Z");
        assert_eq!(
            window_from(&weekly(40.0), now),
            Some(at("2026-10-28T09:00:00Z"))
        );
        let reset = Window {
            resets_at: Some(at("2026-10-31T10:00:00Z")),
            minutes: 300,
            used_percent: 90.0,
        };
        assert_eq!(window_from(&reset, now), None);
        assert_eq!(
            window_from(
                &Window {
                    resets_at: None,
                    ..reset
                },
                now
            ),
            None
        );
    }

    #[test]
    fn since_reads_periods_and_dates() {
        let start = |p: Period| match p {
            Period::Day => at("2026-10-31T00:00:00Z"),
            Period::FiveHours => at("2026-10-31T07:00:00Z"),
            Period::Week => at("2026-10-26T00:00:00Z"),
            Period::Month => at("2026-10-01T00:00:00Z"),
        };
        assert_eq!(
            since_of("today", start).unwrap(),
            at("2026-10-31T00:00:00Z")
        );
        assert_eq!(since_of("week", start).unwrap(), at("2026-10-26T00:00:00Z"));
        assert_eq!(
            since_of("2026-10-30", start).unwrap(),
            at("2026-10-30T00:00:00Z")
        );
        assert_eq!(
            since_of("2026-10-30T09:00:00Z", start).unwrap(),
            at("2026-10-30T09:00:00Z")
        );
        assert!(since_of("last tuesday", start).is_err());
        assert_eq!(window_name(300), "5-hour");
        assert_eq!(window_name(10_080), "weekly");
        assert_eq!(window_name(2_880), "2-day");
    }

    #[test]
    fn a_claude_account_is_its_email_and_the_readings_key_of_its_ids() {
        let signed = workforce_desk::engine::claude_sign_in(
            r#"{"loggedIn":true,"authMethod":"claude.ai","email":"allan@example.com","orgId":"org-1","subscriptionType":"ultra"}"#,
        );
        let (label, key) = claude_account(&signed).unwrap();
        assert_eq!(label, "allan@example.com");
        assert_eq!(
            Some(key),
            account_key(
                PlanKind::Claude,
                &["org-1".to_string(), "allan@example.com".to_string()]
            )
        );
        assert_eq!(claude_account(&SignIn::Out), None);
    }
}
