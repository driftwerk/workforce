//! Build provenance for the running binary.
//!
//! `COMMIT_SHA` and `BUILD_DATE` are baked into the Docker image as env vars
//! at image-build time (Dockerfile.ci / Dockerfile, fed by CI build-args) and
//! read here at *runtime* — the binary itself is compiled before either value
//! exists, so compile-time `env!` would always see them unset.
//!
//! Consumed by the gateway `/api/version` endpoint (UI footer) and the
//! `system.version` agent tool.

/// Full git commit SHA the image was built from, or `"dev"` outside Docker.
pub fn commit_sha() -> String {
    non_empty_env("COMMIT_SHA").unwrap_or_else(|| "dev".to_string())
}

/// UTC ISO-8601 timestamp the image was built at, or `"unknown"` outside Docker.
pub fn build_date() -> String {
    non_empty_env("BUILD_DATE").unwrap_or_else(|| "unknown".to_string())
}

/// Crate version from Cargo.toml (compile-time).
pub fn crate_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The Docker image tag CI publishes for this commit (`sha-<full sha>`),
/// or `None` for non-Docker builds. Lets ops verify the *deployed image*
/// against the registry — see the silent-rollback incidents.
pub fn image_tag() -> Option<String> {
    non_empty_env("COMMIT_SHA").map(|sha| format!("sha-{sha}"))
}

/// Build info as the canonical JSON shape shared by the API endpoint and
/// agent tool. Carries the FULL sha — display layers truncate.
pub fn as_json() -> serde_json::Value {
    serde_json::json!({
        "version": crate_version(),
        "commit_sha": commit_sha(),
        "build_date": build_date(),
        "image_tag": image_tag(),
    })
}

/// Env var, treating unset, empty, and the Dockerfile `unknown` default as
/// absent (a local `docker build` without build-args bakes the defaults in).
fn non_empty_env(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() && v != "unknown" => Some(v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shape_has_all_fields() {
        let v = as_json();
        assert!(v.get("version").is_some_and(|x| x.is_string()));
        assert!(v.get("commit_sha").is_some_and(|x| x.is_string()));
        assert!(v.get("build_date").is_some_and(|x| x.is_string()));
        // image_tag may be null outside Docker, but the key must exist.
        assert!(v.get("image_tag").is_some());
    }

    #[test]
    fn version_matches_crate() {
        assert_eq!(as_json()["version"], env!("CARGO_PKG_VERSION"));
    }
}
