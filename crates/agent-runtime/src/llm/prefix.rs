//! What a round's cached prompt prefix reduces to, so the runner can tell when
//! the prefix moved between two rounds of one turn — and whether the move was
//! a load the turn asked for — and what a replayed history reduces to, so the
//! hub can tell when a turn's history stopped extending the previous turn's
//! — and what a stable system prompt reduces to, for the same check on the
//! system prefix.

use std::collections::BTreeMap;
use std::fmt;
use std::hash::{DefaultHasher, Hash, Hasher};

use serde::{Deserialize, Serialize};

use super::types::{ContentBlock, LlmMessage, LlmToolDef, Role};

/// The cached prefix a round will send, reduced to what can move it: the
/// system prompt and each declared tool definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrefixFingerprint {
    system: u64,
    tools: BTreeMap<String, u64>,
}

/// What moved between two rounds.
#[derive(Debug)]
pub(crate) struct PrefixDrift {
    pub system: bool,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

impl PrefixFingerprint {
    pub(crate) fn of(system_prompt: &str, tools: &[LlmToolDef]) -> Self {
        Self {
            system: digest(system_prompt.as_bytes()),
            tools: tools
                .iter()
                .map(|tool| {
                    (
                        tool.name.clone(),
                        digest(&serde_json::to_vec(tool).unwrap_or_default()),
                    )
                })
                .collect(),
        }
    }

    /// What changed from `self` to `next`, or `None` when nothing did.
    pub(crate) fn drift(&self, next: &Self) -> Option<PrefixDrift> {
        let names = |from: &Self, to: &Self| -> Vec<String> {
            to.tools
                .keys()
                .filter(|name| !from.tools.contains_key(*name))
                .cloned()
                .collect()
        };
        (self != next).then(|| PrefixDrift {
            system: self.system != next.system,
            added: names(self, next),
            removed: names(next, self),
            changed: self
                .tools
                .iter()
                .filter(|(name, digest)| next.tools.get(*name).is_some_and(|d| d != *digest))
                .map(|(name, _)| name.clone())
                .collect(),
        })
    }
}

impl PrefixDrift {
    /// Added definitions alone are a load; anything else re-bills the prefix
    /// for no reason the turn asked for.
    pub(crate) fn is_growth(&self) -> bool {
        !self.system && self.removed.is_empty() && self.changed.is_empty()
    }
}

impl fmt::Display for PrefixDrift {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "system={} added={:?} removed={:?} changed={:?}",
            self.system, self.added, self.removed, self.changed
        )
    }
}

/// A replayed history reduced to its wire byte length and digest, so the next
/// turn can tell whether its history extends this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayPrefix {
    pub len: usize,
    pub digest: u64,
}

impl ReplayPrefix {
    pub fn of(messages: &[LlmMessage]) -> Self {
        let bytes = wire_bytes(messages);
        Self {
            len: bytes.len(),
            digest: digest(&bytes),
        }
    }

    /// `true` when `messages` begin with the bytes this prefix was taken from.
    pub fn is_extended_by(&self, messages: &[LlmMessage]) -> bool {
        let bytes = wire_bytes(messages);
        bytes.len() >= self.len && digest(&bytes[..self.len]) == self.digest
    }
}

/// A stable system prompt reduced to its byte length and digest, so the next
/// turn can tell whether its system prefix is the previous turn's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemPrefix {
    pub len: usize,
    pub digest: u64,
}

impl SystemPrefix {
    pub fn of(stable: &str) -> Self {
        Self {
            len: stable.len(),
            digest: digest(stable.as_bytes()),
        }
    }
}

/// The bytes a history contributes to the cached prefix: a role tag, then each
/// block's text or serialised form. A merged text block only appends, so an
/// extension here is an extension on the wire.
fn wire_bytes(messages: &[LlmMessage]) -> Vec<u8> {
    messages
        .iter()
        .flat_map(|m| {
            let role = match m.role {
                Role::User => b'u',
                Role::Assistant => b'a',
            };
            std::iter::once(role).chain(m.content.iter().flat_map(|block| match block {
                ContentBlock::Text { text } => text.as_bytes().to_vec(),
                other => serde_json::to_vec(other).unwrap_or_default(),
            }))
        })
        .collect()
}

fn digest(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::{ContentBlock, Role};

    fn tool(name: &str, schema: serde_json::Value) -> LlmToolDef {
        LlmToolDef {
            name: name.to_string(),
            description: "test".to_string(),
            input_schema: schema,
        }
    }

    fn user(text: &str) -> LlmMessage {
        LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    fn tools(names: &[&str]) -> Vec<LlmToolDef> {
        names
            .iter()
            .map(|name| tool(name, serde_json::json!({})))
            .collect()
    }

    #[test]
    fn identical_rounds_report_no_drift() {
        let first = PrefixFingerprint::of("system", &tools(&["a", "b"]));
        let second = PrefixFingerprint::of("system", &tools(&["a", "b"]));
        assert!(first.drift(&second).is_none());
    }

    #[test]
    fn a_definition_byte_change_with_the_same_names_is_not_growth() {
        let before = PrefixFingerprint::of(
            "system",
            &[tool("memory", serde_json::json!({"executions": 0}))],
        );
        let after = PrefixFingerprint::of(
            "system",
            &[tool("memory", serde_json::json!({"executions": 1}))],
        );
        let drift = before.drift(&after).expect("the bytes moved");
        assert_eq!(drift.changed, vec!["memory".to_string()]);
        assert!(!drift.system);
        assert!(drift.added.is_empty() && drift.removed.is_empty());
        assert!(!drift.is_growth());
    }

    #[test]
    fn a_loaded_tool_is_growth() {
        let before = PrefixFingerprint::of("system", &tools(&["memory", "tool_load"]));
        let after = PrefixFingerprint::of("system", &tools(&["memory", "tool_load", "vault_set"]));
        let drift = before.drift(&after).expect("a tool was loaded");
        assert_eq!(drift.added, vec!["vault_set".to_string()]);
        assert!(drift.is_growth());
    }

    #[test]
    fn a_dropped_tool_is_not_growth() {
        let before = PrefixFingerprint::of("system", &tools(&["memory", "vault_set"]));
        let after = PrefixFingerprint::of("system", &tools(&["memory"]));
        let drift = before.drift(&after).expect("a tool left");
        assert_eq!(drift.removed, vec!["vault_set".to_string()]);
        assert!(!drift.is_growth());
    }

    fn assistant(text: &str) -> LlmMessage {
        LlmMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    #[test]
    fn a_replay_extended_by_appended_messages_is_recognised() {
        let first = vec![user("hello"), assistant("hi")];
        let mut second = first.clone();
        second.push(user("[Tool Result: repo.grep]\nrows"));
        second.push(assistant("done"));
        assert!(ReplayPrefix::of(&first).is_extended_by(&second));
        assert!(ReplayPrefix::of(&first).is_extended_by(&first));
    }

    #[test]
    fn a_replay_whose_last_text_block_grew_is_still_an_extension() {
        let first = vec![user("hello"), assistant("hi")];
        let mut second = first.clone();
        match second.last_mut().map(|m| m.content.first_mut()) {
            Some(Some(ContentBlock::Text { text })) => text.push_str("\nmore"),
            _ => unreachable!("the fixture ends in a text block"),
        }
        assert!(ReplayPrefix::of(&first).is_extended_by(&second));
    }

    #[test]
    fn a_replay_with_an_earlier_message_rewritten_is_not_an_extension() {
        let first = vec![
            user("[Tool Result: repo.grep]\nrows"),
            assistant("hi"),
            user("next"),
        ];
        let mut second = first.clone();
        second[0] = user("[Tool Result: repo.grep — elided (4 chars)]");
        assert!(!ReplayPrefix::of(&first).is_extended_by(&second));
        assert!(
            !ReplayPrefix::of(&first).is_extended_by(&first[..2]),
            "a shorter history cannot extend a longer one"
        );
    }

    #[test]
    fn an_empty_replay_is_extended_by_anything() {
        let empty = ReplayPrefix::of(&[]);
        assert!(empty.is_extended_by(&[]));
        assert!(empty.is_extended_by(&[user("hello")]));
    }

    #[test]
    fn system_prefix_matches_only_the_same_bytes() {
        let stable = SystemPrefix::of("identity\nskills");
        assert_eq!(stable, SystemPrefix::of("identity\nskills"));
        assert_ne!(stable, SystemPrefix::of("identity\nskills\n"));
        assert_ne!(stable, SystemPrefix::of("identity\nskillz"));
        assert_eq!(stable.len, "identity\nskills".len());
    }

    #[test]
    fn a_system_prompt_change_is_reported() {
        let before = PrefixFingerprint::of("system v1", &tools(&["a"]));
        let after = PrefixFingerprint::of("system v2", &tools(&["a"]));
        let drift = before.drift(&after).expect("the system prompt moved");
        assert!(drift.system);
        assert!(drift.added.is_empty() && drift.changed.is_empty());
        assert!(!drift.is_growth());
    }
}
