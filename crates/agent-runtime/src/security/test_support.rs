//! Shared scaffolding for the security modules' tests.
//!
//! The secret cache is a process-wide global, so the fixtures that install one
//! have to agree on a single lock and a single stub — one per module would race
//! in the same test binary.

use std::sync::{Arc, Mutex};

use workforce_common::secret::Secret;
use workforce_common::secret_marker::{
    ConsumedSecret, MarkerId, SecretCache, SecretSource, NAMED_LABEL,
};

/// Serializes every test that installs a global secret cache.
///
/// `set_global_cache` / `clear_global_cache` are process-wide, and the test
/// binary runs modules in parallel, so each such test must hold this.
pub(crate) static SECRET_CACHE_TEST_LOCK: Mutex<()> = Mutex::new(());

struct StubCache(Mutex<Vec<(String, MarkerId, String, SecretSource)>>);

impl SecretCache for StubCache {
    fn capture(&self, _: &str, _: Secret<String>, _: &str, _: SecretSource) -> MarkerId {
        unreachable!("resolution tests don't capture")
    }
    fn capture_named(&self, _: &str, _: &MarkerId, _: Secret<String>, _: SecretSource) {
        unreachable!("resolution tests don't capture")
    }
    fn list_named(&self, _: &str) -> Vec<String> {
        Vec::new()
    }
    fn lookup(&self, session: &str, id: &MarkerId) -> Option<(Secret<String>, ConsumedSecret)> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|(s, i, _, _)| s == session && i == id)
            .map(|(_, i, value, source)| {
                (
                    Secret::new(value.clone()),
                    ConsumedSecret {
                        id: i.clone(),
                        label: NAMED_LABEL.to_string(),
                        preview: "***".to_string(),
                        source: source.clone(),
                    },
                )
            })
    }
    fn purge_session(&self, _: &str) {}
    fn forget(&self, _: &str, _: &MarkerId) {}
}

/// A cache backed by a flat table of `(session, id, value, source)`.
pub(crate) fn stub_secret_cache(
    entries: &[(&str, MarkerId, &str, SecretSource)],
) -> Arc<dyn SecretCache> {
    Arc::new(StubCache(Mutex::new(
        entries
            .iter()
            .map(|(session, id, value, source)| {
                (
                    (*session).to_string(),
                    id.clone(),
                    (*value).to_string(),
                    source.clone(),
                )
            })
            .collect(),
    )))
}

/// [`stub_secret_cache`] for the common case: values the user set with
/// `cmd: secret <name> <value>`, keyed by name.
pub(crate) fn stub_named_secret_cache(entries: &[(&str, &str, &str)]) -> Arc<dyn SecretCache> {
    let user_pasted = SecretSource::Slack {
        workspace_id: "T1".into(),
        channel: "C1".into(),
        message_ts: "1.0".into(),
        user: "U1".into(),
    };
    let rows: Vec<_> = entries
        .iter()
        .map(|(session, name, value)| {
            (
                *session,
                MarkerId::named(name).expect("valid secret name"),
                *value,
                user_pasted.clone(),
            )
        })
        .collect();
    stub_secret_cache(&rows)
}
