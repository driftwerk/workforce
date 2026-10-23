//! Combined resolution of the two credential-reference grammars a tool
//! argument may carry.
//!
//! * `[[secret:<name>]]` — a value the *user* stashed for this conversation
//!   (`cmd: secret <name> <value>`, or an ingress-tokenized paste). Lives in
//!   the per-session marker cache, expires on idle, and is scoped to the
//!   session it was set in. See [`crate::security::secret_sinks`].
//! * `[[vault:<key>]]` — a value the *operator* stored server-side, resolved
//!   under a caller-chosen prefix. See [`crate::security::vault_refs`].
//! * `[[vault:<scope>:<key>]]` — the scoped form a workflow `type: secret`
//!   variable substitutes, resolved against the run's declaration policy. See
//!   [`workforce_common::secret_policy`].
//!
//! The two are resolved together, in one splice pass over the original string,
//! so neither resolver ever re-scans the other's substituted cleartext: a
//! session secret whose value happens to contain the text `[[vault:…]]` cannot
//! dereference a vault entry, and a vault value containing `[[secret:…]]`
//! cannot pull a session secret.
//!
//! What may resolve turns on one question: who chose the destination host —
//! see [`Destination`]. Against a host an *operator* wrote down, both grammars
//! resolve. Against a host the *agent* named, `[[vault:…]]` is refused
//! outright, and `[[secret:…]]` resolves for everything except a credential the
//! platform scraped out of a tool response
//! ([`workforce_common::secret_marker::SecretSource::is_tool_scraped`]): that
//! one carries no user intent about where it goes, and the response it came
//! from could be the injection naming the recipient.
//!
//! Only for credential-*value* fields (headers, query values, body leaves) —
//! never for a field that addresses the request (`url`, `path`) or names a
//! resource. Object keys are copied verbatim for the same reason.
//!
//! Everything fails closed. An expired, hallucinated, or wrong-session secret
//! marker and a missing vault key both abort the call; the literal marker text
//! is never transmitted, which is the difference between a loud error and a
//! silent 401 against the remote service.

use std::sync::Arc;

use eyre::{eyre, Result};
use serde_json::Value;
use workforce_common::secret_marker::{global_cache, parse_markers_spanned, MarkerId};
use workforce_common::settings::SecretStore;

use super::secret_sinks::unknown_marker_reason;
use super::vault_refs::{lookup_vault_key, vault_refs_spanned};

/// Who chose the host this request is going to. The single input that decides
/// which references may resolve.
pub enum Destination {
    /// An operator wrote the host down (an `http.request` endpoint). Bare
    /// `[[vault:<key>]]` resolves under `creds_prefix`.
    Pinned { creds_prefix: String },
    /// The agent named the host. No operator-stored credential resolves here,
    /// and neither does a secret the platform captured out of a tool response.
    AgentChosen,
}

/// Where a resolver may look, and on whose behalf.
pub struct CredentialResolver<'a> {
    /// Tool name, for the audit record.
    pub tool: &'a str,
    /// Session whose secret cache `[[secret:…]]` resolves against. An empty
    /// key (unattended run, no conversation) resolves nothing, so every
    /// marker in such a run fails closed.
    pub session_key: &'a str,
    /// Vault every `[[vault:…]]` form reads from.
    pub store: &'a Arc<dyn SecretStore>,
    /// Host the request will be sent to, for the `allowed_hosts` check on a
    /// workflow-declared secret. `None` (an unparseable base URL) refuses any
    /// declaration that names hosts.
    pub host: Option<&'a str>,
    pub destination: Destination,
}

impl CredentialResolver<'_> {
    /// Resolve every reference in the string leaves of `value`.
    ///
    /// Returns the rewritten value and whether anything actually resolved —
    /// the caller needs that second value to decide whether the request now
    /// carries a credential (and so must not follow a redirect).
    pub fn resolve(&self, field: &str, value: &Value) -> Result<(Value, bool)> {
        let mut resolved = false;
        let out = self.walk(field, value, &mut resolved)?;
        Ok((out, resolved))
    }

    fn walk(&self, field: &str, value: &Value, resolved: &mut bool) -> Result<Value> {
        match value {
            Value::String(s) => self.resolve_leaf(field, s, resolved).map(Value::String),
            Value::Array(items) => items
                .iter()
                .map(|item| self.walk(field, item, resolved))
                .collect::<Result<Vec<_>>>()
                .map(Value::Array),
            Value::Object(map) => map
                .iter()
                .map(|(key, item)| self.walk(field, item, resolved).map(|v| (key.clone(), v)))
                .collect::<Result<serde_json::Map<_, _>>>()
                .map(Value::Object),
            // Numbers, booleans, and null carry no reference. Leaving them
            // alone is what lets one body mix plain values with credentials.
            other => Ok(other.clone()),
        }
    }

    /// Splice every reference of both grammars out of one string.
    ///
    /// Substitution is textual, so a leaf of exactly `"[[vault:port]]"` yields
    /// the JSON string `"8080"` rather than the number — inferring a type from
    /// a secret's contents would make the request shape depend on the secret.
    fn resolve_leaf(&self, field: &str, text: &str, resolved: &mut bool) -> Result<String> {
        let mut spans: Vec<(std::ops::Range<usize>, Replacement)> = Vec::new();
        for (range, marker) in parse_markers_spanned(text) {
            spans.push((range, Replacement::Secret(marker.id)));
        }
        for (range, key) in vault_refs_spanned(text) {
            spans.push((range, Replacement::Vault(key)));
        }
        spans.sort_by_key(|(range, _)| range.start);

        // A leaf that opens a reference the grammars then don't match is a
        // malformed marker — `[[secret:API_KEY]]`, `[[secret:my-email]]`,
        // an unclosed bracket. Returning it unchanged would put that text on
        // the wire and produce exactly the unexplained 401 this resolver
        // exists to stop, so it is rejected here rather than at the top of the
        // call: one bad leaf in an otherwise-resolvable body must still fail.
        if let Some(opener) = unmatched_opener(text, &spans) {
            return Err(eyre!(
                "`{opener}` in `{field}` is not a usable credential reference. Three forms \
                 resolve:\n\
                 • `[[secret:<32-hex-id>]]` — a credential the platform captured out of an \
                 earlier tool response, such as a token a login call returned. Copy the id from \
                 the marker as it appeared in that response. It resolves against a configured \
                 `endpoint`, never an ad-hoc `url`.\n\
                 • `[[secret:<name>]]` — a value the user set with `cmd: secret`, named in \
                 lowercase letters, digits and underscores (as `secret.list` reports it).\n\
                 • `[[vault:<key>]]` — a credential an operator stored under an endpoint.\n\
                 Fix the reference — do not send it as text, and do not ask the user to hand you \
                 a value you are already holding a marker for."
            ));
        }
        if spans.is_empty() {
            return Ok(text.to_string());
        }

        // Collected rather than returned at the first miss: naming every
        // unresolvable marker at once means the user re-sets them in one go.
        let mut unknown: Vec<MarkerId> = Vec::new();
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0usize;
        for (range, replacement) in spans {
            // The two grammars are textually disjoint, so this only trips if a
            // future grammar change lets them nest. Skip rather than corrupt.
            if range.start < cursor {
                continue;
            }
            out.push_str(&text[cursor..range.start]);
            match replacement {
                Replacement::Secret(id) => match self.lookup_secret(field, &id)? {
                    Some(cleartext) => out.push_str(&cleartext),
                    None => unknown.push(id),
                },
                Replacement::Vault(key) => {
                    out.push_str(&self.resolve_vault(&text[range.clone()], &key)?);
                }
            }
            cursor = range.end;
        }
        out.push_str(&text[cursor..]);

        if !unknown.is_empty() {
            return Err(eyre!("{}", unknown_marker_reason(&unknown)));
        }
        *resolved = true;
        Ok(out)
    }

    /// Dereference one `[[vault:…]]`, in whichever of its two forms it is.
    ///
    /// **Scoped** — `[[vault:<scope>:<key>]]`, what a workflow `type: secret`
    /// variable substitutes. Routed to the run's `SecretPolicy`, which requires
    /// the `(scope, key)` pair to be *declared* by the workflow and enforces
    /// that variable's `allowed_hosts` against this request's host. That
    /// declaration is the pin, so it needs no endpoint and works for an ad-hoc
    /// `url` too — the workflow author, not the agent, chose where it may go.
    /// Outside a workflow there is no policy to declare anything, so it fails.
    ///
    /// **Bare** — `[[vault:<key>]]`, an endpoint's own `creds/`. Needs an
    /// endpoint, which is its pin. Deliberately *not* routed through the
    /// policy, which refuses every undeclared marker: inside a workflow this
    /// form is already confined to one operator-written host, so the
    /// declaration gate would forbid a call that is contained by construction.
    fn resolve_vault(&self, token: &str, key: &str) -> Result<String> {
        use workforce_common::secret_policy::{resolve_secret_markers, SECRET_POLICY};
        use workforce_common::vault_scope::parse_exact_scoped_marker;

        if parse_exact_scoped_marker(token).is_some() {
            if SECRET_POLICY.try_with(|_| ()).is_err() {
                return Err(eyre!(
                    "`{token}` is a workflow secret variable and only resolves inside a workflow \
                     run that declares it. For an ad-hoc call, use `[[secret:<name>]]` (the user \
                     sets it with `cmd: secret`) or an endpoint's own `[[vault:<key>]]`."
                ));
            }
            return resolve_secret_markers(token, self.store, self.host);
        }

        let Destination::Pinned { creds_prefix } = &self.destination else {
            return Err(eyre!(
                "[[vault:…]] references need an `endpoint` — a stored credential is only ever \
                 sent to the endpoint it is configured under, never to an ad-hoc `url`"
            ));
        };
        lookup_vault_key(key, self.store, creds_prefix)
    }

    /// `Ok(None)` means "not in the cache" — collected as unknown. `Err` means
    /// the marker exists but may not go to this destination, which is a
    /// different answer and must not be reported as an expiry.
    fn lookup_secret(&self, field: &str, id: &MarkerId) -> Result<Option<String>> {
        let Some(cache) = global_cache() else {
            return Ok(None);
        };
        match cache.lookup(self.session_key, id) {
            Some((cleartext, meta)) => {
                // A value the platform scraped out of a tool response carries no
                // user intent about where it goes — and the response it came
                // from could be the injection naming the recipient. It resolves
                // only against a host an operator wrote down.
                if matches!(self.destination, Destination::AgentChosen)
                    && meta.source.is_tool_scraped()
                {
                    tracing::warn!(
                        target: "sentinel.audit",
                        tool = %self.tool,
                        field = %field,
                        session_key = %self.session_key,
                        marker_id = %meta.id,
                        label = %meta.label,
                        "tool-sourced secret refused for an agent-chosen host"
                    );
                    return Err(eyre!(
                        "`{}` holds a credential the platform captured from a tool response, not \
                         one the user handed you. It can only be sent to a configured `endpoint`, \
                         never to a `url` you chose. Call `http.endpoints`, or ask the user to \
                         re-supply the value with `cmd: secret <name> <value>` if this request is \
                         what they want.",
                        meta.id
                    ));
                }
                tracing::info!(
                    target: "sentinel.audit",
                    tool = %self.tool,
                    field = %field,
                    session_key = %self.session_key,
                    marker_id = %meta.id,
                    label = %meta.label,
                    preview = %meta.preview,
                    "secret marker resolved into an outbound request"
                );
                Ok(Some(cleartext.expose_secret().clone()))
            }
            None => {
                tracing::warn!(
                    target: "sentinel.audit",
                    tool = %self.tool,
                    field = %field,
                    session_key = %self.session_key,
                    marker_id = %id,
                    "secret marker referenced but not in cache — rejecting tool call"
                );
                Ok(None)
            }
        }
    }
}

enum Replacement {
    Secret(MarkerId),
    Vault(String),
}

/// The first `[[secret:` / `[[vault:` in `text` that is not part of a matched
/// reference, rendered as a short quotable excerpt.
///
/// An opener *inside* a matched span is not a miss — an auto marker's `preview`
/// field is a slice of a real credential and could contain anything.
fn unmatched_opener(text: &str, spans: &[(std::ops::Range<usize>, Replacement)]) -> Option<String> {
    /// Cap on the excerpt when the reference never closes.
    const EXCERPT_CHARS: usize = 40;

    ["[[secret:", "[[vault:"]
        .iter()
        .filter_map(|opener| {
            text.match_indices(opener)
                .map(|(at, _)| at)
                .find(|at| !spans.iter().any(|(range, _)| range.contains(at)))
        })
        .min()
        .map(|at| {
            let tail = &text[at..];
            match tail.find("]]") {
                Some(close) => tail[..close + 2].to_string(),
                None => tail.chars().take(EXCERPT_CHARS).collect(),
            }
        })
}

/// Whether `value` carries a reference of either grammar, well-formed or not.
///
/// Deliberately a substring test: a field this is used to guard (`url`,
/// `path`) addresses the request rather than authenticating it, so even a
/// malformed marker there is a mistake worth rejecting loudly instead of
/// putting on the wire as text.
pub fn contains_credential_ref(value: &Value) -> bool {
    match value {
        Value::String(s) => s.contains("[[secret:") || s.contains("[[vault:"),
        Value::Array(items) => items.iter().any(contains_credential_ref),
        Value::Object(map) => map.values().any(contains_credential_ref),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use workforce_common::secret_marker::{
        clear_global_cache, set_global_cache, SecretCache, SecretSource,
    };

    use crate::security::test_support::SECRET_CACHE_TEST_LOCK as TEST_LOCK;

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
        fn set(&self, _: &str, _: &str) -> Result<()> {
            unreachable!("tests do not write")
        }
        fn delete(&self, _: &str) -> Result<bool> {
            unreachable!("tests do not delete")
        }
        fn list(&self, _: Option<&str>) -> Vec<String> {
            Vec::new()
        }
    }

    /// A store that answers `get_at_tier`, which is how the workflow secret
    /// policy pins a scoped marker to exactly one vault tier.
    fn tiered_store(
        entries: &[(&str, workforce_common::settings::VaultSource, &str)],
    ) -> Arc<dyn SecretStore> {
        use workforce_common::settings::VaultSource;

        struct TieredStore(Vec<(String, VaultSource, String)>);
        impl SecretStore for TieredStore {
            fn get(&self, _: &str) -> Option<String> {
                None
            }
            fn set(&self, _: &str, _: &str) -> Result<()> {
                unreachable!("tests do not write")
            }
            fn delete(&self, _: &str) -> Result<bool> {
                unreachable!("tests do not delete")
            }
            fn list(&self, _: Option<&str>) -> Vec<String> {
                Vec::new()
            }
            fn inspect(&self, path: &str) -> Vec<(VaultSource, String)> {
                self.0
                    .iter()
                    .filter(|(p, _, _)| p == path)
                    .map(|(_, tier, value)| (*tier, value.clone()))
                    .collect()
            }
        }

        Arc::new(TieredStore(
            entries
                .iter()
                .map(|(p, t, v)| ((*p).to_string(), *t, (*v).to_string()))
                .collect(),
        ))
    }

    /// Named-secret cache keyed by (session, name).
    struct StubCache;

    impl StubCache {
        fn with(entries: &[(&str, &str, &str)]) -> Arc<dyn SecretCache> {
            crate::security::test_support::stub_named_secret_cache(entries)
        }
    }

    /// A resolver aimed at an operator-configured endpoint.
    fn resolver<'a>(store: &'a Arc<dyn SecretStore>, prefix: &str) -> CredentialResolver<'a> {
        CredentialResolver {
            tool: "http.request",
            session_key: "sess-1",
            store,
            host: Some("api.example.com"),
            destination: Destination::Pinned {
                creds_prefix: prefix.to_string(),
            },
        }
    }

    /// A resolver aimed at a host the agent named.
    fn adhoc_resolver<'a>(store: &'a Arc<dyn SecretStore>) -> CredentialResolver<'a> {
        CredentialResolver {
            tool: "http.request",
            session_key: "sess-1",
            store,
            host: Some("agent-chosen.example.com"),
            destination: Destination::AgentChosen,
        }
    }

    #[test]
    fn json_body_mixes_secrets_plain_strings_and_numbers() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        set_global_cache(StubCache::with(&[
            ("sess-1", "email", "kingsley@example.com"),
            ("sess-1", "password", "hunter2-real-password"),
        ]));
        let store = StubStore::with(&[]);

        let body = json!({
            "grant_type": "password",
            "username": "[[secret:email]]",
            "password": "[[secret:password]]",
            "attempts": 3,
            "remember": true,
            "scopes": ["openid", "profile"],
            "nested": { "client_id": "user-service", "hint": "[[secret:email]]" }
        });
        let (out, resolved) = resolver(&store, "http/endpoints/a/creds/")
            .resolve("body", &body)
            .expect("resolves");

        assert!(resolved);
        assert_eq!(out["grant_type"], json!("password"));
        assert_eq!(out["username"], json!("kingsley@example.com"));
        assert_eq!(out["password"], json!("hunter2-real-password"));
        // Non-string leaves keep their JSON type — a mixed body stays valid.
        assert_eq!(out["attempts"], json!(3));
        assert_eq!(out["remember"], json!(true));
        assert_eq!(out["scopes"], json!(["openid", "profile"]));
        assert_eq!(out["nested"]["client_id"], json!("user-service"));
        assert_eq!(out["nested"]["hint"], json!("kingsley@example.com"));

        clear_global_cache();
    }

    #[test]
    fn body_with_no_references_is_unchanged_and_reports_nothing_resolved() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        set_global_cache(StubCache::with(&[("sess-1", "email", "e@x.com")]));
        let store = StubStore::with(&[]);

        let body = json!({ "a": "plain", "b": 1, "c": [true, null] });
        let (out, resolved) = resolver(&store, "http/endpoints/a/creds/")
            .resolve("body", &body)
            .expect("resolves");
        assert_eq!(out, body);
        assert!(!resolved);

        clear_global_cache();
    }

    #[test]
    fn secret_inside_a_larger_string_substitutes_in_place() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        set_global_cache(StubCache::with(&[
            ("sess-1", "email", "alice@example.com"),
            ("sess-1", "password", "pw123"),
        ]));
        let store = StubStore::with(&[]);

        let value = json!("Basic [[secret:email]]:[[secret:password]] end");
        let (out, resolved) = resolver(&store, "http/endpoints/a/creds/")
            .resolve("headers", &value)
            .expect("resolves");
        assert_eq!(out, json!("Basic alice@example.com:pw123 end"));
        assert!(resolved);

        clear_global_cache();
    }

    #[test]
    fn secret_and_vault_reference_resolve_in_one_string() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        set_global_cache(StubCache::with(&[("sess-1", "password", "session-pw")]));
        let store = StubStore::with(&[("http/endpoints/a/creds/client_secret", "vault-cs")]);

        let value = json!("[[vault:client_secret]]/[[secret:password]]");
        let (out, resolved) = resolver(&store, "http/endpoints/a/creds/")
            .resolve("body", &value)
            .expect("resolves");
        assert_eq!(out, json!("vault-cs/session-pw"));
        assert!(resolved);

        clear_global_cache();
    }

    #[test]
    fn a_secret_value_containing_a_vault_marker_is_not_dereferenced() {
        // The single splice pass is over the ORIGINAL text, so cleartext
        // spliced in is never itself scanned for the other grammar.
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        set_global_cache(StubCache::with(&[(
            "sess-1",
            "tricky",
            "[[vault:client_secret]]",
        )]));
        let store = StubStore::with(&[("http/endpoints/a/creds/client_secret", "MUST-NOT-LEAK")]);

        let (out, _) = resolver(&store, "http/endpoints/a/creds/")
            .resolve("body", &json!("[[secret:tricky]]"))
            .expect("resolves");
        assert_eq!(out, json!("[[vault:client_secret]]"));

        clear_global_cache();
    }

    #[test]
    fn unknown_secret_marker_fails_closed_without_emitting_the_marker() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        set_global_cache(StubCache::with(&[]));
        let store = StubStore::with(&[]);

        let err = resolver(&store, "http/endpoints/a/creds/")
            .resolve("body", &json!({ "password": "[[secret:expired]]" }))
            .expect_err("expected failure");
        let msg = format!("{err}");
        assert!(msg.contains("expired"), "names the marker: {msg}");
        assert!(msg.contains("cmd: secret"), "names the fix: {msg}");

        clear_global_cache();
    }

    #[test]
    fn resolution_is_session_scoped() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        set_global_cache(StubCache::with(&[("sess-OTHER", "password", "pw")]));
        let store = StubStore::with(&[]);

        let err = resolver(&store, "http/endpoints/a/creds/")
            .resolve("body", &json!({ "p": "[[secret:password]]" }))
            .expect_err("a secret from another session must not resolve");
        assert!(format!("{err}").contains("password"));

        clear_global_cache();
    }

    #[test]
    fn vault_reference_without_an_endpoint_is_rejected() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        set_global_cache(StubCache::with(&[]));
        let store = StubStore::with(&[]);

        let err = adhoc_resolver(&store)
            .resolve("body", &json!({ "k": "[[vault:token]]" }))
            .expect_err("expected failure");
        assert!(format!("{err}").contains("`endpoint`"));

        clear_global_cache();
    }

    #[test]
    fn a_user_supplied_secret_resolves_for_an_agent_chosen_host() {
        // The user pasted it for this task, so unlike a vault credential it is
        // not pinned to an operator-chosen host.
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        set_global_cache(StubCache::with(&[("sess-1", "password", "pw123")]));
        let store = StubStore::with(&[]);

        let (out, resolved) = adhoc_resolver(&store)
            .resolve("body", &json!({ "p": "[[secret:password]]" }))
            .expect("resolves");
        assert_eq!(out["p"], json!("pw123"));
        assert!(resolved);

        clear_global_cache();
    }

    /// A response body is tokenized on the way to the model, so an access token
    /// a login endpoint returned is itself addressable as a marker. Sending one
    /// on to a host the *agent* named is the exfiltration path an injected
    /// instruction in that same response would take, so it is refused — while
    /// staying usable against an operator-configured endpoint.
    #[test]
    fn a_tool_sourced_secret_is_refused_for_an_agent_chosen_host() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        let id = MarkerId::fresh();
        set_global_cache(crate::security::test_support::stub_secret_cache(&[(
            "sess-1",
            id.clone(),
            "scraped-access-token",
            SecretSource::ToolResult {
                tool: "http.request".into(),
            },
        )]));
        let marker = format!("[[secret:{id} type=API_KEY preview=scra...oken]]");
        let store = StubStore::with(&[]);

        let err = adhoc_resolver(&store)
            .resolve("headers", &json!({ "authorization": marker.clone() }))
            .expect_err("expected refusal");
        let msg = format!("{err}");
        assert!(msg.contains("captured from a tool response"), "{msg}");
        assert!(!msg.contains("scraped-access-token"), "{msg}");

        // Same marker, operator-pinned host: allowed.
        let (out, resolved) = resolver(&store, "http/endpoints/a/creds/")
            .resolve("headers", &json!({ "authorization": marker }))
            .expect("resolves against a configured endpoint");
        assert_eq!(out["authorization"], json!("scraped-access-token"));
        assert!(resolved);

        clear_global_cache();
    }

    /// The login-then-call flow, which is the whole point of tokenizing a
    /// response: call A returns a short-lived token, the platform captures it,
    /// and the agent spends it on call B without the value ever passing through
    /// the model or needing to be stored. The agent references it by id alone —
    /// the form it actually produces — and the destination gate is unchanged.
    #[test]
    fn a_captured_token_spends_at_an_endpoint_when_referenced_by_id_alone() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        let id = MarkerId::fresh();
        set_global_cache(crate::security::test_support::stub_secret_cache(&[(
            "sess-1",
            id.clone(),
            "short-lived-access-token",
            SecretSource::ToolResult {
                tool: "http.request".into(),
            },
        )]));
        let store = StubStore::with(&[]);
        let bare = format!("[[secret:{id}]]");

        let (out, resolved) = resolver(&store, "http/endpoints/pw_staging/creds/")
            .resolve("headers", &json!({ "x-authorization": bare.clone() }))
            .expect("a captured token resolves at a configured endpoint");
        assert_eq!(out["x-authorization"], json!("short-lived-access-token"));
        assert!(resolved);

        // The containment gate is untouched: same reference, agent-chosen host.
        let err = adhoc_resolver(&store)
            .resolve("headers", &json!({ "x-authorization": bare }))
            .expect_err("still refused for a host the agent named");
        assert!(format!("{err}").contains("captured from a tool response"));
        assert!(!format!("{err}").contains("short-lived-access-token"));

        clear_global_cache();
    }

    /// The message a malformed reference produces is what the model acts on.
    /// It named only the two grammars this case *isn't*, so the model concluded
    /// the platform needed a pre-provisioned value and asked the user to paste
    /// a token that lives for minutes. It must name the captured-token form.
    #[test]
    fn the_malformed_reference_error_names_the_captured_token_form() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        set_global_cache(StubCache::with(&[]));
        let store = StubStore::with(&[]);

        let err = resolver(&store, "http/endpoints/pw_staging/creds/")
            .resolve(
                "headers",
                &json!({ "x-authorization": "[[secret:Bad-Name]]" }),
            )
            .expect_err("expected rejection")
            .to_string();
        assert!(err.contains("32-hex-id"), "{err}");
        assert!(err.contains("tool response"), "{err}");
        // Must not send the model back to the user for a value it already holds.
        assert!(!err.contains("ask the user to re-supply"), "{err}");

        clear_global_cache();
    }

    #[test]
    fn object_keys_are_never_treated_as_references() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        set_global_cache(StubCache::with(&[("sess-1", "email", "leak@example.com")]));
        let store = StubStore::with(&[]);

        let (out, resolved) = resolver(&store, "http/endpoints/a/creds/")
            .resolve("body", &json!({ "[[secret:email]]": "v" }))
            .expect("resolves");
        assert!(out.as_object().unwrap().contains_key("[[secret:email]]"));
        assert!(!resolved);

        clear_global_cache();
    }

    /// One agent typo — an uppercase or hyphenated name — used to fall through
    /// both grammars and be POSTed as literal text, reproducing the very
    /// silent-401 this resolver exists to prevent.
    #[test]
    fn a_malformed_marker_fails_the_call_rather_than_going_out_as_text() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        set_global_cache(StubCache::with(&[("sess-1", "email", "a@b.com")]));
        let store = StubStore::with(&[]);

        for bad in [
            "[[secret:API_KEY]]",
            "[[secret:my-email]]",
            "[[secret:email",
            "[[vault:bad key]]",
            "prefix [[secret:Nope]] suffix",
        ] {
            let err = resolver(&store, "http/endpoints/a/creds/")
                .resolve("body", &json!({ "v": bad }))
                .expect_err(bad)
                .to_string();
            assert!(err.contains("not a usable credential reference"), "{err}");
        }

        clear_global_cache();
    }

    /// A body where one leaf resolves and another is malformed must fail as a
    /// whole — checking "did anything resolve" at the end would let the bad
    /// leaf ride along.
    #[test]
    fn a_malformed_leaf_fails_a_body_that_also_has_a_good_one() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        set_global_cache(StubCache::with(&[("sess-1", "email", "a@b.com")]));
        let store = StubStore::with(&[]);

        let err = resolver(&store, "http/endpoints/a/creds/")
            .resolve(
                "body",
                &json!({ "good": "[[secret:email]]", "bad": "[[secret:Bad-Name]]" }),
            )
            .expect_err("expected rejection");
        let msg = format!("{err}");
        assert!(msg.contains("[[secret:Bad-Name]]"), "{msg}");
        assert!(msg.contains("lowercase"), "{msg}");

        clear_global_cache();
    }

    // ── workflow `type: secret` variables — `[[vault:<scope>:<key>]]` ──

    /// Declared by the workflow, so the *author* chose where it may go. It needs
    /// no endpoint: `allowed_hosts` is the pin.
    #[test]
    fn a_declared_workflow_secret_resolves_for_an_agent_chosen_host() {
        use workforce_common::secret_policy::{DeclaredSecret, SecretPolicy, SECRET_POLICY};
        use workforce_common::settings::VaultSource;

        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        set_global_cache(StubCache::with(&[]));
        let store = tiered_store(&[(
            "creds/sa_password",
            VaultSource::AgentSpecific,
            "workflow-pw",
        )]);
        let policy = SecretPolicy {
            per_user_identity: None,
            shared_secret_prefix: None,
            declared: [(
                ("agent".to_string(), "creds/sa_password".to_string()),
                DeclaredSecret {
                    allowed_hosts: vec!["agent-chosen.example.com".to_string()],
                },
            )]
            .into_iter()
            .collect(),
        };

        let (out, resolved) = SECRET_POLICY.sync_scope(policy, || {
            adhoc_resolver(&store)
                .resolve(
                    "body",
                    &json!({ "password": "[[vault:agent:creds/sa_password]]", "plain": 1 }),
                )
                .expect("resolves")
        });
        assert_eq!(out["password"], json!("workflow-pw"));
        assert_eq!(out["plain"], json!(1));
        assert!(resolved);

        clear_global_cache();
    }

    /// `allowed_hosts` is enforced against the request's destination, so the
    /// same declaration refuses a host the workflow author didn't name.
    #[test]
    fn a_declared_workflow_secret_is_refused_for_a_host_outside_allowed_hosts() {
        use workforce_common::secret_policy::{DeclaredSecret, SecretPolicy, SECRET_POLICY};
        use workforce_common::settings::VaultSource;

        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        set_global_cache(StubCache::with(&[]));
        let store = tiered_store(&[("creds/pw", VaultSource::AgentSpecific, "workflow-pw")]);
        let policy = SecretPolicy {
            per_user_identity: None,
            shared_secret_prefix: None,
            declared: [(
                ("agent".to_string(), "creds/pw".to_string()),
                DeclaredSecret {
                    allowed_hosts: vec!["only-this-host.example.com".to_string()],
                },
            )]
            .into_iter()
            .collect(),
        };

        let err = SECRET_POLICY.sync_scope(policy, || {
            adhoc_resolver(&store)
                .resolve("body", &json!({ "p": "[[vault:agent:creds/pw]]" }))
                .expect_err("host is not in allowed_hosts")
        });
        assert!(!format!("{err}").contains("workflow-pw"));

        clear_global_cache();
    }

    /// Outside a workflow there is no policy to have declared anything, so the
    /// scoped form fails closed rather than falling back to the bare resolver
    /// (whose `is_safe_key` would reject the colon with a generic error).
    #[test]
    fn a_scoped_marker_outside_a_workflow_run_names_the_alternatives() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        set_global_cache(StubCache::with(&[]));
        let store = StubStore::with(&[]);

        let err = resolver(&store, "http/endpoints/a/creds/")
            .resolve("body", &json!({ "p": "[[vault:agent:creds/pw]]" }))
            .expect_err("expected refusal")
            .to_string();
        assert!(err.contains("workflow secret variable"), "{err}");
        assert!(err.contains("cmd: secret"), "{err}");

        clear_global_cache();
    }

    /// Inside a workflow run the policy refuses every *undeclared* marker — but
    /// an endpoint's own bare `[[vault:<key>]]` is contained by construction
    /// (one operator-written host), so it must keep working there.
    #[test]
    fn an_endpoint_credential_still_resolves_inside_a_workflow_run() {
        use workforce_common::secret_policy::{SecretPolicy, SECRET_POLICY};

        let _g = TEST_LOCK.lock().unwrap();
        clear_global_cache();
        set_global_cache(StubCache::with(&[]));
        let store = StubStore::with(&[("http/endpoints/a/creds/password", "endpoint-pw")]);
        let policy = SecretPolicy {
            per_user_identity: None,
            shared_secret_prefix: None,
            declared: Default::default(),
        };

        let (out, resolved) = SECRET_POLICY.sync_scope(policy, || {
            resolver(&store, "http/endpoints/a/creds/")
                .resolve("body", &json!({ "p": "[[vault:password]]" }))
                .expect("endpoint creds are not subject to the declaration gate")
        });
        assert_eq!(out["p"], json!("endpoint-pw"));
        assert!(resolved);

        clear_global_cache();
    }

    #[test]
    fn contains_credential_ref_spots_both_grammars_at_any_depth() {
        assert!(contains_credential_ref(&json!("[[secret:a]]")));
        assert!(contains_credential_ref(&json!("[[vault:a]]")));
        assert!(contains_credential_ref(
            &json!({ "a": ["x", "[[secret:b]]"] })
        ));
        // Malformed but clearly intended as a reference — still a mistake in
        // an addressing field.
        assert!(contains_credential_ref(&json!("[[secret:Bad-Name]]")));
        assert!(!contains_credential_ref(&json!({ "a": "plain", "b": 2 })));
    }
}
