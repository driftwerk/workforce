use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! text_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, ::serde::Serialize, ::serde::Deserialize)]
        #[serde(rename_all = "lowercase")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            pub fn parse(s: &str) -> eyre::Result<Self> {
                match s {
                    $($text => Ok(Self::$variant),)+
                    other => Err(eyre::eyre!(concat!("unknown ", stringify!($name), " {:?}"), other)),
                }
            }
        }
    };
}

pub(crate) use text_enum;

pub use crate::engine::{Engine, View};

text_enum!(Origin { Desk => "desk", External => "external" });

text_enum!(Approvals { Desk => "desk", Terminal => "terminal" });

text_enum!(SessionState {
    Starting => "starting",
    Working => "working",
    Blocked => "blocked",
    Asking => "asking",
    Done => "done",
    Idle => "idle",
    Interrupted => "interrupted",
    Stopped => "stopped",
    Closed => "closed",
});

impl SessionState {
    /// States a session can only be in while its process is running.
    pub fn is_running(self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Working | Self::Blocked | Self::Asking | Self::Done | Self::Idle
        )
    }

    /// The state a running session is recorded in once its process is gone
    /// without having ended on its own (hub restart, reboot, crash).
    pub fn after_loss(self) -> Self {
        match self {
            Self::Starting | Self::Working | Self::Blocked => Self::Interrupted,
            Self::Asking | Self::Done | Self::Idle => Self::Stopped,
            other => other,
        }
    }
}

/// One Claude Code or Codex session as the registry stores it.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRecord {
    pub id: Uuid,
    pub engine: Engine,
    /// Where it runs: a terminal, or Desk's own view of it.
    pub view: View,
    /// The engine's version when it last started, as it reported it.
    pub engine_version: Option<String>,
    /// Codex's own reviewer answers its approvals ("Approve for me").
    pub approve_for_me: bool,
    /// The person's account it runs on ([`crate::engine_accounts`]); none:
    /// the CLI's own sign-in on this computer.
    pub account: Option<Uuid>,
    pub name: String,
    pub goal: String,
    pub cwd: String,
    pub branch: Option<String>,
    pub origin: Origin,
    pub state: SessionState,
    pub approvals: Approvals,
    pub cli_session_id: Option<String>,
    pub transcript: Option<String>,
    pub last_prompt: Option<String>,
    pub last_reply: Option<String>,
    pub external_pid: Option<i32>,
    pub restore_offer: bool,
    /// The Desk terminal it runs in, when it was started in one.
    pub terminal_id: Option<Uuid>,
    /// What the person calls it in Desk, when they renamed it.
    pub display_name: Option<String>,
    /// While it is asking: what, and why Desk read it so.
    pub asked: Option<crate::asking::Asked>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl SessionRecord {
    /// What Desk can do with this session: its engine's [`crate::engine::caps`]
    /// for the version it runs and where it runs. One adopted from another
    /// terminal has no environment of Desk's, so no line.
    pub fn caps(&self) -> crate::engine::Caps {
        let mut caps = crate::engine::session_caps(
            self.engine,
            self.view,
            self.engine_version.as_deref(),
            self.terminal_id.is_some(),
        );
        if self.origin == Origin::External {
            caps.line = crate::engine::Deliver::None;
            caps.steer = crate::engine::Steer::None;
        }
        caps
    }
}

/// An approval a session is waiting on, held open by its hook call.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingApproval {
    pub id: Uuid,
    pub tool: String,
    pub detail: String,
    pub since: DateTime<Utc>,
    /// False when the terminal is asking (approvals = terminal), so Desk only
    /// shows that the session is waiting.
    pub answerable: bool,
    /// What it can be answered with, when the engine says (Codex): empty
    /// for allow or deny.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<Decision>,
    /// What the engine said about it, in plain words (where it runs, why).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// The answers a question it asks offers, as the terminal lists them
    /// (Claude Code's AskUserQuestion, Codex's request for input).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
}

/// The wire shape of a session: the record plus what only exists while it runs.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub id: Uuid,
    pub engine: Engine,
    pub view: View,
    /// What Desk can do with it ([`crate::engine::caps`]).
    pub caps: crate::engine::Caps,
    pub name: String,
    pub goal: String,
    pub cwd: String,
    pub branch: Option<String>,
    pub origin: Origin,
    pub state: SessionState,
    pub live: bool,
    pub cli_session_id: Option<String>,
    pub last_prompt: Option<String>,
    pub last_reply: Option<String>,
    pub current_tool: Option<String>,
    pub pending: Option<PendingApproval>,
    pub approvals: Approvals,
    /// Codex's own reviewer answers its approvals ("Approve for me").
    pub approve_for_me: bool,
    /// The person's account it runs on; none: the CLI's own sign-in.
    pub account: Option<crate::engine_accounts::AccountRef>,
    /// The account it moves to once its turn ends, when a switch waits.
    pub switching_to: Option<crate::engine_accounts::AccountRef>,
    /// The Desk terminal it runs in, when it was started in one.
    pub terminal_id: Option<Uuid>,
    /// What the person calls it in Desk (`name` is the hub's own).
    pub display_name: Option<String>,
    /// The sidebar group it is in, if any.
    pub group: Option<GroupRef>,
    /// Work it started that still runs: while it is done, it waits on it.
    pub background: Option<crate::floor::Background>,
    /// While it is asking: what, and why Desk read it so.
    pub asked: Option<crate::asking::Asked>,
    /// How many prompts the person has given it while it ran here, so the
    /// same words said again are told from none.
    #[serde(skip)]
    pub prompts: u64,
    /// Its turn was ended from Desk, not finished.
    #[serde(skip)]
    pub interrupted: bool,
    /// The hub's model is reading whether its turn's reply asks the person:
    /// the turn isn't over until it says.
    #[serde(skip)]
    pub reading: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl SessionView {
    /// What it waits on of its own work while its turn is done; none while
    /// it works, or while it asks the person something (asking beats
    /// background: it needs them either way).
    pub fn waiting_on(&self) -> u32 {
        match (self.state, self.background) {
            (SessionState::Done, Some(b)) => b.count,
            _ => 0,
        }
    }
}

/// A sidebar group, as a session or agent in it names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupRef {
    pub id: String,
    pub name: String,
}

impl SessionView {
    /// What to call it: the person's name for it, else the hub's.
    pub fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }

    /// The words the person may find it by: both names, its folder, its
    /// group, its goal.
    pub fn search_text(&self) -> String {
        let folder = std::path::Path::new(&self.cwd)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        [
            self.display_name.as_deref().unwrap_or_default(),
            &self.name,
            &folder,
            self.group.as_ref().map_or("", |g| g.name.as_str()),
            &self.goal,
        ]
        .join(" ")
        .to_lowercase()
    }

    pub fn new(
        r: &SessionRecord,
        live: bool,
        current_tool: Option<String>,
        pending: Option<PendingApproval>,
    ) -> Self {
        Self {
            id: r.id,
            engine: r.engine,
            view: r.view,
            caps: r.caps(),
            name: r.name.clone(),
            goal: r.goal.clone(),
            cwd: r.cwd.clone(),
            branch: r.branch.clone(),
            origin: r.origin,
            state: r.state,
            live,
            cli_session_id: r.cli_session_id.clone(),
            last_prompt: r.last_prompt.clone(),
            last_reply: r.last_reply.clone(),
            current_tool,
            pending,
            approvals: r.approvals,
            approve_for_me: r.approve_for_me,
            account: None,
            switching_to: None,
            terminal_id: r.terminal_id,
            display_name: r.display_name.clone(),
            group: None,
            background: None,
            asked: r.asked.clone().filter(|_| r.state == SessionState::Asking),
            prompts: 0,
            interrupted: false,
            reading: false,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DeskEvent {
    Snapshot {
        sessions: Vec<SessionView>,
        restore: Vec<SessionView>,
        rooms: Vec<crate::rooms::Room>,
        terminals: Vec<crate::service::TerminalView>,
        layout: crate::service::Layout,
    },
    /// The sidebar's groups changed; the whole layout as it now is.
    Layout {
        layout: crate::service::Layout,
    },
    Terminal {
        terminal: Box<crate::service::TerminalView>,
    },
    TerminalRemoved {
        id: Uuid,
    },
    Session {
        session: Box<SessionView>,
    },
    Removed {
        id: Uuid,
    },
    Room {
        room: Box<crate::rooms::Room>,
    },
    RoomRemoved {
        id: Uuid,
    },
    RoomMessage {
        message: Box<crate::rooms::RoomMessage>,
    },
    /// Something that happened on the crew, as Activity records it.
    Activity {
        event: Box<crate::crew::Event>,
    },
    /// A read point moved: every window hears it.
    Read {
        place: crate::reads::ReadPlace,
        point: crate::reads::ReadPoint,
    },
    /// What a session spent changed: its engine's record was read.
    Spend {
        session: Uuid,
    },
    /// The person's engine accounts changed: all of them as they now are.
    Accounts {
        accounts: Vec<crate::engine_accounts::AccountView>,
        needs: Vec<crate::engine_accounts::AccountNeed>,
        #[serde(rename = "autoSwitch")]
        auto_switch: bool,
        problems: Vec<crate::engine_accounts::ProblemView>,
    },
    /// Signing in to an account through its engine's own command moved on.
    AccountSignIn {
        #[serde(rename = "signIn")]
        sign_in: crate::engine_accounts::SignInView,
    },
    /// A message on a session's line was sent, or moved on (delivered,
    /// answered, edited); `removed` once the person took it back.
    Line {
        message: Box<crate::line::LineMessage>,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        removed: bool,
    },
    /// A key of Desk's own state changed; `value` is `null` once removed.
    State {
        key: String,
        value: serde_json::Value,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSession {
    pub engine: Engine,
    /// Where it runs; the engine's default view ([`crate::engine::views`])
    /// when not said.
    #[serde(default)]
    pub view: Option<View>,
    pub name: String,
    pub cwd: String,
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub worktree: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub approvals: Option<Approvals>,
    /// Codex only: its own reviewer answers its approvals.
    #[serde(default)]
    pub approve_for_me: bool,
    /// The person's account it runs on: absent, the engine's default
    /// account; `null`, the CLI's own sign-in on this computer.
    #[serde(default, deserialize_with = "present")]
    pub account: Option<Option<Uuid>>,
    /// Added to the CLI's environment, as a terminal's is: Desk's plugin
    /// folder in `CLAUDE_CODE_PLUGIN_DIRS`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPatch {
    pub name: Option<String>,
    pub goal: Option<String>,
    pub approvals: Option<Approvals>,
    /// The person's name for it; `null` (or blank) clears it.
    #[serde(default, deserialize_with = "present")]
    pub display_name: Option<Option<String>>,
}

/// A field that may be absent (`None`), `null` (`Some(None)`) or a value.
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
    Terminal,
    /// Allow it and what is like it for the rest of the session (Codex).
    AllowForSession,
    /// Deny it and end the turn (Codex).
    Cancel,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_states_become_interrupted_or_stopped_when_lost() {
        assert_eq!(
            SessionState::Working.after_loss(),
            SessionState::Interrupted
        );
        assert_eq!(
            SessionState::Blocked.after_loss(),
            SessionState::Interrupted
        );
        assert_eq!(
            SessionState::Starting.after_loss(),
            SessionState::Interrupted
        );
        assert_eq!(SessionState::Done.after_loss(), SessionState::Stopped);
        assert_eq!(SessionState::Asking.after_loss(), SessionState::Stopped);
        assert_eq!(SessionState::Idle.after_loss(), SessionState::Stopped);
        assert_eq!(SessionState::Closed.after_loss(), SessionState::Closed);
    }

    #[test]
    fn text_enums_round_trip() {
        for s in [
            SessionState::Starting,
            SessionState::Working,
            SessionState::Blocked,
            SessionState::Asking,
            SessionState::Done,
            SessionState::Idle,
            SessionState::Interrupted,
            SessionState::Stopped,
            SessionState::Closed,
        ] {
            assert_eq!(SessionState::parse(s.as_str()).unwrap(), s);
        }
        assert!(Engine::parse("grok").is_err());
    }
}
