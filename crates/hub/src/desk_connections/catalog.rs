//! The services a person connects on Desk, and where each one's keys are
//! kept. Every path here is one the adapter already reads and
//! `credential_catalog()` already lists: Connections is a view over the
//! vault and invents no paths.
//!
//! A service holds several connections, each an instance of its adapter at
//! the adapter's own multi-instance prefix (`skills/jira/workspaces/<key>/`).
//! The flat paths (`skills/jira/api_token`) are where a connection was kept
//! before; Connections moves one it finds to the instance `default`, which
//! is the name the adapters already give flat mode. Slack keeps one
//! connection, at its flat path.

use serde::Serialize;
use workforce_common::settings::VaultSource;

/// How a connector is signed in to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SignIn {
    /// The fields below.
    Fields,
    /// GitHub's device flow, or a token in the fields.
    Github,
    /// Google's sign-in on the person's own OAuth client.
    Google,
}

/// One thing the person types to connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Field {
    pub name: &'static str,
    pub label: &'static str,
    pub secret: bool,
    pub required: bool,
    pub placeholder: &'static str,
    /// Where a flat connection kept it, as the adapter reads it (before any
    /// `users/<id>/`).
    #[serde(skip)]
    pub path: &'static str,
    /// Where an instance keeps it, under `<prefix><key>/`.
    #[serde(skip)]
    pub sub: &'static str,
}

const fn secret(
    name: &'static str,
    label: &'static str,
    path: &'static str,
    sub: &'static str,
) -> Field {
    Field {
        name,
        label,
        secret: true,
        required: true,
        placeholder: "",
        path,
        sub,
    }
}

const fn plain(
    name: &'static str,
    label: &'static str,
    placeholder: &'static str,
    path: &'static str,
    sub: &'static str,
) -> Field {
    Field {
        name,
        label,
        secret: false,
        required: true,
        placeholder,
        path,
        sub,
    }
}

const fn optional(field: Field) -> Field {
    Field {
        required: false,
        ..field
    }
}

/// A service agents can be given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Connector {
    /// The adapter's `tool_prefix` and catalog name.
    pub id: &'static str,
    pub name: &'static str,
    /// One line on what it gives agents.
    pub about: &'static str,
    pub sign_in: SignIn,
    pub fields: &'static [Field],
    /// Kept only in the shared tiers (Slack's bot token): "all my agents" is
    /// the hub-wide tier, "just this agent" the agent's own.
    pub shared: bool,
    /// Every path a flat connection holds, so a disconnect removes it all.
    pub held: &'static [&'static str],
    /// The flat path whose value means it is connected.
    pub primary: &'static str,
    /// Where its instances are kept (`skills/jira/workspaces/`); `None`
    /// for a service of one connection (Slack).
    pub prefix: Option<&'static str>,
    /// Every path an instance holds under `<prefix><key>/`, its name
    /// ([`LABEL`]) aside.
    pub subs: &'static [&'static str],
    /// The one whose value means the instance is connected.
    pub primary_sub: &'static str,
    /// The field that says where it is (a site, an address), shown beside
    /// its name.
    pub site: Option<&'static str>,
    /// What a new connection's name should be, as the form says.
    pub name_hint: &'static str,
}

/// Where an instance keeps the name the person gave it.
pub const LABEL: &str = "label";

/// The instance a flat connection becomes: the one the adapters already
/// expose flat mode as.
pub const DEFAULT_INSTANCE: &str = "default";

const GOOGLE_HELD: &[&str] = &[
    "skills/google/workspaces/default/refresh_token",
    "skills/google/workspaces/default/access_token",
    "skills/google/workspaces/default/expires_at",
    "skills/google/workspaces/default/scopes",
    "skills/google/workspaces/default/email",
];

/// The Google OAuth client the person brings: hub-wide, as the callback
/// reads it.
pub const GOOGLE_CLIENT_ID: &str = "skills/google/oauth_client/client_id";
pub const GOOGLE_CLIENT_SECRET: &str = "skills/google/oauth_client/client_secret";
pub const GOOGLE_REDIRECT_URI: &str = "skills/google/oauth_client/redirect_uri";
/// Where Google's account email is kept beside its tokens.
pub const GOOGLE_EMAIL: &str = "skills/google/workspaces/default/email";
pub const GOOGLE_WORKSPACE: &str = "default";
/// The email account a connection is kept as.
pub const EMAIL_ACCOUNT: &str = "default";

const GITHUB_FIELDS: &[Field] = &[secret(
    "token",
    "Personal access token",
    "skills/github/token",
    "token",
)];
const EMAIL_FIELDS: &[Field] = &[
    plain(
        "username",
        "Email address",
        "sarah@acme.example",
        "skills/email/accounts/default/username",
        "username",
    ),
    secret(
        "password",
        "App password",
        "skills/email/accounts/default/password",
        "password",
    ),
    plain(
        "imap_host",
        "IMAP server",
        "imap.gmail.com",
        "skills/email/accounts/default/imap_host",
        "imap_host",
    ),
    // The loader skips an account without one.
    plain(
        "smtp_host",
        "SMTP server",
        "smtp.gmail.com",
        "skills/email/accounts/default/smtp_host",
        "smtp_host",
    ),
];
const JIRA_FIELDS: &[Field] = &[
    plain(
        "site",
        "Site",
        "acme.atlassian.net",
        "skills/jira/site",
        "site",
    ),
    plain(
        "email",
        "Email",
        "sarah@acme.example",
        "skills/jira/email",
        "email",
    ),
    secret(
        "api_token",
        "API token",
        "skills/jira/api_token",
        "api_token",
    ),
];
const LINEAR_FIELDS: &[Field] = &[secret(
    "api_key",
    "API key",
    "skills/linear/api_key",
    "api_key",
)];
const SLACK_FIELDS: &[Field] = &[secret("bot_token", "Bot token", "slack.bot_token", "")];
const DATADOG_FIELDS: &[Field] = &[
    secret("api_key", "API key", "skills/datadog/api_key", "api_key"),
    secret(
        "app_key",
        "Application key",
        "skills/datadog/app_key",
        "app_key",
    ),
    optional(plain("site", "Site", "us1", "skills/datadog/site", "site")),
];
const METABASE_FIELDS: &[Field] = &[
    plain(
        "site_url",
        "Address",
        "https://metabase.acme.example",
        "skills/metabase/site_url",
        "site_url",
    ),
    secret("api_key", "API key", "skills/metabase/api_key", "api_key"),
];

const GITHUB_HELD: &[&str] = &["skills/github/token"];
const EMAIL_HELD: &[&str] = &[
    "skills/email/accounts/default/username",
    "skills/email/accounts/default/password",
    "skills/email/accounts/default/imap_host",
    "skills/email/accounts/default/smtp_host",
];
const JIRA_HELD: &[&str] = &[
    "skills/jira/site",
    "skills/jira/email",
    "skills/jira/api_token",
];
const LINEAR_HELD: &[&str] = &["skills/linear/api_key"];
const SLACK_HELD: &[&str] = &["slack.bot_token"];
const DATADOG_HELD: &[&str] = &[
    "skills/datadog/api_key",
    "skills/datadog/app_key",
    "skills/datadog/site",
];
const METABASE_HELD: &[&str] = &["skills/metabase/site_url", "skills/metabase/api_key"];

/// The connectors of v1, in the order Settings lists them.
pub const CATALOG: &[Connector] = &[
    Connector {
        id: "github",
        name: "GitHub",
        about: "Repositories, issues and pull requests.",
        sign_in: SignIn::Github,
        fields: GITHUB_FIELDS,
        shared: false,
        held: GITHUB_HELD,
        primary: "skills/github/token",
        prefix: Some("skills/github/workspaces/"),
        subs: &["token"],
        primary_sub: "token",
        site: None,
        name_hint: "The organisation or account it works in, like acme",
    },
    Connector {
        id: "google",
        name: "Google",
        about: "Gmail, Calendar, Drive, Docs and Sheets.",
        sign_in: SignIn::Google,
        fields: &[],
        shared: false,
        held: GOOGLE_HELD,
        primary: "skills/google/workspaces/default/refresh_token",
        prefix: Some("skills/google/workspaces/"),
        subs: &[
            "refresh_token",
            "access_token",
            "expires_at",
            "scopes",
            "email",
        ],
        primary_sub: "refresh_token",
        site: None,
        name_hint: "Work, Personal",
    },
    Connector {
        id: "email",
        name: "Email",
        about: "Reads and sends mail from one mailbox, over IMAP and SMTP.",
        sign_in: SignIn::Fields,
        fields: EMAIL_FIELDS,
        shared: false,
        held: EMAIL_HELD,
        primary: "skills/email/accounts/default/password",
        prefix: Some("skills/email/accounts/"),
        subs: &["username", "password", "imap_host", "smtp_host"],
        primary_sub: "password",
        site: None,
        name_hint: "Work, Personal",
    },
    Connector {
        id: "jira",
        name: "Jira",
        about: "Issues, boards and sprints.",
        sign_in: SignIn::Fields,
        fields: JIRA_FIELDS,
        shared: false,
        held: JIRA_HELD,
        primary: "skills/jira/api_token",
        prefix: Some("skills/jira/workspaces/"),
        subs: &["site", "email", "api_token"],
        primary_sub: "api_token",
        site: Some("site"),
        name_hint: "Acme",
    },
    Connector {
        id: "linear",
        name: "Linear",
        about: "Issues, projects and cycles.",
        sign_in: SignIn::Fields,
        fields: LINEAR_FIELDS,
        shared: false,
        held: LINEAR_HELD,
        primary: "skills/linear/api_key",
        prefix: Some("skills/linear/workspaces/"),
        subs: &["api_key"],
        primary_sub: "api_key",
        site: None,
        name_hint: "Acme",
    },
    Connector {
        id: "slack",
        name: "Slack",
        about: "Reads channels and posts as your Slack app.",
        sign_in: SignIn::Fields,
        fields: SLACK_FIELDS,
        shared: true,
        held: SLACK_HELD,
        primary: "slack.bot_token",
        prefix: None,
        subs: &[],
        primary_sub: "",
        site: None,
        name_hint: "",
    },
    Connector {
        id: "datadog",
        name: "Datadog",
        about: "Logs, metrics and monitors.",
        sign_in: SignIn::Fields,
        fields: DATADOG_FIELDS,
        shared: false,
        held: DATADOG_HELD,
        primary: "skills/datadog/api_key",
        prefix: Some("skills/datadog/accounts/"),
        subs: &["api_key", "app_key", "site"],
        primary_sub: "api_key",
        site: Some("site"),
        name_hint: "Production",
    },
    Connector {
        id: "metabase",
        name: "Metabase",
        about: "Questions, dashboards and SQL on your Metabase.",
        sign_in: SignIn::Fields,
        fields: METABASE_FIELDS,
        shared: false,
        held: METABASE_HELD,
        primary: "skills/metabase/api_key",
        prefix: Some("skills/metabase/instances/"),
        subs: &["site_url", "api_key"],
        primary_sub: "api_key",
        site: Some("site_url"),
        name_hint: "Acme",
    },
];

pub fn find(id: &str) -> Option<&'static Connector> {
    CATALOG.iter().find(|c| c.id == id)
}

impl Connector {
    /// Whether it holds several connections.
    pub fn named(&self) -> bool {
        self.prefix.is_some()
    }

    /// Where `sub` of instance `key` is kept; a service of one connection
    /// keeps `sub`'s field at its flat path.
    pub fn at(&self, key: &str, sub: &str) -> String {
        match self.prefix {
            Some(prefix) => format!("{prefix}{key}/{sub}"),
            None => self
                .fields
                .iter()
                .find(|f| f.sub == sub)
                .map_or(self.primary, |f| f.path)
                .to_string(),
        }
    }

    /// Where `field` of instance `key` is kept.
    pub fn field_at(&self, field: &Field, key: &str) -> String {
        match self.prefix {
            Some(_) => self.at(key, field.sub),
            None => field.path.to_string(),
        }
    }

    /// The path whose value means instance `key` is connected.
    pub fn primary_at(&self, key: &str) -> String {
        match self.prefix {
            Some(_) => self.at(key, self.primary_sub),
            None => self.primary.to_string(),
        }
    }

    /// Every path instance `key` holds, its name with them.
    pub fn held_at(&self, key: &str) -> Vec<String> {
        match self.prefix {
            Some(_) => self
                .subs
                .iter()
                .chain([&LABEL])
                .map(|sub| self.at(key, sub))
                .collect(),
            None => self.held.iter().map(|p| p.to_string()).collect(),
        }
    }

    /// Where instance `key`'s name is kept; a service of one connection
    /// keeps none.
    pub fn label_at(&self, key: &str) -> Option<String> {
        self.prefix.map(|_| self.at(key, LABEL))
    }

    /// The key of the instance the logical path `path` is part of.
    pub fn key_of<'a>(&self, path: &'a str) -> Option<&'a str> {
        let rest = path.strip_prefix(self.prefix?)?;
        let (key, _) = rest.split_once('/')?;
        (!key.is_empty()).then_some(key)
    }

    /// The tiers a connection at `agent_level` is kept in, the one written
    /// first. The second is what was there before Connections: a key saved
    /// at the bare tier, which still counts and is what a disconnect at
    /// that level removes.
    pub fn tiers(&self, agent_level: bool) -> [VaultSource; 2] {
        match (self.shared, agent_level) {
            (true, false) => [VaultSource::Global, VaultSource::Global],
            (true, true) => [VaultSource::AgentSpecific, VaultSource::AgentSpecific],
            (false, false) => [VaultSource::GlobalPerUser, VaultSource::Global],
            (false, true) => [VaultSource::AgentPerUser, VaultSource::AgentSpecific],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_path_is_one_the_vault_editor_already_offers() {
        let offered: Vec<String> = crate::skill_loader::credential_catalog()
            .into_iter()
            .map(|e| e.template)
            .collect();
        let known = |path: &str| {
            offered.iter().any(|t| {
                t == path
                    || match t.split_once('{') {
                        // A template: `skills/email/accounts/{account}/username`.
                        Some((head, rest)) => rest.split_once('}').is_some_and(|(_, tail)| {
                            path.starts_with(head) && path.ends_with(tail)
                        }),
                        None => false,
                    }
            })
        };
        for c in CATALOG {
            assert!(
                crate::skill_loader::builtin_skill_catalog()
                    .iter()
                    .any(|m| m.tool_prefix == c.id),
                "{} is a built-in adapter",
                c.id
            );
            for path in c.held.iter().chain([&c.primary]) {
                assert!(known(path), "{path} is in credential_catalog()");
            }
            for f in c.fields {
                assert!(c.held.contains(&f.path), "{} holds {}", c.id, f.path);
            }
            // Every instance path, its name with them.
            for path in c.held_at("acme") {
                assert!(known(&path), "{path} is in credential_catalog()");
            }
            for f in c.fields {
                assert!(
                    c.held_at("acme").contains(&c.field_at(f, "acme")),
                    "{} holds {}",
                    c.id,
                    f.name
                );
            }
            if c.named() {
                assert_eq!(c.key_of(&c.primary_at("acme")), Some("acme"));
            }
        }
    }

    #[test]
    fn a_flat_connection_becomes_the_default_instance_the_adapters_already_name() {
        // Email and Google always kept theirs at `default`.
        for id in ["email", "google"] {
            let c = find(id).unwrap();
            assert_eq!(c.primary, c.primary_at(DEFAULT_INSTANCE), "{id}");
        }
        let jira = find("jira").unwrap();
        assert_eq!(
            jira.primary_at("acme"),
            "skills/jira/workspaces/acme/api_token"
        );
        assert_eq!(
            jira.label_at("acme").as_deref(),
            Some("skills/jira/workspaces/acme/label")
        );
        let slack = find("slack").unwrap();
        assert!(!slack.named());
        assert_eq!(slack.primary_at(DEFAULT_INSTANCE), "slack.bot_token");
        assert_eq!(slack.held_at(DEFAULT_INSTANCE), ["slack.bot_token"]);
    }

    #[test]
    fn only_slack_is_kept_in_the_shared_tiers_and_every_other_path_may_be_a_persons() {
        for c in CATALOG {
            for path in c.held {
                assert!(
                    !crate::vault_policy::is_shared_only(path),
                    "{path} may be written per person"
                );
            }
            assert_eq!(c.shared, c.id == "slack", "{}", c.id);
        }
        let slack = find("slack").unwrap();
        assert_eq!(
            slack.tiers(false),
            [VaultSource::Global, VaultSource::Global]
        );
        assert_eq!(
            slack.tiers(true),
            [VaultSource::AgentSpecific, VaultSource::AgentSpecific]
        );
        let linear = find("linear").unwrap();
        assert_eq!(linear.tiers(false)[0], VaultSource::GlobalPerUser);
        assert_eq!(linear.tiers(true)[0], VaultSource::AgentPerUser);
    }
}
