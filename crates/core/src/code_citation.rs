//! Deterministic code citations: the exact source files and line ranges the
//! answer-ready `code_context` injector grounded an answer in.
//!
//! Unlike the model's inconsistent inline `file:line` prose, these are produced
//! by the assembler itself — independent of what the model writes — so grounding
//! is verifiable on every turn. They ride the chat progress stream as a single
//! terminal event and surface as clickable GitHub-permalink chips under the
//! answer in the UI.
//!
//! Lives in `workforce_core` because three crates must see the same shape: the
//! hub assembles them, `workforce-nats` carries them on `ChatProgress`, and
//! `workforce-gateway-client` deserialises them off `ServerMessage` — mirroring
//! how `policy::PermissionPromptOptions` is shared across the same edges.

use serde::{Deserialize, Serialize};

/// One source location the answer was grounded in: a file + inclusive line span
/// in an indexed repo, tagged with the symbol it covers and how it entered the
/// context closure.
///
/// `owner`/`repo_name`/`commit` are the GitHub permalink coordinates. When the
/// repo's source isn't GitHub (or its owner/repo can't be resolved) `owner` is
/// empty and the UI renders the citation as plain non-link text — the source
/// location is still shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeCitation {
    /// Display name of the repo the source came from (e.g. `workforce`).
    pub repo: String,
    /// GitHub owner/org for the permalink (empty when not a GitHub source).
    pub owner: String,
    /// GitHub repo name for the permalink (empty when not a GitHub source).
    pub repo_name: String,
    /// Commit the body was read at — pins the permalink to the exact revision.
    pub commit: String,
    /// Repo-relative file path (e.g. `crates/hub/src/vault.rs`).
    pub path: String,
    /// 1-indexed inclusive start line.
    pub start_line: usize,
    /// 1-indexed inclusive end line.
    pub end_line: usize,
    /// Entity kind label (e.g. `fn`, `struct`, `const`).
    pub kind: String,
    /// Qualified symbol name (e.g. `Vault::encrypt_blob`).
    pub symbol: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    // The citation round-trips through serde unchanged — it crosses three crate
    // boundaries (hub → NATS `ChatProgress` → gateway-client `ServerMessage` →
    // the UI), so a stable wire shape is load-bearing.
    #[test]
    fn code_citation_serde_round_trip() {
        let cite = CodeCitation {
            repo: "workforce".to_string(),
            owner: "driftwerk".to_string(),
            repo_name: "workforce".to_string(),
            commit: "abc123".to_string(),
            path: "crates/hub/src/vault.rs".to_string(),
            start_line: 377,
            end_line: 401,
            kind: "function".to_string(),
            symbol: "Vault::encrypt_blob".to_string(),
        };
        let json = serde_json::to_string(&cite).unwrap();
        let back: CodeCitation = serde_json::from_str(&json).unwrap();
        assert_eq!(cite, back);
        // The field names are the wire contract the TS client deserialises.
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        for key in [
            "repo",
            "owner",
            "repo_name",
            "commit",
            "path",
            "start_line",
            "end_line",
            "kind",
            "symbol",
        ] {
            assert!(value.get(key).is_some(), "missing wire field {key}");
        }
    }
}
