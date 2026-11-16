//! `crew.updates_cadence`: the person says how they want to be told
//! ("roundup every two hours", "nothing overnight", "only tell me when
//! something's finished") and the boss changes its own cadence
//! (`[oversight.updates]`'s cadence keys), then says what it changed. These
//! are how it talks to the person, not its limits, so it sets them itself
//! on their word, as Desk's settings save them: through the agent's own
//! config.toml, hot-reloaded.
//!
//! - **Their word only.** It changes them only in a turn answering the
//!   person's own message in its DM: never in a check-in, never on what a
//!   session or another overseer said, so no session's text can mute what
//!   the person hears.
//! - **The cadence keys only.** Nothing else in its config is touched; its
//!   level, budget and switches stay the person's own
//!   (`crew.ask_oversight_change`).

use serde_json::{json, Value};
use workforce_common::channels::DESK_CHANNEL;
use workforce_common::oversight::{Cadence, Oversight, PRESETS};
use workforce_orchestrator::workflow_store::EditContext;

use super::tools::Bound;
use super::CrewHub;

pub const NAME: &str = "crew.updates_cadence";

pub const DESCRIPTION: &str = "Change how and when you tell the person what happened, when they say so in their own message to you (\"roundup every two hours\", \"nothing overnight\", \"only tell me when something's finished\"): a preset (as_it_happens, balanced, roundups_only), or any of needs_you, problems, finished, progress, routine (right_away, roundup, ask; needs_you never ask), roundup (30m, 1h, 2h, meals, never), quiet_hours (\"22:00-07:00\", or \"\" for none), quiet_needs_you (whether what needs them still comes at once in quiet hours), decisions (important, important_right_away, all, ask) and also_to (desk, slack, iphone). With nothing to change it says the cadence as it is. It works only in a turn answering their own message; then tell them in one line what you changed.";

/// The keys it may write, in `[oversight.updates]`.
const KEYS: [&str; 10] = [
    "needs_you",
    "problems",
    "finished",
    "progress",
    "routine",
    "roundup",
    "quiet_hours",
    "quiet_needs_you",
    "decisions",
    "also_to",
];

pub fn schema() -> Value {
    let route = json!({ "type": "string", "enum": ["right_away", "roundup", "ask"] });
    json!({ "type": "object", "properties": {
        "preset": { "type": "string", "enum": PRESETS, "description": "Start from a preset; any other key given changes it after" },
        "needs_you": { "type": "string", "enum": ["right_away", "roundup"] },
        "problems": route, "finished": route, "progress": route, "routine": route,
        "roundup": { "type": "string", "enum": ["30m", "1h", "2h", "meals", "never"] },
        "quiet_hours": { "type": "string", "description": "\"22:00-07:00\", or \"\" for none" },
        "quiet_needs_you": { "type": "boolean" },
        "decisions": { "type": "string", "enum": ["important", "important_right_away", "all", "ask"] },
        "also_to": { "type": "array", "items": { "type": "string", "enum": ["desk", "slack", "iphone"] } }
    }})
}

/// The cadence keys `input` sets, as TOML values: a preset's first, then
/// each key given. None given: nothing to change.
fn changes(input: &Value) -> Result<Vec<(&'static str, toml_edit::Value)>, String> {
    let mut out: Vec<(&'static str, toml_edit::Value)> = Vec::new();
    if let Some(p) = input["preset"].as_str() {
        let c = Cadence::preset(p.trim())
            .ok_or_else(|| format!("preset must be one of {}.", PRESETS.join(", ")))?;
        let names = ["needs_you", "problems", "finished", "progress", "routine"];
        for (k, r) in names.into_iter().zip(c.rows()) {
            out.push((k, r.as_str().into()));
        }
        out.push(("roundup", c.roundup.as_str().into()));
    }
    for key in KEYS {
        let Some(v) = input.get(key).filter(|v| !v.is_null()) else {
            continue;
        };
        let value: toml_edit::Value = match v {
            Value::String(s) => s.trim().into(),
            Value::Bool(b) => (*b).into(),
            Value::Array(items) => {
                let mut a = toml_edit::Array::new();
                for i in items {
                    a.push(i.as_str().ok_or("also_to must be a list of names.")?.trim());
                }
                toml_edit::Value::Array(a)
            }
            _ => return Err(format!("{key} isn't a value it takes.")),
        };
        out.retain(|(k, _)| *k != key);
        out.push((key, value));
    }
    Ok(out)
}

/// `body` (a config.toml) with `changes` made in `[oversight.updates]`,
/// everything else kept as written, and the cadence it then reads as; or
/// what is wrong, in the settings' own words, with nothing changed.
pub(crate) fn apply(
    body: &str,
    changes: &[(&'static str, toml_edit::Value)],
) -> Result<(String, Cadence), String> {
    let mut doc: toml_edit::DocumentMut = body
        .parse()
        .map_err(|_| "Its config.toml can't be read, so nothing was changed.".to_string())?;
    {
        let oversight = table_in(doc.as_table_mut(), "oversight")?;
        let updates = table_in(oversight, "updates")?;
        for (k, v) in changes {
            updates.insert(k, toml_edit::Item::Value(v.clone()));
        }
    }
    let out = doc.to_string();
    let parsed: toml::Table = toml::from_str(&out)
        .map_err(|_| "That would make its config.toml unreadable, so nothing was changed.")?;
    let o = Oversight::of(parsed.get("oversight")).map_err(|e| format!("Not changed: {e}"))?;
    Ok((out, o.updates.cadence))
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

/// `c` said in plain words, as the card's summary says it.
pub fn describe(c: &Cadence) -> String {
    use workforce_common::oversight::{Decided, RoundupEvery, Route};
    let rows = [
        ("what needs you", c.needs_you),
        ("problems", c.problems),
        ("finished work", c.finished),
        ("progress", c.progress),
        ("routine things", c.routine),
    ];
    let named = |r: Route| -> Vec<&str> {
        rows.iter()
            .filter(|(_, x)| *x == r)
            .map(|(n, _)| *n)
            .collect()
    };
    let list = |v: Vec<&str>| match v.as_slice() {
        [] => String::new(),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    let every = match c.roundup {
        RoundupEvery::Minutes(30) => "every 30 minutes",
        RoundupEvery::Minutes(120) => "every 2 hours",
        RoundupEvery::Minutes(_) => "every hour",
        RoundupEvery::Meals => "at morning, lunch and end of day",
        RoundupEvery::Never => "never",
    };
    let mut out = Vec::new();
    let now = named(Route::RightAway);
    if !now.is_empty() {
        out.push(format!("Right away: {}", list(now)));
    }
    let later = named(Route::Roundup);
    match (later.is_empty(), c.roundup) {
        (_, RoundupEvery::Never) => out.push("No roundups".into()),
        (true, _) => out.push(format!("Roundups {every}")),
        (false, _) => out.push(format!("Roundup {every}: {}", list(later))),
    }
    let ask = named(Route::Ask);
    if !ask.is_empty() {
        out.push(format!("Only if you ask: {}", list(ask)));
    }
    out.push(format!(
        "Decisions made without you: {}",
        match c.decisions {
            Decided::Important => "the important ones, in the roundup",
            Decided::ImportantRightAway => "the important ones, right away",
            Decided::All => "every one, in the roundup",
            Decided::Ask => "only when you ask",
        }
    ));
    out.push(match c.quiet_hours {
        None => "No quiet hours".into(),
        Some(((fh, fm), (uh, um))) => format!(
            "Quiet {fh:02}:{fm:02}–{uh:02}:{um:02}{}",
            match c.quiet_needs_you {
                true => ", except what needs you",
                false => "",
            }
        ),
    });
    format!("{}.", out.join("; "))
}

impl CrewHub {
    /// `crew.updates_cadence` for the boss `b`: its cadence as it is, or
    /// changed as `input` says, in a turn answering the person's own
    /// message alone.
    pub(super) async fn cadence_tool(&self, b: &Bound, input: &Value) -> Result<Value, String> {
        let changes = changes(input)?;
        if changes.is_empty() {
            return Ok(json!(format!(
                "Your cadence now: {}",
                describe(&b.oversight.updates.cadence)
            )));
        }
        let owner = self
            .person(Some(&b.service))
            .ok_or("There is no person to change it for.")?;
        // Their word, in their own message this turn answers: never a
        // check-in, a session's text or another overseer's.
        let theirs = b
            .service
            .sessions
            .find(DESK_CHANNEL, &owner)
            .ok()
            .flatten()
            .is_some_and(|dm| {
                self.person_turns
                    .lock()
                    .get(&dm.id)
                    .is_some_and(|m| !m.trim().is_empty())
            });
        if !theirs {
            return Err("Only the person changes how you tell them things, in their own message to you: suggest it to them instead.".into());
        }
        if workforce_desk::grant::is_served(b.service.agent_id.as_str()) {
            return Err(
                "Your settings are kept on the hub that serves you, so they can't be changed here."
                    .into(),
            );
        }
        let path = b
            .service
            .workspace
            .join(workforce_common::config_files::WORKSPACE_CONFIG);
        let current = workforce_common::config_files::read(&path)
            .map_err(|_| "Its config.toml can't be read, so nothing was changed.".to_string())?
            .unwrap_or_default();
        let (body, cadence) = apply(&current, &changes)?;
        crate::agent_config_write::save(
            &b.service,
            b.service.agent_id.as_str(),
            self.state.def_store.as_deref(),
            &body,
            EditContext::new("web", Some(owner.as_str())),
            |now| now.unwrap_or_default() == current,
        )
        .await
        .map_err(|e| format!("Not changed: {e}."))?;
        Ok(json!(format!(
            "Changed. Your cadence now: {} Tell the person in one line what you changed.",
            describe(&cadence)
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::oversight::{Decided, RoundupEvery, Route};

    /// "roundup every two hours", "nothing overnight": the boss writes the
    /// cadence keys alone, keeps the rest as written, and what it wrote
    /// reads back as the cadence it said.
    #[test]
    fn the_person_says_it_and_only_the_cadence_keys_change() {
        let body = "# mine\n[llm]\nprovider = \"claude-code\"\n\n[oversight]\nenabled = true\nlevel = \"full\"\nupdates = { every_minutes = 5 }\n";
        let c = changes(
            &json!({"roundup": "2h", "quiet_hours": "21:00-08:00", "quiet_needs_you": false}),
        )
        .unwrap();
        let (out, cadence) = apply(body, &c).unwrap();
        assert!(
            out.starts_with("# mine\n[llm]\nprovider = \"claude-code\""),
            "{out}"
        );
        assert!(out.contains("level = \"full\""), "{out}");
        assert!(out.contains("every_minutes = 5"), "{out}");
        assert_eq!(cadence.roundup, RoundupEvery::Minutes(120));
        assert_eq!(cadence.quiet_hours, Some(((21, 0), (8, 0))));
        assert!(!cadence.quiet_needs_you);
        assert_eq!(cadence.preset_name(), Some("balanced"));
        assert!(describe(&cadence).contains("Roundup every 2 hours: finished work and progress"));
        assert!(
            describe(&cadence).contains("Quiet 21:00–08:00;")
                || describe(&cadence).ends_with("Quiet 21:00–08:00.")
        );

        let only = changes(&json!({"preset": "roundups_only", "decisions": "all"})).unwrap();
        let (_, c) = apply("", &only).unwrap();
        assert_eq!(c.preset_name(), Some("roundups_only"));
        assert_eq!(c.problems, Route::Roundup);
        assert_eq!(c.decisions, Decided::All);

        // Wrong in the settings' own words, and nothing changed.
        let wrong = changes(&json!({"roundup": "5m"})).unwrap();
        assert!(apply(body, &wrong)
            .unwrap_err()
            .contains("How often the roundup comes"));
        let never = changes(&json!({"needs_you": "ask"})).unwrap();
        assert!(
            apply(body, &never).is_err(),
            "needs-you is never only on asking"
        );
        assert!(changes(&json!({"preset": "loud"})).is_err());
        assert!(changes(&json!({})).unwrap().is_empty());
    }
}
