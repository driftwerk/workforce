//! An overseer asked to change its own `[oversight]` (its level, its Act
//! fully budget, its switches): those are the person's, never its own to
//! change, by any tool or any other budget. It says so plainly and posts a
//! one-click card, `crew.ask_oversight_change`: a suggestion only the
//! person can accept, which then writes the change through the person's
//! own path (the agent's config.toml, as Desk's settings save it), so the
//! revision is theirs.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use workforce_common::oversight::{Level, MAX_FULL_BUDGET_USD};
use workforce_orchestrator::workflow_store::EditContext;

use super::{name_of, CrewHub};

pub const NAME: &str = "crew.ask_oversight_change";

pub const DESCRIPTION: &str = "Ask the person to change your own oversight settings: your level \
(watch, suggest, act, full), Act fully's daily budget (raise or remove it, or clear what today's \
count has spent so far), Act fully anywhere, or your switches. Your own limits are never yours to \
change, by this or any other tool or budget: this only posts a card beside your message, which \
the person approves with one click (or dismisses), and only then is it changed, by them. Use it \
whenever they ask you to change any of these, and tell them plainly that you can't change your \
own limits yourself. Act fully's budget counts real money only (calls paid by an API key, \
Bedrock or Vertex); their Claude and ChatGPT plans never count. It is not the `budget` tool's \
model-spend ceiling.";

/// The `[oversight.act]` switches by their keys, in the order Desk shows
/// them.
pub const SWITCHES: [&str; 10] = [
    "nudge",
    "resume_restart",
    "read_only_approvals",
    "hand_out_work",
    "interrupt",
    "message_agents",
    "message_terminal_sessions",
    "answer_sessions",
    "hand_off",
    "set_mode",
];

/// What a switch is called where the person reads it.
fn switch_title(key: &str) -> &'static str {
    match key {
        "nudge" => "nudging sessions",
        "resume_restart" => "resuming and restarting sessions",
        "read_only_approvals" => "allowing read-only requests",
        "hand_out_work" => "handing out work",
        "interrupt" => "interrupting turns",
        "message_agents" => "messaging agents",
        "message_terminal_sessions" => "messaging sessions in other terminals",
        "answer_sessions" => "answering sessions",
        "hand_off" => "suggesting another engine",
        "set_mode" => "changing permission modes",
        _ => "a switch",
    }
}

pub fn schema() -> Value {
    let switches: serde_json::Map<String, Value> = SWITCHES
        .iter()
        .map(|k| ((*k).to_string(), json!({ "type": "boolean" })))
        .collect();
    json!({ "type": "object", "properties": {
        "level": { "type": "string", "enum": ["watch", "suggest", "act", "full"], "description": "The level to run at" },
        "budget_usd_per_day": { "type": "number", "exclusiveMinimum": 0, "maximum": MAX_FULL_BUDGET_USD, "description": "Act fully's daily budget in US dollars of real money" },
        "remove_budget": { "type": "boolean", "description": "Remove Act fully's daily budget (only Act fully anywhere runs without one)" },
        "clear_spent_today": { "type": "boolean", "description": "Start today's count of Act fully's budget again from when the person approves" },
        "anywhere": { "type": "boolean", "description": "Act fully anywhere: not limited to the named repositories and workflows" },
        "act": { "type": "object", "properties": switches, "additionalProperties": false, "description": "Switches to turn on (true) or off (false)" },
        "reason": { "type": "string", "description": "What the person asked for, in a sentence they read on the card" }
    }, "required": ["reason"] })
}

/// A change to an overseer's own `[oversight]`, as asked for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Change {
    pub level: Option<Level>,
    /// Act fully's daily budget in cents: `Some(None)` removes it.
    pub budget_cents: Option<Option<u64>>,
    pub clear_spent_today: bool,
    pub anywhere: Option<bool>,
    pub act: BTreeMap<String, bool>,
}

impl Change {
    /// The change `input` asks for, or what is wrong with it.
    pub(crate) fn parse(input: &Value) -> Result<Self, String> {
        let mut c = Self::default();
        if let Some(l) = input.get("level").filter(|v| !v.is_null()) {
            c.level = Some(match l.as_str().map(str::trim) {
                Some("watch") => Level::Watch,
                Some("suggest") => Level::Suggest,
                Some("act") => Level::Act,
                Some("full") => Level::Full,
                _ => return Err("level must be watch, suggest, act or full.".into()),
            });
        }
        let remove = input["remove_budget"].as_bool().unwrap_or(false);
        match input.get("budget_usd_per_day").filter(|v| !v.is_null()) {
            Some(_) if remove => {
                return Err("Say a new budget or remove it, not both.".into());
            }
            Some(v) => {
                let usd = v
                    .as_f64()
                    .filter(|u| u.is_finite() && *u > 0.0 && *u <= MAX_FULL_BUDGET_USD);
                let usd = usd.ok_or(format!(
                    "budget_usd_per_day must be dollars above 0 and up to {MAX_FULL_BUDGET_USD}."
                ))?;
                c.budget_cents = Some(Some(((usd * 100.0).round() as u64).max(1)));
            }
            None if remove => c.budget_cents = Some(None),
            None => {}
        }
        c.clear_spent_today = input["clear_spent_today"].as_bool().unwrap_or(false);
        c.anywhere = input["anywhere"].as_bool();
        if let Some(act) = input.get("act").filter(|v| !v.is_null()) {
            let act = act
                .as_object()
                .ok_or("act must be switches by name, each true or false.")?;
            for (k, v) in act {
                if !SWITCHES.contains(&k.as_str()) {
                    return Err(format!(
                        "{k} isn't one of your switches: {}.",
                        SWITCHES.join(", ")
                    ));
                }
                let on = v.as_bool().ok_or(format!("{k} must be true or false."))?;
                c.act.insert(k.clone(), on);
            }
        }
        if c == Self::default() {
            return Err("Say what to change: level, budget_usd_per_day, remove_budget, clear_spent_today, anywhere or act.".into());
        }
        Ok(c)
    }

    /// It as the tool's input, to keep in a suggestion.
    #[cfg(test)]
    pub(crate) fn input(&self) -> Value {
        let mut o = serde_json::Map::new();
        if let Some(l) = self.level {
            o.insert("level".into(), json!(l.as_str()));
        }
        match self.budget_cents {
            Some(Some(c)) => {
                o.insert("budget_usd_per_day".into(), json!(c as f64 / 100.0));
            }
            Some(None) => {
                o.insert("remove_budget".into(), json!(true));
            }
            None => {}
        }
        if self.clear_spent_today {
            o.insert("clear_spent_today".into(), json!(true));
        }
        if let Some(a) = self.anywhere {
            o.insert("anywhere".into(), json!(a));
        }
        if !self.act.is_empty() {
            o.insert("act".into(), json!(self.act));
        }
        Value::Object(o)
    }

    /// What it changes, as the card says it: "raise its Act fully budget
    /// to $5,000.00 a day; …".
    pub(crate) fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(l) = self.level {
            parts.push(format!(
                "run at {}",
                match l {
                    Level::Watch => "Watch",
                    Level::Suggest => "Suggest",
                    Level::Act => "Act",
                    Level::Full => "Act fully",
                }
            ));
        }
        match self.budget_cents {
            Some(Some(c)) => parts.push(format!(
                "set Act fully's daily budget to {} of real money",
                dollars(c)
            )),
            Some(None) => parts.push("remove Act fully's daily budget".into()),
            None => {}
        }
        if self.clear_spent_today {
            parts.push("clear what Act fully's budget has counted today".into());
        }
        match self.anywhere {
            Some(true) => parts.push("act fully anywhere".into()),
            Some(false) => parts.push("act fully only in the named repositories".into()),
            None => {}
        }
        for (k, on) in &self.act {
            parts.push(format!(
                "turn {} {}",
                if *on { "on" } else { "off" },
                switch_title(k)
            ));
        }
        parts.join("; ")
    }

    /// `body`, a config.toml, with the change made, `now` being when the
    /// person approves it. Everything else in it is kept as written.
    pub(crate) fn apply(
        &self,
        body: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<String, String> {
        let mut doc: toml_edit::DocumentMut = body
            .parse()
            .map_err(|_| "Its config.toml can't be read, so nothing was changed.".to_string())?;
        let oversight = table_in(doc.as_table_mut(), "oversight")?;
        if let Some(l) = self.level {
            oversight.insert("level", toml_edit::value(l.as_str()));
        }
        let touches_full =
            self.budget_cents.is_some() || self.clear_spent_today || self.anywhere.is_some();
        if touches_full {
            let full = table_in(oversight, "full")?;
            match self.budget_cents {
                Some(Some(c)) => {
                    full.insert(
                        "budget_usd_per_day",
                        if c % 100 == 0 {
                            toml_edit::value((c / 100) as i64)
                        } else {
                            toml_edit::value(c as f64 / 100.0)
                        },
                    );
                }
                Some(None) => {
                    full.remove("budget_usd_per_day");
                }
                None => {}
            }
            if self.clear_spent_today {
                full.insert(
                    "budget_cleared_at",
                    toml_edit::value(now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
                );
            }
            if let Some(a) = self.anywhere {
                full.insert("anywhere", toml_edit::value(a));
            }
        }
        if !self.act.is_empty() {
            let act = table_in(oversight, "act")?;
            for (k, on) in &self.act {
                act.insert(k, toml_edit::value(*on));
            }
        }
        Ok(doc.to_string())
    }
}

/// `$5,000.00`.
fn dollars(cents: u64) -> String {
    let whole = (cents / 100).to_string();
    let mut grouped = String::new();
    for (i, ch) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    format!("${grouped}.{:02}", cents % 100)
}

/// The table `key` of `t`, made when missing; an inline one is kept
/// inline.
fn table_in<'t>(
    t: &'t mut dyn toml_edit::TableLike,
    key: &str,
) -> Result<&'t mut dyn toml_edit::TableLike, String> {
    if !t.contains_key(key) {
        let mut made = toml_edit::Table::new();
        made.set_implicit(false);
        t.insert(key, toml_edit::Item::Table(made));
    }
    t.get_mut(key)
        .and_then(|i| i.as_table_like_mut())
        .ok_or_else(|| format!("[{key}] in its config.toml isn't a table, so nothing was changed."))
}

impl CrewHub {
    /// Makes `change` to the overseer `agent`'s own `[oversight]`, as the
    /// person approved it: through their own path, its config.toml saved
    /// as Desk's settings save it, never the agent's.
    pub(crate) async fn change_own_oversight(
        &self,
        agent: &str,
        change: &Change,
    ) -> Result<String, String> {
        // Only an agent of this hub: a boss served from a grant is the
        // remote hub's to change.
        let service = self
            .services()
            .into_iter()
            .find(|s| s.agent_id.as_str() == agent && !workforce_desk::grant::is_served(agent))
            .ok_or("That agent isn't on this hub any more, so nothing was changed.")?;
        let path = service
            .workspace
            .join(workforce_common::config_files::WORKSPACE_CONFIG);
        let current = workforce_common::config_files::read(&path)
            .map_err(|_| "Its config.toml can't be read, so nothing was changed.".to_string())?
            .unwrap_or_default();
        let body = change.apply(&current, chrono::Utc::now())?;
        let owner = self.person(Some(&service));
        crate::agent_config_write::save(
            &service,
            agent,
            self.state.def_store.as_deref(),
            &body,
            EditContext::new("web", owner.as_deref()),
            |now| now.unwrap_or_default() == current,
        )
        .await
        .map_err(|e| format!("Not changed: {e}."))?;
        let name = name_of(&service);
        Ok(format!("{name}: {}", change.describe()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_parses_says_itself_and_round_trips_through_a_suggestion() {
        let c = Change::parse(&json!({
            "budget_usd_per_day": 5000, "clear_spent_today": true,
            "act": {"interrupt": true}, "reason": "The person asked."
        }))
        .unwrap();
        assert_eq!(c.budget_cents, Some(Some(500_000)));
        assert_eq!(
            c.describe(),
            "set Act fully's daily budget to $5,000.00 of real money; clear what Act fully's budget has counted today; turn on interrupting turns"
        );
        assert_eq!(Change::parse(&c.input()).unwrap(), c);
        assert!(Change::parse(&json!({"reason": "x"})).is_err());
        assert!(Change::parse(&json!({"budget_usd_per_day": 0})).is_err());
        assert!(Change::parse(&json!({"budget_usd_per_day": 10001})).is_err());
        assert!(Change::parse(&json!({"budget_usd_per_day": 5, "remove_budget": true})).is_err());
        assert!(Change::parse(&json!({"act": {"publish": true}})).is_err());
        assert!(Change::parse(&json!({"level": "boss"})).is_err());
    }

    #[test]
    fn applying_a_change_keeps_the_rest_of_the_config_as_written() {
        let body = "# mine\n[llm]\nprovider = \"claude-code\"\n\n[oversight]\nenabled = true\nlevel = \"full\" # all in\nact = { nudge = true }\n\n[oversight.full]\nanywhere = true\nbudget_usd_per_day = 200\n";
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-11T14:02:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let c = Change::parse(&json!({"budget_usd_per_day": 5000, "clear_spent_today": true, "act": {"set_mode": true}})).unwrap();
        let out = c.apply(body, now).unwrap();
        assert!(
            out.starts_with("# mine\n[llm]\nprovider = \"claude-code\""),
            "{out}"
        );
        assert!(out.contains("level = \"full\" # all in"), "{out}");
        assert!(out.contains("budget_usd_per_day = 5000\n"), "{out}");
        assert!(
            out.contains("budget_cleared_at = \"2026-11-11T14:02:00.000Z\""),
            "{out}"
        );
        let doc: toml::Table = toml::from_str(&out).unwrap();
        let o = workforce_common::oversight::Oversight::of(doc.get("oversight")).unwrap();
        assert_eq!(o.full.budget_cents_per_day, Some(500_000));
        assert_eq!(o.full.budget_cleared_at, Some(now));
        assert!(o.act.nudge && o.act.set_mode);

        let removed = Change::parse(&json!({"remove_budget": true, "level": "act"}))
            .unwrap()
            .apply(body, now)
            .unwrap();
        let doc: toml::Table = toml::from_str(&removed).unwrap();
        let o = workforce_common::oversight::Oversight::of(doc.get("oversight")).unwrap();
        assert_eq!(o.full.budget_cents_per_day, None);
        assert_eq!(o.level, Level::Act);

        let cents = Change::parse(&json!({"budget_usd_per_day": 7.5}))
            .unwrap()
            .apply("", now)
            .unwrap();
        assert!(cents.contains("budget_usd_per_day = 7.5"), "{cents}");
        assert_eq!(dollars(123_456_789), "$1,234,567.89");
    }
}
