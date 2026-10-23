//! Shared parsing for scoped vault markers `[[vault:<scope>:<key>]]`.
//!
//! Workflow secret variables carry a scoped marker whose `<scope>` selects which
//! vault tier the secret resolves against. The same parser is used at three
//! layers — definition validation, the deny-by-default policy, and the skill
//! sink resolver — so the scope keywords and key-safety rules stay identical.

use crate::settings::VaultSource;

pub const SCOPE_AGENT_USER: &str = "agent_user";
pub const SCOPE_GLOBAL_USER: &str = "global_user";
pub const SCOPE_AGENT: &str = "agent";
pub const SCOPE_GLOBAL: &str = "global";

/// A parsed scoped vault marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedMarker {
    pub scope: VaultSource,
    pub scope_name: String,
    pub key: String,
}

/// Map a scope keyword to the vault tier it resolves against. `None` for an
/// unknown keyword.
pub fn scope_tier(scope: &str) -> Option<VaultSource> {
    match scope {
        SCOPE_AGENT_USER => Some(VaultSource::AgentPerUser),
        SCOPE_GLOBAL_USER => Some(VaultSource::GlobalPerUser),
        SCOPE_AGENT => Some(VaultSource::AgentSpecific),
        SCOPE_GLOBAL => Some(VaultSource::Global),
        _ => None,
    }
}

/// Whether a tier is per-user (resolves under `users/{uid}/`).
pub fn is_per_user_scope(scope: VaultSource) -> bool {
    matches!(
        scope,
        VaultSource::AgentPerUser | VaultSource::GlobalPerUser
    )
}

/// Whether a tier is shared (`agent`/`global`, not per-user).
pub fn is_shared_scope(scope: VaultSource) -> bool {
    matches!(scope, VaultSource::AgentSpecific | VaultSource::Global)
}

/// Parse a string that must be EXACTLY one `[[vault:<scope>:<key>]]` marker.
/// Returns `None` unless the whole string is a single well-formed scoped marker
/// with a known scope and a safe key.
pub fn parse_exact_scoped_marker(s: &str) -> Option<ScopedMarker> {
    let inner = s.strip_prefix("[[vault:")?.strip_suffix("]]")?;
    let (scope_name, key) = inner.split_once(':')?;
    let scope = scope_tier(scope_name)?;
    if !is_safe_vault_key(key) {
        return None;
    }
    Some(ScopedMarker {
        scope,
        scope_name: scope_name.to_string(),
        key: key.to_string(),
    })
}

/// Whether `text` references any vault marker at all (scoped or legacy). Used to
/// reject vault references in non-secret values.
pub fn contains_vault_ref(text: &str) -> bool {
    text.contains("[[vault:")
}

/// A vault key may not be empty, traverse, or be absolute; it may contain only
/// alphanumerics and `_ . - /`.
pub fn is_safe_vault_key(key: &str) -> bool {
    if key.is_empty()
        || key.starts_with('/')
        || key.starts_with('.')
        || key.ends_with('/')
        || key.contains("..")
        || key.contains("//")
    {
        return false;
    }
    key.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_scope() {
        for (name, tier) in [
            (SCOPE_AGENT_USER, VaultSource::AgentPerUser),
            (SCOPE_GLOBAL_USER, VaultSource::GlobalPerUser),
            (SCOPE_AGENT, VaultSource::AgentSpecific),
            (SCOPE_GLOBAL, VaultSource::Global),
        ] {
            let m = parse_exact_scoped_marker(&format!("[[vault:{name}:portal_pw]]")).unwrap();
            assert_eq!(m.scope, tier);
            assert_eq!(m.key, "portal_pw");
        }
    }

    #[test]
    fn rejects_malformed() {
        // Unknown scope.
        assert!(parse_exact_scoped_marker("[[vault:root:k]]").is_none());
        // Legacy single-colon (no scope).
        assert!(parse_exact_scoped_marker("[[vault:k]]").is_none());
        // Path traversal / absolute / trailing.
        assert!(parse_exact_scoped_marker("[[vault:agent:../x]]").is_none());
        assert!(parse_exact_scoped_marker("[[vault:agent:/abs]]").is_none());
        // Surrounding text — not an exact marker.
        assert!(parse_exact_scoped_marker("pw=[[vault:agent:k]]").is_none());
    }

    #[test]
    fn scope_predicates() {
        assert!(is_per_user_scope(VaultSource::AgentPerUser));
        assert!(is_per_user_scope(VaultSource::GlobalPerUser));
        assert!(is_shared_scope(VaultSource::AgentSpecific));
        assert!(is_shared_scope(VaultSource::Global));
        assert!(!is_per_user_scope(VaultSource::Global));
    }
}
