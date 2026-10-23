//! Principal & agent display resolution shared across stats, cost, session,
//! and scheduler endpoints. One module owns the rules so the fallback chain
//! (uuid → identity_store → raw key) and the `system:*` classification can't
//! drift between endpoints.
//!
//! Lookups are batched per request: callers collect the distinct keys they
//! need to display, hand them in, and read back a `HashMap` — the identity
//! store is hit once per distinct id, not once per row.
//!
//! Functions take the minimum slice of state they need (`identity_store`,
//! `agent_registry`) rather than `&GatewayState`, so they're cheap to unit
//! test without building the full gateway scaffold.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use serde::Serialize;
use uuid::Uuid;

use workforce_common::subscription::PayerRef;
use workforce_identity::store::IdentityStore;

use crate::agent_registry::AgentRegistry;

/// Sentinel returned by `cost_by_identity` for rows whose `identity_id`
/// column is NULL. The store layer guarantees this exact string.
pub const UNKNOWN_PRINCIPAL_KEY: &str = "unknown";

const SYSTEM_OWNER_LITERALS: &[&str] = &["scheduler", "subagent", "system"];

const SYSTEM_CHANNEL_PREFIXES: &[&str] =
    &["scheduler:", "workflow:", "memory-consolidation", "cron:"];

const SYSTEM_CHANNEL_LITERALS: &[&str] = &["rest", "gateway", "clawdy"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PrincipalKind {
    User,
    System,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct PrincipalRef {
    pub id: String,
    pub display_name: String,
    pub kind: PrincipalKind,
}

impl PrincipalRef {
    fn unknown() -> Self {
        Self {
            id: UNKNOWN_PRINCIPAL_KEY.to_string(),
            display_name: "Unknown".to_string(),
            kind: PrincipalKind::Unknown,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentRef {
    pub id: String,
    pub display_name: String,
}

/// Classify keys whose label is fixed by the key itself — the `unknown`
/// sentinel, owners with a `system:` prefix, and bot/scheduler literals.
/// `None` means the key needs an identity-store lookup.
fn classify_principal_by_key(id: &str) -> Option<PrincipalRef> {
    if id == UNKNOWN_PRINCIPAL_KEY {
        return Some(PrincipalRef::unknown());
    }
    if id.starts_with("system:") || SYSTEM_OWNER_LITERALS.contains(&id) {
        return Some(PrincipalRef {
            id: id.to_string(),
            display_name: id.to_string(),
            kind: PrincipalKind::System,
        });
    }
    None
}

/// Resolution rule shared by `resolve_principal` and `resolve_principals`:
/// classify by key first, fall through to one identity-store lookup, then
/// fall back to the raw id as label.
fn resolve_one(store: Option<&Arc<dyn IdentityStore>>, id: &str) -> PrincipalRef {
    classify_principal_by_key(id).unwrap_or_else(|| PrincipalRef {
        id: id.to_string(),
        display_name: lookup_identity_name(store, id).unwrap_or_else(|| id.to_string()),
        kind: PrincipalKind::User,
    })
}

/// Resolve a single principal id. For >1 id, prefer [`resolve_principals`]
/// so the identity-store hit is shared.
pub fn resolve_principal(
    identity_store: Option<&Arc<dyn IdentityStore>>,
    id: &str,
) -> PrincipalRef {
    resolve_one(identity_store, id)
}

/// Resolve every distinct id in one pass. `identity_store` is hit at most
/// once per distinct, UUID-shaped, non-system key.
pub fn resolve_principals<I, S>(
    identity_store: Option<&Arc<dyn IdentityStore>>,
    ids: I,
) -> HashMap<String, PrincipalRef>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let distinct: BTreeSet<String> = ids.into_iter().map(|s| s.as_ref().to_string()).collect();
    distinct
        .into_iter()
        .map(|id| {
            let resolved = resolve_one(identity_store, &id);
            (id, resolved)
        })
        .collect()
}

/// Translate `(identity_id, value)` rows into `(PrincipalRef, value)`
/// while preserving input order.
pub fn hydrate_identity_rows(
    identity_store: Option<&Arc<dyn IdentityStore>>,
    rows: Vec<(String, f64)>,
) -> Vec<(PrincipalRef, f64)> {
    let lookup = resolve_principals(identity_store, rows.iter().map(|(id, _)| id.as_str()));
    rows.into_iter()
        .map(|(id, v)| {
            let principal = lookup
                .get(&id)
                .cloned()
                .unwrap_or_else(|| resolve_one(identity_store, &id));
            (principal, v)
        })
        .collect()
}

/// Resolve an agent's display name from config. Returns the id itself
/// when no `name` is set or the agent isn't registered.
pub fn resolve_agent_display(registry: &AgentRegistry, id: &str) -> AgentRef {
    let display = registry
        .get(id)
        .and_then(|svc| svc.config.agent_by_id(id).and_then(|a| a.name.clone()));
    AgentRef {
        id: id.to_string(),
        display_name: display.unwrap_or_else(|| id.to_string()),
    }
}

pub fn resolve_agents<I, S>(registry: &AgentRegistry, ids: I) -> HashMap<String, AgentRef>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let distinct: BTreeSet<String> = ids.into_iter().map(|s| s.as_ref().to_string()).collect();
    distinct
        .into_iter()
        .map(|id| (id.clone(), resolve_agent_display(registry, &id)))
        .collect()
}

/// How each subscription payer on a cost row reads: a person's name, a
/// team's, "<agent>'s shared login" or "Install login".
pub fn resolve_payers<I, S>(
    identity_store: Option<&Arc<dyn IdentityStore>>,
    registry: &AgentRegistry,
    payers: I,
) -> HashMap<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let distinct: BTreeSet<String> = payers.into_iter().map(|s| s.as_ref().to_string()).collect();
    let teams = registry
        .default_agent()
        .and_then(|service| service.team_store.read().clone());
    distinct
        .into_iter()
        .map(|payer| {
            let name = match PayerRef::parse(&payer) {
                PayerRef::Install => "Install login".to_string(),
                PayerRef::Agent(id) => format!(
                    "{}'s shared login",
                    resolve_agent_display(registry, id).display_name
                ),
                PayerRef::Team(id) => crate::recipients::team_name(teams.as_deref(), id)
                    .unwrap_or_else(|| payer.clone()),
                PayerRef::Person(id) => resolve_one(identity_store, id).display_name,
            };
            (payer, name)
        })
        .collect()
}

/// Enrich a serialisable row with `user_display_name`, `user_kind`, and
/// `agent_display_name` fields drawn from pre-batched lookup maps. The
/// callers (`cost_detail`, `scheduled_jobs_history`) both shape rows the
/// same way; this is their single source of truth.
pub fn enrich_row_json<R: Serialize>(
    row: &R,
    user_id: Option<&str>,
    agent_id: &str,
    principals: &HashMap<String, PrincipalRef>,
    agents: &HashMap<String, AgentRef>,
) -> serde_json::Value {
    let mut value = serde_json::to_value(row).unwrap_or_else(|_| serde_json::json!({}));
    match user_id {
        Some(uid) => {
            let p = principals.get(uid);
            value["user_display_name"] = serde_json::json!(p
                .map(|p| p.display_name.clone())
                .unwrap_or_else(|| uid.to_string()));
            value["user_kind"] =
                serde_json::json!(p.map(|p| p.kind).unwrap_or(PrincipalKind::User));
        }
        None => {
            value["user_display_name"] = serde_json::Value::Null;
            value["user_kind"] = serde_json::Value::Null;
        }
    }
    value["agent_display_name"] = serde_json::json!(agents
        .get(agent_id)
        .map(|a| a.display_name.clone())
        .unwrap_or_else(|| agent_id.to_string()));
    value
}

pub fn hydrate_agent_rows(
    registry: &AgentRegistry,
    rows: Vec<(String, f64)>,
) -> Vec<(AgentRef, f64)> {
    let lookup = resolve_agents(registry, rows.iter().map(|(id, _)| id.as_str()));
    rows.into_iter()
        .map(|(id, v)| {
            let agent = lookup
                .get(&id)
                .cloned()
                .unwrap_or_else(|| resolve_agent_display(registry, &id));
            (agent, v)
        })
        .collect()
}

/// Categorise a session for the UI's user/system filter, using only
/// fields already on `Session` (`owner`, `channel`).
pub fn classify_session(owner: &str, channel: &str) -> PrincipalKind {
    if owner.is_empty() || owner.starts_with("system:") || SYSTEM_OWNER_LITERALS.contains(&owner) {
        return PrincipalKind::System;
    }
    let ch = channel.to_ascii_lowercase();
    if SYSTEM_CHANNEL_LITERALS.contains(&ch.as_str())
        || SYSTEM_CHANNEL_PREFIXES.iter().any(|p| ch.starts_with(p))
    {
        return PrincipalKind::System;
    }
    PrincipalKind::User
}

fn lookup_identity_name(store: Option<&Arc<dyn IdentityStore>>, id: &str) -> Option<String> {
    let store = store?;
    let uuid = Uuid::parse_str(id).ok()?;
    store.get_by_id(uuid).ok().flatten().map(|i| i.display_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use uuid::Uuid;
    use workforce_identity::store::{IdentityFilter, IdentityStore};
    use workforce_identity::types::{Identity, IdentityRole};

    struct CountingStore {
        inner: HashMap<Uuid, Identity>,
        calls: AtomicUsize,
    }

    impl CountingStore {
        fn with(identities: Vec<Identity>) -> Arc<CountingStore> {
            let inner = identities.into_iter().map(|i| (i.id, i)).collect();
            Arc::new(Self {
                inner,
                calls: AtomicUsize::new(0),
            })
        }
        fn as_store(self: &Arc<Self>) -> Arc<dyn IdentityStore> {
            self.clone() as Arc<dyn IdentityStore>
        }
    }

    impl IdentityStore for CountingStore {
        fn create(&self, _: &Identity) -> eyre::Result<()> {
            unimplemented!()
        }
        fn get_by_id(&self, id: Uuid) -> eyre::Result<Option<Identity>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.inner.get(&id).cloned())
        }
        fn get_by_email(&self, _: &str) -> eyre::Result<Option<Identity>> {
            unimplemented!()
        }
        fn get_by_linked_id(&self, _: &str, _: &str) -> eyre::Result<Option<Identity>> {
            unimplemented!()
        }
        fn update(&self, _: &Identity) -> eyre::Result<()> {
            unimplemented!()
        }
        fn delete(&self, _: Uuid) -> eyre::Result<()> {
            unimplemented!()
        }
        fn list(&self, _: &IdentityFilter) -> eyre::Result<Vec<Identity>> {
            unimplemented!()
        }
        fn count_filtered(&self, _: &IdentityFilter) -> eyre::Result<usize> {
            Ok(self.inner.len())
        }
    }

    fn make_identity(name: &str) -> Identity {
        Identity::new(name, IdentityRole::Member)
    }

    #[test]
    fn resolve_principal_returns_display_name_for_known_uuid() {
        let alice = make_identity("Alice");
        let store = CountingStore::with(vec![alice.clone()]);
        let dyn_store = store.as_store();

        let p = resolve_principal(Some(&dyn_store), &alice.id.to_string());
        assert_eq!(p.kind, PrincipalKind::User);
        assert_eq!(p.display_name, "Alice");
        assert_eq!(p.id, alice.id.to_string());
    }

    #[test]
    fn resolve_principal_falls_back_to_id_when_store_absent() {
        let id = Uuid::new_v4().to_string();
        let p = resolve_principal(None, &id);
        assert_eq!(p.display_name, id);
        assert_eq!(p.kind, PrincipalKind::User);
    }

    #[test]
    fn resolve_principal_classifies_system_prefix() {
        let p = resolve_principal(None, "system:consolidation");
        assert_eq!(p.kind, PrincipalKind::System);
        assert_eq!(p.display_name, "system:consolidation");
    }

    #[test]
    fn resolve_principal_classifies_unknown_sentinel() {
        let p = resolve_principal(None, UNKNOWN_PRINCIPAL_KEY);
        assert_eq!(p.kind, PrincipalKind::Unknown);
        assert_eq!(p.display_name, "Unknown");
    }

    #[test]
    fn resolve_principal_handles_invalid_uuid() {
        let p = resolve_principal(None, "not-a-uuid");
        assert_eq!(p.kind, PrincipalKind::User);
        assert_eq!(p.display_name, "not-a-uuid");
    }

    #[test]
    fn resolve_principals_dedupes_lookups() {
        let alice = make_identity("Alice");
        let bob = make_identity("Bob");
        let store = CountingStore::with(vec![alice.clone(), bob.clone()]);
        let dyn_store = store.as_store();

        let mut ids = Vec::new();
        for _ in 0..50 {
            ids.push(alice.id.to_string());
            ids.push(bob.id.to_string());
            ids.push("system:consolidation".to_string());
            ids.push(UNKNOWN_PRINCIPAL_KEY.to_string());
        }

        let map = resolve_principals(Some(&dyn_store), ids.iter().map(|s| s.as_str()));
        assert_eq!(map.len(), 4);
        assert_eq!(
            map.get(&alice.id.to_string()).unwrap().display_name,
            "Alice"
        );
        assert_eq!(map.get(&bob.id.to_string()).unwrap().display_name, "Bob");
        assert_eq!(store.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn hydrate_identity_rows_preserves_order() {
        let alice = make_identity("Alice");
        let bob = make_identity("Bob");
        let store = CountingStore::with(vec![alice.clone(), bob.clone()]);
        let dyn_store = store.as_store();

        let rows = vec![
            (bob.id.to_string(), 12.0),
            (alice.id.to_string(), 5.0),
            (UNKNOWN_PRINCIPAL_KEY.to_string(), 1.0),
        ];
        let hydrated = hydrate_identity_rows(Some(&dyn_store), rows);
        assert_eq!(hydrated[0].0.display_name, "Bob");
        assert_eq!(hydrated[1].0.display_name, "Alice");
        assert_eq!(hydrated[2].0.kind, PrincipalKind::Unknown);
    }

    #[test]
    fn classify_session_system_owners() {
        for owner in ["system", "scheduler", "subagent", "system:consolidation"] {
            assert_eq!(
                classify_session(owner, "ios"),
                PrincipalKind::System,
                "{owner}"
            );
        }
    }

    #[test]
    fn classify_session_system_channels() {
        assert_eq!(
            classify_session("alice", "scheduler:job-1:123"),
            PrincipalKind::System
        );
        assert_eq!(
            classify_session("alice", "workflow:abc:design"),
            PrincipalKind::System
        );
        assert_eq!(
            classify_session("alice", "memory-consolidation"),
            PrincipalKind::System
        );
    }

    #[test]
    fn classify_session_user_channels() {
        for channel in ["ios", "web", "terminal", "slack:DM:U0FAKEUSER1"] {
            assert_eq!(
                classify_session("alice", channel),
                PrincipalKind::User,
                "{channel}"
            );
        }
    }
}
