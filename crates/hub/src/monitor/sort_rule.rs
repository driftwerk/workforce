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
use workforce_agent::tools::{Tool, ToolResult};
use workforce_orchestrator::monitor::{Action, MonitorDefinition, MonitorMeta, Source};
use workforce_skills::adapters::email::criteria::Criteria;
use workforce_skills::adapters::email::schemas;

use super::MonitorTool;

/// Every sort rule's monitor id starts so.
pub const PREFIX: &str = "sort-";
const EVERY_SECS: u64 = 300;
/// How far back each poll looks: new mail, cheaply.
const RECENT: &str = "2d";
const POLL_LIMIT: u64 = 100;
const MAIL_READERS: &[&str] = &["email.inbox", "email.search"];

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

fn folder_of(input: &Value) -> &str {
    input["folder"]
        .as_str()
        .filter(|f| !f.is_empty())
        .unwrap_or("INBOX")
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

    fn tool_and_input(&self, uid: Value) -> (&'static str, Value) {
        let (account, folder) = (self.account.as_str(), self.folder.as_str());
        match &self.then {
            Then::Move { to } => (
                "email.move",
                json!({ "account": account, "from_folder": folder, "to_folder": to, "uid": uid }),
            ),
            Then::Archive => (
                "email.archive",
                json!({ "account": account, "folder": folder, "uid": uid }),
            ),
            Then::MarkRead => (
                "email.mark",
                json!({ "account": account, "folder": folder, "uid": uid, "action": "read" }),
            ),
            Then::Flag => (
                "email.mark",
                json!({ "account": account, "folder": folder, "uid": uid, "action": "flag" }),
            ),
        }
    }

    /// The call that applies this rule once to the mail already there.
    pub fn apply_once(&self) -> Value {
        let (tool, mut input) = self.tool_and_input(Value::Null);
        if let Some(obj) = input.as_object_mut() {
            obj.remove("uid");
            obj.insert("search".to_string(), json!(self.matches));
            obj.insert("dry_run".to_string(), json!(true));
        }
        json!({ "tool": tool, "input": input })
    }

    /// The monitor that is this rule.
    pub fn monitor(&self, id: &str) -> MonitorDefinition {
        let (tool, input) = self.tool_and_input(json!("{{id}}"));
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
                    item_id: Some("/uid".to_string()),
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
                && s.item_id() == "/uid"
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
            folder: folder_of(&source.input).to_string(),
            matches: serde_json::from_value(source.input["search"].clone()).ok()?,
            then,
            watched_by: Vec::new(),
        })
    }

    /// Whether `other`, another enabled monitor, watches the mail this rule
    /// takes: it reads the same account's folder.
    pub fn watched_by_monitor(&self, other: &MonitorMeta) -> bool {
        other.id != self.rule_id
            && other.enabled
            && other.source().is_some_and(|s| {
                MAIL_READERS.contains(&s.tool.as_str())
                    && s.input["account"]
                        .as_str()
                        .is_none_or(|a| a == self.account)
                    && folder_of(&s.input).eq_ignore_ascii_case(&self.folder)
            })
    }

    fn watchers<'a>(&self, monitors: impl IntoIterator<Item = &'a MonitorMeta>) -> Vec<String> {
        monitors
            .into_iter()
            .filter(|m| self.watched_by_monitor(m))
            .map(|m| m.name.clone())
            .collect()
    }
}

fn ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
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

/// The sort rules among `monitors` (each with its owner), in words, each
/// with the other monitors that watch the mail it takes.
pub fn rules_in(monitors: &[(MonitorMeta, Option<String>)]) -> Vec<Value> {
    monitors
        .iter()
        .filter_map(|(meta, owner)| SortRule::of(meta).map(|rule| (meta, owner, rule)))
        .map(|(meta, owner, rule)| {
            json!({
                "rule_id": rule.rule_id,
                "owner": owner,
                "says": rule.says(),
                "enabled": meta.enabled,
                "watched_by": rule.watchers(monitors.iter().map(|(m, _)| m)),
                "apply_once": rule.apply_once(),
                "rule": rule,
            })
        })
        .collect()
}

/// The turn's user's monitors that read mail, by their definitions, with
/// their owners.
async fn mail_monitors(
    monitors: &MonitorTool,
) -> Result<Vec<(MonitorMeta, Option<String>)>, String> {
    let listed = call(monitors, json!({ "action": "list" })).await?;
    let readers: Vec<(String, Option<String>)> = listed["monitors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| {
            m["source"]
                .as_str()
                .is_some_and(|tool| MAIL_READERS.contains(&tool))
        })
        .filter_map(|m| {
            Some((
                m["id"].as_str()?.to_string(),
                m["owner"].as_str().map(str::to_string),
            ))
        })
        .collect();
    let mut metas = Vec::new();
    for (id, owner) in readers {
        let got = call(
            monitors,
            json!({ "action": "get", "monitor_id": id, "owner": owner.clone().unwrap_or_default() }),
        )
        .await?;
        if let Some(def) = got["toml"]
            .as_str()
            .and_then(|toml| MonitorDefinition::from_toml(toml).ok())
        {
            metas.push((def.monitor, owner));
        }
    }
    Ok(metas)
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

pub struct SortRulesTool {
    monitors: Arc<MonitorTool>,
}

impl SortRulesTool {
    pub fn new(monitors: Arc<MonitorTool>) -> Self {
        Self { monitors }
    }

    async fn run(&self, input: &Value) -> Result<Value, String> {
        match input["action"].as_str().unwrap_or_default() {
            "list" => {
                let rules = rules_in(&mail_monitors(&self.monitors).await?);
                Ok(json!({ "rules": rules, "count": rules.len() }))
            }
            "propose" => {
                let mut rule = rule_of(input)?;
                let monitors = mail_monitors(&self.monitors).await?;
                rule.watched_by = rule.watchers(monitors.iter().map(|(m, _)| m));
                let next = match rule.watched_by.is_empty() {
                    true => "Show the person the rule in words; on their yes, save it with email.sort_rule_save, passing `rule` as it is.".to_string(),
                    false => format!(
                        "Tell the person this rule takes mail that {} also watch, so those may not see it; on their yes, save it with email.sort_rule_save, passing `rule` as it is.",
                        rule.watched_by.join(", ")
                    ),
                };
                Ok(
                    json!({ "says": rule.says(), "watched_by": rule.watched_by, "rule": rule, "next": next }),
                )
            }
            "set_enabled" => {
                let enabled = input["enabled"]
                    .as_bool()
                    .ok_or("Say whether the rule is on: enabled = true or false.")?;
                call(
                    &self.monitors,
                    json!({ "action": "set_enabled", "monitor_id": rule_id_of(input)?, "enabled": enabled }),
                )
                .await
            }
            "remove" => {
                call(
                    &self.monitors,
                    json!({ "action": "delete", "monitor_id": rule_id_of(input)? }),
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
        "Standing rules that sort new mail as it arrives (move, archive, mark read, flag), on any account: list them, propose one (says it in words and names any monitor that watches the same mail), pause or resume one, or remove one. Saving needs the person's yes: email.sort_rule_save. Each listed rule carries `apply_once`, the call that applies it to mail already there."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["list", "propose", "set_enabled", "remove"] },
                "rule_id": { "type": "string", "description": "The rule (set_enabled, remove)" },
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
        Ok(match self.run(&input).await {
            Ok(out) => ToolResult::success(out, ms(start)),
            Err(why) => ToolResult::failure(why, ms(start)),
        })
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
}

impl SortRuleSaveTool {
    pub fn new(monitors: Arc<MonitorTool>) -> Self {
        Self { monitors }
    }

    async fn run(&self, input: &Value) -> Result<Value, String> {
        let rule = rule_of(input)?;
        let monitors = mail_monitors(&self.monitors).await?;
        let unnamed: Vec<String> = rule
            .watchers(monitors.iter().map(|(m, _)| m))
            .into_iter()
            .filter(|w| !rule.watched_by.contains(w))
            .collect();
        if !unnamed.is_empty() {
            return Err(format!(
                "{} also watch this mail, and the person wasn't told; propose the rule again and tell them.",
                unnamed.join(", ")
            ));
        }
        let id = match rule.rule_id.is_empty() {
            true => format!(
                "{PREFIX}{}",
                &uuid::Uuid::new_v4().simple().to_string()[..12]
            ),
            false => rule.rule_id.clone(),
        };
        let toml = toml::to_string(&rule.monitor(&id)).map_err(|e| e.to_string())?;
        let saved = call(
            &self.monitors,
            json!({ "action": "save", "monitor_id": id, "toml_content": toml }),
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
        Ok(match self.run(&input).await {
            Ok(out) => ToolResult::success(out, ms(start)),
            Err(why) => ToolResult::failure(why, ms(start)),
        })
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
    fn each_new_message_is_handed_to_the_mail_tool_by_its_uid() {
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
        assert_eq!(input["uid"], "{{id}}");
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
        assert_eq!(r.watchers(&monitors), vec!["boss-mail", "all-mail"]);
        let owned: Vec<_> = monitors.into_iter().map(|m| (m, None)).collect();
        let rule_monitor = (r.monitor("sort-me").monitor, Some("kat".to_string()));
        let listed = rules_in(&[owned, vec![rule_monitor]].concat());
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["owner"], "kat");
        assert_eq!(listed[0]["watched_by"], json!(["boss-mail", "all-mail"]));
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
    fn saving_a_rule_needs_the_persons_yes_and_names_who_else_watches() {
        let dir = tempfile::tempdir().unwrap();
        let monitors = Arc::new(MonitorTool::new(
            Arc::new(MonitorRuntime::new(dir.path().to_path_buf())),
            None,
        ));
        let save = SortRuleSaveTool::new(Arc::clone(&monitors));
        assert!(save.approval_requirement().is_some());
        let input = json!({
            "account": "home", "match": {"from": "news@shop.com"},
            "then": {"do": "archive"}, "watched_by": ["boss-mail"]
        });
        assert_eq!(
            save.approval_summary(&input).unwrap(),
            "New mail in home INBOX from news@shop.com → archive it — takes mail boss-mail also watch"
        );
        assert!(SortRulesTool::new(monitors)
            .approval_requirement()
            .is_none());
    }

    #[tokio::test]
    async fn a_proposed_rule_saves_as_a_monitor_and_lists_back() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MonitorRuntime::new(dir.path().to_path_buf()));
        let monitors = Arc::new(MonitorTool::new(Arc::clone(&runtime), None));
        let rules = SortRulesTool::new(Arc::clone(&monitors));
        let save = SortRuleSaveTool::new(Arc::clone(&monitors));
        let proposed = rules
            .run(&json!({
                "action": "propose", "account": "home",
                "match": {"from": "news@shop.com"}, "then": {"do": "move", "to": "News"}
            }))
            .await
            .unwrap();
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
    }
}
