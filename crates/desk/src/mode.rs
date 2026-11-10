//! A session's permission mode: how much its engine asks before it acts.
//!
//! Neither engine takes a new mode reliably while it runs: Claude Code only
//! cycles its modes from the keyboard (Shift+Tab), with nothing to say which
//! it landed on until its next hook, and Codex's approval policy and sandbox
//! reach its thread only from the TUI's own settings at launch (verified on
//! CLI 0.166, [`crate::codex::APPROVE_FOR_ME`]). So Desk keeps the mode the
//! session should run in and starts it in that mode: a running session is
//! restarted in place, resumed by id with its conversation kept. Claude Code
//! says the mode it runs in on every hook (`permission_mode`), which is what
//! Desk reads back.

use serde::{Deserialize, Serialize};

/// How much a session asks before it acts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    /// It asks before what its settings don't allow: the engine's default.
    Default,
    /// Edits go ahead; commands still ask.
    AcceptEdits,
    /// The engine decides for itself what is safe (Claude Code's auto mode;
    /// Codex asks only when a command fails in its sandbox).
    Auto,
    /// It only reads and plans; nothing changes.
    Plan,
    /// It asks for nothing at all.
    BypassPermissions,
}

impl Mode {
    pub const ALL: [Mode; 5] = [
        Mode::Default,
        Mode::AcceptEdits,
        Mode::Auto,
        Mode::Plan,
        Mode::BypassPermissions,
    ];

    /// Its name, as Claude Code's `--permission-mode` spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Auto => "auto",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
        }
    }

    /// What the person reads: "auto", "accept edits".
    pub fn title(self) -> &'static str {
        match self {
            Self::Default => "manual",
            Self::AcceptEdits => "accept edits",
            Self::Auto => "auto",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypass permissions",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        Self::ALL
            .into_iter()
            .find(|m| m.as_str().eq_ignore_ascii_case(s))
            .or(match s.to_ascii_lowercase().as_str() {
                "manual" | "ask" => Some(Self::Default),
                "accept_edits" | "accept-edits" | "accept edits" => Some(Self::AcceptEdits),
                "bypass" | "bypass_permissions" => Some(Self::BypassPermissions),
                _ => None,
            })
    }

    /// Whether, in it, the session still asks before it runs a command.
    pub fn asks(self) -> bool {
        matches!(self, Self::Default | Self::AcceptEdits | Self::Plan)
    }

    /// Claude Code's arguments to start in it.
    pub fn claude_args(self) -> [String; 2] {
        ["--permission-mode".to_string(), self.as_str().to_string()]
    }

    /// Codex's settings to start in it: its approval policy and sandbox.
    pub fn codex_settings(self) -> [&'static str; 2] {
        match self {
            Self::Default | Self::AcceptEdits => {
                ["approval_policy=on-request", "sandbox_mode=workspace-write"]
            }
            Self::Auto => ["approval_policy=on-failure", "sandbox_mode=workspace-write"],
            Self::Plan => ["approval_policy=on-request", "sandbox_mode=read-only"],
            Self::BypassPermissions => ["approval_policy=never", "sandbox_mode=danger-full-access"],
        }
    }
}

/// The mode a Claude Code hook says it runs in (`permission_mode`).
pub fn of_hook(body: &[u8]) -> Option<Mode> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    Mode::parse(v.get("permission_mode")?.as_str()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_read_as_claude_code_spells_them_and_as_the_person_says_them() {
        for m in Mode::ALL {
            assert_eq!(Mode::parse(m.as_str()), Some(m));
        }
        assert_eq!(Mode::parse("manual"), Some(Mode::Default));
        assert_eq!(Mode::parse("accept_edits"), Some(Mode::AcceptEdits));
        assert_eq!(Mode::parse("yolo"), None);
        assert_eq!(
            of_hook(br#"{"hook_event_name":"SessionStart","permission_mode":"auto"}"#),
            Some(Mode::Auto)
        );
        assert_eq!(of_hook(br#"{"hook_event_name":"Stop"}"#), None);
        assert_eq!(
            Mode::Plan.claude_args(),
            ["--permission-mode".to_string(), "plan".to_string()]
        );
        assert!(Mode::Default.asks() && !Mode::Auto.asks());
    }
}
