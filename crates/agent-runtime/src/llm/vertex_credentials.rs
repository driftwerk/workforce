//! Resolves Vertex AI credentials, project, and location from packed
//! configuration plus environment variables.
//!
//! The factory passes the user's `api_key` string in here; it is parsed into a
//! [`VertexCredentialSource`] describing whether to use Application Default
//! Credentials or a service-account JSON file. Project and location are taken
//! from explicit overrides or the standard GCP environment variables.

use std::path::PathBuf;

use eyre::{eyre, Result};

/// Sentinel `api_key` value meaning "use the Application Default Credentials
/// chain at request time". Mirrors [`super::bedrock_credentials::CHAIN_SENTINEL`]
/// for the AWS chain.
pub const CHAIN_SENTINEL: &str = "adc";

/// Where the Vertex client should pull its OAuth2 access token from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VertexCredentialSource {
    /// Application Default Credentials — `gcp_auth` walks
    /// `GOOGLE_APPLICATION_CREDENTIALS`, gcloud user creds, the metadata
    /// server, and workload identity.
    Adc,
    /// Explicit service-account JSON loaded from the given path.
    ServiceAccountFile(PathBuf),
    /// A service-account key's JSON itself, as a vault account holds it.
    ServiceAccountKey(ServiceAccountKey),
}

impl VertexCredentialSource {
    /// The project a service-account key names, if it is one that does.
    pub fn project_id(&self) -> Option<String> {
        match self {
            Self::ServiceAccountKey(key) => key.project_id(),
            Self::Adc | Self::ServiceAccountFile(_) => None,
        }
    }
}

/// A service-account key's JSON. Its `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct ServiceAccountKey(String);

impl ServiceAccountKey {
    pub fn json(&self) -> &str {
        &self.0
    }

    /// The `project_id` the key names.
    pub fn project_id(&self) -> Option<String> {
        serde_json::from_str::<serde_json::Value>(&self.0)
            .ok()?
            .get("project_id")?
            .as_str()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
    }
}

impl std::fmt::Debug for ServiceAccountKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ServiceAccountKey(<redacted>)")
    }
}

/// Parse the packed `api_key` field into a credential source.
///
/// Accepted forms:
/// - empty, `"adc"`, `"none"` → [`VertexCredentialSource::Adc`]
/// - a `.json` path that exists on disk → [`VertexCredentialSource::ServiceAccountFile`]
/// - a JSON object (a service-account key itself) → [`VertexCredentialSource::ServiceAccountKey`]
pub fn resolve_source(api_key: &str) -> Result<VertexCredentialSource> {
    let trimmed = api_key.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case(CHAIN_SENTINEL)
        || trimmed.eq_ignore_ascii_case("none")
    {
        return Ok(VertexCredentialSource::Adc);
    }
    if trimmed.starts_with('{') {
        return match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(serde_json::Value::Object(_)) => Ok(VertexCredentialSource::ServiceAccountKey(
                ServiceAccountKey(trimmed.to_string()),
            )),
            _ => Err(eyre!(
                "the Vertex service-account key is not a JSON object; paste the whole key file"
            )),
        };
    }
    let path = PathBuf::from(trimmed);
    let looks_like_json = path
        .extension()
        .map(|ext| ext.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    if looks_like_json && path.exists() {
        return Ok(VertexCredentialSource::ServiceAccountFile(path));
    }
    Err(eyre!(
        "Vertex credential source must be \"adc\" (or empty) for ADC, or a path \
         to an existing service-account JSON file; got {:?}",
        api_key
    ))
}

/// Resolve the target GCP project. Precedence: explicit → `GOOGLE_CLOUD_PROJECT`
/// → `GCP_PROJECT`. Errors with a setup hint when nothing is set.
pub fn resolve_project(explicit: Option<&str>) -> Result<String> {
    explicit
        .map(str::to_string)
        .or_else(|| std::env::var("GOOGLE_CLOUD_PROJECT").ok())
        .or_else(|| std::env::var("GCP_PROJECT").ok())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            eyre!(
                "Vertex provider requires a GCP project — set GOOGLE_CLOUD_PROJECT \
                 (or run `gcloud config set project <id>`)"
            )
        })
}

/// Resolve the Vertex location. Precedence: explicit → `VERTEX_LOCATION`
/// → `GOOGLE_CLOUD_REGION`. Errors with a list of common Claude regions
/// when unset.
pub fn resolve_location(explicit: Option<&str>) -> Result<String> {
    explicit
        .map(str::to_string)
        .or_else(|| std::env::var("VERTEX_LOCATION").ok())
        .or_else(|| std::env::var("GOOGLE_CLOUD_REGION").ok())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            eyre!(
                "Vertex provider requires a location — set VERTEX_LOCATION \
                 (Claude is hosted in us-east5, europe-west1, asia-southeast1, \
                 and a few others; consult Vertex Model Garden)"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn a_pasted_key_is_a_source_that_names_its_project_and_never_shows_itself() {
        let json =
            r#"{"type":"service_account","project_id":"acme-ai","private_key":"-----BEGIN"}"#;
        let source = resolve_source(&format!("  {json}\n")).unwrap();
        assert_eq!(source.project_id().as_deref(), Some("acme-ai"));
        let shown = format!("{source:?}");
        assert!(
            !shown.contains("BEGIN") && !shown.contains("acme"),
            "{shown}"
        );
        let err = resolve_source("{not json").unwrap_err().to_string();
        assert!(!err.contains("not json"), "{err}");
    }

    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let saved: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(k, _)| (k.to_string(), std::env::var(k).ok()))
            .collect();
        for (k, v) in vars {
            match v {
                Some(value) => std::env::set_var(k, value),
                None => std::env::remove_var(k),
            }
        }
        f();
        for (k, v) in saved {
            match v {
                Some(value) => std::env::set_var(&k, value),
                None => std::env::remove_var(&k),
            }
        }
    }

    #[test]
    fn resolve_source_adc_aliases() {
        for input in ["", "adc", "ADC", "  adc  ", "none", "NONE"] {
            assert_eq!(
                resolve_source(input).unwrap(),
                VertexCredentialSource::Adc,
                "input={input:?}"
            );
        }
    }

    #[test]
    fn resolve_source_service_account_file() {
        let tmp = tempfile::NamedTempFile::with_suffix(".json").unwrap();
        let path = tmp.path().to_path_buf();
        let resolved = resolve_source(path.to_str().unwrap()).unwrap();
        assert_eq!(resolved, VertexCredentialSource::ServiceAccountFile(path));
    }

    #[test]
    fn resolve_source_rejects_missing_file() {
        let result = resolve_source("/definitely/not/a/real/path/key.json");
        assert!(result.is_err(), "expected error for nonexistent file");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("adc"),
            "error should mention adc fallback: {msg}"
        );
    }

    #[test]
    fn resolve_source_rejects_garbage() {
        let result = resolve_source("not-a-path-not-adc");
        assert!(result.is_err());
    }

    #[test]
    fn resolve_source_rejects_existing_non_json() {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".txt").unwrap();
        writeln!(tmp, "not json").unwrap();
        let result = resolve_source(tmp.path().to_str().unwrap());
        assert!(result.is_err(), "non-json file should be rejected");
    }

    #[test]
    fn resolve_project_prefers_explicit() {
        with_env(
            &[
                ("GOOGLE_CLOUD_PROJECT", Some("from-env")),
                ("GCP_PROJECT", Some("from-fallback")),
            ],
            || {
                assert_eq!(resolve_project(Some("explicit")).unwrap(), "explicit");
            },
        );
    }

    #[test]
    fn resolve_project_falls_back_through_env_chain() {
        with_env(
            &[
                ("GOOGLE_CLOUD_PROJECT", None),
                ("GCP_PROJECT", Some("from-fallback")),
            ],
            || {
                assert_eq!(resolve_project(None).unwrap(), "from-fallback");
            },
        );
    }

    #[test]
    fn resolve_project_errors_when_unset() {
        with_env(
            &[("GOOGLE_CLOUD_PROJECT", None), ("GCP_PROJECT", None)],
            || {
                let err = resolve_project(None).unwrap_err().to_string();
                assert!(err.contains("GOOGLE_CLOUD_PROJECT"), "{err}");
            },
        );
    }

    #[test]
    fn resolve_location_prefers_explicit() {
        with_env(
            &[
                ("VERTEX_LOCATION", Some("from-env")),
                ("GOOGLE_CLOUD_REGION", Some("from-fallback")),
            ],
            || {
                assert_eq!(resolve_location(Some("us-east5")).unwrap(), "us-east5");
            },
        );
    }

    #[test]
    fn resolve_location_errors_when_unset() {
        with_env(
            &[("VERTEX_LOCATION", None), ("GOOGLE_CLOUD_REGION", None)],
            || {
                let err = resolve_location(None).unwrap_err().to_string();
                assert!(err.contains("VERTEX_LOCATION"), "{err}");
                assert!(err.contains("us-east5"), "should hint regions: {err}");
            },
        );
    }
}
