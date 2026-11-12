//! `[skills]` in an agent's `config.toml`: which connectors reach the agent,
//! which of them only read, and which use the agent's own connection rather
//! than the person's. Read only on a hub with `[desk]` enabled.
//!
//! ```toml
//! [skills]
//! enabled        = ["linear", "github", "jira"]  # present: only these connectors' tools reach it
//! read_only      = ["github", "jira:globex"]     # a connector's writes are hidden; one connection's are refused
//! own_credential = ["linear"]                    # "its own connection"; the rest use "my connection"
//!
//! [skills.connections]
//! jira = ["acme", "globex"]                      # the connections it uses, the first its default
//! ```
//!
//! `[skills.connections]` names, per connector, the instances of its adapter
//! (`skills/jira/workspaces/<instance>/…`) the agent uses, whichever level
//! each is kept at. A connector it doesn't name keeps the tiers
//! `own_credential` says; an empty list is the connector off.
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

/// Where each connector keeps its instances, one connection each: every
/// connector but Slack, which holds one.
pub const INSTANCE_PREFIXES: &[(&str, &str)] = &[
    ("github", "skills/github/workspaces/"),
    ("google", "skills/google/workspaces/"),
    ("email", "skills/email/accounts/"),
    ("jira", "skills/jira/workspaces/"),
    ("linear", "skills/linear/workspaces/"),
    ("datadog", "skills/datadog/accounts/"),
    ("metabase", "skills/metabase/instances/"),
];

/// The connectors whose tools resolve one instance per call
/// (`environment::resolve_instance`), so one connection of theirs can be
/// read only while another takes writes. Email's and Google's read only
/// whole.
pub const READ_ONLY_EACH: &[&str] = &["github", "jira", "linear", "datadog", "metabase"];

/// Where `connector` keeps its instances.
pub fn instance_prefix(connector: &str) -> Option<&'static str> {
    INSTANCE_PREFIXES
        .iter()
        .find(|(c, _)| *c == connector)
        .map(|(_, p)| *p)
}

/// The instance the logical path `path` of `connector` is part of: `None`
/// for a path outside its instances (a flat key, a selector), `Some(None)`
/// for its instances' listing (`skills/jira/workspaces/`), and
/// `Some(Some(key))` for one of an instance's.
pub fn instance_of<'a>(connector: &str, path: &'a str) -> Option<Option<&'a str>> {
    let rest = path.strip_prefix(instance_prefix(connector)?)?;
    Some(match rest.split_once('/') {
        Some((key, _)) if !key.is_empty() => Some(key),
        Some(_) => None,
        None if rest.is_empty() => None,
        None => Some(rest),
    })
}

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
    /// Connectors (`github`), and single connections (`jira:globex`), that
    /// only read.
    pub read_only: Vec<String>,
    pub own_credential: Vec<String>,
    /// `[skills.connections]`: per connector, the instances it uses, the
    /// first its default.
    pub connections: std::collections::BTreeMap<String, Vec<String>>,
}

impl ConnectorBinding {
    /// Whether `connector`'s tools reach the agent at all.
    pub fn is_on(&self, connector: &str) -> bool {
        self.enabled
            .as_ref()
            .is_none_or(|on| on.iter().any(|c| c == connector))
            && self.picks(connector).is_none_or(|p| !p.is_empty())
    }

    /// Whether `connector` only reads for the agent.
    pub fn reads_only(&self, connector: &str) -> bool {
        self.read_only.iter().any(|c| c == connector)
    }

    /// The instances of `connector` it uses, the first its default, where
    /// it names them.
    pub fn picks(&self, connector: &str) -> Option<&[String]> {
        self.connections.get(connector).map(Vec::as_slice)
    }

    /// Whether instance `instance` of `connector` only reads for the agent:
    /// the connector does, or that one connection does.
    pub fn reads_only_instance(&self, connector: &str, instance: &str) -> bool {
        self.reads_only(connector)
            || self
                .read_only
                .iter()
                .any(|r| r.split_once(':') == Some((connector, instance)))
    }

    /// Whether the agent uses its own connection to `connector`.
    pub fn own(&self, connector: &str) -> bool {
        self.own_credential.iter().any(|c| c == connector)
    }
}

/// What `after` lets an agent reach that `before` didn't, in words, one
/// line each: a connector turned on, a connection it now uses, a connector
/// or connection it may now change, a change of whose connection it uses.
/// Empty when the change only narrows what it reaches, or changes nothing.
pub fn widened(before: &ConnectorBinding, after: &ConnectorBinding) -> Vec<String> {
    let mut said = Vec::new();
    for c in CONNECTORS {
        if !after.is_on(c) {
            continue;
        }
        if !before.is_on(c) {
            said.push(format!("{c}: turned on"));
            continue;
        }
        match (before.picks(c), after.picks(c)) {
            (Some(was), Some(now)) => {
                for k in now.iter().filter(|k| !was.contains(k)) {
                    said.push(format!("{c}: now uses the `{k}` connection"));
                }
            }
            (Some(_), None) => said.push(format!("{c}: now uses every connection")),
            _ => {}
        }
        if before.own(c) != after.own(c) {
            said.push(format!("{c}: uses a different connection"));
        }
        let writes = |b: &ConnectorBinding, k: Option<&str>| match k {
            Some(k) => !b.reads_only_instance(c, k),
            None => !b.reads_only(c),
        };
        if !writes(before, None) && writes(after, None) && after.picks(c).is_none() {
            said.push(format!("{c}: may now change things"));
        }
        if let Some(now) = after.picks(c) {
            for k in now {
                if !writes(before, Some(k)) && writes(after, Some(k)) {
                    said.push(format!("{c}: may now change things through `{k}`"));
                }
            }
        }
    }
    said
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

fn connections(t: &Table) -> Result<std::collections::BTreeMap<String, Vec<String>>, String> {
    let wrong = || {
        "The connections an agent uses need to be, per connector, a list of connection names."
            .to_string()
    };
    match t.get("connections") {
        None => Ok(Default::default()),
        Some(Value::Table(per)) => per
            .iter()
            .map(|(connector, picks)| {
                let Value::Array(items) = picks else {
                    return Err(wrong());
                };
                let mut keys: Vec<String> = Vec::new();
                for item in items {
                    match item {
                        Value::String(s) if !s.trim().is_empty() => {
                            if !keys.iter().any(|k| k == s.trim()) {
                                keys.push(s.trim().to_string());
                            }
                        }
                        _ => return Err(wrong()),
                    }
                }
                Ok((connector.clone(), keys))
            })
            .collect(),
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
        connections: connections(t)?,
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
    fn the_connections_an_agent_uses_are_named_per_connector_the_first_its_default() {
        let b = parse(
            "[skills]\nenabled = [\"jira\", \"linear\"]\nread_only = [\"jira:globex\"]\n\n[skills.connections]\njira = [\"acme\", \"globex\", \"acme\"]\nlinear = []\n",
        )
        .unwrap();
        assert_eq!(b.picks("jira").unwrap(), ["acme", "globex"]);
        assert!(b.picks("github").is_none());
        // An empty list is the connector off.
        assert!(b.is_on("jira") && !b.is_on("linear"));
        // One connection only reads; the connector's writes aren't hidden.
        assert!(b.reads_only_instance("jira", "globex"));
        assert!(!b.reads_only_instance("jira", "acme"));
        assert!(!b.reads_only("jira"));
        assert!(parse("[skills]\nread_only = [\"jira\"]\n")
            .unwrap()
            .reads_only_instance("jira", "acme"));
        assert!(parse("[skills.connections]\njira = \"acme\"\n")
            .unwrap_err()
            .contains("list of connection names"));
    }

    #[test]
    fn what_widens_an_agents_reach_is_told_apart_from_what_narrows_it() {
        let b = |src: &str| parse(src).unwrap();
        let base = b("[skills]\nenabled = [\"jira\"]\nread_only = [\"jira:globex\"]\n\n[skills.connections]\njira = [\"acme\", \"globex\"]\n");
        // Narrowing, or no change: nothing to tell.
        assert!(widened(&base, &base).is_empty());
        assert!(widened(&base, &b("[skills]\nenabled = []\n")).is_empty());
        assert!(widened(
            &base,
            &b("[skills]\nenabled = [\"jira\"]\nread_only = [\"jira\"]\n\n[skills.connections]\njira = [\"acme\"]\n")
        )
        .is_empty());
        // Widening: each said.
        let more = b("[skills]\nenabled = [\"jira\", \"linear\"]\n\n[skills.connections]\njira = [\"acme\", \"globex\", \"initech\"]\n");
        assert_eq!(
            widened(&base, &more),
            [
                "jira: now uses the `initech` connection",
                "jira: may now change things through `globex`",
                "linear: turned on",
            ]
        );
        let every = b("[skills]\nenabled = [\"jira\"]\nread_only = [\"jira:globex\"]\n");
        assert_eq!(widened(&base, &every), ["jira: now uses every connection"]);
    }

    #[test]
    fn a_path_says_which_instance_it_is_part_of() {
        assert_eq!(
            instance_of("jira", "skills/jira/workspaces/acme/api_token"),
            Some(Some("acme"))
        );
        assert_eq!(instance_of("jira", "skills/jira/workspaces/"), Some(None));
        assert_eq!(instance_of("jira", "skills/jira/api_token"), None);
        assert_eq!(instance_of("slack", "slack.bot_token"), None);
        assert!(INSTANCE_PREFIXES
            .iter()
            .all(|(c, _)| CONNECTORS.contains(c)));
        assert!(READ_ONLY_EACH.iter().all(|c| instance_prefix(c).is_some()));
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
