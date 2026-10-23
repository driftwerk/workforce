use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use workforce_common::settings::SecretStore;

use crate::serve::AgentService;
use crate::trigger_auth::{
    any_vault_secret_satisfies, hmac_sha256_base64_matches, hmac_sha256_header_matches,
    hmac_sha256_hex_matches, match_rules_pass, trigger_secret_stores,
};

/// Inbound webhook event (normalized).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookEvent {
    pub source: String,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub timestamp: String,
    pub raw_headers: HashMap<String, String>,
}

/// Webhook server state.
pub struct WebhookState {
    pub service: Arc<AgentService>,
    pub workflow_runtime: Option<Arc<dyn crate::hosts::WorkflowHost>>,
}

/// One inbound delivery as the sender signed it: the raw body, the public URL
/// it was addressed to (`[hub].public_url` plus the request target — unknown
/// when no public URL is configured) and the receipt time freshness checks
/// measure against.
struct Delivery {
    body: Bytes,
    url: Option<String>,
    received_at: DateTime<Utc>,
}

/// Build the webhook receiver router.
pub fn router(state: Arc<WebhookState>) -> Router {
    Router::new()
        .route("/webhooks/{source}/{event_type}", post(receive_webhook))
        .route("/webhooks/{source}", post(receive_webhook_generic))
        .with_state(state)
}

/// Receive a webhook with explicit source and event type.
async fn receive_webhook(
    State(state): State<Arc<WebhookState>>,
    Path((source, event_type)): Path<(String, String)>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    accept_webhook(state, &source, Some(&event_type), &uri, &headers, body)
}

/// Receive a webhook with auto-detected event type.
async fn receive_webhook_generic(
    State(state): State<Arc<WebhookState>>,
    Path(source): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    accept_webhook(state, &source, None, &uri, &headers, body)
}

/// Parse the payload, then hand all slow work (agent turn, workflow start,
/// re-indexing) to a detached task so the sender is acknowledged immediately.
///
/// GitHub's delivery timeout is ~10s and an agent turn easily exceeds it;
/// doing the work inline let the request future be cancelled on disconnect,
/// dropping the workflow before it started. Detaching makes processing
/// independent of the request lifecycle.
fn accept_webhook(
    state: Arc<WebhookState>,
    source: &str,
    explicit_event_type: Option<&str>,
    uri: &Uri,
    headers: &HeaderMap,
    body: Bytes,
) -> StatusCode {
    let payload: serde_json::Value = serde_json::from_slice(&body)
        .unwrap_or_else(|_| serde_json::json!({"raw": String::from_utf8_lossy(&body).to_string()}));

    let received_at = Utc::now();
    let events = webhook_events(source, explicit_event_type, headers, payload, received_at);
    info!(
        source = source,
        events = events.len(),
        event_types = ?events.iter().map(|event| event.event_type.as_str()).collect::<Vec<_>>(),
        "Webhook received"
    );

    let delivery = Delivery {
        body,
        url: public_request_url(state.service.config.hub.public_url.as_deref(), uri),
        received_at,
    };
    tokio::spawn(dispatch_delivery(state, events, delivery));
    StatusCode::OK
}

/// One normalized event per unit a trigger can match: HubSpot delivers a
/// batch array of independent events, every other source delivers one.
fn webhook_events(
    source: &str,
    explicit_event_type: Option<&str>,
    headers: &HeaderMap,
    payload: serde_json::Value,
    received_at: DateTime<Utc>,
) -> Vec<WebhookEvent> {
    let raw_headers = extract_relevant_headers(source, headers);
    let payloads = match (source, payload) {
        ("hubspot", serde_json::Value::Array(events)) => events,
        (_, payload) => vec![payload],
    };
    payloads
        .into_iter()
        .map(|payload| WebhookEvent {
            source: source.to_string(),
            event_type: explicit_event_type
                .map(str::to_string)
                .unwrap_or_else(|| detect_event_type(source, headers, &payload)),
            payload,
            timestamp: received_at.to_rfc3339(),
            raw_headers: raw_headers.clone(),
        })
        .collect()
}

/// The URL the sender addressed: `[hub].public_url` plus the request path and
/// query. Unknown without a configured public URL.
fn public_request_url(public_url: Option<&str>, uri: &Uri) -> Option<String> {
    let target = uri.path_and_query().map_or("/", |pq| pq.as_str());
    public_url.map(|base| format!("{}{target}", base.trim_end_matches('/')))
}

/// Run the independent reactions to a delivery concurrently: knowledge
/// re-indexing and deterministic workflow auto-start.
async fn dispatch_delivery(
    state: Arc<WebhookState>,
    events: Vec<WebhookEvent>,
    delivery: Delivery,
) {
    tokio::join!(
        futures::future::join_all(events.iter().map(|event| maybe_reindex(&state, event))),
        run_matching_workflows(&state, &events, &delivery),
    );
}

/// Re-index the knowledge base on GitHub `push` events. Prefers the canonical
/// maintenance pass (one fresh index per repo); falls back to a root re-index
/// only where no runner is wired (single-agent setups).
async fn maybe_reindex(state: &WebhookState, event: &WebhookEvent) {
    if !(event.source == "github" && event.event_type == "push") {
        return;
    }
    if let Some(runner) = crate::knowledge::maintenance_runner(&state.service) {
        info!("Triggering canonical knowledge re-index from GitHub push webhook");
        if let Err(e) = runner
            .run_pass(crate::knowledge::IndexMode::Incremental)
            .await
        {
            warn!(error = %e, "Knowledge re-index from webhook failed");
        }
    } else if let Some(knowledge_service) = state.service.knowledge_service() {
        info!("Triggering knowledge re-index from GitHub push webhook");
        if let Err(e) = knowledge_service.trigger_full_index().await {
            warn!(error = %e, "Knowledge re-index from webhook failed");
        }
    }
}

// ── Signature verification ──────────────────────────────────────────

/// Verify the HMAC-SHA256 signature an event carried in its extracted headers
/// against `secret`, dispatching on the platform's scheme. GitHub sends a
/// `sha256=`-prefixed header; Linear sends bare hex; HubSpot signs the full
/// request URL and a timestamp as well as the body. An unknown source — or one
/// whose signature header is absent — fails closed, since the trigger
/// explicitly asked for verification.
fn verify_event_signature(
    source: &str,
    secret: &str,
    delivery: &Delivery,
    raw_headers: &HashMap<String, String>,
) -> bool {
    match source {
        "github" => raw_headers
            .get("x-hub-signature-256")
            .is_some_and(|sig| hmac_sha256_header_matches(secret, &delivery.body, sig)),
        "linear" => raw_headers
            .get("linear-signature")
            .is_some_and(|sig| hmac_sha256_hex_matches(secret, &delivery.body, sig)),
        "hubspot" => match delivery.url.as_deref() {
            Some(url) => hubspot_signature_matches(
                secret,
                url,
                &delivery.body,
                raw_headers,
                delivery.received_at,
            ),
            None => {
                warn!("HubSpot signatures cover the full request URL — set [hub].public_url");
                false
            }
        },
        _ => false,
    }
}

const HUBSPOT_MAX_TIMESTAMP_AGE_MS: i64 = 5 * 60 * 1000;

/// The percent-encoded sequences HubSpot decodes in the URL before signing.
const HUBSPOT_URI_DECODINGS: [(&str, &str); 12] = [
    ("%3A", ":"),
    ("%2F", "/"),
    ("%3F", "?"),
    ("%40", "@"),
    ("%21", "!"),
    ("%24", "$"),
    ("%27", "'"),
    ("%28", "("),
    ("%29", ")"),
    ("%2A", "*"),
    ("%2C", ","),
    ("%3B", ";"),
];

/// HubSpot v3: standard-base64 HMAC-SHA256 keyed by the app client secret over
/// `POST` + the decoded request URL + the raw body + the millisecond timestamp
/// header, which must be at most five minutes old.
fn hubspot_signature_matches(
    secret: &str,
    url: &str,
    body: &[u8],
    raw_headers: &HashMap<String, String>,
    now: DateTime<Utc>,
) -> bool {
    let Some((signature, timestamp)) = raw_headers
        .get("x-hubspot-signature-v3")
        .zip(raw_headers.get("x-hubspot-request-timestamp"))
    else {
        return false;
    };
    let fresh = timestamp.parse::<i64>().is_ok_and(|sent_ms| {
        now.timestamp_millis().saturating_sub(sent_ms) <= HUBSPOT_MAX_TIMESTAMP_AGE_MS
    });
    fresh
        && hmac_sha256_base64_matches(
            secret,
            &[
                b"POST".as_slice(),
                hubspot_decode_uri(url).as_bytes(),
                body,
                timestamp.as_bytes(),
            ]
            .concat(),
            signature,
        )
}

fn hubspot_decode_uri(url: &str) -> String {
    HUBSPOT_URI_DECODINGS
        .iter()
        .fold(url.to_string(), |acc, (encoded, plain)| {
            acc.replace(encoded, plain)
        })
}

// ── Event type detection ────────────────────────────────────────────

/// Detect event type from headers and payload.
fn detect_event_type(source: &str, headers: &HeaderMap, payload: &serde_json::Value) -> String {
    match source {
        "github" => {
            // Primary: X-GitHub-Event header
            if let Some(event) = headers.get("x-github-event").and_then(|v| v.to_str().ok()) {
                return event.to_string();
            }
            // Fallback: payload action field
            payload
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string()
        }
        "linear" => {
            // Linear sends type + action in payload body
            let entity_type = payload
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let action = payload
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            format!("{entity_type}.{action}")
        }
        "gitlab" => payload
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        "stripe" => payload
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        "hubspot" => payload
            .get("subscriptionType")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        _ => "unknown".to_string(),
    }
}

/// Auto-detect event type from a webhook source and payload (public API).
pub fn detect_event_type_from_payload(source: &str, payload: &serde_json::Value) -> Option<String> {
    match source {
        "github" => payload
            .get("action")
            .and_then(|v| v.as_str())
            .map(String::from),
        "linear" => {
            let entity_type = payload.get("type").and_then(|v| v.as_str())?;
            let action = payload.get("action").and_then(|v| v.as_str())?;
            Some(format!("{entity_type}.{action}"))
        }
        "gitlab" => payload
            .get("event_type")
            .and_then(|v| v.as_str())
            .map(String::from),
        "stripe" => payload
            .get("type")
            .and_then(|v| v.as_str())
            .map(String::from),
        "hubspot" => payload
            .get("subscriptionType")
            .and_then(|v| v.as_str())
            .map(String::from),
        _ => None,
    }
}

/// Extract relevant headers for event context.
fn extract_relevant_headers(source: &str, headers: &HeaderMap) -> HashMap<String, String> {
    let keys: &[&str] = match source {
        "github" => &["x-github-event", "x-github-delivery", "x-hub-signature-256"],
        "linear" => &["linear-signature", "linear-delivery"],
        "hubspot" => &["x-hubspot-signature-v3", "x-hubspot-request-timestamp"],
        _ => &[],
    };

    keys.iter()
        .filter_map(|&key| {
            headers
                .get(key)
                .and_then(|v| v.to_str().ok())
                .map(|v| (key.to_string(), v.to_string()))
        })
        .collect()
}

// ── Workflow trigger matching ───────────────────────────────────────

use workforce_orchestrator::workflow::definition::{TriggerDefinition, WorkflowDefinition};
use workforce_orchestrator::workflow::instance::{TriggerInfo, TriggerSource};

/// Auto-start every loaded workflow whose webhook trigger matches an event in
/// this delivery. Every webhook trigger declares a `secret`, so a workflow
/// starts only when the delivery's signature verifies against it; the
/// signature belongs to the delivery, shared by all its events, so each secret
/// is verified once per delivery. HTTP and schedule triggers never match a
/// webhook event.
async fn run_matching_workflows(
    state: &WebhookState,
    events: &[WebhookEvent],
    delivery: &Delivery,
) {
    let Some(runtime) = state.workflow_runtime.as_ref() else {
        return;
    };
    let stores = trigger_secret_stores(&state.service);
    let definitions = runtime.all_definitions_keyed().await;
    let mut verified_secrets: HashMap<&str, bool> = HashMap::new();

    for event in events {
        for (key, trigger) in matching_triggers(&definitions, event) {
            let verified = trigger_secret(trigger).is_none_or(|marker| {
                *verified_secrets
                    .entry(marker)
                    .or_insert_with(|| verify_trigger_secret(&stores, event, delivery, marker))
            });
            if !verified {
                warn!(
                    workflow_id = %key,
                    source = %event.source,
                    "Webhook signature verification failed — workflow not started"
                );
                continue;
            }
            start_from_trigger(runtime, key, event).await;
        }
    }
}

/// Every (runtime key, webhook trigger) pair `event` fires. A member-owned
/// definition is keyed `{owner}/{id}`; its bare `workflow.id` starts nothing.
fn matching_triggers<'a>(
    definitions: &'a [(String, WorkflowDefinition)],
    event: &WebhookEvent,
) -> Vec<(&'a str, &'a TriggerDefinition)> {
    definitions
        .iter()
        .flat_map(|(key, def)| {
            def.triggers
                .iter()
                .filter(|trigger| matches_trigger(event, trigger))
                .map(move |trigger| (key.as_str(), trigger))
        })
        .collect()
}

async fn start_from_trigger(
    runtime: &Arc<dyn crate::hosts::WorkflowHost>,
    workflow_id: &str,
    event: &WebhookEvent,
) {
    info!(
        workflow_id,
        source = %event.source,
        event_type = %event.event_type,
        "Webhook trigger matched — starting workflow"
    );
    let trigger_info = TriggerInfo {
        source: TriggerSource::Webhook {
            source: event.source.clone(),
            event_type: event.event_type.clone(),
        },
        payload: event.payload.clone(),
    };
    match Arc::clone(runtime)
        .start_workflow_from_trigger(workflow_id, trigger_info, None, None, None, None)
        .await
    {
        Ok(instance_id) => info!(
            instance_id = %instance_id,
            workflow_id,
            "Workflow auto-started from webhook trigger"
        ),
        Err(e) => warn!(
            workflow_id,
            error = %e,
            "Failed to auto-start workflow from trigger"
        ),
    }
}

/// A webhook trigger's `[[vault:<key>]]` secret marker. Schedule and HTTP
/// triggers carry no webhook secret.
fn trigger_secret(trigger: &TriggerDefinition) -> Option<&str> {
    match trigger {
        TriggerDefinition::Webhook { secret, .. } => secret.as_deref(),
        TriggerDefinition::Schedule { .. } | TriggerDefinition::Http { .. } => None,
    }
}

/// Resolve a trigger's `[[vault:<key>]]` secret from the `workflows/` namespace
/// and verify the event signature against it. Accepts the first vault whose
/// resolved secret verifies; fails closed when none do — the trigger asked to
/// be verified.
fn verify_trigger_secret(
    stores: &[Arc<dyn SecretStore>],
    event: &WebhookEvent,
    delivery: &Delivery,
    marker: &str,
) -> bool {
    if stores.is_empty() {
        warn!("Webhook trigger declares a secret but no vault is configured");
        return false;
    }
    let verified = any_vault_secret_satisfies(stores, marker, |secret| {
        verify_event_signature(&event.source, secret, delivery, &event.raw_headers)
    });
    if !verified {
        warn!(target: "sentinel.audit", "Webhook secret resolution or verification failed");
    }
    verified
}

/// Check if a webhook event matches a trigger definition. Only webhook triggers
/// match events; schedule and HTTP triggers fire by other means.
fn matches_trigger(event: &WebhookEvent, trigger: &TriggerDefinition) -> bool {
    let TriggerDefinition::Webhook {
        source,
        event_type,
        match_rules,
        ..
    } = trigger
    else {
        return false;
    };
    event.source == *source
        && event.event_type == *event_type
        && match_rules_pass(&event.payload, match_rules)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use std::sync::Mutex;

    type HmacSha256 = Hmac<Sha256>;

    const HUBSPOT_URL: &str = "https://hub.example.com/webhooks/hubspot?portalId=123";

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

    fn github_signature(secret: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    fn linear_signature(secret: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    fn hubspot_signature(secret: &str, url: &str, body: &[u8], timestamp: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(format!("POST{url}").as_bytes());
        mac.update(body);
        mac.update(timestamp.as_bytes());
        BASE64.encode(mac.finalize().into_bytes())
    }

    fn hubspot_headers(signature: &str, timestamp: &str) -> HashMap<String, String> {
        HashMap::from([
            ("x-hubspot-signature-v3".to_string(), signature.to_string()),
            (
                "x-hubspot-request-timestamp".to_string(),
                timestamp.to_string(),
            ),
        ])
    }

    fn at_ms(ms: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(ms).unwrap()
    }

    fn delivery(body: &[u8], url: Option<&str>, received_at: DateTime<Utc>) -> Delivery {
        Delivery {
            body: Bytes::copy_from_slice(body),
            url: url.map(String::from),
            received_at,
        }
    }

    fn make_event(source: &str, event_type: &str, payload: serde_json::Value) -> WebhookEvent {
        WebhookEvent {
            source: source.to_string(),
            event_type: event_type.to_string(),
            payload,
            timestamp: "2026-02-19T00:00:00Z".to_string(),
            raw_headers: HashMap::new(),
        }
    }

    fn make_trigger(source: &str, event_type: &str, rules: Vec<(&str, &str)>) -> TriggerDefinition {
        TriggerDefinition::Webhook {
            source: source.to_string(),
            event_type: event_type.to_string(),
            match_rules: rules
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            secret: Some("[[vault:github/webhook1]]".to_string()),
        }
    }

    #[test]
    fn detect_github_event_from_payload() {
        let payload = serde_json::json!({"action": "opened", "pull_request": {}});
        assert_eq!(
            detect_event_type_from_payload("github", &payload),
            Some("opened".to_string())
        );
    }

    #[test]
    fn detect_unknown_source() {
        assert_eq!(
            detect_event_type_from_payload("custom", &serde_json::json!({})),
            None
        );
    }

    #[test]
    fn detect_linear_event_type_from_payload_variants() {
        assert_eq!(
            detect_event_type_from_payload(
                "linear",
                &serde_json::json!({"action": "update", "type": "Comment"})
            ),
            Some("Comment.update".to_string())
        );
        assert_eq!(
            detect_event_type_from_payload("linear", &serde_json::json!({})),
            None
        );
    }

    #[test]
    fn webhook_event_serialization() {
        let event = make_event(
            "github",
            "push",
            serde_json::json!({"ref": "refs/heads/main"}),
        );
        let json = serde_json::to_string(&event).unwrap();
        let parsed: WebhookEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.source, "github");
        assert_eq!(parsed.event_type, "push");
    }

    #[test]
    fn verify_event_signature_dispatches_by_source() {
        let received_at = at_ms(1_700_000_000_000);
        let received = delivery(b"payload", Some(HUBSPOT_URL), received_at);
        let body = &received.body;
        let github = HashMap::from([(
            "x-hub-signature-256".to_string(),
            github_signature("sec", body),
        )]);
        assert!(verify_event_signature("github", "sec", &received, &github));
        assert!(!verify_event_signature(
            "github", "wrong", &received, &github
        ));
        // Missing the signature header fails closed.
        assert!(!verify_event_signature(
            "github",
            "sec",
            &received,
            &HashMap::new()
        ));
        // Unknown source can't be verified, so it fails closed.
        assert!(!verify_event_signature("gitlab", "sec", &received, &github));

        let linear =
            HashMap::from([("linear-signature".to_string(), linear_signature("ls", body))]);
        assert!(verify_event_signature("linear", "ls", &received, &linear));
        assert!(!verify_event_signature(
            "linear",
            "ls",
            &received,
            &HashMap::new()
        ));

        let sent = received_at.timestamp_millis().to_string();
        let hubspot = hubspot_headers(&hubspot_signature("hs", HUBSPOT_URL, body, &sent), &sent);
        assert!(verify_event_signature("hubspot", "hs", &received, &hubspot));
        // Without [hub].public_url the signed URL is unknown, so HubSpot fails closed.
        assert!(!verify_event_signature(
            "hubspot",
            "hs",
            &delivery(b"payload", None, received_at),
            &hubspot
        ));
    }

    #[test]
    fn hubspot_decode_uri_restores_reserved_characters() {
        assert_eq!(
            hubspot_decode_uri("https%3A%2F%2Fh%2Fa%3Fq%3D%40%21%24%27%28%29%2A%2C%3B%20x"),
            "https://h/a?q%3D@!$'()*,;%20x"
        );
        assert_eq!(hubspot_decode_uri(HUBSPOT_URL), HUBSPOT_URL);
    }

    #[test]
    fn hubspot_signature_matches_full_url_body_and_timestamp() {
        let body = b"[{\"subscriptionType\":\"contact.creation\"}]";
        let sent = "1700000000000";
        let now = at_ms(1_700_000_000_000 + 60_000);
        let sig = hubspot_signature("app-secret", HUBSPOT_URL, body, sent);
        let headers = hubspot_headers(&sig, sent);

        assert!(hubspot_signature_matches(
            "app-secret",
            HUBSPOT_URL,
            body,
            &headers,
            now
        ));
        assert!(!hubspot_signature_matches(
            "wrong",
            HUBSPOT_URL,
            body,
            &headers,
            now
        ));
        assert!(!hubspot_signature_matches(
            "app-secret",
            "https://other.example.com/webhooks/hubspot",
            body,
            &headers,
            now
        ));
        assert!(!hubspot_signature_matches(
            "app-secret",
            HUBSPOT_URL,
            b"[]",
            &headers,
            now
        ));
        assert!(!hubspot_signature_matches(
            "app-secret",
            HUBSPOT_URL,
            body,
            &HashMap::new(),
            now
        ));
        assert!(!hubspot_signature_matches(
            "app-secret",
            HUBSPOT_URL,
            body,
            &HashMap::from([("x-hubspot-signature-v3".to_string(), sig.clone())]),
            now
        ));
        assert!(!hubspot_signature_matches(
            "app-secret",
            HUBSPOT_URL,
            body,
            &hubspot_headers(&sig, "soon"),
            now
        ));
        assert!(!hubspot_signature_matches(
            "app-secret",
            HUBSPOT_URL,
            body,
            &hubspot_headers("!!", sent),
            now
        ));
    }

    #[test]
    fn hubspot_signature_rejects_only_stale_timestamps() {
        let body = b"[]";
        let sent_ms: i64 = 1_700_000_000_000;
        let sent = sent_ms.to_string();
        let headers = hubspot_headers(&hubspot_signature("s", HUBSPOT_URL, body, &sent), &sent);
        let at_age = |age_ms: i64| at_ms(sent_ms + age_ms);

        assert!(hubspot_signature_matches(
            "s",
            HUBSPOT_URL,
            body,
            &headers,
            at_age(HUBSPOT_MAX_TIMESTAMP_AGE_MS)
        ));
        assert!(!hubspot_signature_matches(
            "s",
            HUBSPOT_URL,
            body,
            &headers,
            at_age(HUBSPOT_MAX_TIMESTAMP_AGE_MS + 1)
        ));
        // A sender clock ahead of the hub is not a replay.
        assert!(hubspot_signature_matches(
            "s",
            HUBSPOT_URL,
            body,
            &headers,
            at_age(-60_000)
        ));

        let extreme = i64::MIN.to_string();
        let absurd = hubspot_headers(
            &hubspot_signature("s", HUBSPOT_URL, body, &extreme),
            &extreme,
        );
        assert!(!hubspot_signature_matches(
            "s",
            HUBSPOT_URL,
            body,
            &absurd,
            at_age(0)
        ));
    }

    #[test]
    fn hubspot_signature_verifies_the_decoded_url() {
        let body = b"[]";
        let sent = "1700000000000";
        let signed_url = "https://hub.example.com/webhooks/hubspot?next=/crm/(a)";
        let received_url = "https://hub.example.com/webhooks/hubspot?next=%2Fcrm%2F%28a%29";
        let headers = hubspot_headers(&hubspot_signature("s", signed_url, body, sent), sent);
        assert!(hubspot_signature_matches(
            "s",
            received_url,
            body,
            &headers,
            at_ms(1_700_000_000_000)
        ));
    }

    #[test]
    fn verify_trigger_secret_verifies_hubspot_against_app_secret() {
        let request = delivery(b"[]", Some(HUBSPOT_URL), at_ms(1_700_000_000_000));
        let sent = "1700000000000";
        let event = WebhookEvent {
            raw_headers: hubspot_headers(
                &hubspot_signature("app-secret", HUBSPOT_URL, &request.body, sent),
                sent,
            ),
            ..make_event("hubspot", "contact.creation", serde_json::json!({}))
        };
        let store = StubStore::with(&[("workflows/hubspot/app", "app-secret")]);
        assert!(verify_trigger_secret(
            std::slice::from_ref(&store),
            &event,
            &request,
            "[[vault:hubspot/app]]",
        ));
        let wrong = StubStore::with(&[("workflows/hubspot/app", "other")]);
        assert!(!verify_trigger_secret(
            std::slice::from_ref(&wrong),
            &event,
            &request,
            "[[vault:hubspot/app]]",
        ));
    }

    #[test]
    fn webhook_events_fan_out_hubspot_batches() {
        let batch = serde_json::json!([
            {"subscriptionType": "contact.creation", "objectId": 1},
            {"subscriptionType": "contact.propertyChange", "objectId": 2},
        ]);
        let events = webhook_events("hubspot", None, &HeaderMap::new(), batch.clone(), at_ms(0));
        let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(types, ["contact.creation", "contact.propertyChange"]);
        assert_eq!(events[1].payload["objectId"], 2);

        let explicit = webhook_events(
            "hubspot",
            Some("contact.creation"),
            &HeaderMap::new(),
            batch,
            at_ms(0),
        );
        assert_eq!(explicit.len(), 2);
        assert!(explicit.iter().all(|e| e.event_type == "contact.creation"));

        assert!(webhook_events(
            "hubspot",
            None,
            &HeaderMap::new(),
            serde_json::json!([]),
            at_ms(0)
        )
        .is_empty());

        let single = webhook_events(
            "hubspot",
            None,
            &HeaderMap::new(),
            serde_json::json!({"subscriptionType": "deal.creation"}),
            at_ms(0),
        );
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].event_type, "deal.creation");

        let github = webhook_events(
            "github",
            None,
            &HeaderMap::new(),
            serde_json::json!([{"action": "opened"}]),
            at_ms(0),
        );
        assert_eq!(github.len(), 1);
        assert!(github[0].payload.is_array());
    }

    #[test]
    fn detect_hubspot_event_type_from_subscription() {
        let event = serde_json::json!({"subscriptionType": "contact.creation"});
        assert_eq!(
            detect_event_type("hubspot", &HeaderMap::new(), &event),
            "contact.creation"
        );
        assert_eq!(
            detect_event_type_from_payload("hubspot", &event),
            Some("contact.creation".to_string())
        );
        assert_eq!(
            detect_event_type_from_payload("hubspot", &serde_json::json!({})),
            None
        );
    }

    #[test]
    fn extract_hubspot_headers() {
        use axum::http::HeaderValue;
        let mut headers = HeaderMap::new();
        headers.insert("x-hubspot-signature-v3", HeaderValue::from_static("c2ln"));
        headers.insert(
            "x-hubspot-request-timestamp",
            HeaderValue::from_static("1700000000000"),
        );
        headers.insert("x-hubspot-signature", HeaderValue::from_static("legacy"));

        let extracted = extract_relevant_headers("hubspot", &headers);
        assert_eq!(
            extracted.get("x-hubspot-signature-v3").map(String::as_str),
            Some("c2ln")
        );
        assert_eq!(
            extracted
                .get("x-hubspot-request-timestamp")
                .map(String::as_str),
            Some("1700000000000")
        );
        assert!(!extracted.contains_key("x-hubspot-signature"));
    }

    #[test]
    fn public_request_url_joins_base_and_request_target() {
        let uri: Uri = "/webhooks/hubspot?portalId=123".parse().unwrap();
        assert_eq!(
            public_request_url(Some("https://hub.example.com/"), &uri).as_deref(),
            Some(HUBSPOT_URL)
        );
        assert_eq!(
            public_request_url(Some("https://hub.example.com"), &uri).as_deref(),
            Some(HUBSPOT_URL)
        );
        let bare: Uri = "/webhooks/hubspot".parse().unwrap();
        assert_eq!(
            public_request_url(Some("https://hub.example.com"), &bare).as_deref(),
            Some("https://hub.example.com/webhooks/hubspot")
        );
        assert_eq!(public_request_url(None, &uri), None);
    }

    #[test]
    fn trigger_secret_reads_marker_and_ignores_other_kinds() {
        let webhook = make_trigger("github", "pull_request", vec![]);
        assert_eq!(trigger_secret(&webhook), Some("[[vault:github/webhook1]]"));

        let schedule = TriggerDefinition::Schedule {
            cron: "0 0 * * *".to_string(),
            tz: None,
            run_as: None,
            payload: serde_json::Value::Null,
        };
        assert_eq!(trigger_secret(&schedule), None);

        let http = TriggerDefinition::Http {
            id: "deploy".to_string(),
            match_rules: HashMap::new(),
            auth: workforce_orchestrator::workflow::definition::TriggerAuth::Bearer {
                token: "[[vault:deploy/token]]".to_string(),
            },
            response: Default::default(),
        };
        assert_eq!(trigger_secret(&http), None);
    }

    #[test]
    fn verify_trigger_secret_resolves_from_workflows_namespace() {
        let request = delivery(b"the-raw-body", None, at_ms(0));
        let store: Arc<dyn SecretStore> =
            StubStore::with(&[("workflows/github/webhook1", "topsecret")]);
        let event = WebhookEvent {
            raw_headers: HashMap::from([(
                "x-hub-signature-256".to_string(),
                github_signature("topsecret", &request.body),
            )]),
            ..make_event("github", "pull_request", serde_json::json!({}))
        };

        assert!(verify_trigger_secret(
            std::slice::from_ref(&store),
            &event,
            &request,
            "[[vault:github/webhook1]]",
        ));
        let wrong: Arc<dyn SecretStore> =
            StubStore::with(&[("workflows/github/webhook1", "different")]);
        assert!(!verify_trigger_secret(
            std::slice::from_ref(&wrong),
            &event,
            &request,
            "[[vault:github/webhook1]]",
        ));
        // Unknown key → resolution fails closed.
        assert!(!verify_trigger_secret(
            std::slice::from_ref(&store),
            &event,
            &request,
            "[[vault:github/absent]]",
        ));
        // No vault configured → fails closed.
        assert!(!verify_trigger_secret(
            &[],
            &event,
            &request,
            "[[vault:github/webhook1]]"
        ));
        // Found in the second vault when the first lacks the key.
        let empty: Arc<dyn SecretStore> = StubStore::with(&[]);
        assert!(verify_trigger_secret(
            &[empty, store],
            &event,
            &request,
            "[[vault:github/webhook1]]",
        ));
    }

    #[test]
    fn verify_trigger_secret_rejects_namespace_escape() {
        let request = delivery(b"body", None, at_ms(0));
        let store: Arc<dyn SecretStore> = StubStore::with(&[("skills/github/token", "ghp_x")]);
        let event = WebhookEvent {
            raw_headers: HashMap::from([(
                "x-hub-signature-256".to_string(),
                github_signature("ghp_x", &request.body),
            )]),
            ..make_event("github", "pull_request", serde_json::json!({}))
        };
        assert!(!verify_trigger_secret(
            std::slice::from_ref(&store),
            &event,
            &request,
            "[[vault:../skills/github/token]]",
        ));
    }

    #[test]
    fn detect_event_type_header_then_payload_fallback() {
        use axum::http::HeaderValue;
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", HeaderValue::from_static("pull_request"));
        assert_eq!(
            detect_event_type("github", &headers, &serde_json::json!({"action": "opened"})),
            "pull_request"
        );
        assert_eq!(
            detect_event_type(
                "github",
                &HeaderMap::new(),
                &serde_json::json!({"action": "opened"})
            ),
            "opened"
        );
        assert_eq!(
            detect_event_type(
                "linear",
                &HeaderMap::new(),
                &serde_json::json!({"action": "create", "type": "Issue"})
            ),
            "Issue.create"
        );
    }

    #[test]
    fn extract_linear_headers() {
        use axum::http::HeaderValue;
        let mut headers = HeaderMap::new();
        headers.insert("linear-signature", HeaderValue::from_static("abc123"));
        headers.insert("linear-delivery", HeaderValue::from_static("delivery-456"));
        headers.insert("x-unrelated", HeaderValue::from_static("ignore"));

        let extracted = extract_relevant_headers("linear", &headers);
        assert_eq!(
            extracted.get("linear-signature"),
            Some(&"abc123".to_string())
        );
        assert_eq!(
            extracted.get("linear-delivery"),
            Some(&"delivery-456".to_string())
        );
        assert!(!extracted.contains_key("x-unrelated"));
    }

    #[test]
    fn trigger_matches_source_event_and_rules() {
        let trigger = make_trigger("github", "issues", vec![("action", "labeled")]);
        assert!(matches_trigger(
            &make_event("github", "issues", serde_json::json!({"action": "labeled"})),
            &trigger
        ));
        assert!(!matches_trigger(
            &make_event("linear", "issues", serde_json::json!({"action": "labeled"})),
            &trigger
        ));
        assert!(!matches_trigger(
            &make_event("github", "push", serde_json::json!({"action": "labeled"})),
            &trigger
        ));
        assert!(!matches_trigger(
            &make_event("github", "issues", serde_json::json!({"action": "opened"})),
            &trigger
        ));
    }

    #[test]
    fn trigger_never_matches_non_webhook_kinds() {
        let event = make_event("github", "push", serde_json::json!({}));
        let schedule = TriggerDefinition::Schedule {
            cron: "0 0 * * *".to_string(),
            tz: None,
            run_as: None,
            payload: serde_json::Value::Null,
        };
        assert!(!matches_trigger(&event, &schedule));
    }

    #[test]
    fn matching_triggers_start_owned_definitions_by_their_runtime_key() {
        let toml = r#"
[workflow]
id = "hubspot-ticket-triage"
name = "Triage"

[[trigger]]
type = "webhook"
source = "hubspot"
event_type = "ticket.creation"
secret = "[[vault:hubspot/app]]"

[[step]]
id = "only"
name = "Only"
task = "noop"
exit = true
"#;
        let def = WorkflowDefinition::from_toml(toml).unwrap();
        let owned_key = "69116b82-6a40-4f24-8809-8c01b69a4d52/hubspot-ticket-triage".to_string();
        let definitions = vec![
            (owned_key.clone(), def.clone()),
            ("hubspot-ticket-triage".to_string(), def),
        ];

        let hit = make_event(
            "hubspot",
            "ticket.creation",
            serde_json::json!({"objectId": 1}),
        );
        let keys: Vec<&str> = matching_triggers(&definitions, &hit)
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(keys, vec![owned_key.as_str(), "hubspot-ticket-triage"]);

        let miss = make_event("hubspot", "contact.creation", serde_json::json!({}));
        assert!(matching_triggers(&definitions, &miss).is_empty());
    }
}
