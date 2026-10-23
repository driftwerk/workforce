//! Argument parsing and request construction for `http.request`.
//!
//! Everything that decides *what* goes on the wire lives here, so the two
//! request modes converge on one [`PreparedRequest`] before anything is sent.

use std::sync::Arc;
use std::time::Duration;

use eyre::{eyre, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
use url::Url;
use workforce_common::settings::SecretStore;

use crate::runner::CURRENT_SESSION_KEY;
use crate::security::credential_refs::{contains_credential_ref, CredentialResolver, Destination};

use super::config::{clamp_timeout, join_url, validate_name, Auth, EndpointConfig};

const JSON_CONTENT_TYPE: &str = "application/json";
const TEXT_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

/// Default cap on characters of a non-JSON response body returned to the model.
pub const DEFAULT_MAX_CHARS: usize = 50_000;
/// Hard cap on `max_chars`, whatever the caller asks for.
pub const MAX_MAX_CHARS: usize = 200_000;

/// Headers an agent may not set: hop-by-hop framing that belongs to the client,
/// and `host`, which would let a request claim to be for a different origin.
const DENIED_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "connection",
    "transfer-encoding",
    "upgrade",
    "proxy-authorization",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "GET" => Ok(Method::Get),
            "HEAD" => Ok(Method::Head),
            "POST" => Ok(Method::Post),
            "PUT" => Ok(Method::Put),
            "PATCH" => Ok(Method::Patch),
            "DELETE" => Ok(Method::Delete),
            other => Err(eyre!(
                "unsupported method '{other}': use GET, HEAD, POST, PUT, PATCH, or DELETE"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }

    /// Whether this method may change remote state, and so requires
    /// `Capability::NetworkWrite`.
    pub fn is_mutating(self) -> bool {
        !matches!(self, Method::Get | Method::Head)
    }
}

impl From<Method> for reqwest::Method {
    fn from(method: Method) -> Self {
        match method {
            Method::Get => reqwest::Method::GET,
            Method::Head => reqwest::Method::HEAD,
            Method::Post => reqwest::Method::POST,
            Method::Put => reqwest::Method::PUT,
            Method::Patch => reqwest::Method::PATCH,
            Method::Delete => reqwest::Method::DELETE,
        }
    }
}

/// A request body, already serialized. `content_type` is `None` when the agent
/// supplied an explicit `content-type` header, which wins over the default the
/// body kind implies.
pub struct Body {
    pub bytes: Vec<u8>,
    pub content_type: Option<&'static str>,
}

/// A fully resolved request: nothing here still needs the vault or the config.
pub struct PreparedRequest {
    pub method: Method,
    pub url: Url,
    pub headers: HeaderMap,
    pub body: Option<Body>,
    pub timeout: Duration,
    /// Credentialled requests never follow a redirect — `reqwest` would carry
    /// the `Authorization` header to the redirect target.
    pub follow_redirects: bool,
    /// Endpoint name for the audit record; `None` for an ad-hoc request.
    pub endpoint: Option<String>,
    /// Cap on characters of a non-JSON response body returned to the model.
    pub max_chars: usize,
}

/// Headers and body hold resolved credentials by design, so they are reported
/// by shape only — a `PreparedRequest` must stay safe to log. The query string
/// is dropped for the same reason: a credential reference resolves in `query`
/// as readily as in a header.
impl std::fmt::Debug for PreparedRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut url = self.url.clone();
        url.set_query(None);
        url.set_fragment(None);
        f.debug_struct("PreparedRequest")
            .field("method", &self.method)
            .field("url", &url.as_str())
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("body_bytes", &self.body.as_ref().map(|b| b.bytes.len()))
            .field("timeout", &self.timeout)
            .field("follow_redirects", &self.follow_redirects)
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

/// Which of the two mutually exclusive modes a call is in. Endpoint mode
/// carries its path so the pairing is structural rather than an invariant the
/// caller has to re-check.
enum Target {
    Endpoint(Box<EndpointConfig>, String),
    AdHoc(Url),
}

fn str_arg<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input[key].as_str().map(str::trim).filter(|s| !s.is_empty())
}

/// Resolve the request mode from the arguments, rejecting every ambiguous or
/// incomplete combination with a message that names the fix.
fn resolve_target(input: &Value, load: impl Fn(&str) -> Result<EndpointConfig>) -> Result<Target> {
    let endpoint = str_arg(input, "endpoint");
    let url = str_arg(input, "url");
    let path = str_arg(input, "path");

    match (endpoint, url) {
        (Some(_), Some(_)) => Err(eyre!(
            "pass either `endpoint` + `path` (a configured endpoint) or `url` (an ad-hoc \
             request), not both"
        )),
        (None, None) => Err(eyre!(
            "one of `endpoint` (with `path`) or `url` is required. Call `http.endpoints` to \
             see the configured endpoints"
        )),
        (Some(name), None) => {
            let path = path.ok_or_else(|| {
                eyre!("`path` is required with `endpoint`, and must start with '/'")
            })?;
            if !path.starts_with('/') {
                return Err(eyre!("`path` must start with '/' (e.g. '/v1/things')"));
            }
            let config = load(validate_name(name)?)?;
            Ok(Target::Endpoint(Box::new(config), path.to_string()))
        }
        (None, Some(url)) => {
            if path.is_some() {
                return Err(eyre!(
                    "`path` applies to `endpoint` only — put the full path in `url`"
                ));
            }
            let parsed = Url::parse(url).map_err(|e| eyre!("invalid `url` '{url}': {e}"))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(eyre!(
                    "`url` must be http or https, got '{}'",
                    parsed.scheme()
                ));
            }
            Ok(Target::AdHoc(parsed))
        }
    }
}

/// Fields whose values may carry a credential reference. Everything else in
/// the tool input either addresses the request or configures the client.
const CREDENTIAL_FIELDS: &[&str] = &["headers", "query", "body", "body_text"];

/// Resolve credential references in the agent-supplied header/query/body
/// arguments — `[[secret:<name>]]` against this conversation's secret cache,
/// `[[vault:<key>]]` against the endpoint's own credential namespace. Both
/// resolve at any depth of a JSON body, so a payload may mix credentials with
/// ordinary values, or carry none at all.
///
/// A vault reference needs an `endpoint`: an operator stored that credential
/// for one host, and honouring it against an agent-chosen `url` would let an
/// injected instruction pick the recipient. A secret the *user* supplied
/// carries no such pin — they pasted it into this conversation for this task —
/// so it resolves in both modes, matching `browser.fill` / `obscura.fill`. A
/// secret the platform captured out of a tool *response* is pinned like a vault
/// credential: see [`Destination`].
///
/// Returns the rewritten input and whether any reference actually resolved —
/// the caller needs that second value, because a request carrying a resolved
/// credential must not follow redirects even when the endpoint itself declares
/// no auth.
fn resolve_credentials(
    input: &Value,
    store: &Arc<dyn SecretStore>,
    endpoint: Option<&EndpointConfig>,
    host: Option<&str>,
) -> Result<(Value, bool)> {
    // `path` and `url` address the request; a reference there is a mistake, not
    // a credential, and passing it through verbatim would silently build a
    // nonsense URL — or, for `url`, transmit the marker text to whatever host
    // the rest of the string named.
    if ["path", "url"]
        .iter()
        .filter_map(|f| input.get(*f))
        .any(contains_credential_ref)
    {
        return Err(eyre!(
            "[[secret:…]] and [[vault:…]] resolve in `headers`, `query`, and the body only — not \
             in `path` or `url`, which address the request rather than authenticate it"
        ));
    }

    if !CREDENTIAL_FIELDS
        .iter()
        .filter_map(|f| input.get(*f))
        .any(contains_credential_ref)
    {
        return Ok((input.clone(), false));
    }

    let resolver = CredentialResolver {
        tool: "http.request",
        session_key: &CURRENT_SESSION_KEY
            .try_with(|k| k.clone())
            .unwrap_or_default(),
        store,
        host,
        destination: match endpoint {
            Some(config) => Destination::Pinned {
                creds_prefix: config.creds_prefix(),
            },
            None => Destination::AgentChosen,
        },
    };

    CREDENTIAL_FIELDS
        .iter()
        .try_fold((input.clone(), false), |(acc, any), field| {
            match acc.get(*field).cloned() {
                None => Ok((acc, any)),
                Some(value) => resolver
                    .resolve(field, &value)
                    .map(|(value, did)| (with_field(acc, field, value), any || did)),
            }
        })
}

/// `input` with `field` replaced. A non-object passes through untouched — the
/// tool's own validator reports the shape error.
fn with_field(input: Value, field: &str, value: Value) -> Value {
    match input {
        Value::Object(mut map) => {
            map.insert(field.to_string(), value);
            Value::Object(map)
        }
        other => other,
    }
}

/// Parse the agent-supplied headers, rejecting framing headers and — when the
/// endpoint carries a credential — an `authorization` override.
fn agent_headers(input: &Value, endpoint_is_authenticated: bool) -> Result<HeaderMap> {
    let Some(map) = input.get("headers") else {
        return Ok(HeaderMap::new());
    };
    let map = map
        .as_object()
        .ok_or_else(|| eyre!("`headers` must be an object of string values"))?;

    map.iter()
        .try_fold(HeaderMap::new(), |mut acc, (key, value)| {
            let lower = key.trim().to_ascii_lowercase();
            if DENIED_HEADERS.contains(&lower.as_str()) {
                return Err(eyre!(
                    "header '{key}' is set by the client and cannot be overridden"
                ));
            }
            if lower == AUTHORIZATION.as_str() && endpoint_is_authenticated {
                return Err(eyre!(
                    "this endpoint authenticates from its configured credential — remove the \
                 `authorization` header"
                ));
            }
            let value = value
                .as_str()
                .ok_or_else(|| eyre!("header '{key}' must be a string"))?;
            let name = HeaderName::from_bytes(lower.as_bytes())
                .map_err(|_| eyre!("invalid header name '{key}'"))?;
            let value = HeaderValue::from_str(value)
                .map_err(|_| eyre!("invalid value for header '{key}'"))?;
            acc.insert(name, value);
            Ok(acc)
        })
}

/// Append the endpoint's credential. Applied after the agent's headers so a
/// configured credential always wins.
fn apply_auth(headers: &mut HeaderMap, auth: &Auth) -> Result<()> {
    let (name, value) = match auth {
        Auth::None => return Ok(()),
        Auth::Bearer { token } => (AUTHORIZATION, format!("Bearer {token}")),
        Auth::Basic { username, password } => {
            use base64::Engine;
            let encoded =
                base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
            (AUTHORIZATION, format!("Basic {encoded}"))
        }
        Auth::Header { name, value } => (
            HeaderName::from_bytes(name.trim().to_ascii_lowercase().as_bytes())
                .map_err(|_| eyre!("endpoint is configured with an invalid auth header name"))?,
            value.clone(),
        ),
    };
    let mut value = HeaderValue::from_str(&value)
        .map_err(|_| eyre!("endpoint credential is not a valid header value"))?;
    value.set_sensitive(true);
    headers.insert(name, value);
    Ok(())
}

/// Serialize the body. `body` (JSON) and `body_text` (raw) are mutually
/// exclusive, and neither is allowed on a method that takes no body.
fn build_body(input: &Value, method: Method, headers: &HeaderMap) -> Result<Option<Body>> {
    let json = input.get("body").filter(|v| !v.is_null());
    let text = input.get("body_text").filter(|v| !v.is_null());

    let body = match (json, text) {
        (Some(_), Some(_)) => {
            return Err(eyre!(
                "pass either `body` (JSON) or `body_text` (raw), not both"
            ))
        }
        (None, None) => return Ok(None),
        (Some(json), None) => Body {
            bytes: serde_json::to_vec(json)?,
            content_type: Some(JSON_CONTENT_TYPE),
        },
        (None, Some(text)) => Body {
            bytes: text
                .as_str()
                .ok_or_else(|| eyre!("`body_text` must be a string"))?
                .as_bytes()
                .to_vec(),
            content_type: Some(TEXT_CONTENT_TYPE),
        },
    };

    if !method.is_mutating() {
        return Err(eyre!(
            "{} takes no request body — use POST, PUT, PATCH, or DELETE",
            method.as_str()
        ));
    }
    Ok(Some(Body {
        content_type: body
            .content_type
            .filter(|_| !headers.contains_key(CONTENT_TYPE)),
        ..body
    }))
}

/// Apply the `query` argument to an already-parsed URL.
fn apply_query(url: &mut Url, input: &Value) -> Result<()> {
    let Some(query) = input.get("query") else {
        return Ok(());
    };
    let query = query
        .as_object()
        .ok_or_else(|| eyre!("`query` must be an object of string values"))?;
    let mut pairs = url.query_pairs_mut();
    query.iter().try_for_each(|(key, value)| {
        let value = value
            .as_str()
            .ok_or_else(|| eyre!("query parameter '{key}' must be a string"))?;
        pairs.append_pair(key, value);
        Ok::<_, eyre::Report>(())
    })?;
    drop(pairs);
    Ok(())
}

/// Turn raw tool arguments into a request ready to send.
pub fn prepare(
    input: &Value,
    store: &Arc<dyn SecretStore>,
    load: impl Fn(&str) -> Result<EndpointConfig>,
) -> Result<PreparedRequest> {
    let method =
        Method::parse(str_arg(input, "method").ok_or_else(|| eyre!("`method` is required"))?)?;
    let target = resolve_target(input, load)?;
    let endpoint = match &target {
        Target::Endpoint(config, _) => Some(config.as_ref()),
        Target::AdHoc(_) => None,
    };

    // The URL is settled before credentials resolve: its host gates a
    // workflow-declared secret's `allowed_hosts`, and `query` — applied after,
    // from the resolved input — cannot move the request off that host.
    let mut url = match &target {
        Target::Endpoint(config, path) => {
            let joined = join_url(&config.base_url, path);
            Url::parse(&joined).map_err(|e| {
                eyre!(
                    "endpoint '{}' has a base_url that does not form a valid URL with '{path}': {e}",
                    config.name
                )
            })?
        }
        Target::AdHoc(url) => url.clone(),
    };
    if !matches!(url.scheme(), "http" | "https") {
        return Err(eyre!("endpoint base_url must be http or https"));
    }

    let (input, resolved_credential) = resolve_credentials(input, store, endpoint, url.host_str())?;
    apply_query(&mut url, &input)?;

    let authenticated = endpoint.is_some_and(|c| c.auth.is_authenticated());
    let mut headers = agent_headers(&input, authenticated)?;
    let body = build_body(&input, method, &headers)?;
    if let Some(config) = endpoint {
        apply_auth(&mut headers, &config.auth)?;
    }
    // Anything that put a credential on this request — the endpoint's own auth,
    // a resolved `[[vault:…]]` in a header or body, or an agent-supplied
    // `Authorization` — pins it to this host. `reqwest` strips only
    // `Authorization`/`Cookie`/`Proxy-Authorization`/`WWW-Authenticate` across a
    // cross-host hop, so a custom auth header would be carried verbatim and a
    // 307/308 would replay the body to the redirect target.
    let carries_credential =
        authenticated || resolved_credential || headers.contains_key(AUTHORIZATION);

    Ok(PreparedRequest {
        method,
        url,
        headers,
        body,
        timeout: clamp_timeout(
            input["timeout_ms"].as_u64(),
            endpoint.map_or_else(
                || Duration::from_millis(super::config::DEFAULT_TIMEOUT_MS),
                |c| c.timeout,
            ),
        ),
        follow_redirects: !carries_credential,
        endpoint: endpoint.map(|c| c.name.clone()),
        max_chars: input["max_chars"]
            .as_u64()
            .map_or(DEFAULT_MAX_CHARS, |n| (n as usize).clamp(1, MAX_MAX_CHARS)),
    })
}

/// Whether a tool input names a state-changing method. The single source of
/// truth `hub::authz::tool_capability` reads, so the gate and the tool can't
/// disagree about which calls mutate. An unparseable method is treated as
/// mutating: the tool rejects it anyway, and the gate must fail closed.
pub fn is_mutating_request(input: &Value) -> bool {
    match input["method"].as_str().map(Method::parse) {
        Some(Ok(method)) => method.is_mutating(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::http::config::DEFAULT_TIMEOUT_MS;
    use serde_json::json;
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

    fn endpoint(auth: Auth) -> EndpointConfig {
        EndpointConfig {
            name: "svc".into(),
            base_url: "https://api.example.com".into(),
            auth,
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
        }
    }

    /// `prepare` against a fixed endpoint and an empty credential store.
    fn prep(input: Value) -> Result<PreparedRequest> {
        prep_with(input, Auth::None, &[])
    }

    fn prep_with(input: Value, auth: Auth, creds: &[(&str, &str)]) -> Result<PreparedRequest> {
        let store = StubStore::with(creds);
        prepare(
            &input,
            &store,
            |name| Ok(endpoint(auth.clone()).named(name)),
        )
    }

    impl EndpointConfig {
        fn named(mut self, name: &str) -> Self {
            self.name = name.to_string();
            self
        }
    }

    fn err_of(input: Value) -> String {
        prep(input).expect_err("expected rejection").to_string()
    }

    // ── mode exclusivity ────────────────────────────────────────────

    #[test]
    fn endpoint_and_path_is_accepted() {
        let prepared =
            prep(json!({"method": "GET", "endpoint": "svc", "path": "/v1/x"})).expect("valid");
        assert_eq!(prepared.url.as_str(), "https://api.example.com/v1/x");
        assert_eq!(prepared.endpoint.as_deref(), Some("svc"));
    }

    #[test]
    fn absolute_url_is_accepted() {
        let prepared =
            prep(json!({"method": "GET", "url": "https://other.example.com/y"})).expect("valid");
        assert_eq!(prepared.url.as_str(), "https://other.example.com/y");
        assert!(prepared.endpoint.is_none());
    }

    #[test]
    fn endpoint_and_url_together_are_rejected() {
        let msg = err_of(json!({
            "method": "GET", "endpoint": "svc", "path": "/x", "url": "https://e.example.com"
        }));
        assert!(msg.contains("not both"), "{msg}");
    }

    #[test]
    fn neither_endpoint_nor_url_is_rejected() {
        let msg = err_of(json!({"method": "GET"}));
        assert!(msg.contains("is required"), "{msg}");
    }

    #[test]
    fn endpoint_without_path_is_rejected() {
        let msg = err_of(json!({"method": "GET", "endpoint": "svc"}));
        assert!(msg.contains("`path` is required"), "{msg}");
    }

    #[test]
    fn path_without_leading_slash_is_rejected() {
        let msg = err_of(json!({"method": "GET", "endpoint": "svc", "path": "v1/x"}));
        assert!(msg.contains("must start with '/'"), "{msg}");
    }

    #[test]
    fn path_with_url_is_rejected() {
        let msg = err_of(json!({
            "method": "GET", "url": "https://e.example.com", "path": "/x"
        }));
        assert!(msg.contains("applies to `endpoint` only"), "{msg}");
    }

    #[test]
    fn non_http_scheme_is_rejected() {
        let msg = err_of(json!({"method": "GET", "url": "file:///etc/passwd"}));
        assert!(msg.contains("must be http or https"), "{msg}");
    }

    #[test]
    fn endpoint_name_that_escapes_the_namespace_is_rejected() {
        let msg = err_of(json!({
            "method": "GET", "endpoint": "../databases/instances/prod", "path": "/x"
        }));
        assert!(msg.contains("invalid endpoint name"), "{msg}");
    }

    /// The containment invariant depends on `path` being unable to move the
    /// request off the endpoint's configured host. Pinned for every separator
    /// trick that retargets a host in some URL parsers — including the
    /// backslash, which the WHATWG rules treat as a separator.
    #[test]
    fn path_can_never_retarget_the_host() {
        for path in [
            "//evil.com/x",
            "///evil.com",
            "/../../../x",
            "/\\evil.com",
            "/@evil.com/",
            "/..%2f..%2fx",
        ] {
            let prepared = prep(json!({
                "method": "GET", "endpoint": "svc", "path": path
            }))
            .unwrap_or_else(|e| panic!("{path} should parse: {e}"));
            assert_eq!(
                prepared.url.host_str(),
                Some("api.example.com"),
                "{path} escaped the endpoint host"
            );
        }
    }

    // ── credential containment ──────────────────────────────────────

    #[test]
    fn vault_ref_in_adhoc_mode_is_rejected() {
        let msg = err_of(json!({
            "method": "POST",
            "url": "https://evil.example.com",
            "body": {"password": "[[vault:password]]"}
        }));
        assert!(msg.contains("need an `endpoint`"), "{msg}");
    }

    #[test]
    fn vault_ref_resolves_from_the_endpoints_own_namespace() {
        let prepared = prep_with(
            json!({
                "method": "POST", "endpoint": "svc", "path": "/login",
                "body": {"email": "[[vault:email]]", "password": "[[vault:password]]"}
            }),
            Auth::None,
            &[
                ("http/endpoints/svc/creds/email", "a@b.com"),
                ("http/endpoints/svc/creds/password", "hunter2"),
            ],
        )
        .expect("valid");
        let body = String::from_utf8(prepared.body.expect("body").bytes).unwrap();
        assert_eq!(body, r#"{"email":"a@b.com","password":"hunter2"}"#);
    }

    #[test]
    fn vault_ref_resolves_in_a_nested_object_and_an_array() {
        let prepared = prep_with(
            json!({
                "method": "POST", "endpoint": "svc", "path": "/login",
                "body": {"auth": {"pw": "[[vault:password]]"}, "list": ["[[vault:password]]"]}
            }),
            Auth::None,
            &[("http/endpoints/svc/creds/password", "hunter2")],
        )
        .expect("valid");
        let body = String::from_utf8(prepared.body.expect("body").bytes).unwrap();
        assert!(body.contains(r#""pw":"hunter2""#), "{body}");
        assert!(body.contains(r#"["hunter2"]"#), "{body}");
    }

    #[test]
    fn vault_ref_in_a_json_key_is_never_resolved() {
        let prepared = prep_with(
            json!({
                "method": "POST", "endpoint": "svc", "path": "/login",
                "body": {"[[vault:password]]": "literal"}
            }),
            Auth::None,
            &[("http/endpoints/svc/creds/password", "hunter2")],
        )
        .expect("valid");
        let body = String::from_utf8(prepared.body.expect("body").bytes).unwrap();
        assert!(body.contains("[[vault:password]]"), "{body}");
        assert!(!body.contains("hunter2"), "{body}");
    }

    #[test]
    fn a_fully_marker_string_stays_a_json_string() {
        let prepared = prep_with(
            json!({
                "method": "POST", "endpoint": "svc", "path": "/x",
                "body": {"port": "[[vault:port]]"}
            }),
            Auth::None,
            &[("http/endpoints/svc/creds/port", "8080")],
        )
        .expect("valid");
        let body = String::from_utf8(prepared.body.expect("body").bytes).unwrap();
        assert_eq!(body, r#"{"port":"8080"}"#);
    }

    #[test]
    fn unknown_vault_key_fails_closed_without_naming_the_key() {
        let msg = prep_with(
            json!({
                "method": "POST", "endpoint": "svc", "path": "/x",
                "body": {"pw": "[[vault:nope]]"}
            }),
            Auth::None,
            &[],
        )
        .expect_err("expected rejection")
        .to_string();
        assert!(!msg.contains("nope"), "{msg}");
        assert!(!msg.contains("http/endpoints"), "{msg}");
    }

    #[test]
    fn vault_ref_resolves_in_headers_and_query_too() {
        let prepared = prep_with(
            json!({
                "method": "GET", "endpoint": "svc", "path": "/x",
                "headers": {"x-key": "[[vault:key]]"},
                "query": {"t": "[[vault:key]]"}
            }),
            Auth::None,
            &[("http/endpoints/svc/creds/key", "abc123")],
        )
        .expect("valid");
        assert_eq!(prepared.headers["x-key"], "abc123");
        assert_eq!(prepared.url.query(), Some("t=abc123"));
    }

    // ── auth ────────────────────────────────────────────────────────

    #[test]
    fn bearer_auth_sets_the_authorization_header() {
        let prepared = prep_with(
            json!({"method": "GET", "endpoint": "svc", "path": "/x"}),
            Auth::Bearer {
                token: "tok".into(),
            },
            &[],
        )
        .expect("valid");
        assert_eq!(prepared.headers[AUTHORIZATION], "Bearer tok");
        assert!(prepared.headers[AUTHORIZATION].is_sensitive());
    }

    #[test]
    fn basic_auth_base64_encodes_the_pair() {
        let prepared = prep_with(
            json!({"method": "GET", "endpoint": "svc", "path": "/x"}),
            Auth::Basic {
                username: "u".into(),
                password: "p".into(),
            },
            &[],
        )
        .expect("valid");
        assert_eq!(prepared.headers[AUTHORIZATION], "Basic dTpw");
    }

    #[test]
    fn header_auth_uses_its_configured_name() {
        let prepared = prep_with(
            json!({"method": "GET", "endpoint": "svc", "path": "/x"}),
            Auth::Header {
                name: "X-Api-Key".into(),
                value: "k".into(),
            },
            &[],
        )
        .expect("valid");
        assert_eq!(prepared.headers["x-api-key"], "k");
    }

    #[test]
    fn an_authenticated_request_never_follows_redirects() {
        let authed = prep_with(
            json!({"method": "GET", "endpoint": "svc", "path": "/x"}),
            Auth::Bearer {
                token: "tok".into(),
            },
            &[],
        )
        .expect("valid");
        assert!(!authed.follow_redirects);

        let anonymous =
            prep(json!({"method": "GET", "url": "https://e.example.com"})).expect("valid");
        assert!(anonymous.follow_redirects);
    }

    /// The regression this pins: `follow_redirects` used to key off the
    /// endpoint's configured `Auth` alone, so an endpoint with NO declared auth
    /// that receives its credential through `[[vault:…]]` — the shape the setup
    /// hint tells operators to use for a login POST — still followed redirects.
    /// `reqwest` strips only `Authorization`/`Cookie`/`Proxy-Authorization`/
    /// `WWW-Authenticate` cross-host, so a custom auth header would have been
    /// carried to the redirect target and a 307/308 would have replayed the
    /// body containing the password.
    #[test]
    fn a_vault_resolved_credential_pins_the_request_even_with_no_endpoint_auth() {
        let prepared = prep_with(
            json!({
                "method": "POST", "endpoint": "svc", "path": "/login",
                "body": {"password": "[[vault:password]]"}
            }),
            Auth::None,
            &[("http/endpoints/svc/creds/password", "hunter2")],
        )
        .expect("valid");
        assert!(
            !prepared.follow_redirects,
            "a resolved credential must pin the request to its host"
        );

        let header_credential = prep_with(
            json!({
                "method": "GET", "endpoint": "svc", "path": "/x",
                "headers": {"x-api-key": "[[vault:key]]"}
            }),
            Auth::None,
            &[("http/endpoints/svc/creds/key", "abc123")],
        )
        .expect("valid");
        assert!(!header_credential.follow_redirects);
    }

    #[test]
    fn an_agent_supplied_authorization_header_also_pins_the_request() {
        let prepared = prep(json!({
            "method": "GET", "endpoint": "svc", "path": "/x",
            "headers": {"Authorization": "Bearer mine"}
        }))
        .expect("valid");
        assert!(!prepared.follow_redirects);
    }

    #[test]
    fn a_request_with_no_credential_at_all_still_follows_redirects() {
        let prepared = prep_with(
            json!({
                "method": "POST", "endpoint": "svc", "path": "/x", "body": {"a": 1}
            }),
            Auth::None,
            &[],
        )
        .expect("valid");
        assert!(prepared.follow_redirects);
    }

    #[test]
    fn vault_refs_are_rejected_in_path_and_url() {
        let in_path = prep_with(
            json!({"method": "GET", "endpoint": "svc", "path": "/[[vault:key]]"}),
            Auth::None,
            &[("http/endpoints/svc/creds/key", "abc")],
        )
        .expect_err("rejected")
        .to_string();
        assert!(in_path.contains("not in `path` or `url`"), "{in_path}");

        let in_url = err_of(json!({
            "method": "GET", "url": "https://e.example.com/[[vault:key]]"
        }));
        assert!(in_url.contains("not in `path` or `url`"), "{in_url}");
    }

    // ── session secrets (`cmd: secret` → `[[secret:<name>]]`) ────────

    /// Run `prepare` with a named-secret cache installed and a session key in
    /// scope, the way a real conversational turn does.
    fn prep_in_session(
        input: Value,
        secrets: &[(&str, &str)],
        auth: Auth,
        creds: &[(&str, &str)],
    ) -> Result<PreparedRequest> {
        let _guard = crate::security::test_support::SECRET_CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let entries: Vec<(&str, &str, &str)> = secrets
            .iter()
            .map(|(name, value)| ("sess-1", *name, *value))
            .collect();
        workforce_common::secret_marker::clear_global_cache();
        workforce_common::secret_marker::set_global_cache(
            crate::security::test_support::stub_named_secret_cache(&entries),
        );
        let out =
            CURRENT_SESSION_KEY.sync_scope("sess-1".to_string(), || prep_with(input, auth, creds));
        workforce_common::secret_marker::clear_global_cache();
        out
    }

    /// The reported bug: a login POST whose body mixed plain fields with two
    /// `[[secret:…]]` values sent the literal marker text to the remote service
    /// and came back 401, because `http.request` was not a secret sink and the
    /// registry that governed sinks substituted flat top-level strings only.
    #[test]
    fn a_json_body_resolves_session_secrets_and_leaves_everything_else_alone() {
        let prepared = prep_in_session(
            json!({
                "method": "POST", "url": "https://keycloak.example.com/token",
                "body": {
                    "grant_type": "password",
                    "client_id": "user-service",
                    "username": "[[secret:email]]",
                    "password": "[[secret:password]]",
                    "ttl": 300,
                    "nested": {"hint": "[[secret:email]]"},
                }
            }),
            &[("email", "a@b.com"), ("password", "hunter2")],
            Auth::None,
            &[],
        )
        .expect("valid");
        let body = String::from_utf8(prepared.body.expect("body").bytes).unwrap();
        assert!(body.contains(r#""username":"a@b.com""#), "{body}");
        assert!(body.contains(r#""password":"hunter2""#), "{body}");
        assert!(body.contains(r#""grant_type":"password""#), "{body}");
        assert!(body.contains(r#""ttl":300"#), "{body}");
        assert!(body.contains(r#""hint":"a@b.com""#), "{body}");
        assert!(!body.contains("[[secret:"), "{body}");
    }

    #[test]
    fn session_secrets_resolve_in_headers_query_and_body_text() {
        let prepared = prep_in_session(
            json!({
                "method": "POST", "endpoint": "svc", "path": "/x",
                "headers": {"x-api-key": "[[secret:token]]"},
                "query": {"t": "[[secret:token]]"},
                "body_text": "token=[[secret:token]]&plain=1"
            }),
            &[("token", "abc123")],
            Auth::None,
            &[],
        )
        .expect("valid");
        assert_eq!(prepared.headers["x-api-key"], "abc123");
        assert_eq!(prepared.url.query(), Some("t=abc123"));
        let body = String::from_utf8(prepared.body.expect("body").bytes).unwrap();
        assert_eq!(body, "token=abc123&plain=1");
    }

    /// Unlike `[[vault:…]]`, a session secret is not pinned to an operator-
    /// configured host — the user pasted it into this conversation for this
    /// task, the same trust `browser.fill` already acts on.
    #[test]
    fn a_session_secret_resolves_for_an_adhoc_url_and_pins_the_request() {
        let prepared = prep_in_session(
            json!({
                "method": "POST", "url": "https://api.example.com/login",
                "body": {"password": "[[secret:password]]"}
            }),
            &[("password", "hunter2")],
            Auth::None,
            &[],
        )
        .expect("valid");
        assert!(
            !prepared.follow_redirects,
            "a resolved credential must pin the request to its host"
        );
        let body = String::from_utf8(prepared.body.expect("body").bytes).unwrap();
        assert_eq!(body, r#"{"password":"hunter2"}"#);
    }

    /// The failure mode the whole report was about: an unresolvable marker used
    /// to reach the remote service as text and return a generic 401. It must be
    /// a tool error that names the marker instead.
    #[test]
    fn an_unknown_session_secret_fails_the_call_rather_than_going_on_the_wire() {
        let msg = prep_in_session(
            json!({
                "method": "POST", "url": "https://api.example.com/login",
                "body": {"password": "[[secret:expired]]"}
            }),
            &[("password", "hunter2")],
            Auth::None,
            &[],
        )
        .expect_err("expected rejection")
        .to_string();
        assert!(msg.contains("expired"), "{msg}");
        assert!(msg.contains("cmd: secret"), "{msg}");
    }

    /// Outside a conversation there is no secret cache to resolve against, so
    /// a marker must fail closed rather than be transmitted verbatim.
    #[test]
    fn a_session_secret_outside_a_session_fails_closed() {
        let msg = prep(json!({
            "method": "POST", "url": "https://api.example.com/login",
            "body": {"password": "[[secret:password]]"}
        }))
        .expect_err("expected rejection")
        .to_string();
        assert!(msg.contains("not found"), "{msg}");
    }

    #[test]
    fn a_body_with_no_references_never_pins_the_request() {
        let prepared = prep_in_session(
            json!({
                "method": "POST", "url": "https://api.example.com/x",
                "body": {"a": 1, "b": "plain"}
            }),
            &[("password", "hunter2")],
            Auth::None,
            &[],
        )
        .expect("valid");
        assert!(prepared.follow_redirects);
    }

    #[test]
    fn session_secrets_are_rejected_in_path_and_url() {
        let in_path = prep_in_session(
            json!({"method": "GET", "endpoint": "svc", "path": "/[[secret:token]]"}),
            &[("token", "abc")],
            Auth::None,
            &[],
        )
        .expect_err("rejected")
        .to_string();
        assert!(in_path.contains("not in `path` or `url`"), "{in_path}");

        let in_url = prep_in_session(
            json!({"method": "GET", "url": "https://e.example.com/[[secret:token]]"}),
            &[("token", "abc")],
            Auth::None,
            &[],
        )
        .expect_err("rejected")
        .to_string();
        assert!(in_url.contains("not in `path` or `url`"), "{in_url}");
    }

    #[test]
    fn debug_of_a_prepared_request_drops_a_credential_in_the_query() {
        let prepared = prep_in_session(
            json!({
                "method": "GET", "url": "https://api.example.com/x",
                "query": {"api_key": "[[secret:token]]"}
            }),
            &[("token", "super-secret-token")],
            Auth::None,
            &[],
        )
        .expect("valid");
        assert_eq!(prepared.url.query(), Some("api_key=super-secret-token"));
        let rendered = format!("{prepared:?}");
        assert!(!rendered.contains("super-secret-token"), "{rendered}");
        assert!(rendered.contains("https://api.example.com/x"), "{rendered}");
    }

    #[test]
    fn a_session_secret_and_a_vault_credential_can_share_one_request() {
        let prepared = prep_in_session(
            json!({
                "method": "POST", "endpoint": "svc", "path": "/login",
                "body": {"user": "[[vault:username]]", "password": "[[secret:password]]"}
            }),
            &[("password", "session-pw")],
            Auth::None,
            &[("http/endpoints/svc/creds/username", "svc-user")],
        )
        .expect("valid");
        let body = String::from_utf8(prepared.body.expect("body").bytes).unwrap();
        assert_eq!(body, r#"{"password":"session-pw","user":"svc-user"}"#);
    }

    #[test]
    fn agent_authorization_header_is_rejected_on_an_authenticated_endpoint() {
        let msg = prep_with(
            json!({
                "method": "GET", "endpoint": "svc", "path": "/x",
                "headers": {"Authorization": "Bearer mine"}
            }),
            Auth::Bearer {
                token: "tok".into(),
            },
            &[],
        )
        .expect_err("expected rejection")
        .to_string();
        assert!(msg.contains("configured credential"), "{msg}");
    }

    #[test]
    fn agent_authorization_header_is_allowed_when_the_endpoint_has_no_credential() {
        let prepared = prep(json!({
            "method": "GET", "endpoint": "svc", "path": "/x",
            "headers": {"Authorization": "Bearer mine"}
        }))
        .expect("valid");
        assert_eq!(prepared.headers[AUTHORIZATION], "Bearer mine");
    }

    // ── headers, body, query ────────────────────────────────────────

    #[test]
    fn framing_headers_are_rejected() {
        for header in ["Host", "content-length", "Connection", "transfer-encoding"] {
            let msg = err_of(json!({
                "method": "GET", "url": "https://e.example.com",
                "headers": {header: "x"}
            }));
            assert!(msg.contains("cannot be overridden"), "{header}: {msg}");
        }
    }

    #[test]
    fn a_header_value_with_a_newline_is_a_clean_error() {
        let msg = err_of(json!({
            "method": "GET", "url": "https://e.example.com",
            "headers": {"x-a": "value\r\nInjected: yes"}
        }));
        assert!(msg.contains("invalid value for header"), "{msg}");
    }

    #[test]
    fn a_non_string_header_value_is_rejected() {
        let msg = err_of(json!({
            "method": "GET", "url": "https://e.example.com", "headers": {"x-a": 1}
        }));
        assert!(msg.contains("must be a string"), "{msg}");
    }

    #[test]
    fn body_and_body_text_together_are_rejected() {
        let msg = err_of(json!({
            "method": "POST", "url": "https://e.example.com",
            "body": {"a": 1}, "body_text": "a"
        }));
        assert!(msg.contains("not both"), "{msg}");
    }

    #[test]
    fn a_body_on_get_is_rejected() {
        let msg = err_of(json!({
            "method": "GET", "url": "https://e.example.com", "body": {"a": 1}
        }));
        assert!(msg.contains("takes no request body"), "{msg}");
    }

    #[test]
    fn json_body_defaults_its_content_type() {
        let prepared = prep(json!({
            "method": "POST", "url": "https://e.example.com", "body": {"a": 1}
        }))
        .expect("valid");
        let body = prepared.body.expect("body");
        assert_eq!(body.content_type, Some(JSON_CONTENT_TYPE));
        assert_eq!(String::from_utf8(body.bytes).unwrap(), r#"{"a":1}"#);
    }

    #[test]
    fn body_text_defaults_to_text_plain() {
        let prepared = prep(json!({
            "method": "POST", "url": "https://e.example.com", "body_text": "hello"
        }))
        .expect("valid");
        assert_eq!(
            prepared.body.expect("body").content_type,
            Some(TEXT_CONTENT_TYPE)
        );
    }

    #[test]
    fn an_explicit_content_type_header_wins_over_the_default() {
        let prepared = prep(json!({
            "method": "POST", "url": "https://e.example.com",
            "headers": {"content-type": "application/xml"},
            "body_text": "<a/>"
        }))
        .expect("valid");
        assert_eq!(prepared.body.expect("body").content_type, None);
        assert_eq!(prepared.headers[CONTENT_TYPE], "application/xml");
    }

    #[test]
    fn query_params_are_appended_to_an_existing_query_string() {
        let prepared = prep(json!({
            "method": "GET", "url": "https://e.example.com/x?a=1", "query": {"b": "2"}
        }))
        .expect("valid");
        assert_eq!(prepared.url.query(), Some("a=1&b=2"));
    }

    #[test]
    fn a_non_string_query_value_is_rejected() {
        let msg = err_of(json!({
            "method": "GET", "url": "https://e.example.com", "query": {"a": 1}
        }));
        assert!(msg.contains("must be a string"), "{msg}");
    }

    // ── methods and limits ──────────────────────────────────────────

    #[test]
    fn an_unsupported_method_is_rejected() {
        let msg = err_of(json!({"method": "TRACE", "url": "https://e.example.com"}));
        assert!(msg.contains("unsupported method"), "{msg}");
    }

    #[test]
    fn a_missing_method_is_rejected() {
        let msg = err_of(json!({"url": "https://e.example.com"}));
        assert!(msg.contains("`method` is required"), "{msg}");
    }

    #[test]
    fn is_mutating_request_fails_closed() {
        assert!(!is_mutating_request(&json!({"method": "GET"})));
        assert!(!is_mutating_request(&json!({"method": "head"})));
        for method in ["POST", "PUT", "PATCH", "DELETE"] {
            assert!(
                is_mutating_request(&json!({ "method": method })),
                "{method}"
            );
        }
        assert!(is_mutating_request(&json!({})));
        assert!(is_mutating_request(&json!({"method": "NONSENSE"})));
        assert!(is_mutating_request(&json!({"method": 7})));
    }

    #[test]
    fn max_chars_is_clamped() {
        let at_cap = prep(json!({
            "method": "GET", "url": "https://e.example.com", "max_chars": u64::MAX
        }))
        .expect("valid");
        assert_eq!(at_cap.max_chars, MAX_MAX_CHARS);

        let defaulted =
            prep(json!({"method": "GET", "url": "https://e.example.com"})).expect("valid");
        assert_eq!(defaulted.max_chars, DEFAULT_MAX_CHARS);
    }
}
