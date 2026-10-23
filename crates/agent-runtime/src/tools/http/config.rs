//! Endpoint configuration for the outbound HTTP tools (`http.*`).
//!
//! An endpoint is an operator-provisioned target: a base URL plus, optionally,
//! the credential to authenticate against it. Both live in the vault under
//! [`ENDPOINTS_PREFIX`], a namespace denied to the agent's `vault.*` tools
//! (`hub::vault_policy::agent_can_read`), so an agent can name an endpoint but
//! can neither read its credential back into context nor create one.
//!
//! Every read honours the per-user overlay before the shared value, so an
//! individual user can point an endpoint at their own credential.

use std::collections::BTreeSet;
use std::fmt;
use std::time::Duration;

use eyre::{eyre, Result};
use workforce_common::settings::SecretStore;

/// Top-level vault namespace for outbound HTTP configuration.
pub const VAULT_PREFIX: &str = "http/";
/// Prefix under which named endpoints live: `http/endpoints/<name>/<field>`.
pub const ENDPOINTS_PREFIX: &str = "http/endpoints/";

pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const MIN_TIMEOUT_MS: u64 = 1_000;
/// Ceiling on a single request. Kept well under `runner::CHECK_IN_INTERVAL` so
/// the tool's own timeout always fires before the turn's silence budget.
pub const MAX_TIMEOUT_MS: u64 = 120_000;

/// Default header name for `auth = header` when none is configured.
const DEFAULT_AUTH_HEADER: &str = "X-API-Key";

/// How a request to an endpoint authenticates. Holds live credentials, so its
/// [`fmt::Debug`] prints the kind only.
#[derive(Clone, PartialEq, Eq)]
pub enum Auth {
    None,
    Bearer { token: String },
    Basic { username: String, password: String },
    Header { name: String, value: String },
}

impl Auth {
    pub fn kind(&self) -> &'static str {
        match self {
            Auth::None => "none",
            Auth::Bearer { .. } => "bearer",
            Auth::Basic { .. } => "basic",
            Auth::Header { .. } => "header",
        }
    }

    pub fn is_authenticated(&self) -> bool {
        !matches!(self, Auth::None)
    }
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Auth::{}", self.kind())
    }
}

/// One configured endpoint.
#[derive(Clone, Debug)]
pub struct EndpointConfig {
    pub name: String,
    pub base_url: String,
    pub auth: Auth,
    pub timeout: Duration,
}

impl EndpointConfig {
    /// A secret-free summary for `http.endpoints`. The base URL is stripped of
    /// any `user:pass@` userinfo — `reqwest` sends that as basic auth, so it is
    /// a credential and must not be handed to the model.
    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "endpoint": self.name,
            "base_url": redact_userinfo(&self.base_url),
            "auth": self.auth.kind(),
            "timeout_ms": self.timeout.as_millis() as u64,
        })
    }

    /// [`Self::summary`] plus what this endpoint is still short of: the auth
    /// fields its declared kind needs but lacks, and the `creds/` keys that
    /// *are* set (names only — a value never leaves the vault).
    ///
    /// Worth reporting because the gap is otherwise silent: [`read_endpoint`]
    /// falls back to [`Auth::None`] when a declared kind has no credential, so
    /// a half-configured endpoint looks configured and fails at request time
    /// with a 401 from the remote service.
    pub fn summary_with_gaps(&self, store: &dyn SecretStore) -> serde_json::Value {
        let mut out = self.summary();
        let missing = self.missing_auth_fields(store);
        let map = out.as_object_mut().expect("summary is an object");
        map.insert("creds_keys".into(), creds_keys(store, &self.name).into());
        map.insert("ready".into(), missing.is_empty().into());
        if !missing.is_empty() {
            map.insert("missing".into(), missing.into());
        }
        out
    }

    /// Auth fields the endpoint's *declared* `auth` requires but does not have.
    /// Empty when nothing is declared — an endpoint with no auth is legal.
    pub fn missing_auth_fields(&self, store: &dyn SecretStore) -> Vec<String> {
        let field = |f: &str| format!("{}{}/{f}", ENDPOINTS_PREFIX, self.name);
        let declared = resolve_at(store, &field("auth")).unwrap_or_default();
        let absent = |f: &str| resolve_at(store, &field(f)).is_none();
        match declared.trim().to_ascii_lowercase().as_str() {
            "bearer" | "header" => ["token"].iter(),
            "basic" => ["username", "password"].iter(),
            _ => [].iter(),
        }
        .filter(|f| absent(f))
        .map(|f| (*f).to_string())
        .collect()
    }

    /// Vault prefix the endpoint's own `[[vault:<key>]]` references resolve
    /// against. Scoping it under the endpoint is what binds a credential to the
    /// single host it may be sent to.
    pub fn creds_prefix(&self) -> String {
        format!("{}{}/creds/", ENDPOINTS_PREFIX, self.name)
    }
}

/// Credential keys set under an endpoint's `creds/`, sorted, names only.
pub fn creds_keys(store: &dyn SecretStore, name: &str) -> Vec<String> {
    let prefix = format!("{ENDPOINTS_PREFIX}{name}/creds/");
    let mut keys: Vec<String> = store
        .list(Some(&prefix))
        .into_iter()
        .filter_map(|p| p.strip_prefix(&prefix).map(str::to_string))
        .filter(|k| !k.is_empty())
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// Reject an agent-supplied endpoint name that could escape the endpoint
/// namespace — a name containing `/` or `..` would otherwise let a crafted
/// call read `databases/` or another user's credentials through the field
/// path this module builds.
pub fn validate_name(name: &str) -> Result<&str> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.contains("..");
    ok.then_some(name).ok_or_else(|| {
        eyre!(
            "invalid endpoint name '{name}': use letters, digits, '-', '_', or '.' \
             (no path separators)"
        )
    })
}

/// Strip `user:pass@` from a URL's authority, leaving the rest intact. Falls
/// back to the scheme and host alone if the URL will not parse, so a
/// credential can never survive by malforming the input.
///
/// Public because every surface that renders a `base_url` — the tool summary
/// and `cmd: endpoint list` alike — has to apply it: `reqwest` sends userinfo
/// as basic auth, so it is a credential wherever it appears.
pub fn redact_userinfo(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(mut url) if !url.username().is_empty() || url.password().is_some() => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.to_string()
        }
        Ok(_) => raw.to_string(),
        Err(_) => "<unparseable base_url>".to_string(),
    }
}

/// Read a secret honouring the per-user overlay before the shared value.
pub fn resolve_at(store: &dyn SecretStore, path: &str) -> Option<String> {
    store.get_user_override(path).or_else(|| store.get(path))
}

fn endpoint_field(name: &str, field: &str) -> String {
    format!("{ENDPOINTS_PREFIX}{name}/{field}")
}

/// Resolve the authentication for an endpoint. An explicit `auth` value wins;
/// otherwise a token implies bearer and a username/password pair implies basic.
fn read_auth(field: &impl Fn(&str) -> Option<String>) -> Auth {
    let token = field("token");
    let username = field("username");
    let password = field("password");
    let declared = field("auth").map(|a| a.trim().to_ascii_lowercase());

    let header = |value: String| Auth::Header {
        name: field("header_name").unwrap_or_else(|| DEFAULT_AUTH_HEADER.to_string()),
        value,
    };

    match declared.as_deref() {
        Some("none") => Auth::None,
        Some("bearer") => token.map_or(Auth::None, |token| Auth::Bearer { token }),
        Some("basic") => match (username, password) {
            (Some(username), Some(password)) => Auth::Basic { username, password },
            _ => Auth::None,
        },
        Some("header") => token.map_or(Auth::None, header),
        _ => match (token, username, password) {
            (Some(token), _, _) => Auth::Bearer { token },
            (None, Some(username), Some(password)) => Auth::Basic { username, password },
            _ => Auth::None,
        },
    }
}

/// Read one endpoint. `None` when no `base_url` is configured — an endpoint
/// without a target is not a usable endpoint.
pub fn read_endpoint(store: &dyn SecretStore, name: &str) -> Option<EndpointConfig> {
    // Validated here as well as at the call site: this function builds vault
    // paths from `name`, so the guard belongs with the path construction rather
    // than only with today's one caller.
    validate_name(name).ok()?;
    let field = |f: &str| resolve_at(store, &endpoint_field(name, f));
    let base_url = field("base_url").map(|u| u.trim().to_string())?;
    if base_url.is_empty() {
        return None;
    }
    let timeout = Duration::from_millis(
        field("timeout_ms")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS),
    );
    Some(EndpointConfig {
        name: name.to_string(),
        base_url,
        auth: read_auth(&field),
        timeout,
    })
}

/// Every configured endpoint, sorted by name. Reads non-secret metadata only.
pub fn list_endpoints(store: &dyn SecretStore) -> Vec<EndpointConfig> {
    store
        .list(Some(ENDPOINTS_PREFIX))
        .into_iter()
        .filter_map(|path| {
            path.strip_prefix(ENDPOINTS_PREFIX)
                .and_then(|rest| rest.split('/').next())
                .filter(|name| !name.is_empty() && validate_name(name).is_ok())
                .map(str::to_string)
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|name| read_endpoint(store, &name))
        .collect()
}

/// Join an endpoint's base URL and a request path on exactly one `/`.
pub fn join_url(base_url: &str, path: &str) -> String {
    format!("{}{}", base_url.trim_end_matches('/'), path)
}

/// Clamp a caller-supplied timeout, falling back to the endpoint's default.
pub fn clamp_timeout(requested: Option<u64>, default: Duration) -> Duration {
    requested.map_or(default, |ms| {
        Duration::from_millis(ms.clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct StubStore {
        shared: Mutex<HashMap<String, String>>,
        user: Mutex<HashMap<String, String>>,
    }

    impl StubStore {
        fn with(entries: &[(&str, &str)]) -> Self {
            let store = StubStore::default();
            for (k, v) in entries {
                store
                    .shared
                    .lock()
                    .unwrap()
                    .insert((*k).to_string(), (*v).to_string());
            }
            store
        }

        fn with_user_override(self, path: &str, value: &str) -> Self {
            self.user
                .lock()
                .unwrap()
                .insert(path.to_string(), value.to_string());
            self
        }
    }

    impl SecretStore for StubStore {
        fn get(&self, path: &str) -> Option<String> {
            self.shared.lock().unwrap().get(path).cloned()
        }
        fn set(&self, _: &str, _: &str) -> eyre::Result<()> {
            unreachable!("tests do not write")
        }
        fn delete(&self, _: &str) -> eyre::Result<bool> {
            unreachable!("tests do not delete")
        }
        fn list(&self, prefix: Option<&str>) -> Vec<String> {
            self.shared
                .lock()
                .unwrap()
                .keys()
                .filter(|k| prefix.is_none_or(|p| k.starts_with(p)))
                .cloned()
                .collect()
        }
        fn get_user_override(&self, path: &str) -> Option<String> {
            self.user.lock().unwrap().get(path).cloned()
        }
    }

    #[test]
    fn endpoint_without_base_url_is_not_configured() {
        let store = StubStore::with(&[("http/endpoints/a/token", "t")]);
        assert!(read_endpoint(&store, "a").is_none());
        assert!(list_endpoints(&store).is_empty());
    }

    #[test]
    fn token_alone_infers_bearer() {
        let store = StubStore::with(&[
            ("http/endpoints/a/base_url", "https://api.example.com"),
            ("http/endpoints/a/token", "tok"),
        ]);
        let cfg = read_endpoint(&store, "a").expect("configured");
        assert_eq!(
            cfg.auth,
            Auth::Bearer {
                token: "tok".into()
            }
        );
    }

    #[test]
    fn username_and_password_infer_basic() {
        let store = StubStore::with(&[
            ("http/endpoints/a/base_url", "https://api.example.com"),
            ("http/endpoints/a/username", "u"),
            ("http/endpoints/a/password", "p"),
        ]);
        let cfg = read_endpoint(&store, "a").expect("configured");
        assert_eq!(
            cfg.auth,
            Auth::Basic {
                username: "u".into(),
                password: "p".into()
            }
        );
    }

    #[test]
    fn explicit_auth_none_overrides_a_present_token() {
        let store = StubStore::with(&[
            ("http/endpoints/a/base_url", "https://api.example.com"),
            ("http/endpoints/a/token", "tok"),
            ("http/endpoints/a/auth", "none"),
        ]);
        let cfg = read_endpoint(&store, "a").expect("configured");
        assert_eq!(cfg.auth, Auth::None);
        assert!(!cfg.auth.is_authenticated());
    }

    #[test]
    fn header_auth_defaults_its_header_name() {
        let store = StubStore::with(&[
            ("http/endpoints/a/base_url", "https://api.example.com"),
            ("http/endpoints/a/token", "k"),
            ("http/endpoints/a/auth", "header"),
        ]);
        let cfg = read_endpoint(&store, "a").expect("configured");
        assert_eq!(
            cfg.auth,
            Auth::Header {
                name: DEFAULT_AUTH_HEADER.into(),
                value: "k".into()
            }
        );
    }

    #[test]
    fn header_auth_honours_a_configured_header_name() {
        let store = StubStore::with(&[
            ("http/endpoints/a/base_url", "https://api.example.com"),
            ("http/endpoints/a/token", "k"),
            ("http/endpoints/a/auth", "header"),
            ("http/endpoints/a/header_name", "X-Custom"),
        ]);
        let cfg = read_endpoint(&store, "a").expect("configured");
        assert_eq!(
            cfg.auth,
            Auth::Header {
                name: "X-Custom".into(),
                value: "k".into()
            }
        );
    }

    #[test]
    fn declared_auth_without_its_credential_falls_back_to_none() {
        let store = StubStore::with(&[
            ("http/endpoints/a/base_url", "https://api.example.com"),
            ("http/endpoints/a/auth", "basic"),
            ("http/endpoints/a/username", "u"),
        ]);
        let cfg = read_endpoint(&store, "a").expect("configured");
        assert_eq!(cfg.auth, Auth::None);
    }

    #[test]
    fn user_override_wins_over_the_shared_value() {
        let store = StubStore::with(&[
            ("http/endpoints/a/base_url", "https://api.example.com"),
            ("http/endpoints/a/token", "shared"),
        ])
        .with_user_override("http/endpoints/a/token", "mine");
        let cfg = read_endpoint(&store, "a").expect("configured");
        assert_eq!(
            cfg.auth,
            Auth::Bearer {
                token: "mine".into()
            }
        );
    }

    #[test]
    fn list_endpoints_is_deduplicated_and_sorted() {
        let store = StubStore::with(&[
            ("http/endpoints/beta/base_url", "https://b.example.com"),
            ("http/endpoints/beta/token", "t"),
            ("http/endpoints/alpha/base_url", "https://a.example.com"),
        ]);
        let names: Vec<_> = list_endpoints(&store).into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    /// `read_endpoint` silently degrades a declared-but-uncredentialled auth to
    /// `Auth::None`, so the endpoint looks fine and 401s at request time. The
    /// gap report is what makes that visible before the call.
    #[test]
    fn summary_with_gaps_names_the_missing_auth_field() {
        let store = StubStore::with(&[
            ("http/endpoints/a/base_url", "https://api.example.com"),
            ("http/endpoints/a/auth", "basic"),
            ("http/endpoints/a/username", "u"),
        ]);
        let config = read_endpoint(&store, "a").expect("configured");
        let out = config.summary_with_gaps(&store);
        assert_eq!(out["ready"], serde_json::json!(false));
        assert_eq!(out["missing"], serde_json::json!(["password"]));
    }

    #[test]
    fn summary_with_gaps_lists_creds_keys_without_their_values() {
        let store = StubStore::with(&[
            ("http/endpoints/a/base_url", "https://api.example.com"),
            ("http/endpoints/a/creds/email", "a@b.com"),
            ("http/endpoints/a/creds/password", "hunter2"),
        ]);
        let config = read_endpoint(&store, "a").expect("configured");
        let out = config.summary_with_gaps(&store);
        assert_eq!(out["creds_keys"], serde_json::json!(["email", "password"]));
        assert_eq!(out["ready"], serde_json::json!(true));
        let rendered = out.to_string();
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(!rendered.contains("a@b.com"), "{rendered}");
    }

    #[test]
    fn an_endpoint_with_no_declared_auth_is_ready() {
        let store = StubStore::with(&[("http/endpoints/a/base_url", "https://api.example.com")]);
        let config = read_endpoint(&store, "a").expect("configured");
        let out = config.summary_with_gaps(&store);
        assert_eq!(out["ready"], serde_json::json!(true));
        assert!(out.get("missing").is_none());
    }

    #[test]
    fn summary_never_carries_a_credential() {
        let store = StubStore::with(&[
            ("http/endpoints/a/base_url", "https://api.example.com"),
            ("http/endpoints/a/token", "super-secret-token"),
        ]);
        let cfg = read_endpoint(&store, "a").expect("configured");
        let rendered = cfg.summary().to_string();
        assert!(!rendered.contains("super-secret-token"), "{rendered}");
        assert!(rendered.contains("bearer"));
    }

    #[test]
    fn debug_of_auth_redacts_the_credential() {
        let auth = Auth::Bearer {
            token: "super-secret-token".into(),
        };
        assert_eq!(format!("{auth:?}"), "Auth::bearer");
    }

    #[test]
    fn endpoint_names_that_could_escape_the_namespace_are_rejected() {
        for bad in [
            "",
            "../databases/instances/prod",
            "a/b",
            "..",
            "a..b",
            "has space",
        ] {
            assert!(validate_name(bad).is_err(), "{bad} should be rejected");
        }
        for good in ["harbor-staging", "a.b_c", "A1"] {
            assert!(validate_name(good).is_ok(), "{good} should be accepted");
        }
    }

    #[test]
    fn creds_prefix_is_scoped_to_the_endpoint() {
        let cfg = EndpointConfig {
            name: "harbor-staging".into(),
            base_url: "https://x".into(),
            auth: Auth::None,
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
        };
        assert_eq!(cfg.creds_prefix(), "http/endpoints/harbor-staging/creds/");
    }

    #[test]
    fn join_url_collapses_the_seam_to_one_slash() {
        assert_eq!(
            join_url("https://a.example.com/", "/v1/x"),
            "https://a.example.com/v1/x"
        );
        assert_eq!(
            join_url("https://a.example.com", "/v1/x"),
            "https://a.example.com/v1/x"
        );
        assert_eq!(
            join_url("https://a.example.com/base/", "/v1"),
            "https://a.example.com/base/v1"
        );
    }

    #[test]
    fn timeouts_clamp_at_both_bounds() {
        let default = Duration::from_millis(DEFAULT_TIMEOUT_MS);
        assert_eq!(clamp_timeout(None, default), default);
        assert_eq!(
            clamp_timeout(Some(1), default),
            Duration::from_millis(MIN_TIMEOUT_MS)
        );
        assert_eq!(
            clamp_timeout(Some(u64::MAX), default),
            Duration::from_millis(MAX_TIMEOUT_MS)
        );
    }

    #[test]
    fn configured_timeout_is_clamped_on_read() {
        let store = StubStore::with(&[
            ("http/endpoints/a/base_url", "https://api.example.com"),
            ("http/endpoints/a/timeout_ms", "999999"),
        ]);
        let cfg = read_endpoint(&store, "a").expect("configured");
        assert_eq!(cfg.timeout, Duration::from_millis(MAX_TIMEOUT_MS));
    }
}
