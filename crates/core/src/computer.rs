//! A person's own computer as their Desk presents it to a remote hub: the
//! node a Desk becomes on its hub link, and what a turn sees of it.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Macos,
    Linux,
    Windows,
}

/// Where Desk's backend runs on the computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Runtime {
    Native,
    Wsl { distro: String },
}

/// A Desk as it introduces itself on its link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeskNode {
    /// What the person calls the computer, e.g. "Sarah's MacBook Pro".
    pub name: String,
    pub platform: Platform,
    pub runtime: Runtime,
    /// This workspace's switch on the computer, its kill switch included.
    pub enabled: bool,
    /// The folders agents may work in, as the person reads them
    /// ("~/dev/acme-web").
    #[serde(default)]
    pub folders: Vec<String>,
    /// When the person last used Desk on this computer.
    pub active_at: DateTime<Utc>,
}

/// One of the person's Desks, online: what a turn sees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnlineDesk {
    pub key: String,
    pub device_id: String,
    pub node: DeskNode,
}

/// How far an agent on this computer reaches: its `[computer] access`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// Reads and changes only its own folder.
    OwnFolder,
    /// Reads anything but the withheld set; changes only its own folder.
    ReadAnywhere,
    /// Reads, changes and runs anything but the withheld set; only what
    /// [`Asking`] names asks first.
    #[default]
    Full,
}

impl Access {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OwnFolder => "own_folder",
            Self::ReadAnywhere => "read_anywhere",
            Self::Full => "full",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "own_folder" => Some(Self::OwnFolder),
            "read_anywhere" => Some(Self::ReadAnywhere),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    /// As the person reads it in Settings.
    pub fn label(self) -> &'static str {
        match self {
            Self::OwnFolder => "Own folder",
            Self::ReadAnywhere => "Read anywhere",
            Self::Full => "Full access",
        }
    }
}

/// An agent's `[computer]`, every default filled in. An agent with none is
/// at Full access, asking only before what can't be undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ComputerAccess {
    pub access: Access,
    /// At `full`, whether a check decides routine changes instead of asking.
    pub autonomous: bool,
    /// Whether changes inside its own folder ask too.
    pub ask_in_own_folder: bool,
    /// What happens at Full access to what would ask, where nobody can be
    /// asked (a scheduled or background job, a workflow step).
    #[serde(default)]
    pub unattended: Unattended,
    /// At Full access, what still asks first.
    #[serde(default)]
    pub asks: Asking,
}

/// At Full access, what asks before it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Asking {
    /// What can't be undone or reaches people (the always-asks list); the
    /// rest runs.
    #[default]
    Irreversible,
    /// Nothing: it does anything its access reaches.
    Never,
}

impl Asking {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Irreversible => "irreversible",
            Self::Never => "never",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "irreversible" => Some(Self::Irreversible),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

/// Where nobody can be asked, at Full access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unattended {
    /// It runs, as before the asking: what the person set the agent to do
    /// on its own is theirs to tighten.
    #[default]
    Run,
    /// It is refused.
    Refuse,
}

impl Unattended {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Refuse => "refuse",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "run" => Some(Self::Run),
            "refuse" => Some(Self::Refuse),
            _ => None,
        }
    }
}

/// What one tool call does to this computer or a connector, as the tool
/// declares it from its input. Paths are canonical by the time a verdict
/// reads them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Effect {
    /// Touches nothing here (memory, web, messaging, ...): never asks.
    None,
    Read {
        paths: Vec<PathBuf>,
    },
    Write {
        paths: Vec<PathBuf>,
    },
    Delete {
        paths: Vec<PathBuf>,
    },
    /// A delete of `from` and a write of `to`.
    Move {
        from: Vec<PathBuf>,
        to: PathBuf,
    },
    /// A command, as the shell runs it, and where.
    Exec {
        command: String,
        cwd: Option<PathBuf>,
    },
    /// A coding session started in `cwd`.
    Session {
        cwd: Option<PathBuf>,
    },
    Connector {
        connector: String,
        write: bool,
    },
    /// Declared nothing, in a namespace that touches this computer: counts
    /// as a write to a place it can't name.
    Unknown,
}

/// What a grant lets an agent do without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ability {
    Write,
    Delete,
    Run,
    Session,
    ConnectorWrite,
}

impl Ability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Delete => "delete",
            Self::Run => "run",
            Self::Session => "session",
            Self::ConnectorWrite => "connector_write",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "write" => Some(Self::Write),
            "delete" => Some(Self::Delete),
            "run" => Some(Self::Run),
            "session" => Some(Self::Session),
            "connector_write" => Some(Self::ConnectorWrite),
            _ => None,
        }
    }
}

/// Where an "always" answer holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Always {
    /// This folder and everything under it.
    Folder(PathBuf),
    /// Writes to this connector.
    Connector(String),
}

/// An answer a card may offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Choice {
    AllowOnce,
    AllowSession,
    Always(Always),
    Deny,
}

/// What let a call run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "grant", rename_all = "snake_case")]
pub enum By {
    /// The level allows it as it is: a read, or a change in its own folder.
    Rule,
    /// A standing grant.
    Grant(Uuid),
    /// An allowance for this session.
    Session(Uuid),
    /// Nobody could be asked, and at Full access the agent runs what would
    /// ask (`[computer] unattended = "run"`, the default). Never a verdict's
    /// own answer: the gate's, on a path with nobody on it.
    Unattended,
}

/// Which rule refused a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refused {
    Stopped,
    Withheld,
    OutsideLevel,
    ConnectorOff,
    ConnectorReadOnly,
}

/// A refusal, with the sentence the agent and the person read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub rule: Refused,
    pub sentence: String,
}

/// What a call on this computer would do, as the hub resolved it and the
/// card shows it: never the agent's own words.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ComputerFacts {
    /// read | write | delete | exec | session | connector | unknown
    pub effect: String,
    /// The paths it names, real, as the person reads them (`~/…`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connector: Option<String>,
    /// The card's "why it asks" line.
    pub why: String,
}

/// "Sign in for me": the call that opens a site for the person to sign in
/// to, and the call that waits on their card while they do.
pub const SIGN_IN: &str = "browser.sign_in";
pub const SIGNED_IN: &str = "browser.signed_in";

/// Whether a browser call goes to an address: a navigation, or opening a
/// site to sign in to. After a hostile page, going anywhere asks.
pub fn navigates(tool: &str) -> bool {
    matches!(tool, "browser.navigate" | SIGN_IN)
}

/// Whether only the person in Desk may answer this call's card: never an
/// approver it is routed to, nor a client answering for them.
pub fn person_only(tool: &str) -> bool {
    tool == SIGNED_IN
}

/// Why a card asks, as its one "why" line says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Why {
    /// A change outside the agent's own folder.
    OutsideFolder,
    /// A change inside it, with `ask_in_own_folder` on.
    InOwnFolder,
    /// Running a command or starting a session.
    Runs,
    /// It can't be undone.
    CantBeUndone,
    /// The command can't be read as simple commands.
    Unclear,
    /// It sends to other people.
    SendsToPeople,
    /// A write to a connector.
    ConnectorWrite,
    /// The tool can't say what it changes.
    Unknown,
    /// It acts on a web page: clicks, types, submits.
    OnAPage,
    /// The page it read tried to give it instructions.
    HostilePage,
    /// It would use the person's own Chrome, signed in as them.
    TheirChrome,
    /// It would open a site in the person's Chrome it hasn't this session.
    NewSite,
    /// It waits while the person signs in to a site themselves.
    SignIn,
}

impl Why {
    pub fn sentence(self) -> &'static str {
        match self {
            Self::OutsideFolder => "Outside its folder",
            Self::InOwnFolder => "You asked to be asked in its folder too",
            Self::Runs => "Runs on this computer",
            Self::CantBeUndone => "Can't be undone",
            Self::Unclear => "Can't tell what this command runs",
            Self::SendsToPeople => "Sends to other people",
            Self::ConnectorWrite => "Changes something in a connected service",
            Self::Unknown => "This tool can't say what it changes",
            Self::OnAPage => "Acts on a web page",
            Self::HostilePage => "The page it read tried to give it instructions",
            Self::TheirChrome => "Uses your own Chrome, signed in as you",
            Self::NewSite => "Opens a new site in your Chrome",
            Self::SignIn => "Waits while you sign in. Desk never sees your password",
        }
    }
}

/// What happens to one call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Verdict {
    Run {
        by: By,
    },
    /// Ask the person, offering these answers in this order.
    Ask {
        choices: Vec<Choice>,
        why: Why,
    },
    /// The agent works on its own: the check decides between run and ask.
    Review,
    Refuse {
        refusal: Refusal,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_desk_node_reads_as_desk_sends_it() {
        let node: DeskNode = serde_json::from_value(serde_json::json!({
            "name": "Sarah's MacBook Pro",
            "platform": "windows",
            "runtime": {"kind": "wsl", "distro": "Ubuntu"},
            "enabled": true,
            "active_at": "2026-10-30T09:00:00Z",
        }))
        .unwrap();
        assert_eq!(
            node.runtime,
            Runtime::Wsl {
                distro: "Ubuntu".into()
            }
        );
        assert!(node.folders.is_empty());
        let back = serde_json::to_value(&node).unwrap();
        assert_eq!(back["platform"], "windows");
        assert_eq!(serde_json::from_value::<DeskNode>(back).unwrap(), node);
    }
}
