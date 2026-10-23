//! What an agent's configuration is in effect: each value, the layer it
//! comes from and whether it is doing anything
//!.
//!
//! The report reads what the runtime reads: the install config, the agent's
//! [`ResolvedConfig`](crate::resolved_config::ResolvedConfig) snapshot, the
//! `for_agent` merges and the live objects built from them. It decides
//! nothing itself. A secret is never part of it: a credential field shows
//! only whether it is set, a vault key only the level that holds it.

mod layers;
mod sections;
pub mod tool;

use serde::Serialize;
use serde_json::Value;
use workforce_common::session::Session;

use crate::serve::AgentService;

/// The layer a value comes from, lowest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Default,
    Install,
    /// The agent's `[[agents]]` entry in the install config.
    InstallAgent,
    /// The agent's own `config.toml`.
    Agent,
    /// A budget override, a per-session model, a playbook step's model or a
    /// value set on the running agent.
    Runtime,
    Environment,
    Vault,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "built-in default",
            Self::Install => "install config",
            Self::InstallAgent => "install's [[agents]] entry",
            Self::Agent => "the agent's config.toml",
            Self::Runtime => "runtime override",
            Self::Environment => "environment",
            Self::Vault => "vault",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    Off,
    /// Off for every agent by the install, whatever the agent's config says.
    KillSwitched,
    /// Worked out and recorded, changing nothing.
    Shadow,
    /// Configured but not taking effect; the row's reason says why.
    Inert,
}

/// A value one layer sets.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Layer {
    pub source: Source,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Row {
    pub section: &'static str,
    pub key: String,
    pub value: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Every layer that sets the value, lowest first; the last one in
    /// effect wins.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub chain: Vec<Layer>,
    #[serde(skip)]
    install_wide: bool,
}

impl Row {
    /// A fact read off a live object rather than a config key.
    fn live(section: &'static str, key: impl Into<String>, value: impl Into<Value>) -> Self {
        Self {
            section,
            key: key.into(),
            value: layers::scrub(value.into()),
            source: None,
            status: None,
            reason: None,
            chain: Vec::new(),
            install_wide: false,
        }
    }

    fn from(mut self, source: Source) -> Self {
        self.source = Some(source);
        self
    }

    fn status(mut self, status: Status) -> Self {
        self.status = Some(status);
        self
    }

    fn because(mut self, reason: impl AsRef<str>) -> Self {
        self.reason = Some(layers::public_text(reason.as_ref()));
        self
    }

    fn path(&self) -> String {
        format!("{}.{}", self.section, self.key)
    }
}

/// Who is asking. Per-user rows are theirs alone; install-wide rows need
/// `admin`.
#[derive(Debug, Clone, Default)]
pub struct Viewer {
    pub person: Option<String>,
    pub admin: bool,
}

/// Which rows a report holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Every section's states, live facts and values that are not the
    /// built-in default or are not in effect.
    Summary,
    All,
    Section(String),
    /// One setting, or every setting under a dotted path.
    Key(String),
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("settings are shown to an identified person only, and nobody is identified here")]
    NoIdentity,
    #[error("no section {section:?}. Sections: {}", .sections.join(", "))]
    UnknownSection {
        section: String,
        sections: Vec<&'static str>,
    },
    #[error(
        "no setting {key:?}{}: a key is a config path such as llm.vision.model, and showing a \
         section ({}) lists its keys",
        did_you_mean(.nearest),
        .sections.join(", ")
    )]
    UnknownKey {
        key: String,
        /// Settings whose name is the one asked for, under another path.
        nearest: Vec<String>,
        sections: Vec<&'static str>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub agent: String,
    /// `admin` when install-wide rows are included.
    pub viewer: &'static str,
    pub sections: Vec<&'static str>,
    pub rows: Vec<Row>,
    /// Rows at their built-in default that a summary left out.
    #[serde(skip_serializing_if = "is_zero")]
    pub defaults_omitted: usize,
    /// What a capped report left out, and how to see it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Report {
    /// The report in at most `max` rows: values at their built-in default
    /// go first, then the tail, and `note` says how to see what was cut.
    pub fn capped(mut self, max: usize) -> Self {
        let all = self.rows.len();
        if all <= max {
            return self;
        }
        self.rows
            .retain(|row| row.source != Some(Source::Default) || row.status.is_some());
        self.defaults_omitted += all - self.rows.len();
        self.rows.truncate(max);
        self.note = Some(format!(
            "{} of {all} rows are left out to keep this short; show one section or explain one \
             key to see them",
            all - self.rows.len()
        ));
        self
    }
}

fn did_you_mean(nearest: &[String]) -> String {
    match nearest.is_empty() {
        true => String::new(),
        false => format!(" (did you mean {}?)", nearest.join(", ")),
    }
}

/// Up to five settings `key` may have meant: one named as its last word
/// is, one with `key` in its path, or the state of the table it lies under
/// when that table is off and so has no other row.
fn nearest(rows: &[Row], key: &str) -> Vec<String> {
    let name = key.rsplit('.').next().unwrap_or(key);
    let mut paths: Vec<String> = rows
        .iter()
        .filter(|_| !name.is_empty())
        .filter_map(|row| {
            let path = row.path();
            let (table, leaf) = path.rsplit_once('.').unwrap_or(("", &path));
            let off = leaf == "state"
                && matches!(
                    row.status,
                    Some(Status::Off | Status::KillSwitched | Status::Inert)
                );
            (leaf == name || path.contains(key) || (off && layers::within(key, table)))
                .then_some(path)
        })
        .collect();
    paths.dedup();
    paths.truncate(5);
    paths
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// `service`'s settings as `viewer` may see them, for a turn in `session`.
/// `chain` keeps each row's layers.
pub async fn report(
    service: &AgentService,
    viewer: &Viewer,
    session: &Session,
    scope: &Scope,
    chain: bool,
) -> Result<Report, Refusal> {
    if viewer.person.is_none() && !viewer.admin {
        return Err(Refusal::NoIdentity);
    }
    let mut rows = sections::rows(service, viewer, session).await;
    rows.retain(|row| viewer.admin || !row.install_wide);
    let sections = sections::names()
        .filter(|name| rows.iter().any(|row| row.section == *name))
        .collect::<Vec<_>>();
    let all = rows.len();
    match scope {
        Scope::All => {}
        Scope::Summary => {
            rows.retain(|row| row.source != Some(Source::Default) || row.status.is_some())
        }
        Scope::Section(name) => {
            if !sections.contains(&name.as_str()) {
                return Err(Refusal::UnknownSection {
                    section: name.clone(),
                    sections,
                });
            }
            rows.retain(|row| row.section == name);
        }
        Scope::Key(key) => {
            let key = key.trim().trim_matches('.');
            let asked = |row: &Row| layers::within(&row.path(), key) || row.key == key;
            if !rows.iter().any(asked) {
                return Err(Refusal::UnknownKey {
                    key: key.to_string(),
                    nearest: nearest(&rows, key),
                    sections,
                });
            }
            rows.retain(asked);
        }
    }
    let defaults_omitted = match scope {
        Scope::Summary => all - rows.len(),
        _ => 0,
    };
    if !chain {
        rows.iter_mut().for_each(|row| row.chain.clear());
    }
    Ok(Report {
        agent: service.agent_id.to_string(),
        viewer: match viewer.admin {
            true => "admin",
            false => "member",
        },
        sections,
        rows,
        defaults_omitted,
        note: None,
    })
}
