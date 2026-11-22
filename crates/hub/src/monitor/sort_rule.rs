//! Standing rules that sort new mail as it arrives. A rule is a source
//! monitor of one shape: `email.search` over one account's folder, polled,
//! each new message handed to `email.move`, `email.archive` or `email.mark`.
//! So it reuses monitors' polling, first-poll baseline (mail already there is
//! never touched), owner notices, pause and revisions, and keeps nothing of
//! its own. `email.sort_rules` lists, proposes, pauses and removes rules;
//! `email.sort_rule_save` saves one, once the person approved it.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_orchestrator::monitor::{Action, MonitorDefinition, MonitorMeta, Source};
use workforce_skills::adapters::email::criteria::{folder_named, Criteria};
use workforce_skills::adapters::email::schemas;

use super::binding_access::current_turn_user;
use super::runtime::MonitorSummary;
use super::MonitorTool;
use crate::hosts::MonitorDefinitions;

/// Every sort rule's monitor id starts so.
pub const PREFIX: &str = "sort-";
const EVERY_SECS: u64 = 300;
/// How far back each poll looks: new mail, cheaply.
const RECENT: &str = "2d";
const POLL_LIMIT: u64 = 100;
/// The tools a monitor reads mail with. Either one, given no account,
/// reads every account of the person it polls as.
const MAIL_READERS: &[&str] = &["email.inbox", "email.search"];
/// What a polled message is known by: its folder's UIDVALIDITY and its uid.
const ITEM_KEY: &str = "/key";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum Then {
    Move { to: String },
    Archive,
    MarkRead,
    Flag,
}

fn inbox() -> String {
    "INBOX".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortRule {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rule_id: String,
    pub account: String,
    #[serde(default = "inbox")]
    pub folder: String,
    #[serde(rename = "match")]
    pub matches: Criteria,
    pub then: Then,
    /// Other monitors that watch the mail this rule takes, named when the
    /// person approved it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watched_by: Vec<String>,
}

impl SortRule {
    pub fn says(&self) -> String {
        let then = match &self.then {
            Then::Move { to } => format!("move it to {to}"),
            Then::Archive => "archive it".to_string(),
            Then::MarkRead => "mark it read".to_string(),
            Then::Flag => "flag it".to_string(),
        };
        format!(
            "New mail in {} {} {} → {then}",
            self.account,
            self.folder,
            self.matches.describe()
        )
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.account.trim().is_empty() {
            return Err("Say which account the rule sorts.".to_string());
        }
        if matches!(&self.then, Then::Move { to } if to.trim().is_empty()) {
            return Err("Say which folder the rule moves mail to.".to_string());
        }
        let id_ok = self.rule_id.is_empty()
            || (self.rule_id.starts_with(PREFIX)
                && self.rule_id.len() > PREFIX.len()
                && self
                    .rule_id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        if !id_ok {
            return Err(format!("'{}' is not a sort rule's id.", self.rule_id));
        }
        self.matches.to_imap(false).map(|_| ())
    }

    /// The mail tool this rule calls, and its input for the messages `which`
    /// names.
    fn tool_and_input(&self, which: Value) -> (&'static str, Value) {
        let (account, folder) = (self.account.as_str(), self.folder.as_str());
        let (tool, mut input) = match &self.then {
            Then::Move { to } => (
                "email.move",
                json!({ "account": account, "from_folder": folder, "to_folder": to }),
            ),
            Then::Archive => (
                "email.archive",
                json!({ "account": account, "folder": folder }),
            ),
            Then::MarkRead => (
                "email.mark",
                json!({ "account": account, "folder": folder, "action": "read" }),
            ),
            Then::Flag => (
                "email.mark",
                json!({ "account": account, "folder": folder, "action": "flag" }),
            ),
        };
        if let (Some(input), Value::Object(which)) = (input.as_object_mut(), which) {
            input.extend(which);
        }
        (tool, input)
    }

    /// The call that applies this rule once to the mail already there.
    pub fn apply_once(&self) -> Value {
        let (tool, input) = self.tool_and_input(json!({ "search": self.matches, "dry_run": true }));
        json!({ "tool": tool, "input": input })
    }

    /// The monitor that is this rule: each new message is handed on by its
    /// uid under the numbering it was read with.
    pub fn monitor(&self, id: &str) -> MonitorDefinition {
        let (tool, input) = self
            .tool_and_input(json!({ "uid": "{{json:/uid}}", "validity": "{{json:/validity}}" }));
        let watched = match self.watched_by.is_empty() {
            true => String::new(),
            false => format!(
                " It takes mail these monitors also watch: {}.",
                self.watched_by.join(", ")
            ),
        };
        MonitorDefinition {
            monitor: MonitorMeta {
                id: id.to_string(),
                name: format!("Sort mail: {}", self.matches.describe()),
                description: format!("{}.{watched}", self.says()),
                enabled: true,
                agent: None,
                once: false,
                expires_at: None,
                active_hours: None,
                binding: None,
                source: Some(Source {
                    tool: "email.search".to_string(),
                    input: json!({
                        "account": self.account,
                        "folder": self.folder,
                        "search": self.matches,
                        "since": RECENT,
                        "limit": POLL_LIMIT,
                    }),
                    every_secs: Some(EVERY_SECS),
                    items: Some("/messages".to_string()),
                    item_id: Some(ITEM_KEY.to_string()),
                    path: None,
                    text: None,
                }),
                filter: None,
                condition: None,
                decide: None,
                action: Action::RunSkill {
                    tool: tool.to_string(),
                    input,
                },
            },
        }
    }

    /// The rule `meta` is, when it is one.
    pub fn of(meta: &MonitorMeta) -> Option<SortRule> {
        let source = meta.source().filter(|s| {
            s.tool == "email.search"
                && s.items.as_deref() == Some("/messages")
                && s.item_id() == ITEM_KEY
        })?;
        if !meta.id.starts_with(PREFIX) {
            return None;
        }
        let Action::RunSkill { tool, input } = &meta.action else {
            return None;
        };
        let then = match (tool.as_str(), input["action"].as_str()) {
            ("email.move", _) => Then::Move {
                to: input["to_folder"].as_str()?.to_string(),
            },
            ("email.archive", _) => Then::Archive,
            ("email.mark", Some("read")) => Then::MarkRead,
            ("email.mark", Some("flag")) => Then::Flag,
            _ => return None,
        };
        Some(SortRule {
            rule_id: meta.id.clone(),
            account: source.input["account"].as_str()?.to_string(),
            folder: folder_named(&source.input, "folder").to_string(),
            matches: serde_json::from_value(source.input["search"].clone()).ok()?,
            then,
            watched_by: Vec::new(),
        })
    }

    /// Whether `other`, another enabled monitor, watches the mail this rule
    /// takes: it reads the same account's folder, or every account's.
    pub fn watched_by_monitor(&self, other: &MonitorMeta) -> bool {
        other.id != self.rule_id
            && other.enabled
            && other.source().is_some_and(|s| {
                MAIL_READERS.contains(&s.tool.as_str())
                    && s.input["account"]
                        .as_str()
                        .is_none_or(|a| a == self.account)
                    && folder_named(&s.input, "folder").eq_ignore_ascii_case(&self.folder)
            })
    }

    /// The names of the monitors among `monitors` that watch this rule's
    /// mail: the rule owner's own, or the hub's.
    fn watchers(
        &self,
        owner: Option<&str>,
        monitors: &[(MonitorMeta, Option<String>)],
    ) -> Vec<String> {
        monitors
            .iter()
            .filter(|(m, theirs)| {
                (theirs.is_none() || theirs.as_deref() == owner) && self.watched_by_monitor(m)
            })
            .map(|(m, _)| m.name.clone())
            .collect()
    }
}

async fn call(monitors: &MonitorTool, input: Value) -> Result<Value, String> {
    let result = monitors
        .execute(input)
        .await
        .map_err(|e| format!("{e:#}"))?;
    match result.success {
        true => Ok(result.output),
        false => Err(result
            .error
            .unwrap_or_else(|| "the monitor call failed".to_string())),
    }
}

/// The monitors among `monitors` that are `owner`'s own sort rules, each
/// with its rule: never another person's, whoever else the caller may see.
fn owned_by<'a>(
    monitors: &'a [(MonitorMeta, Option<String>)],
    owner: &'a str,
) -> impl Iterator<Item = (&'a MonitorMeta, &'a Option<String>, SortRule)> + 'a {
    monitors
        .iter()
        .filter(move |(_, theirs)| theirs.as_deref() == Some(owner))
        .filter_map(|(meta, theirs)| SortRule::of(meta).map(|rule| (meta, theirs, rule)))
}

/// `owner`'s sort rules among `monitors` (each with its owner), in words,
/// each with the other monitors that watch the mail it takes.
pub fn rules_in(monitors: &[(MonitorMeta, Option<String>)], owner: &str) -> Vec<Value> {
    owned_by(monitors, owner)
        .map(|(meta, owner, rule)| {
            json!({
                "rule_id": rule.rule_id,
                "owner": owner,
                "says": rule.says(),
                "enabled": meta.enabled,
                "watched_by": rule.watchers(owner.as_deref(), monitors),
                "apply_once": rule.apply_once(),
                "rule": rule,
            })
        })
        .collect()
}

/// The monitors among `summaries` that read mail, by their definitions,
/// with their owners. A summary names the tool it polls, so only readers'
/// definitions are fetched.
pub(crate) async fn mail_monitors(
    definitions: &dyn MonitorDefinitions,
    summaries: Vec<MonitorSummary>,
) -> Vec<(MonitorMeta, Option<String>)> {
    let mut monitors = Vec::new();
    for summary in summaries.into_iter().filter(|s| {
        s.source
            .as_deref()
            .is_some_and(|tool| MAIL_READERS.contains(&tool))
    }) {
        if let Some(def) = definitions
            .stored_definition(&summary.id, summary.owner.as_deref())
            .await
        {
            monitors.push((def.monitor, summary.owner));
        }
    }
    monitors
}

fn rule_of(input: &Value) -> Result<SortRule, String> {
    let rule: SortRule =
        serde_json::from_value(input.clone()).map_err(|e| format!("Invalid rule: {e}"))?;
    rule.validate()?;
    Ok(rule)
}

fn rule_id_of(input: &Value) -> Result<String, String> {
    input["rule_id"]
        .as_str()
        .filter(|id| id.starts_with(PREFIX))
        .map(str::to_string)
        .ok_or_else(|| "Say which rule: its rule_id from list.".to_string())
}

/// `owner`'s sort rule `id` among `monitors`: never a monitor of another
/// shape that took a rule's id, nor another person's rule.
fn existing(
    monitors: &[(MonitorMeta, Option<String>)],
    owner: &str,
    id: &str,
) -> Result<SortRule, String> {
    owned_by(monitors, owner)
        .find(|(meta, _, _)| meta.id == id)
        .map(|(_, _, rule)| rule)
        .ok_or_else(|| format!("There is no sort rule {id}; list them to see their ids."))
}

/// The person a sort rule call acts for: rules are each person's own.
fn person() -> Result<String, String> {
    current_turn_user()
        .ok_or_else(|| "Sort rules are a person's own, and this call acts for no one.".to_string())
}

/// Whether the rule's account and folders are there, by a dry run of
/// applying it once: how many messages it takes now.
async fn checked(tools: &ToolRegistry, rule: &SortRule) -> Result<Value, String> {
    let once = rule.apply_once();
    let name = once["tool"].as_str().unwrap_or_default();
    let tool = tools.get(name).ok_or_else(|| {
        format!("The email tools aren't loaded for this agent, so {name} can't check the rule's folders; turn on the email skill and try again.")
    })?;
    let result = tool
        .execute(once["input"].clone())
        .await
        .map_err(|e| format!("{e:#}"))?;
    match result.success {
        true => Ok(result.output["count"].clone()),
        false => Err(format!(
            "The rule can't run as it is: {}",
            result.error.unwrap_or_default()
        )),
    }
}

pub struct SortRulesTool {
    monitors: Arc<MonitorTool>,
    tools: Arc<ToolRegistry>,
}

impl SortRulesTool {
    pub fn new(monitors: Arc<MonitorTool>, tools: Arc<ToolRegistry>) -> Self {
        Self { monitors, tools }
    }

    async fn run(&self, input: &Value) -> Result<Value, String> {
        let owner = person()?;
        let monitors = self.monitors.mail_monitors().await;
        match input["action"].as_str().unwrap_or_default() {
            "list" => {
                let rules = rules_in(&monitors, &owner);
                Ok(json!({ "rules": rules, "count": rules.len() }))
            }
            "propose" => {
                let mut rule = rule_of(input)?;
                if !rule.rule_id.is_empty() {
                    existing(&monitors, &owner, &rule.rule_id)?;
                }
                let matches_now = checked(&self.tools, &rule).await?;
                rule.watched_by = rule.watchers(Some(&owner), &monitors);
                let next = match rule.watched_by.is_empty() {
                    true => "Show the person the rule in words; on their yes, save it with email.sort_rule_save, passing `rule` as it is.".to_string(),
                    false => format!(
                        "Tell the person this rule takes mail that {} also watch, so those may not see it; on their yes, save it with email.sort_rule_save, passing `rule` as it is.",
                        rule.watched_by.join(", ")
                    ),
                };
                Ok(json!({
                    "says": rule.says(),
                    "matches_now": matches_now,
                    "watched_by": rule.watched_by,
                    "rule": rule,
                    "next": next
                }))
            }
            "set_enabled" => {
                let enabled = input["enabled"]
                    .as_bool()
                    .ok_or("Say whether the rule is on: enabled = true or false.")?;
                let id = rule_id_of(input)?;
                existing(&monitors, &owner, &id)?;
                call(
                    &self.monitors,
                    json!({ "action": "set_enabled", "monitor_id": id, "enabled": enabled, "owner": owner }),
                )
                .await
            }
            "remove" => {
                let id = rule_id_of(input)?;
                existing(&monitors, &owner, &id)?;
                call(
                    &self.monitors,
                    json!({ "action": "delete", "monitor_id": id, "owner": owner }),
                )
                .await
            }
            other => Err(format!(
                "Unknown action '{other}'. Use list, propose, set_enabled or remove."
            )),
        }
    }
}

#[async_trait]
impl Tool for SortRulesTool {
    fn name(&self) -> &str {
        "email.sort_rules"
    }

    fn description(&self) -> &str {
        "Standing rules that sort new mail as it arrives (move, archive, mark read, flag), on any account: list them, propose one (checks its folders, says it in words and names any monitor that watches the same mail), pause or resume one, or remove one. Saving needs the person's yes: email.sort_rule_save. Each listed rule carries `apply_once`, the call that applies it to mail already there."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["list", "propose", "set_enabled", "remove"] },
                "rule_id": { "type": "string", "description": "The rule (set_enabled, remove; propose when editing one)" },
                "enabled": { "type": "boolean", "description": "On or off (set_enabled)" },
                "account": { "type": "string", "description": "The account it sorts (propose)" },
                "folder": { "type": "string", "description": "The folder it watches (default: INBOX)" },
                "match": schemas::search(),
                "then": then_schema()
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        Ok(ToolResult::timed(self.run(&input).await, start))
    }
}

fn then_schema() -> Value {
    json!({
        "type": "object",
        "description": "What happens to each new message that matches",
        "properties": {
            "do": { "type": "string", "enum": ["move", "archive", "mark_read", "flag"] },
            "to": { "type": "string", "description": "The folder to move it to (move)" }
        },
        "required": ["do"]
    })
}

pub struct SortRuleSaveTool {
    monitors: Arc<MonitorTool>,
    tools: Arc<ToolRegistry>,
}

impl SortRuleSaveTool {
    pub fn new(monitors: Arc<MonitorTool>, tools: Arc<ToolRegistry>) -> Self {
        Self { monitors, tools }
    }

    async fn run(&self, input: &Value) -> Result<Value, String> {
        let rule = rule_of(input)?;
        let owner = person()?;
        let monitors = self.monitors.mail_monitors().await;
        let id = match rule.rule_id.is_empty() {
            true => format!(
                "{PREFIX}{}",
                &uuid::Uuid::new_v4().simple().to_string()[..12]
            ),
            false => existing(&monitors, &owner, &rule.rule_id)?.rule_id,
        };
        let unnamed: Vec<String> = rule
            .watchers(Some(&owner), &monitors)
            .into_iter()
            .filter(|w| !rule.watched_by.contains(w))
            .collect();
        if !unnamed.is_empty() {
            return Err(format!(
                "{} also watch this mail, and the person wasn't told; propose the rule again and tell them.",
                unnamed.join(", ")
            ));
        }
        checked(&self.tools, &rule).await?;
        let toml = toml::to_string(&rule.monitor(&id)).map_err(|e| e.to_string())?;
        let saved = call(
            &self.monitors,
            json!({ "action": "save", "monitor_id": id, "toml_content": toml, "owner": owner }),
        )
        .await?;
        Ok(
            json!({ "rule_id": id, "says": rule.says(), "saved": saved, "apply_once": rule.apply_once() }),
        )
    }
}

#[async_trait]
impl Tool for SortRuleSaveTool {
    fn name(&self) -> &str {
        "email.sort_rule_save"
    }

    fn description(&self) -> &str {
        "Save a sort rule the person said yes to — the `rule` email.sort_rules propose returned — or an edit of one (with its rule_id). Mail already in the folder is left alone; `apply_once` in the result sorts it too, when the person wants that."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "rule_id": { "type": "string", "description": "The rule to change; omit for a new one" },
                "account": { "type": "string" },
                "folder": { "type": "string", "description": "Default: INBOX" },
                "match": schemas::search(),
                "then": then_schema(),
                "watched_by": { "type": "array", "items": { "type": "string" }, "description": "As propose returned it" }
            },
            "required": ["account", "match", "then"]
        })
    }

    fn approval_requirement(&self) -> Option<String> {
        Some(
            "A sort rule moves the person's mail on its own from now on, so they approve it once."
                .to_string(),
        )
    }

    fn approval_summary(&self, input: &Value) -> Option<String> {
        let rule = rule_of(input).ok()?;
        Some(match rule.watched_by.is_empty() {
            true => rule.says(),
            false => format!(
                "{} — takes mail {} also watch",
                rule.says(),
                rule.watched_by.join(", ")
            ),
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        Ok(ToolResult::timed(self.run(&input).await, start))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::MonitorRuntime;

    fn rule(then: Then) -> SortRule {
        SortRule {
            rule_id: String::new(),
            account: "home".to_string(),
            folder: "INBOX".to_string(),
            matches: Criteria {
                from: Some("news@shop.com".to_string()),
                ..Criteria::default()
            },
            then,
            watched_by: Vec::new(),
        }
    }

    #[test]
    fn a_rule_is_a_monitor_and_back() {
        for then in [
            Then::Move {
                to: "News".to_string(),
            },
            Then::Archive,
            Then::MarkRead,
            Then::Flag,
        ] {
            let rule = SortRule {
                rule_id: "sort-abc".to_string(),
                ..rule(then)
            };
            let def = rule.monitor("sort-abc");
            def.validate().unwrap();
            let toml = toml::to_string(&def).unwrap();
            let back = MonitorDefinition::from_toml(&toml).unwrap();
            assert_eq!(SortRule::of(&back.monitor), Some(rule.clone()));
        }
    }

    #[test]
    fn each_new_message_is_handed_to_the_mail_tool_by_its_uid_and_numbering() {
        let def = rule(Then::Move {
            to: "News".to_string(),
        })
        .monitor("sort-x");
        let source = def.monitor.source().unwrap();
        assert_eq!(source.tool, "email.search");
        assert_eq!(source.input["since"], RECENT);
        assert_eq!(source.input["search"]["from"], "news@shop.com");
        let Action::RunSkill { tool, input } = &def.monitor.action else {
            panic!("a rule runs a tool");
        };
        assert_eq!(tool, "email.move");
        assert_eq!(
            source.item_id(),
            "/key",
            "a renumbered folder's mail is new mail"
        );
        assert_eq!(input["uid"], "{{json:/uid}}");
        assert_eq!(input["validity"], "{{json:/validity}}");
        assert_eq!(input["to_folder"], "News");
    }

    #[test]
    fn a_monitor_of_another_shape_is_not_a_rule() {
        let mut def = rule(Then::Archive).monitor("sort-x");
        def.monitor.id = "news-digest".to_string();
        assert_eq!(SortRule::of(&def.monitor), None);
        let mut agent = rule(Then::Archive).monitor("sort-y");
        agent.monitor.action = Action::AgentTask {
            agent: None,
            prompt: "x".to_string(),
            deliver_to: String::new(),
            surface: None,
            tools: Vec::new(),
        };
        assert_eq!(SortRule::of(&agent.monitor), None);
    }

    fn watcher(id: &str, tool: &str, input: Value, enabled: bool) -> MonitorMeta {
        let mut def = rule(Then::Archive).monitor(id);
        def.monitor.name = id.to_string();
        def.monitor.enabled = enabled;
        def.monitor.source = Some(Source {
            tool: tool.to_string(),
            input,
            every_secs: None,
            items: Some("/messages".to_string()),
            item_id: Some("/uid".to_string()),
            path: None,
            text: None,
        });
        def.monitor.action = Action::Notify {
            target: None,
            template: "{{text}}".to_string(),
            surface: Some("desk".to_string()),
            in_thread: false,
        };
        def.monitor
    }

    #[test]
    fn a_monitor_reading_the_same_accounts_folder_watches_its_mail() {
        let r = SortRule {
            rule_id: "sort-me".to_string(),
            ..rule(Then::Archive)
        };
        let monitors = vec![
            watcher("boss-mail", "email.inbox", json!({"account": "home"}), true),
            watcher("all-mail", "email.inbox", json!({}), true),
            watcher("work-mail", "email.inbox", json!({"account": "work"}), true),
            watcher(
                "receipts",
                "email.search",
                json!({"account": "home", "folder": "Receipts"}),
                true,
            ),
            watcher("paused", "email.inbox", json!({"account": "home"}), false),
            watcher("sort-me", "email.search", json!({"account": "home"}), true),
        ];
        let kat = Some("kat".to_string());
        let mut owned: Vec<_> = monitors.into_iter().map(|m| (m, kat.clone())).collect();
        owned[1].1 = None;
        owned.push((
            watcher("bobs-mail", "email.inbox", json!({"account": "home"}), true),
            Some("bob".to_string()),
        ));
        assert_eq!(
            r.watchers(Some("kat"), &owned),
            vec!["boss-mail", "all-mail"],
            "the owner's own and the hub's, never another person's"
        );
        owned.push((r.monitor("sort-me").monitor, kat));
        let listed = rules_in(&owned, "kat");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["owner"], "kat");
        assert_eq!(listed[0]["watched_by"], json!(["boss-mail", "all-mail"]));
    }

    #[test]
    fn a_person_sees_and_changes_only_their_own_rules() {
        let theirs = |id: &str, owner: Option<&str>| {
            (
                rule(Then::Archive).monitor(id).monitor,
                owner.map(str::to_string),
            )
        };
        let monitors = vec![
            theirs("sort-kat", Some("kat")),
            theirs("sort-bob", Some("bob")),
            theirs("sort-hub", None),
        ];
        let ids = |owner: &str| -> Vec<Value> {
            rules_in(&monitors, owner)
                .into_iter()
                .map(|r| r["rule_id"].clone())
                .collect()
        };
        assert_eq!(ids("kat"), vec![json!("sort-kat")]);
        assert_eq!(ids("bob"), vec![json!("sort-bob")]);
        assert!(ids("admin").is_empty(), "seeing a monitor isn't owning it");
        assert!(existing(&monitors, "kat", "sort-kat").is_ok());
        for other in ["sort-bob", "sort-hub"] {
            assert!(existing(&monitors, "kat", other)
                .unwrap_err()
                .contains("no sort rule"));
        }
    }

    #[tokio::test]
    async fn a_rule_call_that_acts_for_no_one_is_refused() {
        let (_dir, rules, save) = tools();
        assert!(rules
            .run(&json!({"action": "list"}))
            .await
            .unwrap_err()
            .contains("acts for no one"));
        let input = json!({"account": "home", "match": {"from": "x"}, "then": {"do": "archive"}});
        assert!(save
            .run(&input)
            .await
            .unwrap_err()
            .contains("acts for no one"));
    }

    async fn as_kat<T>(work: impl std::future::Future<Output = T>) -> T {
        workforce_agent::runner::CURRENT_USER_ID
            .scope("kat".to_string(), work)
            .await
    }

    #[test]
    fn a_rule_reads_as_words_and_refuses_what_it_cant_be() {
        assert_eq!(
            rule(Then::Move {
                to: "News".to_string()
            })
            .says(),
            "New mail in home INBOX from news@shop.com → move it to News"
        );
        assert!(rule(Then::Move {
            to: " ".to_string()
        })
        .validate()
        .is_err());
        let everything = SortRule {
            matches: Criteria::default(),
            ..rule(Then::Archive)
        };
        assert!(
            everything.validate().is_err(),
            "a rule never matches all mail"
        );
        let bad_id = SortRule {
            rule_id: "monitor-1".to_string(),
            ..rule(Then::Archive)
        };
        assert!(bad_id.validate().is_err());
        assert!(rule_of(
            &json!({"account": "home", "match": {"from": "x"}, "then": {"do": "move", "to": "A"}})
        )
        .is_ok());
    }

    #[test]
    fn applying_a_rule_once_is_a_dry_run_of_its_search() {
        let once = rule(Then::Move {
            to: "News".to_string(),
        })
        .apply_once();
        assert_eq!(once["tool"], "email.move");
        assert_eq!(once["input"]["search"]["from"], "news@shop.com");
        assert_eq!(once["input"]["dry_run"], true);
        assert!(once["input"].get("uid").is_none());
    }

    #[test]
    fn the_hub_names_its_mail_tools_for_the_skill() {
        use workforce_skills::adapters::email::HUB_TOOLS;
        let (_dir, rules, save) = tools();
        assert!(HUB_TOOLS.contains(&rules.name()) && HUB_TOOLS.contains(&save.name()));
    }

    /// A stand-in for the email skill's tool: a dry run that counts two
    /// messages, or finds no folder `Nowhere`.
    struct DryRun(&'static str);

    #[async_trait]
    impl Tool for DryRun {
        fn name(&self) -> &str {
            self.0
        }

        fn description(&self) -> &str {
            "dry run"
        }

        fn input_schema(&self) -> Value {
            json!({ "type": "object" })
        }

        async fn execute(&self, input: Value) -> Result<ToolResult> {
            assert_eq!(input["dry_run"], true, "checking a rule changes nothing");
            Ok(match input["to_folder"].as_str() {
                Some("Nowhere") => {
                    ToolResult::failure("There is no folder Nowhere.".to_string(), 0)
                }
                _ => ToolResult::success(json!({ "count": 2 }), 0),
            })
        }
    }

    fn tools() -> (tempfile::TempDir, SortRulesTool, SortRuleSaveTool) {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MonitorRuntime::new(dir.path().to_path_buf()));
        let (rules, save) = tools_over(runtime);
        (dir, rules, save)
    }

    fn tools_over(runtime: Arc<MonitorRuntime>) -> (SortRulesTool, SortRuleSaveTool) {
        let monitors = Arc::new(MonitorTool::new(runtime, None));
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(DryRun("email.move")));
        (
            SortRulesTool::new(Arc::clone(&monitors), Arc::clone(&registry)),
            SortRuleSaveTool::new(monitors, registry),
        )
    }

    #[test]
    fn saving_a_rule_needs_the_persons_yes_and_names_who_else_watches() {
        let (_dir, rules, save) = tools();
        assert!(save.approval_requirement().is_some());
        let input = json!({
            "account": "home", "match": {"from": "news@shop.com"},
            "then": {"do": "archive"}, "watched_by": ["boss-mail"]
        });
        assert_eq!(
            save.approval_summary(&input).unwrap(),
            "New mail in home INBOX from news@shop.com → archive it — takes mail boss-mail also watch"
        );
        assert!(rules.approval_requirement().is_none());
    }

    /// A person's rule is a member-owned monitor, which only a database
    /// store keeps.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_proposed_rule_saves_as_a_monitor_and_lists_back() {
        use workforce_orchestrator::workflow_store::WorkflowStore;
        workforce_postgres::test_support::with_test_pool(
            WorkflowStore::build_pool,
            |pool| async move {
                let dir = tempfile::tempdir().unwrap();
                let runtime = Arc::new(MonitorRuntime::new(dir.path().to_path_buf()));
                runtime.set_store(WorkflowStore::new(pool).await.unwrap());
                runtime.reload().await.unwrap();
                as_kat(a_proposed_rule_saves_and_lists_back(tools_over(runtime))).await;
            },
        )
        .await;
    }

    async fn a_proposed_rule_saves_and_lists_back(
        (rules, save): (SortRulesTool, SortRuleSaveTool),
    ) {
        let proposed = rules
            .run(&json!({
                "action": "propose", "account": "home",
                "match": {"from": "news@shop.com"}, "then": {"do": "move", "to": "News"}
            }))
            .await
            .unwrap();
        assert_eq!(proposed["matches_now"], 2);
        let saved = save.run(&proposed["rule"]).await.unwrap();
        let id = saved["rule_id"].as_str().unwrap().to_string();
        assert!(id.starts_with(PREFIX));
        let listed = rules.run(&json!({"action": "list"})).await.unwrap();
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["rules"][0]["rule_id"], id);
        assert_eq!(
            listed["rules"][0]["says"],
            "New mail in home INBOX from news@shop.com → move it to News"
        );
        rules
            .run(&json!({"action": "remove", "rule_id": id}))
            .await
            .unwrap();
        assert_eq!(
            rules.run(&json!({"action": "list"})).await.unwrap()["count"],
            0
        );
        assert!(rules
            .run(&json!({"action": "remove", "rule_id": id}))
            .await
            .unwrap_err()
            .contains("no sort rule"));
    }

    #[tokio::test]
    async fn a_rule_to_a_missing_folder_or_on_another_monitor_is_refused() {
        as_kat(a_rule_on_nothing_or_another_monitor_is_refused()).await;
    }

    async fn a_rule_on_nothing_or_another_monitor_is_refused() {
        let (_dir, rules, save) = tools();
        let err = rules
            .run(&json!({
                "action": "propose", "account": "home",
                "match": {"from": "news@shop.com"}, "then": {"do": "move", "to": "Nowhere"}
            }))
            .await
            .unwrap_err();
        assert!(err.contains("There is no folder Nowhere"), "{err}");
        let mut digest = rule(Then::Archive).monitor("sort-digest");
        digest.monitor.action = Action::RunSkill {
            tool: "email.mark".to_string(),
            input: json!({ "account": "home", "uid": "{{json:/uid}}", "action": "unread" }),
        };
        let other = toml::to_string(&digest).unwrap();
        call(
            &rules.monitors,
            json!({ "action": "save", "monitor_id": "sort-digest", "toml_content": other }),
        )
        .await
        .unwrap();
        for input in [
            json!({"action": "set_enabled", "rule_id": "sort-digest", "enabled": false}),
            json!({"action": "remove", "rule_id": "sort-digest"}),
        ] {
            assert!(rules
                .run(&input)
                .await
                .unwrap_err()
                .contains("no sort rule"));
        }
        let edit = json!({
            "rule_id": "sort-digest", "account": "home",
            "match": {"from": "news@shop.com"}, "then": {"do": "archive"}
        });
        assert!(save.run(&edit).await.unwrap_err().contains("no sort rule"));
    }
}
