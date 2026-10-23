//! Primitives shared by the webhook receiver and the HTTP trigger handler:
//! vault-secret resolution, constant-time secret comparison, HMAC-SHA256
//! verification, and dot-path payload matching. Keeping them here means the
//! two trigger paths share one implementation rather than duplicating it.

use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use workforce_common::settings::SecretStore;
use workforce_skills::adapters::credential_resolver::resolve_vault_refs;

use crate::serve::AgentService;

type HmacSha256 = Hmac<Sha256>;

/// Vault namespace for secrets referenced from workflow trigger definitions.
/// A `[[vault:<key>]]` marker resolves to `workflows/<key>`, mirroring the
/// per-skill prefixes used by skill credential fields.
pub const WORKFLOW_VAULT_PREFIX: &str = "workflows/";

/// Vaults that may hold workflow secrets, most-likely first: the shared global
/// vault (where the REST API writes) then the agent vault, de-duplicated. A
/// secret is accepted from whichever vault holds it so operators need not know
/// which backs a given write path.
pub fn trigger_secret_stores(service: &AgentService) -> Vec<Arc<dyn SecretStore>> {
    let mut stores: Vec<Arc<dyn SecretStore>> = Vec::new();
    for vault in [service.global_vault.clone(), service.vault.clone()]
        .into_iter()
        .flatten()
    {
        let store = vault as Arc<dyn SecretStore>;
        if !stores.iter().any(|existing| Arc::ptr_eq(existing, &store)) {
            stores.push(store);
        }
    }
    stores
}

/// Resolve `marker` (a `[[vault:<key>]]` reference) against each store in turn
/// and return whether any resolved cleartext satisfies `predicate`. Fails
/// closed: an empty store list, an unresolvable marker, or a failing predicate
/// all yield `false`. The predicate carries the actual comparison so this works
/// for both signature verification and constant-time secret equality.
pub fn any_vault_secret_satisfies(
    stores: &[Arc<dyn SecretStore>],
    marker: &str,
    predicate: impl Fn(&str) -> bool,
) -> bool {
    stores.iter().any(|store| {
        resolve_vault_refs(marker, store, WORKFLOW_VAULT_PREFIX)
            .is_ok_and(|secret| predicate(&secret))
    })
}

/// Constant-time equality of two secrets.
pub fn secrets_match(presented: &str, expected: &str) -> bool {
    presented.as_bytes().ct_eq(expected.as_bytes()).unwrap_u8() == 1
}

/// Constant-time check of an `sha256=<hex>` HMAC-SHA256 header over `body`.
pub fn hmac_sha256_header_matches(secret: &str, body: &[u8], sig_header: &str) -> bool {
    sig_header
        .strip_prefix("sha256=")
        .is_some_and(|expected_hex| hmac_sha256_hex_matches(secret, body, expected_hex))
}

/// Constant-time check of a bare-hex HMAC-SHA256 of `body` under `secret`.
pub fn hmac_sha256_hex_matches(secret: &str, body: &[u8], expected_hex: &str) -> bool {
    hex::decode(expected_hex).is_ok_and(|expected| hmac_sha256_matches(secret, body, &expected))
}

/// Constant-time check of a standard-base64 HMAC-SHA256 of `message` under
/// `secret`.
pub fn hmac_sha256_base64_matches(secret: &str, message: &[u8], expected_b64: &str) -> bool {
    BASE64
        .decode(expected_b64)
        .is_ok_and(|expected| hmac_sha256_matches(secret, message, &expected))
}

fn hmac_sha256_matches(secret: &str, message: &[u8], expected: &[u8]) -> bool {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(message);
    mac.verify_slice(expected).is_ok()
}

/// Whether every dot-path rule matches against `payload` (AND logic). An empty
/// rule set always matches.
pub fn match_rules_pass(
    payload: &serde_json::Value,
    rules: &std::collections::HashMap<String, String>,
) -> bool {
    rules.iter().all(|(path, expected)| {
        resolve_json_path(payload, path)
            .iter()
            .any(|v| v.as_str() == Some(expected.as_str()))
    })
}

/// Resolve a dot-separated path into the JSON leaf values it addresses.
///
/// Returns all leaves found. If an intermediate value is an array, the path
/// fans out into each element — so `"data.labels.name"` against
/// `{"data":{"labels":[{"name":"bug"},{"name":"feature"}]}}` returns both
/// `"bug"` and `"feature"`.
pub fn resolve_json_path<'a>(
    value: &'a serde_json::Value,
    path: &str,
) -> Vec<&'a serde_json::Value> {
    let mut current = vec![value];
    for segment in path.split('.') {
        let mut next = Vec::new();
        for val in &current {
            match val {
                serde_json::Value::Object(map) => next.extend(map.get(segment)),
                serde_json::Value::Array(arr) => next.extend(
                    arr.iter()
                        .filter_map(|item| item.as_object().and_then(|m| m.get(segment))),
                ),
                _ => {}
            }
        }
        current = next;
        if current.is_empty() {
            break;
        }
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn signature_hex(secret: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn secrets_match_is_exact() {
        assert!(secrets_match("topsecret", "topsecret"));
        assert!(!secrets_match("topsecret", "topsecre"));
        assert!(!secrets_match("topsecret", "Topsecret"));
        assert!(!secrets_match("", "x"));
    }

    #[test]
    fn hmac_header_matches_valid_and_rejects_invalid() {
        let body = b"the-body";
        let header = format!("sha256={}", signature_hex("sec", body));
        assert!(hmac_sha256_header_matches("sec", body, &header));
        assert!(!hmac_sha256_header_matches("wrong", body, &header));
        // Missing the `sha256=` prefix fails closed.
        assert!(!hmac_sha256_header_matches(
            "sec",
            body,
            &signature_hex("sec", body)
        ));
        // Non-hex fails closed.
        assert!(!hmac_sha256_hex_matches("sec", body, "zzzz"));
    }

    #[test]
    fn hmac_base64_matches_valid_and_rejects_invalid() {
        let message = b"POSThttps://hub.example.com/webhooks/hubspot[]1700000000000";
        let mut mac = HmacSha256::new_from_slice(b"sec").unwrap();
        mac.update(message);
        let expected = BASE64.encode(mac.finalize().into_bytes());
        assert!(hmac_sha256_base64_matches("sec", message, &expected));
        assert!(!hmac_sha256_base64_matches("wrong", message, &expected));
        assert!(!hmac_sha256_base64_matches("sec", b"other", &expected));
        // Non-base64 fails closed.
        assert!(!hmac_sha256_base64_matches("sec", message, "%%%"));
    }

    #[test]
    fn match_rules_simple_and_array_fanout() {
        let payload = serde_json::json!({
            "action": "deploy",
            "data": {"labels": [{"name": "prod"}, {"name": "urgent"}]}
        });
        let mut rules = HashMap::new();
        rules.insert("action".to_string(), "deploy".to_string());
        rules.insert("data.labels.name".to_string(), "urgent".to_string());
        assert!(match_rules_pass(&payload, &rules));

        rules.insert("action".to_string(), "rollback".to_string());
        assert!(!match_rules_pass(&payload, &rules));
    }

    #[test]
    fn empty_rules_always_match() {
        assert!(match_rules_pass(&serde_json::json!({}), &HashMap::new()));
    }

    #[test]
    fn resolve_path_object_and_array_and_missing() {
        let val = serde_json::json!({
            "a": {"b": {"c": "found"}},
            "items": [{"name": "x"}, {"name": "y"}]
        });
        assert_eq!(resolve_json_path(&val, "a.b.c")[0].as_str(), Some("found"));
        let names: Vec<&str> = resolve_json_path(&val, "items.name")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(names, vec!["x", "y"]);
        assert!(resolve_json_path(&val, "a.b.missing").is_empty());
    }
}
