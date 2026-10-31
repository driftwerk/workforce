//! The cost ledger: what each session spent, read from its engine's own
//! record, one row per model call, kept once by the call's own key. The
//! readers here are pure: they turn a record's lines into [`Spend`]s, and
//! the desk's I/O shell (`service/ledger.rs`) reads the files, prices the
//! rows and keeps them.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::engine::Engine;
use crate::model::text_enum;

/// A call's tokens. `input` is the fresh (uncached) part; cache reads and
/// writes are their own. `reasoning` is already inside `output`, kept
/// apart only to be shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

impl Tokens {
    /// Every token the call moved: reasoning is counted in `output`.
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    fn add(&mut self, t: &Tokens) {
        self.input += t.input;
        self.output += t.output;
        self.cache_read += t.cache_read;
        self.cache_write += t.cache_write;
        self.reasoning += t.reasoning;
    }

    /// Each count the larger of the two: what two readings of one call
    /// know between them (a streamed call's earlier line says less).
    fn most(&self, t: &Tokens) -> Tokens {
        Tokens {
            input: self.input.max(t.input),
            output: self.output.max(t.output),
            cache_read: self.cache_read.max(t.cache_read),
            cache_write: self.cache_write.max(t.cache_write),
            reasoning: self.reasoning.max(t.reasoning),
        }
    }
}

text_enum!(Priced {
    Priced => "priced",
    Reported => "reported",
    None => "none",
});

text_enum!(Billing {
    Plan => "plan",
    Key => "key",
    Vendor => "vendor",
    Unknown => "unknown",
});

/// One model call as an engine recorded it.
#[derive(Debug, Clone, PartialEq)]
pub struct Spend {
    /// The call's own id, unique across every session of its engine: a
    /// forked or resumed conversation, and a sub-agent forked from it,
    /// copy their history's calls under the same ids.
    pub key: String,
    pub at: DateTime<Utc>,
    pub model: Option<String>,
    pub tokens: Tokens,
    /// What the engine itself said it cost; `None` for the hub to price.
    pub usd: Option<f64>,
    pub priced: Priced,
    /// How it was paid, where the record itself says.
    pub billing: Option<Billing>,
}

/// Prices tokens with the hub's price table; the desk can't read it.
pub trait Pricer: Send + Sync {
    /// What `t` on `model` costs at API list prices; `None` for a model the
    /// table has no row for (never a guess).
    fn usd(&self, engine: Engine, billing: Billing, model: &str, t: &Tokens) -> Option<f64>;
}

fn number(v: &Value, pointer: &str) -> u64 {
    v.pointer(pointer).and_then(Value::as_u64).unwrap_or(0)
}

fn time(v: &Value, pointer: &str) -> Option<DateTime<Utc>> {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.to_utc())
}

/// One line of a Claude Code transcript, as the call it records: an
/// `assistant` row's `message.usage` (`input_tokens`, `output_tokens`,
/// `cache_read_input_tokens`, `cache_creation_input_tokens`; thinking is
/// within output), keyed by `message.id`. A message whose content blocks
/// each repeat its usage is one call. Claude Code's own `<synthetic>`
/// messages cost nothing.
pub fn claude_line(line: &[u8]) -> Option<Spend> {
    // Most lines aren't calls; skip them before parsing.
    if !contains(line, b"\"usage\"") {
        return None;
    }
    let v: Value = serde_json::from_slice(line).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let message = v.get("message")?;
    let usage = message.get("usage").filter(|u| u.is_object())?;
    let model = message
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty());
    if model == Some("<synthetic>") {
        return None;
    }
    let key = message
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| v.get("requestId").and_then(Value::as_str))
        .filter(|k| !k.is_empty())?;
    Some(Spend {
        key: key.to_string(),
        at: time(&v, "/timestamp")?,
        model: model.map(str::to_string),
        tokens: Tokens {
            input: number(usage, "/input_tokens"),
            output: number(usage, "/output_tokens"),
            cache_read: number(usage, "/cache_read_input_tokens"),
            cache_write: number(usage, "/cache_creation_input_tokens"),
            reasoning: number(usage, "/output_tokens_details/thinking_tokens"),
        },
        usd: None,
        priced: Priced::None,
        billing: None,
    })
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// What a Codex rollout's reader carries from line to line, kept with its
/// cursor: the turn and model its token counts are of, and whether its
/// rate limits named a plan.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexState {
    #[serde(default)]
    pub turn: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub plan: bool,
}

/// Codex's token breakdown (snake_case in its rollout, camelCase on its
/// app-server) as tokens. Its `input_tokens` include the cached ones, and
/// its `output_tokens` the reasoning ones.
fn codex_tokens(b: &Value, camel: bool) -> Tokens {
    let n = |snake: &str, camel_name: &str| number(b, if camel { camel_name } else { snake });
    let input = n("/input_tokens", "/inputTokens");
    let cached = n("/cached_input_tokens", "/cachedInputTokens");
    let written = n("/cache_write_input_tokens", "/cacheWriteInputTokens");
    Tokens {
        input: input.saturating_sub(cached).saturating_sub(written),
        output: n("/output_tokens", "/outputTokens"),
        cache_read: cached,
        cache_write: written,
        reasoning: n("/reasoning_output_tokens", "/reasoningOutputTokens"),
    }
}

/// A Codex call's key: its turn and the thread's running total after it,
/// which the rollout's `token_usage_record` and `token_count` rows and the
/// app-server's `thread/tokenUsage/updated` all say alike.
fn codex_key(turn: &str, thread_total: u64) -> String {
    format!("{turn}:{thread_total}")
}

/// One line of a Codex rollout (`~/.codex/sessions/…/rollout-….jsonl`),
/// as the call it records, with `state` carried on: `turn_context` and
/// `task_started` name the turn and model; `token_usage_record` and
/// `event_msg` `token_count` (verified on CLI 0.164; both appear for one
/// call, with the same numbers) are the call.
pub fn codex_line(line: &[u8], state: &mut CodexState) -> Option<Spend> {
    let v: Value = serde_json::from_slice(line).ok()?;
    let p = v.get("payload")?;
    let at = time(&v, "/timestamp");
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let spend = |key: String, tokens: Tokens, state: &CodexState| {
        Some(Spend {
            key,
            at: at?,
            model: state.model.clone(),
            tokens,
            usd: None,
            priced: Priced::None,
            billing: state.plan.then_some(Billing::Plan),
        })
    };
    match (
        v.get("type").and_then(Value::as_str)?,
        p.get("type").and_then(Value::as_str),
    ) {
        ("turn_context", _) => {
            state.turn = text(p, "turn_id").or(state.turn.take());
            state.model = text(p, "model").or(state.model.take());
            None
        }
        ("event_msg", Some("task_started")) => {
            state.turn = text(p, "turn_id").or(state.turn.take());
            None
        }
        ("token_usage_record", _) => {
            let turn = text(p, "turn_id").or_else(|| state.turn.clone())?;
            let total = number(p, "/thread_token_usage/total_tokens");
            spend(
                codex_key(&turn, total),
                codex_tokens(p.get("usage")?, false),
                state,
            )
        }
        ("event_msg", Some("token_count")) => {
            if p.pointer("/rate_limits/plan_type")
                .and_then(Value::as_str)
                .is_some_and(|t| !t.is_empty() && t != "unknown")
            {
                state.plan = true;
            }
            let info = p.get("info").filter(|i| i.is_object())?;
            let turn = state.turn.clone()?;
            let total = number(info, "/total_token_usage/total_tokens");
            spend(
                codex_key(&turn, total),
                codex_tokens(info.get("last_token_usage")?, false),
                state,
            )
        }
        _ => None,
    }
}

/// Codex's app-server's `thread/tokenUsage/updated` (`{ threadId, turnId,
/// tokenUsage: { last, total } }`, from its 0.164 schema), as the call it
/// reports, under the key its rollout gives the same call. It names no
/// model: the rollout's row fills it in.
pub fn codex_spend(params: &Value, at: DateTime<Utc>) -> Option<Spend> {
    let turn = params.get("turnId").and_then(Value::as_str)?;
    let usage = params.get("tokenUsage")?;
    let total = number(usage, "/total/totalTokens");
    Some(Spend {
        key: codex_key(turn, total),
        at,
        model: None,
        tokens: codex_tokens(usage.get("last")?, true),
        usd: None,
        priced: Priced::None,
        billing: None,
    })
}

/// `spends` with each call once: what its readings know between them.
pub fn once_each(spends: Vec<Spend>) -> Vec<Spend> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<Spend> = Vec::with_capacity(spends.len());
    for s in spends {
        match seen.get(&s.key) {
            Some(&i) => {
                let known = &mut out[i];
                known.tokens = known.tokens.most(&s.tokens);
                known.model = known.model.take().or(s.model);
                known.billing = known.billing.or(s.billing);
            }
            None => {
                seen.insert(s.key.clone(), out.len());
                out.push(s);
            }
        }
    }
    out
}

text_enum!(Period {
    FiveHours => "five_hours",
    Day => "day",
    Week => "week",
    Month => "month",
});

/// Where a period began in `tz`: five hours ago, local midnight, Monday's
/// midnight, the 1st's. A midnight a clock change skips is its first
/// instant after.
pub fn window_start(period: Period, now: DateTime<Utc>, tz: Tz) -> DateTime<Utc> {
    let local = now.with_timezone(&tz).date_naive();
    let day = match period {
        Period::FiveHours => return now - Duration::hours(5),
        Period::Day => local,
        Period::Week => local - Duration::days(i64::from(local.weekday().num_days_from_monday())),
        Period::Month => local.with_day(1).unwrap_or(local),
    };
    midnight(day, tz).unwrap_or(now - Duration::hours(24))
}

fn midnight(day: NaiveDate, tz: Tz) -> Option<DateTime<Utc>> {
    (0..4).find_map(|hour| {
        day.and_hms_opt(hour, 0, 0)?
            .and_local_timezone(tz)
            .earliest()
            .map(|t| t.with_timezone(&Utc))
    })
}

text_enum!(SpendBy {
    Session => "session",
    Project => "project",
    Day => "day",
    Engine => "engine",
    Account => "account",
});

/// The ledger summed by everything a view groups by: what the store
/// returns, one cell per session, model, account, billing and local day.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub session: Uuid,
    pub engine: Engine,
    pub model: Option<String>,
    pub account: Option<String>,
    pub billing: Billing,
    pub day: NaiveDate,
    pub tokens: Tokens,
    /// What its priced calls cost.
    pub usd: f64,
    pub calls: u64,
    /// Its calls with no price.
    pub unpriced: u64,
}

/// What a view names a session by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub name: String,
    pub cwd: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub tokens: Tokens,
    /// What the priced calls would cost at API list prices: what a key
    /// pays, and what a plan's calls are worth ("API-equivalent").
    pub usd: f64,
    pub calls: u64,
    /// Calls with no price, and the models they ran on.
    pub unpriced_calls: u64,
    pub unpriced_models: BTreeSet<String>,
    /// How its calls were paid.
    pub billing: BTreeSet<Billing>,
}

impl Totals {
    fn add(&mut self, c: &Cell) {
        self.tokens.add(&c.tokens);
        self.usd += c.usd;
        self.calls += c.calls;
        self.unpriced_calls += c.unpriced;
        if c.unpriced > 0 {
            self.unpriced_models
                .insert(c.model.clone().unwrap_or_else(|| "an unnamed model".into()));
        }
        self.billing.insert(c.billing);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpendRow {
    pub key: String,
    pub label: String,
    /// The session it is, when grouped by session.
    pub session: Option<Uuid>,
    pub engine: Option<Engine>,
    /// The session's folder, or the project's.
    pub folder: Option<String>,
    pub totals: Totals,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpendView {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub by: SpendBy,
    pub total: Totals,
    pub rows: Vec<SpendRow>,
    /// Rows past `limit`, left out.
    pub more: usize,
}

/// What Desk calls a session it no longer has.
pub const FORGOTTEN: &str = "A session since forgotten";
/// What Desk calls the calls it can't say the account of.
pub const UNKNOWN_ACCOUNT: &str = "Account not known";

/// `cells` grouped `by`, the costliest first (by day: the latest first),
/// at most `limit` rows. `label` names a session, if Desk still has it.
pub fn fold(
    cells: &[Cell],
    by: SpendBy,
    label: impl Fn(Uuid) -> Option<Label>,
    limit: usize,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> SpendView {
    let mut total = Totals::default();
    let mut rows: BTreeMap<String, SpendRow> = BTreeMap::new();
    let mut labels: HashMap<Uuid, Option<Label>> = HashMap::new();
    for c in cells {
        total.add(c);
        let l = labels.entry(c.session).or_insert_with(|| label(c.session));
        let (key, row) = match by {
            SpendBy::Session => (
                c.session.to_string(),
                SpendRow {
                    key: c.session.to_string(),
                    label: l.as_ref().map_or(FORGOTTEN.into(), |l| l.name.clone()),
                    session: Some(c.session),
                    engine: Some(c.engine),
                    folder: l.as_ref().map(|l| l.cwd.clone()),
                    totals: Totals::default(),
                },
            ),
            SpendBy::Project => {
                let folder = l
                    .as_ref()
                    .map(|l| crate::worktree::project_of(&l.cwd).to_string());
                let key = folder.clone().unwrap_or_default();
                let name = folder.as_deref().map_or(FORGOTTEN.into(), |f| {
                    std::path::Path::new(f)
                        .file_name()
                        .map_or_else(|| f.to_string(), |n| n.to_string_lossy().into_owned())
                });
                (
                    key.clone(),
                    SpendRow {
                        key,
                        label: name,
                        session: None,
                        engine: None,
                        folder,
                        totals: Totals::default(),
                    },
                )
            }
            SpendBy::Day => {
                let day = c.day.format("%Y-%m-%d").to_string();
                (
                    day.clone(),
                    SpendRow {
                        key: day.clone(),
                        label: c.day.format("%a %-d %b").to_string(),
                        session: None,
                        engine: None,
                        folder: None,
                        totals: Totals::default(),
                    },
                )
            }
            SpendBy::Engine => (
                c.engine.as_str().to_string(),
                SpendRow {
                    key: c.engine.as_str().to_string(),
                    label: c.engine.title().to_string(),
                    session: None,
                    engine: Some(c.engine),
                    folder: None,
                    totals: Totals::default(),
                },
            ),
            SpendBy::Account => {
                let key = c.account.clone().unwrap_or_default();
                (
                    key.clone(),
                    SpendRow {
                        label: c.account.clone().unwrap_or_else(|| UNKNOWN_ACCOUNT.into()),
                        key,
                        session: None,
                        engine: None,
                        folder: None,
                        totals: Totals::default(),
                    },
                )
            }
        };
        rows.entry(key).or_insert(row).totals.add(c);
    }
    let mut rows: Vec<SpendRow> = rows.into_values().collect();
    match by {
        SpendBy::Day => rows.sort_by(|a, b| b.key.cmp(&a.key)),
        _ => rows.sort_by(|a, b| {
            b.totals
                .usd
                .total_cmp(&a.totals.usd)
                .then(b.totals.tokens.total().cmp(&a.totals.tokens.total()))
                .then(a.label.cmp(&b.label))
        }),
    }
    let more = rows.len().saturating_sub(limit);
    rows.truncate(limit);
    SpendView {
        from,
        to,
        by,
        total,
        rows,
        more,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: &str = include_str!("../fixtures/ledger/claude.jsonl");
    const CODEX: &str = include_str!("../fixtures/ledger/codex-rollout.jsonl");

    fn claude(lines: &str) -> Vec<Spend> {
        once_each(
            lines
                .lines()
                .filter_map(|l| claude_line(l.as_bytes()))
                .collect(),
        )
    }

    fn codex(lines: &str) -> (Vec<Spend>, CodexState) {
        let mut state = CodexState::default();
        let spends = lines
            .lines()
            .filter_map(|l| codex_line(l.as_bytes(), &mut state))
            .collect();
        (once_each(spends), state)
    }

    #[test]
    fn a_claude_message_whose_blocks_repeat_its_usage_counts_once_and_synthetic_ones_not_at_all() {
        let spends = claude(CLAUDE);
        let keys: Vec<&str> = spends.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["msg_01", "msg_02"]);
        assert_eq!(
            spends[0].tokens,
            Tokens {
                input: 12,
                output: 350,
                cache_read: 0,
                cache_write: 20000,
                reasoning: 120
            }
        );
        assert_eq!(spends[0].model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(spends[0].at.to_rfc3339(), "2026-10-31T09:00:04+00:00");
        assert_eq!(spends[1].tokens.cache_read, 20000);
        assert_eq!(
            spends.iter().map(|s| s.tokens.total()).sum::<u64>(),
            12 + 350 + 20000 + 8 + 400 + 20000 + 90,
            "thinking is inside output; a tool's own usage isn't a call"
        );
    }

    #[test]
    fn a_streamed_call_read_twice_keeps_what_it_knew_last() {
        let early = r#"{"type":"assistant","timestamp":"2026-10-31T09:00:04Z","message":{"id":"msg_9","model":"claude-opus-5-5","usage":{"input_tokens":3,"output_tokens":1}}}"#;
        let late = r#"{"type":"assistant","timestamp":"2026-10-31T09:00:09Z","message":{"id":"msg_9","model":"claude-opus-5-5","usage":{"input_tokens":3,"output_tokens":700}}}"#;
        let spends = claude(&format!("{early}\n{late}"));
        assert_eq!(spends.len(), 1);
        assert_eq!(spends[0].tokens.output, 700);
    }

    #[test]
    fn a_line_without_an_id_falls_back_to_its_request_and_one_without_either_is_no_call() {
        let by_request = r#"{"type":"assistant","timestamp":"2026-10-31T09:00:04Z","requestId":"req_7","message":{"model":"claude-opus-5-5","usage":{"input_tokens":3,"output_tokens":1}}}"#;
        assert_eq!(claude_line(by_request.as_bytes()).unwrap().key, "req_7");
        let neither = r#"{"type":"assistant","timestamp":"2026-10-31T09:00:04Z","message":{"usage":{"input_tokens":3}}}"#;
        assert_eq!(claude_line(neither.as_bytes()), None);
        assert_eq!(claude_line(b"{\"usage\": tru"), None, "a torn line");
    }

    #[test]
    fn a_codex_rollout_gives_each_call_once_with_its_turns_model_and_plan() {
        let (spends, state) = codex(CODEX);
        let keys: Vec<&str> = spends.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["turn-1:15314", "turn-1:30704"]);
        assert_eq!(
            spends[0].tokens,
            Tokens {
                input: 15268 - 12032,
                output: 46,
                cache_read: 12032,
                cache_write: 0,
                reasoning: 10
            },
            "cached input is Codex's input too: it is taken out"
        );
        assert_eq!(spends[1].model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(
            spends[1].billing,
            Some(Billing::Plan),
            "its rate limits named a plan"
        );
        assert_eq!(
            state,
            CodexState {
                turn: Some("turn-1".into()),
                model: Some("gpt-5.6-sol".into()),
                plan: true
            }
        );
    }

    #[test]
    fn codexs_live_notice_and_its_rollout_give_a_call_the_same_key() {
        let notice = serde_json::json!({
            "threadId": "0199aaaa-0000-7000-8000-atlas0000001",
            "turnId": "turn-1",
            "tokenUsage": {
                "last": {"inputTokens": 15376, "cachedInputTokens": 12032, "outputTokens": 14,
                         "reasoningOutputTokens": 0, "totalTokens": 15390},
                "total": {"inputTokens": 30644, "cachedInputTokens": 24064, "outputTokens": 60,
                          "reasoningOutputTokens": 10, "totalTokens": 30704}
            }
        });
        let live = codex_spend(&notice, Utc::now()).unwrap();
        let (rollout, _) = codex(CODEX);
        assert_eq!(live.key, rollout[1].key);
        assert_eq!(live.tokens, rollout[1].tokens);
        assert_eq!(live.model, None, "the rollout names it");
    }

    #[test]
    fn a_rollout_read_from_the_middle_goes_on_with_the_state_it_was_left_with() {
        let lines: Vec<&str> = CODEX.lines().collect();
        let (first, state) = codex(&lines[..5].join("\n"));
        assert_eq!(first.len(), 1);
        let mut carried = state;
        let rest: Vec<Spend> = lines[5..]
            .iter()
            .filter_map(|l| codex_line(l.as_bytes(), &mut carried))
            .collect();
        let rest = once_each(rest);
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].model.as_deref(), Some("gpt-5.6-sol"));
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().to_utc()
    }

    #[test]
    fn periods_start_at_local_midnights_across_a_clock_change_and_a_month_end() {
        let london: Tz = "Europe/London".parse().unwrap();
        // Sunday 2026-10-25 is when London's clocks go back.
        let now = at("2026-10-26T12:00:00Z");
        assert_eq!(
            window_start(Period::Day, now, london),
            at("2026-10-26T00:00:00Z")
        );
        assert_eq!(
            window_start(Period::Week, now, london),
            at("2026-10-26T00:00:00Z"),
            "Monday"
        );
        assert_eq!(
            window_start(Period::Week, at("2026-10-25T12:00:00Z"), london),
            at("2026-10-18T23:00:00Z"),
            "the Monday before, in summer time"
        );
        let paris: Tz = "Europe/Paris".parse().unwrap();
        assert_eq!(
            window_start(Period::Month, at("2026-10-31T23:30:00Z"), paris),
            at("2026-10-31T23:00:00Z"),
            "past midnight in Paris it is November"
        );
        assert_eq!(
            window_start(Period::Month, at("2026-10-31T23:30:00Z"), london),
            at("2026-09-30T23:00:00Z"),
            "in London it is still October, begun in summer time"
        );
        assert_eq!(
            window_start(Period::FiveHours, now, london),
            now - Duration::hours(5)
        );
        let sao_paulo: Tz = "America/Sao_Paulo".parse().unwrap();
        assert!(window_start(Period::Day, now, sao_paulo) <= now);
    }

    fn cell(session: Uuid, model: &str, usd: f64, unpriced: u64) -> Cell {
        Cell {
            session,
            engine: Engine::Claude,
            model: Some(model.into()),
            account: Some("allan@example.com".into()),
            billing: Billing::Plan,
            day: NaiveDate::from_ymd_opt(2026, 10, 31).unwrap(),
            tokens: Tokens {
                input: 10,
                output: 20,
                cache_read: 1000,
                cache_write: 100,
                reasoning: 0,
            },
            usd,
            calls: 3,
            unpriced,
        }
    }

    #[test]
    fn sessions_are_ranked_by_what_they_cost_and_unpriced_models_are_named() {
        let (a, b, gone) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let cells = [
            cell(a, "claude-opus-5-5", 1.5, 0),
            cell(b, "claude-opus-5-5", 4.0, 0),
            cell(b, "claude-mystery-1", 0.0, 3),
            cell(gone, "claude-opus-5-5", 0.25, 0),
        ];
        let label = |id: Uuid| {
            (id != gone).then(|| Label {
                name: if id == a {
                    "acme-web".into()
                } else {
                    "Atlas API".into()
                },
                cwd: if id == a {
                    "/dev/acme-web/.worktrees/login-race".into()
                } else {
                    "/dev/atlas".into()
                },
            })
        };
        let (from, to) = (at("2026-10-31T00:00:00Z"), at("2026-10-31T12:00:00Z"));
        let v = fold(&cells, SpendBy::Session, label, 2, from, to);
        let names: Vec<&str> = v.rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(names, ["Atlas API", "acme-web"]);
        assert_eq!(v.more, 1);
        assert_eq!(v.rows[0].totals.calls, 6);
        assert!(v.rows[0]
            .totals
            .unpriced_models
            .contains("claude-mystery-1"));
        assert!(
            (v.total.usd - 5.75).abs() < 1e-9,
            "the total counts every row"
        );
        let all = fold(&cells, SpendBy::Session, label, 10, from, to);
        assert_eq!(all.rows[2].label, FORGOTTEN);

        let projects = fold(&cells, SpendBy::Project, label, 10, from, to);
        assert_eq!(
            projects.rows[1].label, "acme-web",
            "a Desk worktree is its repo's"
        );
        assert_eq!(projects.rows[1].folder.as_deref(), Some("/dev/acme-web"));
        let accounts = fold(&cells, SpendBy::Account, label, 10, from, to);
        assert_eq!(accounts.rows.len(), 1);
        assert_eq!(accounts.rows[0].label, "allan@example.com");
        let engines = fold(&cells, SpendBy::Engine, label, 10, from, to);
        assert_eq!(engines.rows[0].label, "Claude Code");
    }

    #[test]
    fn days_are_listed_latest_first() {
        let s = Uuid::new_v4();
        let mut earlier = cell(s, "claude-opus-5-5", 9.0, 0);
        earlier.day = NaiveDate::from_ymd_opt(2026, 10, 30).unwrap();
        let cells = [earlier, cell(s, "claude-opus-5-5", 1.0, 0)];
        let v = fold(
            &cells,
            SpendBy::Day,
            |_| None,
            10,
            at("2026-10-26T00:00:00Z"),
            at("2026-10-31T12:00:00Z"),
        );
        let keys: Vec<&str> = v.rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["2026-10-31", "2026-10-30"]);
        assert_eq!(v.rows[0].label, "Sat 31 Oct");
    }
}
