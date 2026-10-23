//! End-to-end roundtrip across the secret tokenization pipeline:
//!
//!   user paste → ingress tokenizer → marker in transcript →
//!   tool boundary substitution → cleartext at the secret-sink tool
//!
//! Uses the real production [`InMemorySecretCache`] installed as the
//! global cache, so this test exercises the same wiring the hub uses
//! at runtime.

use std::sync::Arc;

use workforce_agent::security::secret_sinks::substitute_secret_markers;
use workforce_common::redact::tokenize_secrets;
use workforce_common::secret::Secret;
use workforce_common::secret_marker::{
    clear_global_cache, parse_markers, set_global_cache, MarkerId, SecretCache, SecretSource,
};
use workforce_hub::secret_cache::InMemorySecretCache;

/// Serialise tests that touch the global cache slot.
static GLOBAL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn install_fresh_cache() -> Arc<InMemorySecretCache> {
    clear_global_cache();
    let cache = Arc::new(InMemorySecretCache::with_defaults());
    set_global_cache(Arc::clone(&cache) as Arc<dyn SecretCache>);
    cache
}

fn slack_source() -> SecretSource {
    SecretSource::Slack {
        workspace_id: "T1ABC".into(),
        channel: "C1XYZ".into(),
        message_ts: "1700000000.000001".into(),
        user: "U1USER".into(),
    }
}

#[test]
fn roundtrip_paste_to_vault_set() {
    let _g = GLOBAL_LOCK.lock().unwrap();
    let _cache = install_fresh_cache();

    // 1. User pastes a real-shaped Claude plan OAuth token in Slack.
    let real_secret = concat!("sk-ant", "-oat01-abcdefghijklmnopqrstuvwxyz1234567890");
    let user_msg = format!("save this for me: {real_secret}");

    // 2. Ingress tokenizer captures it and rewrites the message.
    let (tokenized, captured) = tokenize_secrets(&user_msg, "sess-1", slack_source());
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].label, "ANTHROPIC_OAUTH_TOKEN");
    assert!(!tokenized.contains(real_secret));
    let markers = parse_markers(&tokenized);
    assert_eq!(markers.len(), 1);

    // 3. The LLM (modelled here) decides to call vault.set with the
    //    marker as the value. Build that tool input.
    let marker_text = markers[0].render();
    let llm_tool_input = serde_json::json!({
        "path": "skills.anthropic.token",
        "value": marker_text,
    });

    // 4. The tool-boundary substituter resolves the marker back to
    //    cleartext just before the tool fires.
    let result = substitute_secret_markers("vault.set", llm_tool_input, "sess-1");
    assert!(result.unknown.is_empty());
    assert_eq!(result.consumed.len(), 1);
    assert_eq!(result.input["value"].as_str().unwrap(), real_secret);
    assert_eq!(
        result.input["path"].as_str().unwrap(),
        "skills.anthropic.token"
    );

    clear_global_cache();
}

#[test]
fn roundtrip_blocks_unknown_marker() {
    let _g = GLOBAL_LOCK.lock().unwrap();
    let _cache = install_fresh_cache();

    // The LLM hallucinates a marker (or the cache TTL'd it out before
    // the tool call) — substitution should fail-closed.
    let bogus_marker =
        "[[secret:ffffffffffffffffffffffffffffffff type=ANTHROPIC_KEY preview=sk-an...XXXX]]";
    let llm_tool_input = serde_json::json!({
        "path": "skills.anthropic.token",
        "value": bogus_marker,
    });

    let result = substitute_secret_markers("vault.set", llm_tool_input, "sess-1");
    assert!(result.consumed.is_empty());
    assert_eq!(result.unknown.len(), 1);
    // The marker text is left intact in the result, so a downstream
    // caller that ignores `unknown` can still see the marker rather
    // than silently smuggle it into the vault.
    assert!(result.input["value"].as_str().unwrap().contains("ffffff"));

    clear_global_cache();
}

#[test]
fn roundtrip_isolates_sessions() {
    let _g = GLOBAL_LOCK.lock().unwrap();
    let _cache = install_fresh_cache();

    let real_secret = concat!("sk-ant", "-oat01-abcdefghijklmnopqrstuvwxyz1234567890");
    let (tokenized_a, captured_a) =
        tokenize_secrets(&format!("paste: {real_secret}"), "sess-A", slack_source());
    assert_eq!(captured_a.len(), 1);
    let marker_text = parse_markers(&tokenized_a)[0].render();

    // Try to consume from a different session.
    let llm_tool_input = serde_json::json!({
        "path": "skills.anthropic.token",
        "value": marker_text,
    });
    let result = substitute_secret_markers("vault.set", llm_tool_input, "sess-B");
    assert!(result.consumed.is_empty());
    assert_eq!(result.unknown.len(), 1);

    clear_global_cache();
}

#[test]
fn roundtrip_named_secret_set_via_slash_command() {
    let _g = GLOBAL_LOCK.lock().unwrap();
    let cache = install_fresh_cache();

    // 1. User runs `/secret datadog_api_key <value>` in a channel. The
    //    slash session key carries the `:slash` suffix.
    let real_secret = "feedface000000000000000000000001";
    let id = MarkerId::named("datadog_api_key").unwrap();
    cache.capture_named(
        "slack:T1ABC:C1XYZ:slash",
        &id,
        Secret::new(real_secret.to_string()),
        SecretSource::SlashCommand {
            user: "U1USER".into(),
        },
    );

    // 2. Later, in a thread, the agent references the placeholder when
    //    persisting the credential. The thread session key differs from
    //    the slash key but shares the channel scope.
    let llm_tool_input = serde_json::json!({
        "path": "skills.datadog.api_key",
        "value": "[[secret:datadog_api_key]]",
    });
    let result = substitute_secret_markers(
        "vault.set",
        llm_tool_input,
        "slack:T1ABC:C1XYZ:1700000000.000099",
    );
    assert!(result.unknown.is_empty());
    assert_eq!(result.consumed.len(), 1);
    assert_eq!(result.input["value"].as_str().unwrap(), real_secret);

    // 3. A different channel cannot resolve the name.
    let other = substitute_secret_markers(
        "vault.set",
        serde_json::json!({ "value": "[[secret:datadog_api_key]]" }),
        "slack:T1ABC:C9OTHER",
    );
    assert_eq!(other.unknown.len(), 1);
    assert!(other.consumed.is_empty());

    clear_global_cache();
}

#[test]
fn roundtrip_metadata_field_does_not_substitute() {
    let _g = GLOBAL_LOCK.lock().unwrap();
    let _cache = install_fresh_cache();

    let real_secret = concat!("sk-ant", "-oat01-abcdefghijklmnopqrstuvwxyz1234567890");
    let (tokenized, _) = tokenize_secrets(real_secret, "sess-1", slack_source());
    let marker_text = parse_markers(&tokenized)[0].render();

    // LLM tries to embed the marker in `path`, not `value`. The
    // allowlist only substitutes declared fields — this leaves the
    // marker in place and audit-logs nothing was consumed.
    let llm_tool_input = serde_json::json!({
        "path": format!("skills.{}.token", marker_text),
        "value": "literal-not-a-secret",
    });
    let result = substitute_secret_markers("vault.set", llm_tool_input, "sess-1");
    assert_eq!(result.consumed.len(), 0);
    assert!(result.input["path"].as_str().unwrap().contains("[[secret:"));
    assert_eq!(
        result.input["value"].as_str().unwrap(),
        "literal-not-a-secret"
    );

    clear_global_cache();
}
