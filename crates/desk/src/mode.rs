//! A session's permission mode: how much its engine asks before it acts.
//!
//! Neither engine takes a new mode reliably while it runs: Claude Code only
//! cycles its modes from the keyboard (Shift+Tab), with nothing to say which
//! it landed on until its next hook, and Codex's approval policy and sandbox
//! reach its thread only as it is started or resumed (verified on CLI 0.166,
//! [`crate::codex::Permissions`]). So Desk keeps the mode the session should
//! run in and starts it in that mode: a running session is restarted in
//! place, resumed by id with its conversation kept. Claude Code says the
//! mode it runs in on every hook (`permission_mode`), which is what Desk
//! reads back.

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
    /// in Codex, its own reviewer answers what its model asks, in its
    /// sandbox: [`crate::codex::Permissions::of`]).
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

    /// Claude Code's arguments to start in it: bypass by
    /// `--dangerously-skip-permissions`, the rest by `--permission-mode`.
    pub fn claude_args(self) -> Vec<String> {
        match self {
            Self::BypassPermissions => vec!["--dangerously-skip-permissions".to_string()],
            m => vec!["--permission-mode".to_string(), m.as_str().to_string()],
        }
    }

    /// Devin's arguments to start in it (`--permission-mode`); none for
    /// plan, which Devin enters only from its terminal. Its `smart` may be
    /// off for the person's account, and Devin then runs as `normal`.
    pub fn devin_args(self) -> Option<Vec<String>> {
        let mode = match self {
            Self::Default => "normal",
            Self::AcceptEdits => "accept-edits",
            Self::Auto => "smart",
            Self::BypassPermissions => "dangerous",
            Self::Plan => return None,
        };
        Some(vec!["--permission-mode".to_string(), mode.to_string()])
    }

    /// What a Codex session runs in, in it: its approval policy and sandbox,
    /// as both its settings and its app-server spell them (never
    /// `on-failure`, which CLIs up to 0.140 ran as a policy of its own and
    /// 0.166's app-server refuses), its network, and who answers what it
    /// asks. `charged`: a boss's live charge holds it, and every escalation
    /// is then the boss's to decide, so neither auto mode's network nor
    /// Codex's own reviewer applies; plan and bypass are the person's
    /// explicit choices and stay as they are.
    pub fn codex_permissions(
        self,
        charged: bool,
        approve_for_me: bool,
    ) -> crate::codex::Permissions {
        use crate::codex::{Network, Permissions, Reviewer};
        let asks = |reviewer| Permissions {
            approval: "on-request",
            sandbox: "workspace-write",
            network: Network::Person,
            reviewer,
        };
        match self {
            Self::Auto if !charged => Permissions {
                network: Network::On,
                ..asks(Reviewer::AutoReview)
            },
            Self::Default | Self::AcceptEdits if approve_for_me && !charged => {
                asks(Reviewer::AutoReview)
            }
            Self::Default | Self::AcceptEdits | Self::Auto => asks(Reviewer::User),
            Self::Plan => Permissions {
                sandbox: "read-only",
                ..asks(Reviewer::User)
            },
            Self::BypassPermissions => Permissions {
                approval: "never",
                sandbox: "danger-full-access",
                ..asks(Reviewer::User)
            },
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
        assert_eq!(Mode::Plan.claude_args(), ["--permission-mode", "plan"]);
        assert_eq!(
            Mode::BypassPermissions.claude_args(),
            ["--dangerously-skip-permissions"]
        );
        assert!(Mode::Default.asks() && !Mode::Auto.asks());
    }

    #[test]
    fn codex_runs_each_mode_as_its_table_says_and_a_charge_wins_over_auto() {
        use crate::codex::{Network, Reviewer};
        let row = |m: Mode, charged, afm| {
            let p = m.codex_permissions(charged, afm);
            (p.approval, p.sandbox, p.network, p.reviewer)
        };
        let ws = ("on-request", "workspace-write");
        for charged in [false, true] {
            for afm in [false, true] {
                let reviewer = match afm && !charged {
                    true => Reviewer::AutoReview,
                    false => Reviewer::User,
                };
                for m in [Mode::Default, Mode::AcceptEdits] {
                    assert_eq!(
                        row(m, charged, afm),
                        (ws.0, ws.1, Network::Person, reviewer)
                    );
                }
                let auto = match charged {
                    false => (ws.0, ws.1, Network::On, Reviewer::AutoReview),
                    true => (ws.0, ws.1, Network::Person, Reviewer::User),
                };
                assert_eq!(row(Mode::Auto, charged, afm), auto, "{charged} {afm}");
                assert_eq!(
                    row(Mode::Plan, charged, afm),
                    ("on-request", "read-only", Network::Person, Reviewer::User)
                );
                assert_eq!(
                    row(Mode::BypassPermissions, charged, afm),
                    (
                        "never",
                        "danger-full-access",
                        Network::Person,
                        Reviewer::User
                    )
                );
            }
        }
    }
}
