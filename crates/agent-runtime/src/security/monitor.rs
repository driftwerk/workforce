//! The guard a turn runs under once outside content has entered it.
//!
//! An email body, a fetched page, a webhook payload or a watched message is
//! fenced as data, but a model can still be talked into acting on it. From the
//! moment such content enters a turn until the turn ends, a call that would
//! carry data somewhere the turn doesn't deliver, reach credentials, change
//! the agent's own setup or standing automation, or rewrite its identity is
//! held before it runs: asked about where someone can answer, refused where
//! the turn's scope says so, and the person told why either way.

use std::sync::Mutex;

use serde_json::Value;
use tracing::warn;

use crate::tools::{Destination, SurfaceEffect};

/// How every fence of outside content opens ([`super::external_content`]).
/// Content inside a fence has its `<<<` defanged, so only a real fence reads
/// as one.
pub const FENCE_MARK: &str = "<<<EXTERNAL_UNTRUSTED_CONTENT_";

/// A call the guard holds, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnomalySignal {
    Exfiltration { tool: String, to: String },
    CredentialAccess { tool: String },
    PrivilegeEscalation { tool: String },
    RuleCreation { tool: String, action: String },
    IdentityTampering { tool: String, file: String },
}

impl AnomalySignal {
    pub fn tool(&self) -> &str {
        match self {
            Self::Exfiltration { tool, .. }
            | Self::CredentialAccess { tool }
            | Self::PrivilegeEscalation { tool }
            | Self::RuleCreation { tool, .. }
            | Self::IdentityTampering { tool, .. } => tool,
        }
    }

    pub fn description(&self) -> String {
        match self {
            Self::Exfiltration { tool, to } => {
                format!("`{tool}` would send to {to}, somewhere this turn doesn't deliver")
            }
            Self::CredentialAccess { tool } => format!("`{tool}` would reach stored credentials"),
            Self::PrivilegeEscalation { tool } => {
                format!("`{tool}` would change the agent's own setup")
            }
            Self::RuleCreation { tool, action } => {
                format!("`{tool}` {action} would change standing automation")
            }
            Self::IdentityTampering { tool, file } => {
                format!("`{tool}` would rewrite the agent's {file}")
            }
        }
    }
}

const CREDENTIAL_NAMESPACE: &str = "vault.";

const SYSTEM_NAMESPACE: &str = "system.";

/// The `system.*` tools that only look.
const SYSTEM_READS: &[&str] = &["version", "logs"];

const POLICY_WRITES: &[&str] = &["policy.propose"];

/// Tools whose named actions change standing automation: what runs later, on
/// its own, without anyone asking.
const STANDING_RULES: &[(&str, &[&str])] = &[
    ("monitor", &["save", "set_enabled", "delete"]),
    ("scheduler", &["create", "update", "remove"]),
    ("workflow", &["save", "delete"]),
    ("email.rules", &["add", "remove"]),
];

const IDENTITY_WRITERS: &[&str] = &["filesystem.write", "filesystem.edit"];

const IDENTITY_FILES: &[&str] = &[
    "SOUL.md",
    "AGENTS.md",
    "IDENTITY.md",
    "PERSONALITY.md",
    "GOALS.md",
];

/// What a turn's guard knows about the turn.
#[derive(Debug, Clone, Default)]
pub struct ContentScope {
    /// Where the turn delivers: a write anywhere else is somewhere new.
    pub expected: Vec<String>,
    /// Tools the turn's operator named for it: their own choice, never held.
    pub named: Vec<String>,
    /// What armed the guard before the turn began, when outside content is
    /// what started it.
    pub armed: Option<String>,
    /// Refuse a held call rather than ask about it: no one is there to ask.
    pub refuses: bool,
}

impl ContentScope {
    /// The scope of an ordinary turn delivering to `channel`.
    pub fn delivering_to(channel: Option<&str>) -> Self {
        Self {
            expected: channel.into_iter().map(str::to_string).collect(),
            ..Self::default()
        }
    }
}

/// A call the guard holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub signal: AnomalySignal,
    pub refused: bool,
    pub reason: String,
}

/// One turn's guard: armed once outside content enters, never disarmed.
#[derive(Debug, Default)]
pub struct ContentGuard {
    scope: ContentScope,
    armed: Mutex<Option<String>>,
    stops: Mutex<Vec<AnomalySignal>>,
}

impl ContentGuard {
    pub fn new(scope: ContentScope) -> Self {
        Self {
            armed: Mutex::new(scope.armed.clone()),
            scope,
            stops: Mutex::new(Vec::new()),
        }
    }

    /// Outside content from `what` entered the turn. The first arming names
    /// the turn's outside content; a later one changes nothing.
    pub fn on_untrusted_content(&self, what: &str) {
        let mut armed = self.armed.lock().unwrap_or_else(|e| e.into_inner());
        if armed.is_none() {
            *armed = Some(what.to_string());
        }
    }

    /// Arm when `text` carries a fence of outside content from `what`.
    pub fn arm_if_fenced(&self, text: &str, what: &str) {
        if text.contains(FENCE_MARK) {
            self.on_untrusted_content(what);
        }
    }

    pub fn armed(&self) -> Option<String> {
        self.armed.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// What the guard refused this turn.
    pub fn stops(&self) -> Vec<AnomalySignal> {
        self.stops.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Whether a call of `tool` with `input`, doing `effect`, is held.
    pub fn check(&self, tool: &str, input: &Value, effect: Option<&SurfaceEffect>) -> Option<Held> {
        let armed = self.armed()?;
        let signal = assess(tool, input, effect, &self.scope)?;
        let refused = self.scope.refuses;
        warn!(
            target: "sentinel.monitor",
            tool = %tool,
            signal = %signal.description(),
            refused,
            "Call held after outside content"
        );
        if refused {
            self.stops
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(signal.clone());
        }
        let reason = format!(
            "{}, and this turn is processing outside content ({armed}) that may be steering it",
            signal.description()
        );
        Some(Held {
            signal,
            refused,
            reason,
        })
    }
}

/// The signal a call of `tool` raises in `scope`, if any. Pure.
pub fn assess(
    tool: &str,
    input: &Value,
    effect: Option<&SurfaceEffect>,
    scope: &ContentScope,
) -> Option<AnomalySignal> {
    if scope.named.iter().any(|entry| names(entry, tool)) {
        return None;
    }
    let owned = || tool.to_string();
    let action = || input["action"].as_str().unwrap_or_default().trim();
    if tool.starts_with(CREDENTIAL_NAMESPACE) {
        return Some(AnomalySignal::CredentialAccess { tool: owned() });
    }
    if tool.strip_prefix(SYSTEM_NAMESPACE).is_some_and(|rest| {
        !(rest.starts_with("list_") || rest.ends_with("_status") || SYSTEM_READS.contains(&rest))
    }) || POLICY_WRITES.contains(&tool)
    {
        return Some(AnomalySignal::PrivilegeEscalation { tool: owned() });
    }
    if let Some((_, writes)) = STANDING_RULES.iter().find(|(name, _)| *name == tool) {
        return writes
            .contains(&action())
            .then(|| AnomalySignal::RuleCreation {
                tool: owned(),
                action: action().to_string(),
            });
    }
    if IDENTITY_WRITERS.contains(&tool) {
        let path = input["path"].as_str().unwrap_or_default();
        if let Some(file) = IDENTITY_FILES.iter().find(|file| path.ends_with(*file)) {
            return Some(AnomalySignal::IdentityTampering {
                tool: owned(),
                file: file.to_string(),
            });
        }
    }
    sends_somewhere_new(tool, effect, scope)
        .map(|to| AnomalySignal::Exfiltration { tool: owned(), to })
}

/// Where a call sends that its turn doesn't deliver, if it does. A Desk
/// write is the person's own conversation, and a write naming no
/// destination goes to the turn's own.
fn sends_somewhere_new(
    tool: &str,
    effect: Option<&SurfaceEffect>,
    scope: &ContentScope,
) -> Option<String> {
    match effect {
        Some(SurfaceEffect::Writes { platform, .. })
            if platform == workforce_common::channels::desk::DESK_PLATFORM =>
        {
            None
        }
        Some(effect @ SurfaceEffect::Writes { .. }) => effect
            .destination()
            .filter(|to| !scope.expected.iter().any(|place| delivers_to(place, to)))
            .map(|to| format!("{} {}", to.platform, to.target)),
        Some(SurfaceEffect::Reads) => None,
        None => crate::runner::is_outbound_delivery_tool(tool).then(|| "outside".to_string()),
    }
}

/// Whether a write to `to` lands at `place`: the same channel, its thread,
/// or it in a named workspace.
fn delivers_to(place: &str, to: &Destination) -> bool {
    Destination::of(&to.platform, place).is_some_and(|place| {
        to.target == place.target
            || to
                .target
                .strip_prefix(place.target.as_str())
                .is_some_and(|rest| rest.starts_with(' '))
    })
}

/// Whether `entry` names `tool`: exactly, or as a namespace it lies under.
fn names(entry: &str, tool: &str) -> bool {
    tool == entry
        || tool
            .strip_prefix(entry)
            .is_some_and(|rest| rest.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn writes(platform: &str, destination: &str) -> SurfaceEffect {
        SurfaceEffect::Writes {
            platform: platform.into(),
            destination: destination.into(),
        }
    }

    fn armed(scope: ContentScope) -> ContentGuard {
        let guard = ContentGuard::new(scope);
        guard.on_untrusted_content("email from x@evil.example");
        guard
    }

    fn delivering(channel: &str) -> ContentScope {
        ContentScope::delivering_to(Some(channel))
    }

    #[test]
    fn nothing_is_held_before_outside_content_enters() {
        let guard = ContentGuard::new(delivering("C1"));
        assert_eq!(guard.check("vault.get", &json!({}), None), None);
        assert_eq!(
            guard.check("messaging.send", &json!({}), Some(&writes("slack", "C9"))),
            None
        );
    }

    #[test]
    fn a_fence_arms_and_plain_text_does_not() {
        let guard = ContentGuard::new(delivering("C1"));
        guard.arm_if_fenced(
            "<<<END MESSAGE CONTENT>>> ‹‹‹EXTERNAL_UNTRUSTED_CONTENT_x",
            "page",
        );
        assert_eq!(guard.armed(), None);
        guard.arm_if_fenced("<<<EXTERNAL_UNTRUSTED_CONTENT_00ff>>>\nbody", "email.read");
        assert_eq!(guard.armed().as_deref(), Some("email.read"));
        guard.arm_if_fenced("<<<EXTERNAL_UNTRUSTED_CONTENT_11ee>>>\nbody", "web.fetch");
        assert_eq!(guard.armed().as_deref(), Some("email.read"));
    }

    #[test]
    fn sending_somewhere_new_is_held_and_the_turns_own_delivery_is_not() {
        let guard = armed(delivering("C1"));
        let held = guard
            .check("messaging.send", &json!({}), Some(&writes("slack", "C9")))
            .unwrap();
        assert_eq!(
            held.signal,
            AnomalySignal::Exfiltration {
                tool: "messaging.send".into(),
                to: "slack c9".into()
            }
        );
        assert!(!held.refused);
        assert!(held.reason.contains("email from x@evil.example"));
        for own in ["C1", "#c1", "C1 › thread 1.5", "C1 in T1", ""] {
            assert_eq!(
                guard.check("messaging.send", &json!({}), Some(&writes("slack", own))),
                None,
                "{own}"
            );
        }
        assert!(guard
            .check("messaging.send", &json!({}), Some(&writes("slack", "C10")))
            .is_some());
        assert_eq!(
            guard.check("messaging.send", &json!({}), Some(&writes("desk", "#ops"))),
            None
        );
    }

    #[test]
    fn forwarding_mail_to_a_stranger_is_held() {
        let guard = armed(delivering("C1"));
        let input = json!({ "to": "x@evil.example", "uid": 7 });
        let held = guard
            .check("email.forward", &input, Some(&SurfaceEffect::email(&input)))
            .unwrap();
        assert!(
            matches!(held.signal, AnomalySignal::Exfiltration { ref to, .. } if to == "email x@evil.example")
        );
        assert!(guard
            .check("browser.upload_file", &json!({}), None)
            .is_some());
    }

    #[test]
    fn credentials_setup_and_identity_are_held() {
        let guard = armed(delivering("C1"));
        let signal = |tool: &str, input: Value| guard.check(tool, &input, None).map(|h| h.signal);
        assert_eq!(
            signal("vault.get", json!({ "key": "k" })),
            Some(AnomalySignal::CredentialAccess {
                tool: "vault.get".into()
            })
        );
        assert!(matches!(
            signal("system.enable_skill", json!({})),
            Some(AnomalySignal::PrivilegeEscalation { .. })
        ));
        assert!(matches!(
            signal("policy.propose", json!({})),
            Some(AnomalySignal::PrivilegeEscalation { .. })
        ));
        for read in ["system.list_skills", "system.version", "system.llm_status"] {
            assert_eq!(signal(read, json!({})), None, "{read}");
        }
        assert_eq!(
            signal("filesystem.write", json!({ "path": "/ws/SOUL.md" })),
            Some(AnomalySignal::IdentityTampering {
                tool: "filesystem.write".into(),
                file: "SOUL.md".into()
            })
        );
        assert_eq!(
            signal("filesystem.write", json!({ "path": "/ws/notes.md" })),
            None
        );
    }

    #[test]
    fn standing_automation_is_held_by_action() {
        let guard = armed(delivering("C1"));
        let signal = |tool: &str, action: &str| {
            guard
                .check(tool, &json!({ "action": action }), None)
                .map(|h| h.signal)
        };
        assert_eq!(
            signal("email.rules", "add"),
            Some(AnomalySignal::RuleCreation {
                tool: "email.rules".into(),
                action: "add".into()
            })
        );
        assert!(signal("monitor", "save").is_some());
        assert!(signal("scheduler", "create").is_some());
        assert!(signal("workflow", "save").is_some());
        for (tool, read) in [
            ("monitor", "list"),
            ("scheduler", "history"),
            ("email.rules", "list"),
            ("workflow", "status"),
        ] {
            assert_eq!(signal(tool, read), None, "{tool} {read}");
        }
    }

    #[test]
    fn named_tools_are_the_operators_choice() {
        let guard = armed(ContentScope {
            named: vec!["email".into(), "vault.get".into()],
            ..delivering("C1")
        });
        let input = json!({ "to": "someone@else.example" });
        assert_eq!(
            guard.check("email.send", &input, Some(&SurfaceEffect::email(&input))),
            None
        );
        assert_eq!(guard.check("vault.get", &json!({}), None), None);
        assert!(guard.check("vault.set", &json!({}), None).is_some());
    }

    #[test]
    fn a_refusing_scope_refuses_and_keeps_what_it_refused() {
        let guard = ContentGuard::new(ContentScope {
            armed: Some("monitored email".into()),
            refuses: true,
            ..delivering("C1")
        });
        let held = guard.check("vault.list", &json!({}), None).unwrap();
        assert!(held.refused);
        assert_eq!(guard.stops(), vec![held.signal]);
        let quiet = armed(delivering("C1"));
        quiet.check("vault.list", &json!({}), None);
        assert!(quiet.stops().is_empty());
    }
}
