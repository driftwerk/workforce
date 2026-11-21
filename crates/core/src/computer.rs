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
    /// A move of `paths` to the Trash: a delete that can be undone.
    Trash {
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

/// A kind of action an agent may take on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// Running commands and starting sessions.
    Commands,
    /// Commands that reach other machines: ssh, scp, sftp, rsync, mosh.
    Network,
    /// The person's Chrome: using it, opening sites, acting on pages.
    Browser,
    /// Changing files outside its folders.
    Files,
    /// Moving things to the Trash.
    Delete,
}

impl ActionKind {
    pub const ALL: [ActionKind; 5] = [
        ActionKind::Commands,
        ActionKind::Network,
        ActionKind::Browser,
        ActionKind::Files,
        ActionKind::Delete,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Commands => "commands",
            Self::Network => "network",
            Self::Browser => "browser",
            Self::Files => "files",
            Self::Delete => "delete",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// Whether "in its folders" means anything for it: not for hosts or
    /// sites.
    pub fn takes_folders(self) -> bool {
        !matches!(self, Self::Network | Self::Browser)
    }
}

/// How far an agent does a kind of action on its own. Asking each time is
/// no level at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// Where its targets are inside its own folder or one it works in.
    InFolders,
    Anywhere,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InFolders => "in_folders",
            Self::Anywhere => "anywhere",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "in_folders" => Some(Self::InFolders),
            "anywhere" => Some(Self::Anywhere),
            _ => None,
        }
    }
}

/// Where a kind's level holds: the whole kind, or one folder, host or site.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Scope {
    Whole,
    Folder(PathBuf),
    /// A host as a command names it (`devbox`, `10.0.1.20`).
    Host(String),
    /// A site's origin (`https://github.com`).
    Site(String),
}

impl Scope {
    /// As it is stored: empty for the whole kind, else `folder:`, `host:` or
    /// `site:` and its value.
    pub fn text(&self) -> String {
        match self {
            Self::Whole => String::new(),
            Self::Folder(f) => format!("folder:{}", f.display()),
            Self::Host(h) => format!("host:{h}"),
            Self::Site(s) => format!("site:{s}"),
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        if s.is_empty() {
            return Some(Self::Whole);
        }
        let (kind, value) = s.split_once(':')?;
        (!value.is_empty()).then_some(())?;
        match kind {
            "folder" => Some(Self::Folder(PathBuf::from(value))),
            "host" => Some(Self::Host(value.to_string())),
            "site" => Some(Self::Site(value.to_string())),
            _ => None,
        }
    }

    /// Whether it fits `kind`: a host for the network, a site for the
    /// browser, a folder for the rest.
    pub fn fits(&self, kind: ActionKind) -> bool {
        match self {
            Self::Whole => true,
            Self::Folder(_) => kind.takes_folders(),
            Self::Host(_) => kind == ActionKind::Network,
            Self::Site(_) => kind == ActionKind::Browser,
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
    /// This kind of action, on its own, in this scope.
    OnItsOwn { kind: ActionKind, scope: Scope },
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
    /// The person lets it do this kind of action on its own.
    OnItsOwn,
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
    /// It deletes on its own only by moving to the Trash.
    UseTrash,
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

/// Opening a page for the person: it goes to an address, then is theirs.
pub const OPEN_FOR_ME: &str = "browser.open_for_me";

/// Whether a browser call goes to an address: a navigation, opening a page
/// for the person, or opening a site to sign in to. After a hostile page,
/// going anywhere asks.
pub fn navigates(tool: &str) -> bool {
    matches!(tool, "browser.navigate" | OPEN_FOR_ME | SIGN_IN)
}

static PERSONS_CHROME: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Says this hub's browser is the person's own Chrome (a Desk hub), not a
/// server's headless one.
pub fn browser_is_the_persons_chrome() {
    PERSONS_CHROME.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Whether only the person in Desk may answer this call's card: never an
/// approver it is routed to, nor a client answering for them. "I'm signed
/// in", and on a Desk hub every browser call's: it is their own Chrome,
/// signed in as them. A server's headless browser routes as any tool.
pub fn person_only(tool: &str) -> bool {
    tool == SIGNED_IN
        || (tool.starts_with("browser.")
            && PERSONS_CHROME.load(std::sync::atomic::Ordering::Relaxed))
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
    /// It changes the computer's own settings or security.
    System,
    /// It moves something to the Trash outside its folder.
    ToTrash,
    /// It deletes or replaces what can't be told until it runs.
    ForGood,
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
            Self::System => "Changes this computer's own settings or security",
            Self::ToTrash => "Moves to the Trash",
            Self::ForGood => "Can't tell what this deletes or replaces",
        }
    }

    /// Whether an ask stays the person's whatever the agent may do on its
    /// own: a page that read as hostile, their own sign-in, what reaches
    /// other people or a connected service, what a tool can't name, what a
    /// command can't be read as, what it can't say it deletes, and the
    /// computer's own settings and security.
    pub fn held(self) -> bool {
        matches!(
            self,
            Self::HostilePage
                | Self::SignIn
                | Self::SendsToPeople
                | Self::ConnectorWrite
                | Self::Unknown
                | Self::Unclear
                | Self::ForGood
                | Self::System
        )
    }

    /// Whether an ask is refused, not run, where nobody can be asked.
    pub fn never_unattended(self) -> bool {
        matches!(self, Self::Unclear | Self::ForGood | Self::System)
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

    #[test]
    fn on_its_own_round_trips_as_stored_and_as_sent() {
        for kind in ActionKind::ALL {
            assert_eq!(ActionKind::parse(kind.as_str()), Some(kind));
            let sent = serde_json::to_value(kind).unwrap();
            assert_eq!(sent, serde_json::json!(kind.as_str()));
            assert_eq!(serde_json::from_value::<ActionKind>(sent).unwrap(), kind);
        }
        for level in [Level::InFolders, Level::Anywhere] {
            assert_eq!(Level::parse(level.as_str()), Some(level));
            assert_eq!(
                serde_json::from_value::<Level>(serde_json::to_value(level).unwrap()).unwrap(),
                level
            );
        }
        for (scope, sent) in [
            (Scope::Whole, serde_json::json!({"kind": "whole"})),
            (
                Scope::Folder(PathBuf::from("/Users/sarah/dev")),
                serde_json::json!({"kind": "folder", "value": "/Users/sarah/dev"}),
            ),
            (
                Scope::Host("devbox".into()),
                serde_json::json!({"kind": "host", "value": "devbox"}),
            ),
            (
                Scope::Site("https://github.com".into()),
                serde_json::json!({"kind": "site", "value": "https://github.com"}),
            ),
        ] {
            assert_eq!(Scope::parse(&scope.text()), Some(scope.clone()));
            assert_eq!(serde_json::to_value(&scope).unwrap(), sent);
            assert_eq!(serde_json::from_value::<Scope>(sent).unwrap(), scope);
        }
        assert_eq!(Scope::Whole.text(), "");
        for bad in ["host:", "planet:x", "devbox"] {
            assert_eq!(Scope::parse(bad), None, "{bad}");
        }
        let always = Always::OnItsOwn {
            kind: ActionKind::Network,
            scope: Scope::Host("devbox".into()),
        };
        let sent = serde_json::to_value(&always).unwrap();
        assert_eq!(serde_json::from_value::<Always>(sent).unwrap(), always);
        assert_eq!(
            serde_json::from_value::<By>(serde_json::to_value(By::OnItsOwn).unwrap()).unwrap(),
            By::OnItsOwn
        );
    }

    #[test]
    fn held_asks_stay_the_persons_and_refuse_unattended() {
        for why in [Why::Unclear, Why::ForGood, Why::System] {
            assert!(why.held() && why.never_unattended(), "{why:?}");
        }
        for why in [
            Why::CantBeUndone,
            Why::OutsideFolder,
            Why::OnAPage,
            Why::NewSite,
        ] {
            assert!(!why.held() && !why.never_unattended(), "{why:?}");
        }
        assert!(Why::SendsToPeople.held() && !Why::SendsToPeople.never_unattended());
    }
}
