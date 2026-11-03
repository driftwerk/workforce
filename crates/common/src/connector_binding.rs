//! `[skills]` in an agent's `config.toml`: which connectors reach the agent,
//! which of them only read, and which use the agent's own connection rather
//! than the person's. Read only on a hub with `[desk]` enabled.
//!
//! ```toml
//! [skills]
//! enabled        = ["linear", "github"]  # present: only these connectors' tools reach it
//! read_only      = ["github"]            # their write tools are hidden too
//! own_credential = ["linear"]            # "its own connection"; the rest use "my connection"
//! ```
//!
//! Per agent, `enabled` is deliberately unlike the install's `[skills]
//! enabled`, where an empty list means every skill is eligible:
//! - absent: every connector, as before the key existed, so an agent nobody
//!   has chosen connectors for keeps what it had;
//! - `[]`: none.
//!
//! The table is kept as a raw value and checked here, so what is wrong with
//! it is said in plain words. Keys this hub does not know are ignored.

use toml::Value;

type Table = toml::map::Map<String, Value>;

/// The connectors a binding governs: the ones Desk lists in Connections and
/// each agent's Connectors tab. Every other built-in adapter (`browser`,
/// `xero`, …) is not chosen per agent and always reaches it.
pub const CONNECTORS: &[&str] = &[
    "github", "google", "email", "jira", "linear", "slack", "datadog", "metabase",
];

/// The connector `tool` (`linear.issue_get`) belongs to, if a binding
/// governs it.
pub fn connector_of(tool: &str) -> Option<&'static str> {
    let (prefix, _) = tool.split_once('.')?;
    CONNECTORS.iter().copied().find(|c| *c == prefix)
}

/// An agent's connectors, as its `[skills]` says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectorBinding {
    /// `None`: every connector reaches it, as before the key existed.
    pub enabled: Option<Vec<String>>,
    pub read_only: Vec<String>,
    pub own_credential: Vec<String>,
}

impl ConnectorBinding {
    /// Whether `connector`'s tools reach the agent at all.
    pub fn is_on(&self, connector: &str) -> bool {
        self.enabled
            .as_ref()
            .is_none_or(|on| on.iter().any(|c| c == connector))
    }

    /// Whether `connector` only reads for the agent.
    pub fn reads_only(&self, connector: &str) -> bool {
        self.read_only.iter().any(|c| c == connector)
    }

    /// Whether the agent uses its own connection to `connector`.
    pub fn own(&self, connector: &str) -> bool {
        self.own_credential.iter().any(|c| c == connector)
    }
}

fn names(t: &Table, key: &str, what: &str) -> Result<Option<Vec<String>>, String> {
    let wrong = || format!("{what} must be a list of connector names.");
    match t.get(key) {
        None => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::String(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
                _ => Err(wrong()),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|mut v| {
                v.dedup();
                Some(v)
            }),
        Some(_) => Err(wrong()),
    }
}

/// The `[skills]` table as the hub runs it for one agent, or what is wrong
/// with it.
pub fn from_value(value: &Value) -> Result<ConnectorBinding, String> {
    let Value::Table(t) = value else {
        return Err("The agent's connectors need to be a group of settings.".into());
    };
    Ok(ConnectorBinding {
        enabled: names(t, "enabled", "The connectors an agent has")?,
        read_only: names(t, "read_only", "The connectors an agent only reads")?.unwrap_or_default(),
        own_credential: names(
            t,
            "own_credential",
            "The connectors an agent uses its own connection for",
        )?
        .unwrap_or_default(),
    })
}

/// [`from_value`] of an agent's `[skills]`, every connector where it has none.
pub fn of(value: Option<&Value>) -> Result<ConnectorBinding, String> {
    value.map_or_else(|| Ok(ConnectorBinding::default()), from_value)
}

/// Words that name a change.
const WRITES: &[&str] = &[
    "add",
    "append",
    "archive",
    "assign",
    "bulk",
    "cancel",
    "clear",
    "clone",
    "configure",
    "confirm",
    "connect",
    "copy",
    "create",
    "delete",
    "disconnect",
    "draft",
    "edit",
    "forward",
    "invite",
    "kick",
    "link",
    "mark",
    "merge",
    "move",
    "mute",
    "native",
    "pin",
    "post",
    "react",
    "remove",
    "rename",
    "reply",
    "rerun",
    "review",
    "scaffold",
    "send",
    "set",
    "setup",
    "share",
    "transition",
    "unarchive",
    "unmute",
    "unpin",
    "update",
    "upload",
    "write",
];

/// Words that name only a read.
const READS: &[&str] = &[
    "accounts",
    "aggregate",
    "children",
    "compare",
    "contacts",
    "current",
    "diff",
    "export",
    "fields",
    "find",
    "folders",
    "get",
    "inbox",
    "info",
    "items",
    "list",
    "logs",
    "lookup",
    "members",
    "metadata",
    "query",
    "read",
    "runs",
    "scalar",
    "search",
    "status",
    "tables",
    "view",
];

/// Whether connector `tool` only reads. Its name after the connector, split
/// into words (`gmail.search`, `issue_list`): a read names a read word and
/// no change; anything else, a generic `api` passthrough included, is a
/// write.
pub fn is_read(tool: &str) -> bool {
    let Some((_, action)) = tool.split_once('.') else {
        return false;
    };
    let words: Vec<&str> = action.split(['.', '_']).collect();
    words.iter().any(|w| READS.contains(w))
        && !words.iter().any(|w| WRITES.contains(w) || *w == "api")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_writes_are_told_apart_and_what_is_unclear_is_a_write() {
        for read in [
            "linear.issue_get",
            "linear.comment_list",
            "github.pr_diff",
            "github.workflow_run_logs",
            "google.gmail.search",
            "google.chat.messages_list",
            "email.inbox",
            "slack.read",
            "slack.lookup_user_by_email",
            "datadog.logs_search",
            "metabase.card_query",
        ] {
            assert!(is_read(read), "{read} reads");
        }
        for write in [
            "linear.issue_create",
            "linear.issue_bulk_update",
            "github.pr_merge",
            "github.pr_comment",
            "github.api",
            "github.branch_protection",
            "google.gmail.send",
            "google.gmail.draft_send",
            "email.send",
            "email.rules",
            "slack.post_message",
            "slack.send",
            "slack.react",
            "datadog.monitors_mute",
            "metabase.dataset_query_native",
            "jira.transition_issue",
            "nameless",
        ] {
            assert!(!is_read(write), "{write} writes");
        }
    }

    fn parse(source: &str) -> Result<ConnectorBinding, String> {
        let v: Value = toml::from_str(source).unwrap();
        of(v.get("skills"))
    }

    #[test]
    fn only_desks_connectors_are_governed() {
        assert_eq!(connector_of("linear.issue_get"), Some("linear"));
        assert_eq!(connector_of("google.gmail.send"), Some("google"));
        assert_eq!(connector_of("browser.navigate"), None);
        assert_eq!(connector_of("xero.invoices_list"), None);
        assert_eq!(connector_of("memory.recall"), None);
        assert_eq!(connector_of("linear"), None);
    }

    #[test]
    fn an_agent_with_no_skills_has_every_connector_as_before() {
        let none = parse("").unwrap();
        assert_eq!(none, ConnectorBinding::default());
        assert!(none.is_on("linear") && !none.reads_only("linear") && !none.own("linear"));
        // A table without `enabled` changes nothing either.
        assert!(parse("[skills]\nread_only = [\"github\"]\n")
            .unwrap()
            .is_on("github"));
    }

    #[test]
    fn an_empty_list_is_none_and_a_list_is_only_those() {
        let none = parse("[skills]\nenabled = []\n").unwrap();
        assert_eq!(none.enabled, Some(vec![]));
        assert!(!none.is_on("linear"));
        let two = parse(
            "[skills]\nenabled = [\"linear\", \"github\"]\nread_only = [\"github\"]\nown_credential = [\"linear\"]\n",
        )
        .unwrap();
        assert!(two.is_on("linear") && two.is_on("github") && !two.is_on("jira"));
        assert!(two.reads_only("github") && !two.reads_only("linear"));
        assert!(two.own("linear") && !two.own("github"));
    }

    #[test]
    fn what_is_wrong_is_said_in_plain_words() {
        for (source, want) in [
            (
                "[skills]\nenabled = \"linear\"\n",
                "list of connector names",
            ),
            ("[skills]\nread_only = [1]\n", "only reads"),
            ("[skills]\nown_credential = [\"\"]\n", "its own connection"),
            ("skills = 1\n", "group of settings"),
        ] {
            let e = parse(source).unwrap_err();
            assert!(e.contains(want), "{source}: {e}");
        }
    }
}
