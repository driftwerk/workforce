//! Shared `[[vault:<key>]]` resolver for credential fields.
//!
//! Unattended flows (scheduled jobs, workflows) don't have a Slack session,
//! so the `[[secret:<name>]]` marker cache used by interactive paths is
//! empty. To let those flows pull a credential into a credential field
//! without exposing it to the LLM, the agent emits a `[[vault:<key>]]`
//! marker which this resolver dereferences against the per-agent vault at
//! call time.
//!
//! Each caller chooses its own `vault_prefix` (e.g. `"browser_creds/"`,
//! `"obscura_creds/"`, `"http/endpoints/<name>/creds/"`) so credentials are
//! scoped to the skill or endpoint that earned them. Missing keys,
//! path-traversal attempts, and malformed markers all fail-closed — the call
//! returns one identical error so the agent gains no oracle for vault
//! membership; specifics go to operator tracing.
//!
//! Suitable only for credential-value fields (passwords, tokens), never for
//! selectors / paths / other metadata.

use std::sync::{Arc, LazyLock};

use eyre::{eyre, Result};
use regex::Regex;
use serde_json::Value;
use tracing::warn;
use workforce_common::settings::SecretStore;

pub const GENERIC_RESOLUTION_ERROR: &str =
    "vault marker resolution failed — see operator logs for details";

static VAULT_MARKER_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[vault:([^\[\]\s]+)\]\]").expect("vault marker regex"));

static SAFE_KEY_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_.\-/]+$").expect("safe key regex"));

/// Resolve every `[[vault:<key>]]` marker in `text` to the cleartext stored
/// at `<vault_prefix><key>` in `store`. Returns the input unchanged when no
/// markers are present. Every failure mode returns the same generic error.
pub fn resolve_vault_refs(
    text: &str,
    store: &Arc<dyn SecretStore>,
    vault_prefix: &str,
) -> Result<String> {
    if !text.contains("[[vault:") {
        return Ok(text.to_string());
    }
    let mut out = text.to_string();
    for caps in VAULT_MARKER_REGEX.captures_iter(text) {
        let whole = caps.get(0).expect("regex match").as_str();
        let key = caps.get(1).expect("regex group").as_str();
        let value = lookup_vault_key(key, store, vault_prefix)?;
        out = out.replace(whole, &value);
    }
    Ok(out)
}

/// Every `[[vault:<key>]]` marker in `text`, as (byte range, key).
///
/// For a caller that splices more than one marker grammar in a single pass
/// over the original string — see [`crate::security::credential_refs`].
pub(crate) fn vault_refs_spanned(text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    if !text.contains("[[vault:") {
        return Vec::new();
    }
    VAULT_MARKER_REGEX
        .captures_iter(text)
        .map(|caps| {
            (
                caps.get(0).expect("regex match").range(),
                caps.get(1).expect("regex group").as_str().to_string(),
            )
        })
        .collect()
}

/// Dereference one vault key under `vault_prefix`. Malformed keys and misses
/// share [`GENERIC_RESOLUTION_ERROR`] so the agent gains no membership oracle.
pub(crate) fn lookup_vault_key(
    key: &str,
    store: &Arc<dyn SecretStore>,
    vault_prefix: &str,
) -> Result<String> {
    if !is_safe_key(key) {
        warn!(
            target: "sentinel.audit",
            key = %key,
            vault_prefix = %vault_prefix,
            "vault marker rejected: malformed key"
        );
        return Err(eyre!("{GENERIC_RESOLUTION_ERROR}"));
    }
    let path = format!("{vault_prefix}{key}");
    store.get(&path).ok_or_else(|| {
        warn!(
            target: "sentinel.audit",
            path = %path,
            "vault marker rejected: entry not found"
        );
        eyre!("{GENERIC_RESOLUTION_ERROR}")
    })
}

/// Whether `text` contains at least one `[[vault:…]]` marker. Lets a caller
/// that forbids vault references (an ad-hoc request to an agent-chosen host)
/// reject them instead of transmitting the marker text verbatim.
pub fn contains_vault_ref(value: &Value) -> bool {
    match value {
        Value::String(s) => s.contains("[[vault:"),
        Value::Array(items) => items.iter().any(contains_vault_ref),
        Value::Object(map) => map.values().any(contains_vault_ref),
        _ => false,
    }
}

/// Resolve every `[[vault:<key>]]` marker in the string leaves of `value`.
///
/// Object keys are copied verbatim: a marker is only ever a credential
/// *value*, never a field name. Substitution is textual, so a leaf of exactly
/// `"[[vault:port]]"` yields the JSON string `"8080"` rather than the number —
/// inferring a type from a secret's contents would make the request shape
/// depend on the secret.
pub fn resolve_vault_refs_in_value(
    value: &Value,
    store: &Arc<dyn SecretStore>,
    vault_prefix: &str,
) -> Result<Value> {
    match value {
        Value::String(s) => resolve_vault_refs(s, store, vault_prefix).map(Value::String),
        Value::Array(items) => items
            .iter()
            .map(|item| resolve_vault_refs_in_value(item, store, vault_prefix))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        Value::Object(map) => map
            .iter()
            .map(|(key, item)| {
                resolve_vault_refs_in_value(item, store, vault_prefix)
                    .map(|resolved| (key.clone(), resolved))
            })
            .collect::<Result<serde_json::Map<_, _>>>()
            .map(Value::Object),
        other => Ok(other.clone()),
    }
}

fn is_safe_key(key: &str) -> bool {
    if key.is_empty()
        || key.starts_with('/')
        || key.starts_with('.')
        || key.ends_with('/')
        || key.contains("..")
        || key.contains("//")
    {
        return false;
    }
    SAFE_KEY_REGEX.is_match(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct StubStore(Mutex<HashMap<String, String>>);

    impl StubStore {
        fn with(entries: &[(&str, &str)]) -> Arc<dyn SecretStore> {
            let store = StubStore::default();
            for (k, v) in entries {
                store
                    .0
                    .lock()
                    .unwrap()
                    .insert((*k).to_string(), (*v).to_string());
            }
            Arc::new(store)
        }
    }

    impl SecretStore for StubStore {
        fn get(&self, path: &str) -> Option<String> {
            self.0.lock().unwrap().get(path).cloned()
        }
        fn set(&self, _: &str, _: &str) -> eyre::Result<()> {
            unreachable!("tests do not write")
        }
        fn delete(&self, _: &str) -> eyre::Result<bool> {
            unreachable!("tests do not delete")
        }
        fn list(&self, _: Option<&str>) -> Vec<String> {
            Vec::new()
        }
    }

    #[test]
    fn passthrough_when_no_marker() {
        let store = StubStore::with(&[]);
        let out = resolve_vault_refs("ghp_literal_value", &store, "browser_creds/").unwrap();
        assert_eq!(out, "ghp_literal_value");
    }

    #[test]
    fn resolves_single_marker() {
        let store = StubStore::with(&[("browser_creds/staging.pw", "secret123")]);
        let out = resolve_vault_refs("[[vault:staging.pw]]", &store, "browser_creds/").unwrap();
        assert_eq!(out, "secret123");
    }

    #[test]
    fn resolves_multiple_markers_in_one_string() {
        let store = StubStore::with(&[
            ("browser_creds/user", "alice"),
            ("browser_creds/host", "example.com"),
        ]);
        let out =
            resolve_vault_refs("[[vault:user]]@[[vault:host]]", &store, "browser_creds/").unwrap();
        assert_eq!(out, "alice@example.com");
    }

    #[test]
    fn prefix_is_per_skill() {
        let store = StubStore::with(&[
            ("browser_creds/pw", "browser-pw"),
            ("obscura_creds/pw", "obscura-pw"),
        ]);
        let browser_out = resolve_vault_refs("[[vault:pw]]", &store, "browser_creds/").unwrap();
        let obscura_out = resolve_vault_refs("[[vault:pw]]", &store, "obscura_creds/").unwrap();
        assert_eq!(browser_out, "browser-pw");
        assert_eq!(obscura_out, "obscura-pw");
    }

    #[test]
    fn rejects_traversal_with_dotdot() {
        let store = StubStore::with(&[("browser_creds/x", "y")]);
        let err = resolve_vault_refs("[[vault:../skills/admin/key]]", &store, "browser_creds/")
            .expect_err("expected error");
        assert_eq!(format!("{err}"), GENERIC_RESOLUTION_ERROR);
    }

    #[test]
    fn rejects_leading_slash() {
        let store = StubStore::with(&[]);
        let err = resolve_vault_refs("[[vault:/abs]]", &store, "browser_creds/")
            .expect_err("expected error");
        assert_eq!(format!("{err}"), GENERIC_RESOLUTION_ERROR);
    }

    #[test]
    fn rejects_leading_dot() {
        let store = StubStore::with(&[]);
        let err = resolve_vault_refs("[[vault:.hidden]]", &store, "browser_creds/")
            .expect_err("expected error");
        assert_eq!(format!("{err}"), GENERIC_RESOLUTION_ERROR);
    }

    #[test]
    fn rejects_double_slash() {
        let store = StubStore::with(&[]);
        let err = resolve_vault_refs("[[vault:a//b]]", &store, "browser_creds/")
            .expect_err("expected error");
        assert_eq!(format!("{err}"), GENERIC_RESOLUTION_ERROR);
    }

    #[test]
    fn fails_closed_on_missing_key_without_leaking_membership() {
        let store = StubStore::with(&[]);
        let err = resolve_vault_refs("[[vault:nope]]", &store, "browser_creds/")
            .expect_err("expected error");
        let msg = format!("{err}");
        assert_eq!(msg, GENERIC_RESOLUTION_ERROR);
        assert!(!msg.contains("nope"));
        assert!(!msg.contains("browser_creds"));
    }

    #[test]
    fn missing_and_malformed_return_identical_error_messages() {
        let store = StubStore::with(&[]);
        let malformed = format!(
            "{}",
            resolve_vault_refs("[[vault:../x]]", &store, "browser_creds/").expect_err("malformed")
        );
        let missing = format!(
            "{}",
            resolve_vault_refs("[[vault:nope]]", &store, "browser_creds/").expect_err("missing")
        );
        assert_eq!(malformed, missing);
    }

    #[test]
    fn rejects_percent_encoded_traversal() {
        let store = StubStore::with(&[]);
        let err = resolve_vault_refs("[[vault:%2e%2e/x]]", &store, "browser_creds/")
            .expect_err("expected error");
        assert_eq!(format!("{err}"), GENERIC_RESOLUTION_ERROR);
    }

    #[test]
    fn composes_with_already_substituted_secret_markers() {
        let store = StubStore::with(&[("browser_creds/host", "example.com")]);
        let post_chokepoint = "user=alice host=[[vault:host]] pw=secretpw";
        let out = resolve_vault_refs(post_chokepoint, &store, "browser_creds/").unwrap();
        assert_eq!(out, "user=alice host=example.com pw=secretpw");
    }

    #[test]
    fn marker_without_brackets_text_is_unchanged() {
        let store = StubStore::with(&[("browser_creds/x", "y")]);
        let out = resolve_vault_refs("vault:x not a marker", &store, "browser_creds/").unwrap();
        assert_eq!(out, "vault:x not a marker");
    }

    #[test]
    fn safe_key_predicates() {
        for k in ["staging.pw", "a/b/c", "USER-1", "snake_case", "x.y-z_1"] {
            assert!(is_safe_key(k), "should accept: {k}");
        }
        for k in [
            "",
            "../x",
            "/abs",
            ".dot",
            "x/",
            "a//b",
            "with space",
            "with\nnewline",
            "semi;colon",
        ] {
            assert!(!is_safe_key(k), "should reject: {k}");
        }
    }
}
