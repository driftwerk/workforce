use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! text_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

text_enum!(Engine { Claude => "claude", Codex => "codex" });

text_enum!(Origin { Desk => "desk", External => "external" });

text_enum!(Approvals { Desk => "desk", Terminal => "terminal" });

text_enum!(SessionState {
    Starting => "starting",
    Working => "working",
    Blocked => "blocked",
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
            Self::Starting | Self::Working | Self::Blocked | Self::Done | Self::Idle
        )
    }

    /// The state a running session is recorded in once its process is gone
    /// without having ended on its own (hub restart, reboot, crash).
    pub fn after_loss(self) -> Self {
        match self {
            Self::Starting | Self::Working | Self::Blocked => Self::Interrupted,
            Self::Done | Self::Idle => Self::Stopped,
            other => other,
        }
    }
}

/// One Claude Code or Codex session as the registry stores it.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRecord {
    pub id: Uuid,
    pub engine: Engine,
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
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
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
}

/// The wire shape of a session: the record plus what only exists while it runs.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub id: Uuid,
    pub engine: Engine,
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
    /// The Desk terminal it runs in, when it was started in one.
    pub terminal_id: Option<Uuid>,
    /// What the person calls it in Desk (`name` is the hub's own).
    pub display_name: Option<String>,
    /// The sidebar group it is in, if any.
    pub group: Option<GroupRef>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
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
            terminal_id: r.terminal_id,
            display_name: r.display_name.clone(),
            group: None,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Deny,
    Terminal,
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
        assert_eq!(SessionState::Idle.after_loss(), SessionState::Stopped);
        assert_eq!(SessionState::Closed.after_loss(), SessionState::Closed);
    }

    #[test]
    fn text_enums_round_trip() {
        for s in [
            SessionState::Starting,
            SessionState::Working,
            SessionState::Blocked,
            SessionState::Done,
            SessionState::Idle,
            SessionState::Interrupted,
            SessionState::Stopped,
            SessionState::Closed,
        ] {
            assert_eq!(SessionState::parse(s.as_str()).unwrap(), s);
        }
        assert!(Engine::parse("gemini").is_err());
    }
}
