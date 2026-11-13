//! The person's preferences on the hub (the boss understands sessions,
//! H9): `crew.prefer` keeps one, `crew.preferences` lists, changes,
//! pauses, resumes and forgets them, and every turn of a boss is given the
//! ones that cover what it is about, the most specific first, then the
//! newest. A crew action that applies one names it (`preference`, with
//! `match` and the others it was chosen `over`); the hub checks only that
//! each named is the person's, for this boss, and active, so the decision
//! line that cites it ("per your preference from 11/12") is true. It gates
//! nothing: the same action without a preference runs as it always did.

use serde_json::{json, Value};
use uuid::Uuid;
use workforce_desk::preference::{self, PrefScope, PrefSource, PrefState, Preference};
use workforce_desk::preference_store::PrefChange;
use workforce_desk::rooms::YOU;

use workforce_desk::crew::ItemRefused;
use workforce_desk::open_item::{NewItem, OpenItem, OpenKind, OpenState};
use workforce_desk::open_item_store::Opened;

use super::tools::Bound;
use super::CrewHub;

pub const PREFER: &str = "crew.prefer";
pub const PREFERENCES: &str = "crew.preferences";

pub const PREFER_DESCRIPTION: &str = "Keep one of the person's preferences: a rule you then apply in place of asking them. Use it when they say one outright (\"from now on, always use the staging key for staging tests\") or answer \"always\" to something you asked (item_id: that item). when is the kind of case (\"a session wants to merge with CI red\"), then what to do (\"make it wait for green\"), and scope what it covers: all (the default), group:<id>, or repo:<owner/name> when the case is about a repository's work (merging, deploying). The same case and scope kept again replaces the older one. Confirm it to the person in one line.";
pub const PREFERENCES_DESCRIPTION: &str = "The person's preferences for you: list them (\"what are my preferences?\": each with its id, when it was made, what it covers and how often you used it), or by id change one (when, then, scope), pause it, resume it, or forget it (\"forget that\": the one you last applied or mentioned; say which). Answer from this, never from memory.";

/// The most of the preferences a turn is given, in characters.
const CONTEXT_CHARS: usize = 2_400;

pub fn schema(name: &str) -> Value {
    match name {
        PREFER => json!({ "type": "object", "properties": {
            "when": { "type": "string", "description": "The kind of case, e.g. \"a session wants to merge with CI red\"" },
            "then": { "type": "string", "description": "What to do then, e.g. \"make it wait for green\"" },
            "scope": { "type": "string", "description": "all (default), group:<id>, or repo:<owner/name>" },
            "item_id": { "type": "string", "description": "The open item the person answered \"always\" to, when it came from one" }
        }, "required": ["when", "then"] }),
        PREFERENCES => json!({ "type": "object", "properties": {
            "action": { "type": "string", "enum": ["list", "change", "pause", "resume", "forget"], "description": "list (default), or what to do with preference_id" },
            "preference_id": { "type": "string" },
            "when": { "type": "string", "description": "change: the new case" },
            "then": { "type": "string", "description": "change: what to do now" },
            "scope": { "type": "string", "description": "change: all, group:<id> or repo:<owner/name>" }
        }}),
        _ => json!({ "type": "object", "properties": {} }),
    }
}

/// The properties every acting crew tool takes to name the preference it
/// applies.
pub fn act_properties() -> Value {
    json!({
        "preference": { "type": "string", "description": "The id of the person's preference this applies, if one does: the decision cites it" },
        "match": { "type": "string", "enum": ["exact", "close"], "description": "With preference: exact when the case is the preference's own, close when it is near enough that you read it as covered" },
        "over": { "type": "array", "items": { "type": "string" }, "description": "With preference: the ids of other preferences that also fit and that you set aside" }
    })
}

/// The fields of an acting tool's input [`act_properties`] adds, taken out
/// before the action is read or kept as a suggestion.
pub const ACT_FIELDS: [&str; 3] = ["preference", "match", "over"];

/// What an action said it applies, checked.
#[derive(Debug, Clone)]
pub(super) struct Applied {
    pub id: Uuid,
    /// The words the decision cites it with.
    pub cite: String,
}

fn text(input: &Value, key: &str) -> Option<String> {
    input[key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn pref_id(s: &str) -> Result<Uuid, String> {
    s.trim()
        .parse()
        .map_err(|_| format!("“{s}” isn't a preference id (crew.preferences lists them)."))
}

impl CrewHub {
    fn pref_err(e: eyre::Report) -> String {
        tracing::warn!(error = %format!("{e:#}"), "the person's preferences couldn't be read or kept");
        "Desk couldn't read or keep the person's preferences just now.".to_string()
    }

    /// The person's preferences `ids`, each checked to be theirs, for the
    /// boss of `boss_scope`, and active.
    async fn preferences_named(
        &self,
        boss_scope: &str,
        ids: &[Uuid],
    ) -> Result<Vec<Preference>, String> {
        let found = self
            .desk
            .store()
            .preferences_by_id(ids)
            .await
            .map_err(Self::pref_err)?;
        let mut out = Vec::new();
        for id in ids {
            let p = found
                .iter()
                .find(|p| p.id == *id && p.owner == YOU && p.boss_scope == boss_scope)
                .ok_or_else(|| {
                    format!("There is no preference {id} of the person's for you (crew.preferences lists them).")
                })?;
            if p.state != PrefState::Active {
                return Err(format!(
                    "The person's preference {id} ({}) is {}, so it can't be applied.",
                    p.rule(),
                    p.state.as_str()
                ));
            }
            out.push(p.clone());
        }
        Ok(out)
    }

    /// The preference an acting tool's `input` says it applies, checked,
    /// and the words its decision cites it with. `Ok(None)` when it names
    /// none. Refused only for what would make the citation untrue (an
    /// unknown, another's, a paused or a forgotten preference): the action
    /// itself is never weighed.
    pub(super) async fn applied_preference(
        &self,
        b: &Bound,
        input: &Value,
    ) -> Result<Option<Applied>, String> {
        let Some(id) = text(input, "preference") else {
            return Ok(None);
        };
        let refuse = |why: String| {
            format!("{why} Nothing was done: do it again without preference, or with one that is active.")
        };
        let id = pref_id(&id).map_err(refuse)?;
        let close = match input["match"].as_str().map(str::trim) {
            None | Some("") | Some("exact") => false,
            Some("close") => true,
            Some(_) => return Err(refuse("match is exact or close.".into())),
        };
        let over: Vec<Uuid> = input["over"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).map(pref_id).collect())
            .transpose()
            .map_err(refuse)?
            .unwrap_or_default();
        let scope = preference::boss_scope_of(b.oversight.scope.group());
        // Only the one applied must be there and active: one it set aside
        // that has since gone is left out of the line, never a refusal.
        let applied = self
            .preferences_named(&scope, &[id])
            .await
            .map_err(refuse)?
            .remove(0);
        let over: Vec<Uuid> = over.into_iter().filter(|o| *o != id).collect();
        let set_aside: Vec<Preference> = self
            .desk
            .store()
            .preferences_by_id(&over)
            .await
            .map_err(Self::pref_err)
            .map_err(refuse)?
            .into_iter()
            .filter(|p| p.owner == YOU && p.boss_scope == scope && p.state == PrefState::Active)
            .collect();
        let tz = self.desk.timezone();
        Ok(Some(Applied {
            id,
            cite: preference::cite(&applied, close, &set_aside, tz),
        }))
    }

    /// `id` made a decision: counted. A write that fails is logged.
    pub(crate) async fn preference_used(&self, id: Uuid) {
        if let Err(e) = self.desk.store().preference_applied(id).await {
            tracing::warn!(error = %e, "a preference's use wasn't counted");
        }
    }

    /// `crew.prefer` and `crew.preferences`, for the boss `b`.
    pub(super) async fn preferences_tool(
        &self,
        name: &str,
        b: &Bound,
        input: &Value,
    ) -> Result<Value, String> {
        let scope = preference::boss_scope_of(b.oversight.scope.group());
        match name {
            PREFER => self.prefer(&scope, b, input).await,
            PREFERENCES => self.preferences_act(&scope, input).await,
            _ => Err("not a preference tool".into()),
        }
    }

    async fn prefer(&self, boss_scope: &str, b: &Bound, input: &Value) -> Result<Value, String> {
        let when = text(input, "when").ok_or("Say the kind of case (when).")?;
        let then = text(input, "then").ok_or("Say what to do then (then).")?;
        let scope = PrefScope::parse(input["scope"].as_str().unwrap_or(""))?;
        let item = text(input, "item_id")
            .map(|s| {
                s.parse::<Uuid>()
                    .map_err(|_| "That isn't an item id from your open items.".to_string())
            })
            .transpose()?;
        if let Some(item) = item {
            let found = self
                .desk
                .store()
                .open_item(item)
                .await
                .map_err(Self::pref_err)?;
            if found.is_none_or(|i| i.boss != b.service.agent_id.as_str()) {
                return Err("That isn't one of your items.".into());
            }
        }
        let mut p = Preference::new(
            boss_scope,
            &when,
            &then,
            scope,
            match item {
                Some(_) => PrefSource::Asked,
                None => PrefSource::Said,
            },
        );
        p.from_item = item;
        let store = self.desk.store();
        // The same case and scope said again replaces the older one.
        let same: Vec<Preference> = store
            .preferences_of(YOU, boss_scope, false)
            .await
            .map_err(Self::pref_err)?
            .into_iter()
            .filter(|o| o.scope == p.scope && o.when_text.eq_ignore_ascii_case(&p.when_text))
            .collect();
        // The older one goes first, so a failure never leaves two that
        // both apply.
        let tz = self.desk.timezone();
        let mut replaced = Vec::new();
        for o in &same {
            store
                .preference_set_state(o.id, PrefState::Retired)
                .await
                .map_err(Self::pref_err)?;
            replaced.push(format!("{} (from {})", o.rule(), o.date(tz)));
        }
        store.preference_put(&p).await.map_err(Self::pref_err)?;
        let mut said = format!(
            "Kept as preference {}: when {}, {} (for {}).",
            p.id,
            p.when_text,
            p.then_text,
            p.scope.said()
        );
        if !replaced.is_empty() {
            said.push_str(&format!(" It replaces {}.", replaced.join("; ")));
        }
        said.push_str(" Confirm it to the person in one line.");
        Ok(json!(said))
    }

    async fn preferences_act(&self, boss_scope: &str, input: &Value) -> Result<Value, String> {
        let store = self.desk.store();
        let tz = self.desk.timezone();
        let action = input["action"].as_str().map(str::trim).unwrap_or("list");
        if action.is_empty() || action == "list" {
            let all = store
                .preferences_of(YOU, boss_scope, true)
                .await
                .map_err(Self::pref_err)?;
            let kept: Vec<Preference> = preference::ranked(all)
                .into_iter()
                .filter(|p| p.state != PrefState::Retired)
                .collect();
            if kept.is_empty() {
                return Ok(json!("The person has no preferences for you yet."));
            }
            return Ok(json!(super::watcher::reported(
                &preference::listed(&kept, tz, usize::MAX),
                "the person's preferences"
            )));
        }
        let id =
            pref_id(&text(input, "preference_id").ok_or("Say which preference (preference_id).")?)?;
        let found = store
            .preference(id)
            .await
            .map_err(Self::pref_err)?
            .filter(|p| {
                p.owner == YOU && p.boss_scope == boss_scope && p.state != PrefState::Retired
            })
            .ok_or_else(|| format!("There is no preference {id} of the person's for you."))?;
        let changed = match action {
            "change" => {
                let change = PrefChange {
                    when_text: text(input, "when"),
                    then_text: text(input, "then").map(|t| t.trim_end_matches('.').to_string()),
                    scope: input["scope"]
                        .as_str()
                        .filter(|s| !s.trim().is_empty())
                        .map(PrefScope::parse)
                        .transpose()?,
                };
                if change.when_text.is_none()
                    && change.then_text.is_none()
                    && change.scope.is_none()
                {
                    return Err("Say what changes: when, then or scope.".into());
                }
                let change = PrefChange {
                    when_text: change.when_text.map(|w| preference::plain_when(&w)),
                    ..change
                };
                store.preference_change(id, &change).await
            }
            "pause" => store.preference_set_state(id, PrefState::Paused).await,
            "resume" => store.preference_set_state(id, PrefState::Active).await,
            "forget" => store.preference_set_state(id, PrefState::Retired).await,
            _ => return Err("action is list, change, pause, resume or forget.".into()),
        }
        .map_err(Self::pref_err)?
        .ok_or_else(|| format!("There is no preference {id} of the person's for you."))?;
        let verb = match action {
            "change" => "Changed",
            "pause" => "Paused",
            "resume" => "Resumed",
            _ => "Forgot",
        };
        Ok(json!(format!(
            "{verb} the preference from {}: when {}, {} (for {}). Tell the person which, in one line.",
            found.date(tz),
            changed.when_text,
            changed.then_text,
            changed.scope.said()
        )))
    }

    /// The person's active preferences for the boss of `boss_scope` that
    /// cover the session `about` (every one in a turn about no session),
    /// as its turns are given them: in the order they win, with how to
    /// apply them, which an unbound boss does without asking.
    pub(crate) async fn preferences_context(
        &self,
        boss_scope: &str,
        about: Option<Uuid>,
        unbound: bool,
    ) -> Option<String> {
        let prefs = match self
            .desk
            .store()
            .preferences_of(YOU, boss_scope, false)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::info!(error = %e, "the person's preferences couldn't be read for a boss's turn");
                return None;
            }
        };
        if prefs.is_empty() {
            return None;
        }
        let prefs = match about {
            None => prefs,
            Some(id) => {
                let group = self.desk.view(id).ok().and_then(|v| v.group.map(|g| g.id));
                // Git is asked off the async workers.
                let desk = self.desk.clone();
                let repo = tokio::task::spawn_blocking(move || desk.repo_of_session(id))
                    .await
                    .ok()
                    .flatten();
                prefs
                    .into_iter()
                    .filter(|p| p.scope.covers(group.as_deref(), repo.as_deref()))
                    .collect()
            }
        };
        if prefs.is_empty() {
            return None;
        }
        let listed = preference::listed(
            &preference::ranked(prefs),
            self.desk.timezone(),
            CONTEXT_CHARS,
        );
        let close = match unbound {
            true => "close when it is near but not quite its own case: apply a close match and say so, never ask",
            false => "close when it is near but not quite its own case: apply a close match when what you do can be undone, otherwise ask the person whether it covers this case (crew.ask_person, options Yes / No / Not this time)",
        };
        Some(format!(
            "The person's preferences (check them before you decide or ask; the first that fits wins: \
             the most specific scope, then the newest). Apply one by passing its id as preference on the \
             crew action, with match: exact when the case is its own, {close}; pass any others that also \
             fit as over. With none that fits, decide yourself; ask the person only when you truly can't, \
             with crew.ask_person and generalizes saying the kind of case, so their answer can become a \
             preference:\n{listed}"
        ))
    }
}

/// The key of the item that asks whether to change a preference the
/// person overruled.
const OVERRULED: &str = "overrule:";
/// Its answers.
const CHANGE_IT: &str = "Change it";
const KEEP_IT: &str = "Keep it";
const FORGET_IT: &str = "Forget it";

/// The person's answer to an ask, as it was closed with it: none for an
/// answer that went on to a session (its words are the session's line's).
fn answer_of(i: &OpenItem) -> Option<String> {
    let why = i.closed_why.as_deref()?;
    ["You answered: ", "They answered: "]
        .iter()
        .find_map(|p| why.strip_prefix(p))
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string)
}

impl CrewHub {
    /// "Always" on item `id`: the person's answer becomes their preference
    /// for the case the item said it settles, from then on. Once: pressed
    /// again, the one kept is said.
    pub(crate) async fn always_item(
        &self,
        id: Uuid,
        then: Option<&str>,
    ) -> Result<String, ItemRefused> {
        let cant = |e: eyre::Report| ItemRefused::Cant(Self::pref_err(e));
        let store = self.desk.store();
        let item = store
            .open_item(id)
            .await
            .map_err(cant)?
            .ok_or(ItemRefused::NotFound)?;
        let tz = self.desk.timezone();
        if let Some(p) = store.preference_from_item(id).await.map_err(cant)? {
            return Ok(format!(
                "Already kept, from {}: when {}, {}.",
                p.date(tz),
                p.when_text,
                p.then_text
            ));
        }
        let when = item
            .generalizes
            .as_deref()
            .map(str::trim)
            .filter(|g| !g.is_empty())
            .ok_or_else(|| {
                ItemRefused::Cant("That question doesn't settle a kind of case, so there is nothing to keep.".into())
            })?;
        if item.state != OpenState::Closed {
            return Err(ItemRefused::Cant("Answer it first.".into()));
        }
        let then = then
            .map(str::to_string)
            .or_else(|| answer_of(&item))
            .ok_or_else(|| ItemRefused::Cant("Say what to do then: your answer went to the session.".into()))?;
        let Some((_, o)) = self.overseer_named(&item.boss) else {
            return Err(ItemRefused::Cant("Its boss isn't here any more.".into()));
        };
        let scope = match store.item_prefer_scope(id).await.map_err(cant)? {
            Some(s) => PrefScope::parse(&s).unwrap_or(PrefScope::All),
            None => PrefScope::All,
        };
        let boss_scope = preference::boss_scope_of(o.scope.group());
        let mut p = Preference::new(&boss_scope, when, &then, scope, PrefSource::Asked);
        p.from_item = Some(id);
        // The same case and scope kept before gives way to this one.
        for old in store
            .preferences_of(YOU, &boss_scope, false)
            .await
            .map_err(cant)?
            .into_iter()
            .filter(|o| o.scope == p.scope && o.when_text.eq_ignore_ascii_case(&p.when_text))
        {
            store
                .preference_set_state(old.id, PrefState::Retired)
                .await
                .map_err(cant)?;
        }
        store.preference_put(&p).await.map_err(cant)?;
        Ok(format!(
            "From now on: when {}, {} (for {}).",
            p.when_text,
            p.then_text,
            p.scope.said()
        ))
    }

    /// The person undid `undone`, a decision made by their preference
    /// `pref`: counted, set aside at the second time, and asked whether to
    /// change it ("Change it", "Keep it", "Forget it"), once per preference
    /// while the question is open.
    pub(crate) async fn overruled(&self, boss: &str, pref: Uuid, undone: &str) {
        let store = self.desk.store();
        let p = match store.preference_overruled(pref).await {
            Ok(Some(p)) => p,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(error = %e, "an overrule of a preference wasn't counted");
                return;
            }
        };
        let tz = self.desk.timezone();
        let paused = match p.state {
            PrefState::Paused => " I've set it aside until you say otherwise, as you undid it twice.",
            _ => "",
        };
        let mut new = NewItem::ask(
            boss,
            OpenKind::Ask,
            None,
            &format!(
                "You undid “{}”, which I did per your preference from {} ({}). Should I change that preference?{paused}",
                undone.trim_end_matches('.'),
                p.date(tz),
                p.rule()
            ),
        );
        new.dedupe_key = format!("{OVERRULED}{pref}");
        new.options = vec![CHANGE_IT.into(), KEEP_IT.into(), FORGET_IT.into()];
        match store.item_open(&new).await {
            Ok(Opened::New(i)) => {
                self.post_item(boss, &i, super::items::needs_you(&i), Vec::new())
                    .await;
            }
            Ok(Opened::Again(_)) => {}
            Err(e) => tracing::warn!(error = %e, "asking about an overruled preference failed"),
        }
    }

    /// The person answered `closed`, when it asked whether to change an
    /// overruled preference: kept or forgotten here, and said; `None` for
    /// any other item, or "Change it", which goes on to the boss for the
    /// new words.
    pub(crate) async fn overrule_answered(&self, closed: &OpenItem, answer: &str) -> Option<String> {
        let pref: Uuid = closed.dedupe_key.strip_prefix(OVERRULED)?.parse().ok()?;
        let state = match answer.trim() {
            a if a.eq_ignore_ascii_case(KEEP_IT) => PrefState::Active,
            a if a.eq_ignore_ascii_case(FORGET_IT) => PrefState::Retired,
            _ => return None,
        };
        match self.desk.store().preference_set_state(pref, state).await {
            Ok(Some(p)) => Some(match state {
                PrefState::Active => format!("Kept: when {}, {}.", p.when_text, p.then_text),
                _ => format!("Forgot: when {}, {}.", p.when_text, p.then_text),
            }),
            Ok(None) => Some("That preference isn't there any more.".into()),
            Err(e) => {
                tracing::warn!(error = %e, "an overruled preference's answer wasn't kept");
                Some("Desk couldn't keep that just now: say it in the chat.".into())
            }
        }
    }

    /// How many needs the boss `agent` (the main one when none) handled
    /// since `since` (the start of today when none), in all and per day.
    pub(crate) async fn handled_counts(
        &self,
        agent: Option<&str>,
        since: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Option<Value> {
        let boss = match agent {
            Some(a) => self.overseer_named(a).map(|(s, _)| s)?,
            None => self
                .overseer_for(&workforce_common::oversight::Scope::Ungrouped)
                .map(|(s, _)| s)?,
        };
        let boss = boss.agent_id.as_str().to_string();
        let since = since.unwrap_or_else(|| workforce_desk::crew::Crew::midnight(self));
        let tz = self.desk.timezone();
        let store = self.desk.store();
        let all = match store.decisions_handled(&boss, since).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = %e, "a boss's handled counts couldn't be read");
                return None;
            }
        };
        let days = store
            .decisions_handled_by_day(&boss, since, tz)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "a boss's handled counts by day couldn't be read");
                Vec::new()
            });
        let counts = |h: &workforce_desk::decision::Handled| {
            json!({
                "needs": h.needs(),
                "yourself": h.itself(),
                "byPreference": h.preference,
                "own": h.own,
                "escalated": h.escalated,
                "personFirst": h.person_first,
            })
        };
        let mut out = counts(&all);
        out["agent"] = json!(boss);
        out["since"] = json!(since.to_rfc3339());
        out["said"] = json!(workforce_desk::decision::handled_said(&all));
        out["days"] = json!(days
            .iter()
            .map(|(d, h)| {
                let mut v = counts(h);
                v["day"] = json!(d.to_string());
                v
            })
            .collect::<Vec<_>>());
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_acting_field_is_taken_out_before_the_action_is_read() {
        let props = act_properties();
        let names: Vec<&str> = props
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        for n in names {
            assert!(ACT_FIELDS.contains(&n), "{n}");
        }
    }
}
