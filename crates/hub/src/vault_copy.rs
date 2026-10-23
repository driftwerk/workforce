//! Shared vault-copy engine.
//!
//! The credential fan-out logic behind both the
//! `POST /api/agents/{id}/vault/copy` HTTP handler ([`crate::gateway`]) and the
//! agent-callable `vault.copy` tool ([`crate::vault_tool`]). Pure with respect
//! to transport: callers layer their own authentication and error rendering on
//! top (the HTTP handler with `reject_json` + `require_named_admin`, the tool
//! with `ToolResult::failure` + an `is_admin` gate).
//!
//! Invariants preserved across both callers:
//! - the source credential is never moved or deleted — copy leaves it intact;
//! - the cleartext value never appears in the returned outcomes (only a
//!   per-user status: copied / exists / source / error);
//! - destination writes are attributed to the destination user
//!   ([`VaultActor::User`]) so the audit log names the right owner.

use workforce_common::settings::{SecretStore, VaultActor, VaultSource};

/// Per-destination outcome of a copy.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CopyStatus {
    /// The value was written to this user's per-user slot.
    Copied,
    /// The destination already had a value and `overwrite` was false.
    Exists,
    /// Destination equals the source (same tier + same user) — skipped.
    Source,
    /// The backend write failed; carries the error string.
    Error(String),
}

pub(crate) struct CopyOutcome {
    pub(crate) user_id: String,
    pub(crate) status: CopyStatus,
}

/// A fully-validated copy request. `path` is a bare logical path (no `users/`
/// prefix); per-user scoping is applied internally via the current-identity
/// task-local.
pub(crate) struct CopyRequest<'a> {
    pub(crate) path: &'a str,
    pub(crate) from_tier: VaultSource,
    pub(crate) to_tier: VaultSource,
    /// Source user — required when `from_tier` is per-user, ignored otherwise.
    pub(crate) from_user: Option<&'a str>,
    pub(crate) to_users: &'a [String],
    pub(crate) overwrite: bool,
}

/// Run `op` with `CURRENT_USER_ID` scoped to `user_id` when present, so a
/// per-user vault path resolves to `users/{uid}/<path>` for that user.
pub(crate) fn run_with_optional_identity<T>(user_id: Option<String>, op: impl FnOnce() -> T) -> T {
    match user_id {
        Some(uid) => workforce_agent::runner::CURRENT_USER_ID.sync_scope(uid, op),
        None => op(),
    }
}

/// Copy the source credential into each destination user's slot, leaving the
/// source intact. Returns `None` when no value exists at the source tier for
/// the source user.
pub(crate) fn copy_credential(
    store: &dyn SecretStore,
    req: &CopyRequest<'_>,
) -> Option<Vec<CopyOutcome>> {
    let value = read_source_value(store, req)?;
    Some(
        req.to_users
            .iter()
            .map(|uid| CopyOutcome {
                user_id: uid.clone(),
                status: copy_to_user(store, req, uid, &value),
            })
            .collect(),
    )
}

fn read_source_value(store: &dyn SecretStore, req: &CopyRequest<'_>) -> Option<String> {
    let scope = req
        .from_tier
        .is_per_user()
        .then_some(req.from_user)
        .flatten()
        .map(str::to_string);
    run_with_optional_identity(scope, || {
        store
            .inspect(req.path)
            .into_iter()
            .find(|(tier, _)| *tier == req.from_tier)
            .map(|(_, value)| value)
    })
}

fn copy_to_user(
    store: &dyn SecretStore,
    req: &CopyRequest<'_>,
    uid: &str,
    value: &str,
) -> CopyStatus {
    if req.from_tier == req.to_tier && req.from_user == Some(uid) {
        return CopyStatus::Source;
    }
    run_with_optional_identity(Some(uid.to_string()), || {
        let exists = store
            .inspect(req.path)
            .iter()
            .any(|(tier, _)| *tier == req.to_tier);
        if exists && !req.overwrite {
            return CopyStatus::Exists;
        }
        match store.set_at_tier(req.path, value, req.to_tier, VaultActor::User(uid)) {
            Ok(()) => CopyStatus::Copied,
            Err(e) => CopyStatus::Error(e.to_string()),
        }
    })
}

/// Drop empty and duplicate destination ids, preserving first-seen order.
pub(crate) fn dedup_destinations(ids: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    ids.iter()
        .filter(|id| !id.is_empty() && seen.insert(id.as_str()))
        .cloned()
        .collect()
}

/// Render a single outcome as the JSON object both callers return. Never
/// contains the cleartext value — status only.
pub(crate) fn copy_outcome_json(outcome: &CopyOutcome) -> serde_json::Value {
    match &outcome.status {
        CopyStatus::Copied => serde_json::json!({ "user_id": outcome.user_id, "status": "copied" }),
        CopyStatus::Exists => serde_json::json!({ "user_id": outcome.user_id, "status": "exists" }),
        CopyStatus::Source => serde_json::json!({ "user_id": outcome.user_id, "status": "source" }),
        CopyStatus::Error(msg) => {
            serde_json::json!({ "user_id": outcome.user_id, "status": "error", "error": msg })
        }
    }
}

/// Number of destinations that received the value (used for the summary
/// `copied` count both callers report).
pub(crate) fn copied_count(outcomes: &[CopyOutcome]) -> usize {
    outcomes
        .iter()
        .filter(|o| o.status == CopyStatus::Copied)
        .count()
}

/// Pure validation for a bare logical vault path. Returns `Some(msg)` when the
/// path is illegal — it carries a `users/` or `wasm-skills/` prefix, or a `..`
/// segment — and `None` when acceptable. The HTTP layer wraps the message in a
/// 400; the tool surfaces it as a `ToolResult::failure`.
pub(crate) fn logical_path_error(path: &str) -> Option<&'static str> {
    let illegal = path.starts_with("users/")
        || path.starts_with("wasm-skills/")
        || path.split('/').any(|seg| seg == "..");
    illegal.then_some(
        "path must be a bare logical path (no 'users/' prefix, '..' segment, or 'wasm-skills/' prefix)",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, crate::user_config::LayeredUserVault) {
        let dir = tempfile::tempdir().unwrap();
        let open = |name: &str| {
            std::sync::Arc::new(
                crate::vault::Vault::open(&dir.path().join(name), "test-key").unwrap(),
            )
        };
        let store =
            crate::user_config::LayeredUserVault::new(open("agent.enc"), Some(open("global.enc")));
        (dir, store)
    }

    fn seed(
        store: &dyn SecretStore,
        uid: Option<&str>,
        path: &str,
        value: &str,
        tier: VaultSource,
    ) {
        let actor = uid.map_or(VaultActor::RestOwner, VaultActor::User);
        run_with_optional_identity(uid.map(str::to_string), || {
            store.set_at_tier(path, value, tier, actor).unwrap()
        });
    }

    fn value_at(
        store: &dyn SecretStore,
        uid: Option<&str>,
        path: &str,
        tier: VaultSource,
    ) -> Option<String> {
        run_with_optional_identity(uid.map(str::to_string), || {
            store
                .inspect(path)
                .into_iter()
                .find(|(t, _)| *t == tier)
                .map(|(_, v)| v)
        })
    }

    fn req<'a>(
        path: &'a str,
        from_tier: VaultSource,
        to_tier: VaultSource,
        from_user: Option<&'a str>,
        to_users: &'a [String],
    ) -> CopyRequest<'a> {
        CopyRequest {
            path,
            from_tier,
            to_tier,
            from_user,
            to_users,
            overwrite: false,
        }
    }

    #[test]
    fn copies_to_each_user_and_preserves_source() {
        let (_dir, store) = store();
        seed(
            &store,
            Some("alice"),
            "skills/xero/secret",
            "S3CR3T",
            VaultSource::AgentPerUser,
        );
        let dests = vec!["bob".to_string(), "carol".to_string()];
        let outcomes = copy_credential(
            &store,
            &req(
                "skills/xero/secret",
                VaultSource::AgentPerUser,
                VaultSource::AgentPerUser,
                Some("alice"),
                &dests,
            ),
        )
        .unwrap();

        assert!(outcomes.iter().all(|o| o.status == CopyStatus::Copied));
        for uid in ["bob", "carol", "alice"] {
            assert_eq!(
                value_at(
                    &store,
                    Some(uid),
                    "skills/xero/secret",
                    VaultSource::AgentPerUser
                )
                .as_deref(),
                Some("S3CR3T"),
                "{uid} should resolve the copied value (source kept)"
            );
        }
    }

    #[test]
    fn overwrite_guard_protects_then_replaces() {
        let (_dir, store) = store();
        seed(&store, Some("alice"), "p", "NEW", VaultSource::AgentPerUser);
        seed(&store, Some("bob"), "p", "OLD", VaultSource::AgentPerUser);
        let dests = vec!["bob".to_string()];

        let guarded = copy_credential(
            &store,
            &req(
                "p",
                VaultSource::AgentPerUser,
                VaultSource::AgentPerUser,
                Some("alice"),
                &dests,
            ),
        )
        .unwrap();
        assert_eq!(guarded[0].status, CopyStatus::Exists);
        assert_eq!(
            value_at(&store, Some("bob"), "p", VaultSource::AgentPerUser).as_deref(),
            Some("OLD")
        );

        let forced = copy_credential(
            &store,
            &CopyRequest {
                overwrite: true,
                ..req(
                    "p",
                    VaultSource::AgentPerUser,
                    VaultSource::AgentPerUser,
                    Some("alice"),
                    &dests,
                )
            },
        )
        .unwrap();
        assert_eq!(forced[0].status, CopyStatus::Copied);
        assert_eq!(
            value_at(&store, Some("bob"), "p", VaultSource::AgentPerUser).as_deref(),
            Some("NEW")
        );
    }

    #[test]
    fn destination_equal_to_source_is_skipped() {
        let (_dir, store) = store();
        seed(&store, Some("alice"), "p", "V", VaultSource::AgentPerUser);
        let dests = vec!["alice".to_string()];
        let outcomes = copy_credential(
            &store,
            &req(
                "p",
                VaultSource::AgentPerUser,
                VaultSource::AgentPerUser,
                Some("alice"),
                &dests,
            ),
        )
        .unwrap();
        assert_eq!(outcomes[0].status, CopyStatus::Source);
    }

    #[test]
    fn missing_source_yields_none() {
        let (_dir, store) = store();
        let dests = vec!["bob".to_string()];
        assert!(copy_credential(
            &store,
            &req(
                "absent",
                VaultSource::AgentPerUser,
                VaultSource::AgentPerUser,
                Some("alice"),
                &dests
            ),
        )
        .is_none());
    }

    #[test]
    fn shared_source_fans_out_to_per_user() {
        let (_dir, store) = store();
        seed(&store, None, "p", "SHARED", VaultSource::AgentSpecific);
        let dests = vec!["bob".to_string()];
        let outcomes = copy_credential(
            &store,
            &req(
                "p",
                VaultSource::AgentSpecific,
                VaultSource::AgentPerUser,
                None,
                &dests,
            ),
        )
        .unwrap();
        assert_eq!(outcomes[0].status, CopyStatus::Copied);
        assert_eq!(
            value_at(&store, Some("bob"), "p", VaultSource::AgentPerUser).as_deref(),
            Some("SHARED")
        );
        assert_eq!(
            value_at(&store, None, "p", VaultSource::AgentSpecific).as_deref(),
            Some("SHARED")
        );
    }

    #[test]
    fn dedup_drops_repeats_and_blanks() {
        assert_eq!(
            dedup_destinations(&["b".into(), "b".into(), "c".into(), String::new()]),
            vec!["b".to_string(), "c".to_string()]
        );
        assert!(dedup_destinations(&[String::new(), String::new()]).is_empty());
    }

    #[test]
    fn logical_path_error_flags_unsafe_paths() {
        assert!(logical_path_error("skills/xero/secret").is_none());
        for bad in ["users/alice/p", "a/../b", "wasm-skills/x"] {
            assert!(
                logical_path_error(bad).is_some(),
                "{bad} should be rejected"
            );
        }
    }
}
