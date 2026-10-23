//! Per-tool / per-field substitution registry for secret-sink tools.
//!
//! Secrets pasted by the user are replaced at ingress with opaque
//! markers (`[[secret:<id> type=<L> preview=<P>]]`) and the cleartext
//! is stashed in the per-session cache. The LLM only ever sees the
//! marker. To complete the round-trip — so the agent can actually
//! *store* a credential the user pasted — an explicit allowlist of
//! "secret-sink" tools and the specific fields within each get the
//! marker substituted back to cleartext just before the tool fires.
//!
//! Architectural rules:
//!  * Allowlist is **per (tool, field)**, not per tool. Substitution
//!    only fires for fields whose semantics are "the credential
//!    value"; metadata fields (paths, account names) never receive
//!    substitution even on a secret-sink tool.
//!  * The list lives in one place ([`SUBSTITUTION_REGISTRY`]) so a
//!    security review only has one file to read.
//!  * Substitution is gated by an existing entry in the per-session
//!    cache. An unknown marker id (LLM hallucinated, expired by TTL,
//!    or pasted in a different session) returns an error rather than
//!    silently passing the marker through to the tool — avoids storing
//!    literal `[[secret:…]]` text in the vault.
//!
//! One tool resolves markers itself rather than through this registry:
//! `http.request`, in [`crate::security::credential_refs`]. Two reasons it
//! cannot use the registry. Its credential fields are `headers`, `query`, and
//! `body` — objects and arbitrary JSON, where the registry substitutes flat
//! top-level strings only. And it must *know* a credential landed on the
//! request, because one that did may not follow a redirect; substituting
//! upstream hands the tool indistinguishable cleartext. A reviewer reading for
//! "what can turn a marker into cleartext" must read both files.

use serde_json::Value;
use workforce_common::secret_marker::{global_cache, parse_markers, ConsumedSecret, MarkerId};

/// Static registry of `(tool_name, &[field_name, …])`. A field name of
/// `*` is reserved (not currently used) for "any string under this
/// path"; for v1 every entry is a flat top-level field name.
const SUBSTITUTION_REGISTRY: &[(&str, &[&str])] = &[
    ("vault.set", &["value"]),
    ("system.add_account", &["api_key"]),
    ("claude_code.setup_token", &["token"]),
    (
        "slack.connect",
        &["bot_token", "app_token", "signing_secret"],
    ),
    (
        "slack.app_configure",
        &["bot_token", "app_token", "signing_secret"],
    ),
    ("browser.fill", &["value"]),
    ("browser.type", &["text"]),
    ("obscura.fill", &["value"]),
    ("obscura.type", &["text"]),
];

fn allowed_fields(tool_name: &str) -> Option<&'static [&'static str]> {
    SUBSTITUTION_REGISTRY
        .iter()
        .find(|(name, _)| *name == tool_name)
        .map(|(_, fields)| *fields)
}

/// Result of a substitution pass.
#[derive(Debug, Default, Clone)]
pub struct SubstitutionResult {
    /// The (possibly modified) input. When no fields were eligible or
    /// no markers were present, this is byte-identical to the input.
    pub input: Value,
    /// The PRE-substitution input (markers intact), present whenever any
    /// marker was resolved to cleartext. This is the ONLY form that may be
    /// recorded, streamed, or persisted (session transcripts, `tool_data`,
    /// progress events) — `input` then contains a live secret and must go
    /// nowhere except the tool call itself.
    pub original: Option<Value>,
    /// Markers that were resolved to cleartext and substituted in.
    pub consumed: Vec<ConsumedSecret>,
    /// Markers the LLM emitted that weren't found in the cache —
    /// expired, hallucinated, or pasted in another session.
    pub unknown: Vec<MarkerId>,
}

impl SubstitutionResult {
    /// Render the [`Self::unknown`] list as a single user-facing reason
    /// string, suitable for tool failure output.
    pub fn unknown_reason(&self) -> String {
        unknown_marker_reason(&self.unknown)
    }
}

/// One wording for "these markers could not be resolved", shared by every
/// resolver so the user gets the same instructions wherever it happens.
pub fn unknown_marker_reason(unknown: &[MarkerId]) -> String {
    let ids: Vec<String> = unknown.iter().map(|i| i.to_string()).collect();
    format!(
        "secret marker(s) {} not found — a pasted credential may have expired, or a named secret may have been set with `cmd: secret` in a different channel. Ask the user to re-paste the credential or re-set it with `cmd: secret <name> <value>` (the thread-safe form; not the `/secret` slash command).",
        ids.join(", ")
    )
}

/// Walk the tool input and substitute markers in declared fields for
/// the given tool. Pure for non-sink tools (returns the input
/// unchanged with empty `consumed`/`unknown`). For sink tools, mutates
/// only the declared string fields; nested objects/arrays are left
/// alone (sinks have flat schemas).
pub fn substitute_secret_markers(
    tool_name: &str,
    input: Value,
    session_key: &str,
) -> SubstitutionResult {
    let Some(fields) = allowed_fields(tool_name) else {
        return SubstitutionResult {
            input,
            ..Default::default()
        };
    };

    if !input.is_object() {
        // Sink tools always take JSON objects; pass non-objects
        // through unchanged so the tool's own validator surfaces the
        // shape error rather than us silently dropping the payload.
        return SubstitutionResult {
            input,
            ..Default::default()
        };
    }
    let Value::Object(mut map) = input else {
        unreachable!("checked is_object above");
    };

    let cache = match global_cache() {
        Some(c) => c,
        None => {
            return SubstitutionResult {
                input: Value::Object(map),
                ..Default::default()
            }
        }
    };

    let mut consumed = Vec::new();
    let mut unknown = Vec::new();
    let mut original: Option<Value> = None;

    for field in fields {
        let Some(Value::String(field_str)) = map.get(*field).cloned() else {
            continue;
        };
        let markers = parse_markers(&field_str);
        if markers.is_empty() {
            continue;
        }
        // Snapshot the marker-form input before the first mutation: this is
        // the only shape that may be recorded anywhere.
        if original.is_none() {
            original = Some(Value::Object(map.clone()));
        }

        let mut rewritten = field_str;
        for marker in markers {
            match cache.lookup(session_key, &marker.id) {
                Some((cleartext, meta)) => {
                    rewritten =
                        rewritten.replace(&marker.render(), cleartext.expose_secret().as_str());
                    tracing::info!(
                        target: "sentinel.audit",
                        tool = %tool_name,
                        field = %field,
                        session_key = %session_key,
                        marker_id = %meta.id,
                        label = %meta.label,
                        preview = %meta.preview,
                        "secret marker substituted at tool boundary"
                    );
                    consumed.push(meta);
                }
                None => {
                    tracing::warn!(
                        target: "sentinel.audit",
                        tool = %tool_name,
                        field = %field,
                        session_key = %session_key,
                        marker_id = %marker.id,
                        "secret marker referenced but not in cache — rejecting tool call"
                    );
                    unknown.push(marker.id);
                }
            }
        }
        map.insert(field.to_string(), Value::String(rewritten));
    }

    SubstitutionResult {
        input: Value::Object(map),
        // Only meaningful when cleartext actually landed in `input`.
        original: if consumed.is_empty() { None } else { original },
        consumed,
        unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use workforce_common::secret::Secret;
    use workforce_common::secret_marker::{
        clear_global_cache, set_global_cache, Marker, MarkerId, SecretCache, SecretSource,
    };

    /// Test cache backed by a flat HashMap. Mirrors the production
    /// trait surface; sufficient for substitution-layer tests.
    #[derive(Default)]
    struct StubCache {
        entries: StdMutex<Vec<(String, MarkerId, String, String)>>,
    }
    impl StubCache {
        fn store(&self, session: &str, label: &str, value: &str) -> MarkerId {
            let id = MarkerId::fresh();
            self.entries.lock().unwrap().push((
                session.to_string(),
                id.clone(),
                label.to_string(),
                value.to_string(),
            ));
            id
        }
    }
    impl SecretCache for StubCache {
        fn capture(&self, _: &str, _: Secret<String>, _: &str, _: SecretSource) -> MarkerId {
            unreachable!("substitution tests don't capture")
        }
        fn capture_named(&self, _: &str, _: &MarkerId, _: Secret<String>, _: SecretSource) {
            unreachable!("substitution tests don't capture")
        }
        fn list_named(&self, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn lookup(
            &self,
            session_key: &str,
            id: &MarkerId,
        ) -> Option<(Secret<String>, ConsumedSecret)> {
            let entries = self.entries.lock().unwrap();
            entries
                .iter()
                .find(|(s, i, _, _)| s == session_key && i == id)
                .map(|(_, i, label, val)| {
                    let preview = workforce_common::secret_marker::preview_of(val);
                    (
                        Secret::new(val.clone()),
                        ConsumedSecret {
                            id: i.clone(),
                            label: label.clone(),
                            preview,
                            source: SecretSource::Slack {
                                workspace_id: "T1".into(),
                                channel: "C1".into(),
                                message_ts: "1.0".into(),
                                user: "U1".into(),
                            },
                        },
                    )
                })
        }
        fn purge_session(&self, _: &str) {}
        fn forget(&self, _: &str, _: &MarkerId) {}
    }

    use crate::security::test_support::SECRET_CACHE_TEST_LOCK as TEST_LOCK;

    fn marker_for(id: &MarkerId, label: &str, preview: &str) -> String {
        Marker {
            id: id.clone(),
            label: label.into(),
            preview: preview.into(),
        }
        .render()
    }

    #[test]
    fn vault_set_value_field_substitutes_marker() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        let cache = Arc::new(StubCache::default());
        let id = cache.store("sess-1", "GITHUB_PAT", "ghp_realtokenvalue123456789");
        set_global_cache(cache.clone() as Arc<dyn SecretCache>);

        let input = serde_json::json!({
            "path": "skills.github.token",
            "value": marker_for(&id, "GITHUB_PAT", "ghp_...6789"),
        });
        let result = substitute_secret_markers("vault.set", input, "sess-1");
        assert_eq!(
            result.input["value"].as_str().unwrap(),
            "ghp_realtokenvalue123456789"
        );
        assert_eq!(
            result.input["path"].as_str().unwrap(),
            "skills.github.token"
        );
        assert_eq!(result.consumed.len(), 1);
        assert!(result.unknown.is_empty());

        // The pre-substitution form is returned for recording: markers
        // intact, NO cleartext anywhere in it. This is the only shape that
        // may reach transcripts/tool_data.
        let original = result.original.expect("original present when consumed");
        let original_str = original.to_string();
        assert!(
            !original_str.contains("ghp_realtokenvalue123456789"),
            "recorded form must not contain cleartext: {original_str}"
        );
        assert!(
            original["value"].as_str().unwrap().contains("[[secret:"),
            "recorded form keeps the marker"
        );

        clear_global_cache();
    }

    #[test]
    fn no_original_returned_when_nothing_was_substituted() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        let input = serde_json::json!({ "path": "a.b", "value": "plain, no markers" });
        let result = substitute_secret_markers("vault.set", input, "sess-1");
        assert!(result.original.is_none());
        assert!(result.consumed.is_empty());
        clear_global_cache();
    }

    #[test]
    fn claude_code_setup_token_token_field_substitutes_marker() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        let cache = Arc::new(StubCache::default());
        let id = cache.store("sess-1", "CLAUDE_CODE_OAUTH", "sk-ant-oat-fakevalue");
        set_global_cache(cache.clone() as Arc<dyn SecretCache>);

        let input = serde_json::json!({
            "token": marker_for(&id, "CLAUDE_CODE_OAUTH", "sk-ant-oat-…value"),
            "scope": "user",
        });
        let result = substitute_secret_markers("claude_code.setup_token", input, "sess-1");
        assert_eq!(
            result.input["token"].as_str().unwrap(),
            "sk-ant-oat-fakevalue"
        );
        assert_eq!(result.input["scope"].as_str().unwrap(), "user");
        assert_eq!(result.consumed.len(), 1);
        assert!(result.unknown.is_empty());

        clear_global_cache();
    }

    #[test]
    fn unknown_marker_returns_error_does_not_substitute() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        let cache = Arc::new(StubCache::default());
        set_global_cache(cache as Arc<dyn SecretCache>);

        let bogus_id = MarkerId::fresh();
        let bogus = marker_for(&bogus_id, "GITHUB_PAT", "ghp_...xxxx");
        let input = serde_json::json!({
            "path": "skills.github.token",
            "value": bogus.clone(),
        });
        let result = substitute_secret_markers("vault.set", input, "sess-1");
        // Marker text remains untouched — we don't smuggle markers
        // into the vault on a miss.
        assert_eq!(result.input["value"].as_str().unwrap(), bogus);
        assert!(result.consumed.is_empty());
        assert_eq!(result.unknown, vec![bogus_id]);
        assert!(result.unknown_reason().contains("not found"));

        clear_global_cache();
    }

    #[test]
    fn metadata_field_on_sink_tool_is_not_substituted() {
        // `path` is not in the allowlist for vault.set — even if the
        // LLM tries to embed a marker there, it stays as-is.
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        let cache = Arc::new(StubCache::default());
        let id = cache.store("sess-1", "GITHUB_PAT", "ghp_realtokenvalue123456789");
        set_global_cache(cache.clone() as Arc<dyn SecretCache>);

        let marker = marker_for(&id, "GITHUB_PAT", "ghp_...6789");
        let input = serde_json::json!({
            "path": format!("skills.{}", marker),
            "value": "some-other-value",
        });
        let result = substitute_secret_markers("vault.set", input, "sess-1");
        assert!(result.input["path"].as_str().unwrap().contains("[[secret:"));
        assert_eq!(result.consumed.len(), 0);

        clear_global_cache();
    }

    #[test]
    fn non_sink_tool_returns_input_unchanged() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        let cache = Arc::new(StubCache::default());
        let id = cache.store("sess-1", "GITHUB_PAT", "ghp_realtokenvalue123456789");
        set_global_cache(cache as Arc<dyn SecretCache>);

        let marker = marker_for(&id, "GITHUB_PAT", "ghp_...6789");
        let input = serde_json::json!({ "url": "https://example.com", "body": marker.clone() });
        let result = substitute_secret_markers("http.get", input, "sess-1");
        assert_eq!(result.input["body"].as_str().unwrap(), marker);
        assert!(result.consumed.is_empty());
        assert!(result.unknown.is_empty());

        clear_global_cache();
    }

    #[test]
    fn slack_connect_substitutes_all_three_token_fields() {
        let _g = TEST_LOCK.lock().unwrap();
        for tool in ["slack.connect", "slack.app_configure"] {
            clear_global_cache();
            let cache = Arc::new(StubCache::default());
            let bot_id = cache.store(
                "sess-1",
                "SLACK_TOKEN",
                concat!("xoxb-r", "eal-bot-1234567890"),
            );
            let app_id = cache.store(
                "sess-1",
                "SLACK_APP_TOKEN",
                concat!("xapp-1", "-real-AAAA-BBBB"),
            );
            let sig_id = cache.store("sess-1", "ENV_CREDENTIAL", "abcdef0123456789abcdef01");
            set_global_cache(cache.clone() as Arc<dyn SecretCache>);

            let input = serde_json::json!({
                "workspace_id": "default",
                "bot_token": marker_for(&bot_id, "SLACK_TOKEN", "xoxb...7890"),
                "app_token": marker_for(&app_id, "SLACK_APP_TOKEN", "xapp...BBBB"),
                "signing_secret": marker_for(&sig_id, "ENV_CREDENTIAL", "abcd...ef01"),
            });
            let result = substitute_secret_markers(tool, input, "sess-1");
            assert_eq!(
                result.input["bot_token"].as_str().unwrap(),
                concat!("xoxb-r", "eal-bot-1234567890")
            );
            assert_eq!(
                result.input["app_token"].as_str().unwrap(),
                concat!("xapp-1", "-real-AAAA-BBBB")
            );
            assert_eq!(
                result.input["signing_secret"].as_str().unwrap(),
                "abcdef0123456789abcdef01"
            );
            assert_eq!(result.consumed.len(), 3, "{tool}");
        }
        clear_global_cache();
    }

    #[test]
    fn substitution_is_session_scoped() {
        // A marker captured in session A is NOT substitutable when the
        // tool runs in session B's task context.
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        let cache = Arc::new(StubCache::default());
        let id = cache.store("sess-A", "GITHUB_PAT", "ghp_realtokenvalue123456789");
        set_global_cache(cache as Arc<dyn SecretCache>);

        let marker = marker_for(&id, "GITHUB_PAT", "ghp_...6789");
        let input = serde_json::json!({
            "path": "skills.github.token",
            "value": marker.clone(),
        });
        let result = substitute_secret_markers("vault.set", input, "sess-B");
        assert_eq!(result.input["value"].as_str().unwrap(), marker);
        assert_eq!(result.unknown.len(), 1);
        assert!(result.consumed.is_empty());

        clear_global_cache();
    }

    #[test]
    fn browser_fill_value_field_substitutes_marker() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        let cache = Arc::new(StubCache::default());
        let id = cache.store("sess-1", "PASSWORD", "stg_secret_pw_12345");
        set_global_cache(cache.clone() as Arc<dyn SecretCache>);

        let input = serde_json::json!({
            "selector": "#password",
            "value": marker_for(&id, "PASSWORD", "stg_...2345"),
        });
        let result = substitute_secret_markers("browser.fill", input, "sess-1");
        assert_eq!(
            result.input["value"].as_str().unwrap(),
            "stg_secret_pw_12345"
        );
        assert_eq!(result.input["selector"].as_str().unwrap(), "#password");
        assert_eq!(result.consumed.len(), 1);
        assert!(result.unknown.is_empty());

        clear_global_cache();
    }

    #[test]
    fn browser_type_text_field_substitutes_marker() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        let cache = Arc::new(StubCache::default());
        let id = cache.store("sess-1", "PASSWORD", "another_pw_value_67890");
        set_global_cache(cache.clone() as Arc<dyn SecretCache>);

        let input = serde_json::json!({
            "selector": "[contenteditable]",
            "text": marker_for(&id, "PASSWORD", "anot...7890"),
        });
        let result = substitute_secret_markers("browser.type", input, "sess-1");
        assert_eq!(
            result.input["text"].as_str().unwrap(),
            "another_pw_value_67890"
        );
        assert_eq!(result.consumed.len(), 1);
        assert!(result.unknown.is_empty());

        clear_global_cache();
    }

    #[test]
    fn browser_fill_selector_is_never_substituted() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        let cache = Arc::new(StubCache::default());
        let id = cache.store("sess-1", "PASSWORD", "shouldnotleak");
        set_global_cache(cache.clone() as Arc<dyn SecretCache>);

        let marker = marker_for(&id, "PASSWORD", "shou...leak");
        let input = serde_json::json!({
            "selector": marker.clone(),
            "value": "literal",
        });
        let result = substitute_secret_markers("browser.fill", input, "sess-1");
        assert_eq!(result.input["selector"].as_str().unwrap(), marker);
        assert_eq!(result.input["value"].as_str().unwrap(), "literal");
        assert!(result.consumed.is_empty());

        clear_global_cache();
    }

    #[test]
    fn empty_input_object_passes_through() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        let cache = Arc::new(StubCache::default());
        set_global_cache(cache as Arc<dyn SecretCache>);

        let input = serde_json::json!({});
        let result = substitute_secret_markers("vault.set", input, "sess-1");
        assert_eq!(result.input, serde_json::json!({}));
        assert!(result.consumed.is_empty());
        assert!(result.unknown.is_empty());

        clear_global_cache();
    }
}
