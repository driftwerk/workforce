//! `[skills]` in an agent's `config.toml`: which connectors reach the agent,
//! which of them only read, and which use the agent's own connection rather
//! than the person's. Read only on a hub with `[desk]` enabled.
//!
//! ```toml
//! [skills]
//! enabled        = ["linear", "github", "jira"]  # present: only these connectors' tools reach it
//! read_only      = ["github", "jira:globex"]     # a connector's writes are hidden; one connection's are refused
//! writable       = ["freeagent:harbor"]          # a connector that starts read only takes writes here
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
    "github",
    "google",
    "email",
    "jira",
    "linear",
    "slack",
    "datadog",
    "metabase",
    "freeagent",
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
    ("freeagent", "skills/freeagent/workspaces/"),
];

/// The connectors whose tools resolve one instance per call
/// (`environment::resolve_instance`), so one connection of theirs can be
/// read only while another takes writes.
pub const READ_ONLY_EACH: &[&str] = &[
    "github",
    "google",
    "email",
    "jira",
    "linear",
    "datadog",
    "metabase",
    "freeagent",
];

/// The connectors whose connections only read until `writable` names them:
/// a change there is one the person makes deliberately.
pub const READ_ONLY_FIRST: &[&str] = &["freeagent"];

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
        // An empty segment (`workspaces//x`) is an instance no pick names,
        // never the listing.
        Some((key, _)) => Some(key),
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
    /// Connectors, and single connections (`freeagent:harbor`), of
    /// [`READ_ONLY_FIRST`] that take writes. `read_only` wins over it.
    pub writable: Vec<String>,
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

    /// Whether `connector` only reads for the agent. One connection of a
    /// connector that can't hold a single connection back (`email:work`)
    /// makes the whole connector read-only, so the entry never holds
    /// nothing back. One of [`READ_ONLY_FIRST`] that `writable` names no
    /// connection of takes no writes anywhere.
    pub fn reads_only(&self, connector: &str) -> bool {
        self.read_only.iter().any(|r| {
            r == connector
                || (!READ_ONLY_EACH.contains(&connector)
                    && r.split_once(':').is_some_and(|(c, _)| c == connector))
        }) || (READ_ONLY_FIRST.contains(&connector)
            && !self
                .writable
                .iter()
                .any(|w| w == connector || w.split_once(':').is_some_and(|(c, _)| c == connector)))
    }

    /// The instances of `connector` it uses, the first its default, where
    /// it names them.
    pub fn picks(&self, connector: &str) -> Option<&[String]> {
        self.connections.get(connector).map(Vec::as_slice)
    }

    /// Whether instance `instance` of `connector` only reads for the agent:
    /// the connector does, or that one connection does, or the connector
    /// starts read only and `writable` names neither it nor that connection.
    pub fn reads_only_instance(&self, connector: &str, instance: &str) -> bool {
        let names = |list: &[String]| {
            list.iter()
                .any(|r| r.split_once(':') == Some((connector, instance)))
        };
        self.reads_only(connector)
            || names(&self.read_only)
            || (READ_ONLY_FIRST.contains(&connector)
                && !self.writable.iter().any(|w| w == connector)
                && !names(&self.writable))
    }

    /// Whether every connection of `connector` takes writes, as far as the
    /// connector as a whole goes: it doesn't read only, and one that starts
    /// read only is named whole in `writable`.
    fn writes_whole(&self, connector: &str) -> bool {
        !self.reads_only(connector)
            && (!READ_ONLY_FIRST.contains(&connector)
                || self.writable.iter().any(|w| w == connector))
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
            // Naming connections where it named none narrows what it
            // reaches, except where it used only its own: a named one may be
            // the person's.
            (None, Some(now)) if before.own(c) => {
                for k in now {
                    said.push(format!("{c}: now uses the `{k}` connection"));
                }
            }
            _ => {}
        }
        if before.own(c) != after.own(c) {
            said.push(format!("{c}: uses a different connection"));
        }
        if !before.writes_whole(c) && after.writes_whole(c) && after.picks(c).is_none() {
            said.push(format!("{c}: may now change things"));
        }
        // Every connection that only read before and may change things
        // now: those it names, those `before` made read-only one by one and
        // those `after` makes writable one by one (it may reach them still,
        // named or not).
        let mut keys: Vec<&str> = after
            .picks(c)
            .unwrap_or_default()
            .iter()
            .map(String::as_str)
            .collect();
        for r in before.read_only.iter().chain(&after.writable) {
            if let Some((rc, k)) = r.split_once(':') {
                if rc == *c && !keys.contains(&k) {
                    keys.push(k);
                }
            }
        }
        for k in keys {
            if after.picks(c).is_some_and(|p| !p.iter().any(|x| x == k)) {
                continue;
            }
            if before.reads_only_instance(c, k) && !after.reads_only_instance(c, k) {
                said.push(format!("{c}: may now change things through `{k}`"));
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
    let read_only =
        names(t, "read_only", "The connectors an agent only reads")?.unwrap_or_default();
    Ok(ConnectorBinding {
        enabled: names(t, "enabled", "The connectors an agent has")?,
        read_only,
        writable: names(t, "writable", "The connectors an agent may change")?.unwrap_or_default(),
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
    "purge",
    "react",
    "remove",
    "rename",
    "reply",
    "rerun",
    "reset",
    "restore",
    "revert",
    "review",
    "scaffold",
    "send",
    "set",
    "setup",
    "share",
    "transition",
    "unarchive",
    "undo",
    "unmute",
    "unpin",
    "update",
    "upload",
    "wipe",
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
    "history",
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

/// Tools whose words say otherwise of them: `status` names a read in
/// `github.ci_status` but sets a board column in `github.project_item_status`.
const NAMED: &[(&str, bool)] = &[
    ("github.viewer", true),
    ("github.workspaces", true),
    ("jira.workspaces", true),
    ("linear.workspaces", true),
    ("github.issues_changed", true),
    ("github.repo_labels", true),
    ("github.project_item_status", false),
    ("github.issue_labels", false),
    ("linear.workspaces", true),
    ("linear.viewer", true),
    ("linear.upload_read", true),
    ("jira.workspaces", true),
    ("jira.myself", true),
    ("jira.board_order", true),
    ("google.workspaces", true),
    ("google.calendar.calendars", true),
    ("google.calendar.events", true),
    ("google.calendar.freebusy", true),
    ("google.calendar.colors", true),
    ("google.drive.ls", true),
    ("google.drive.download", true),
    ("google.drive.permissions", true),
    ("google.drive.drives", true),
    ("google.docs.cat", true),
    ("google.docs.find_replace", false),
    ("google.gmail.attachment", true),
    ("google.gmail.draft_get", true),
    ("google.tasks.lists", true),
    ("google.people.me", true),
    ("freeagent.workspaces", true),
    ("freeagent.company", true),
    ("freeagent.profit_and_loss_summary", true),
    ("email.unsubscribe_lists", true),
];

/// Whether connector `tool` only reads. Its name after the connector, split
/// into words (`gmail.search`, `issue_list`): a read names a read word and
/// no change; anything else, a generic `api` passthrough included, is a
/// write.
pub fn is_read(tool: &str) -> bool {
    let Some((_, action)) = tool.split_once('.') else {
        return false;
    };
    if let Some((_, read)) = NAMED.iter().find(|(name, _)| *name == tool) {
        return *read;
    }
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
            "linear.issue_history",
            "github.pr_diff",
            "github.workflow_run_logs",
            "github.viewer",
            "github.workspaces",
            "jira.workspaces",
            "linear.workspaces",
            "github.projects_list",
            "github.project_get",
            "github.project_items",
            "github.issues_changed",
            "github.repo_labels",
            "google.gmail.search",
            "google.chat.messages_list",
            "email.inbox",
            "email.unsubscribe_lists",
            "slack.read",
            "slack.lookup_user_by_email",
            "datadog.logs_search",
            "metabase.card_query",
        ] {
            assert!(is_read(read), "{read} reads");
        }
        for write in [
            "linear.issue_create",
            "notes.history_purge",
            "notes.history_reset",
            "notes.history_restore",
            "notes.history_revert",
            "notes.history_undo",
            "notes.history_wipe",
            "linear.issue_bulk_update",
            "github.pr_merge",
            "github.pr_comment",
            "github.api",
            "github.branch_protection",
            "github.project_item_status",
            "github.project_item_move",
            "github.project_item_add",
            "github.issue_labels",
            "google.gmail.send",
            "google.gmail.draft_send",
            "email.send",
            "email.rules",
            "email.unsubscribe",
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
    fn naming_connections_after_its_own_and_unmaking_one_read_only_are_widenings() {
        let b = |src: &str| parse(src).unwrap();
        // It read only its own; a named connection may be the person's.
        let own = b("[skills]\nenabled = [\"jira\"]\nown_credential = [\"jira\"]\n");
        let named = b("[skills]\nenabled = [\"jira\"]\nown_credential = [\"jira\"]\n\n[skills.connections]\njira = [\"acme\"]\n");
        assert_eq!(
            widened(&own, &named),
            ["jira: now uses the `acme` connection"]
        );
        // Without its own, naming connections only narrows.
        let all = b("[skills]\nenabled = [\"jira\"]\n");
        let some = b("[skills]\nenabled = [\"jira\"]\n\n[skills.connections]\njira = [\"acme\"]\n");
        assert!(widened(&all, &some).is_empty());
        // One connection made read-only, with no picks: unmaking it is said.
        let ro = b("[skills]\nenabled = [\"jira\"]\nread_only = [\"jira:globex\"]\n");
        assert_eq!(
            widened(&ro, &all),
            ["jira: may now change things through `globex`"]
        );
        // Email holds each account back on its own: `work` reads only,
        // `home` still takes writes.
        let email = parse("[skills]\nread_only = [\"email:work\"]\n").unwrap();
        assert!(!email.reads_only("email"));
        assert!(email.reads_only_instance("email", "work"));
        assert!(!email.reads_only_instance("email", "home"));
        // An empty instance segment is no pick's, never the listing.
        assert_eq!(
            instance_of("jira", "skills/jira/workspaces//globex/x"),
            Some(Some(""))
        );
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
    fn freeagent_and_google_resolve_each_connection() {
        for c in ["google", "freeagent"] {
            assert!(CONNECTORS.contains(&c), "{c}");
            assert!(READ_ONLY_EACH.contains(&c), "{c}");
        }
        assert_eq!(
            instance_of(
                "freeagent",
                "skills/freeagent/workspaces/harbor/refresh_token"
            ),
            Some(Some("harbor"))
        );
        assert_eq!(connector_of("freeagent.invoices_list"), Some("freeagent"));
        let b = parse("[skills]\nread_only = [\"google:work\"]\n").unwrap();
        assert!(b.reads_only_instance("google", "work"));
        assert!(!b.reads_only_instance("google", "home"));
        assert!(!b.reads_only("google"));
    }

    #[test]
    fn a_freeagent_connection_reads_only_until_writable_names_it() {
        // No picks, as "all my agents" reaches it: read only.
        let none = parse("").unwrap();
        assert!(none.reads_only("freeagent"));
        assert!(none.reads_only_instance("freeagent", "harbor"));
        // One named: that one takes writes, the others still read.
        let one = parse("[skills]\nwritable = [\"freeagent:harbor\"]\n").unwrap();
        assert!(!one.reads_only("freeagent"));
        assert!(!one.reads_only_instance("freeagent", "harbor"));
        assert!(one.reads_only_instance("freeagent", "lumen"));
        // Named whole: every one takes writes.
        let all = parse("[skills]\nwritable = [\"freeagent\"]\n").unwrap();
        assert!(!all.reads_only_instance("freeagent", "lumen"));
        // `read_only` wins.
        for source in [
            "[skills]\nwritable = [\"freeagent\"]\nread_only = [\"freeagent\"]\n",
            "[skills]\nwritable = [\"freeagent:harbor\"]\nread_only = [\"freeagent:harbor\"]\n",
        ] {
            assert!(
                parse(source)
                    .unwrap()
                    .reads_only_instance("freeagent", "harbor"),
                "{source}"
            );
        }
        // Other connectors don't start read only, and `writable` doesn't
        // reach them.
        assert!(!none.reads_only_instance("jira", "acme"));
        assert!(
            parse("[skills]\nread_only = [\"jira\"]\nwritable = [\"jira\"]\n")
                .unwrap()
                .reads_only_instance("jira", "acme")
        );
        assert!(parse("[skills]\nwritable = 1\n")
            .unwrap_err()
            .contains("may change"));
    }

    #[test]
    fn a_bare_google_read_only_keeps_every_google_connection_read_only() {
        let bare = parse("[skills]\nread_only = [\"google\"]\n").unwrap();
        assert!(bare.reads_only("google"));
        for k in ["default", "work", "home"] {
            assert!(bare.reads_only_instance("google", k), "{k}");
        }
        let one = parse("[skills]\nread_only = [\"google:work\"]\n").unwrap();
        assert!(!one.reads_only("google"));
        assert!(!one.reads_only_instance("google", "home"));
    }

    #[test]
    fn writable_gaining_an_entry_is_widened() {
        let b = |src: &str| parse(src).unwrap();
        let none = b("[skills]\nenabled = [\"freeagent\"]\n");
        let harbor = b("[skills]\nenabled = [\"freeagent\"]\nwritable = [\"freeagent:harbor\"]\n");
        assert_eq!(
            widened(&none, &harbor),
            ["freeagent: may now change things through `harbor`"]
        );
        let whole = b("[skills]\nenabled = [\"freeagent\"]\nwritable = [\"freeagent\"]\n");
        assert_eq!(
            widened(&harbor, &whole),
            ["freeagent: may now change things"]
        );
        let picked = |w: &str| {
            b(&format!(
                "[skills]\nenabled = [\"freeagent\"]\nwritable = [{w}]\n\n[skills.connections]\nfreeagent = [\"harbor\", \"lumen\"]\n"
            ))
        };
        assert_eq!(
            widened(&picked(""), &picked("\"freeagent:lumen\"")),
            ["freeagent: may now change things through `lumen`"]
        );
        // Losing one only narrows.
        assert!(widened(&harbor, &none).is_empty());
        assert!(widened(&whole, &harbor).is_empty());
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
