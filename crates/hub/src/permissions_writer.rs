//! Adapter implementing [`workforce_agent::runner::PermissionWriter`]
//! over [`workforce_permissions::cache::PermissionEvaluator`].
//!
//! The runner is decoupled from `workforce-permissions` to keep its
//! crate boundary clean; this adapter is the only piece of glue that
//! has to know about both crates. Hub-side construction wires one of
//! these into [`RunnerConfig::permission_writer`] when the cascade is
//! enabled, and the runner calls into it the moment an Always-* choice
//! arrives at the approval channel.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use parking_lot::Mutex;
use tokio::task;
use uuid::Uuid;
use workforce_agent::runner::{CascadeState, PermissionGate, PermissionWriter};
use workforce_core::policy::PermissionChoice;
use workforce_identity::team::TeamStore;
use workforce_permissions::cache::PermissionEvaluator;
use workforce_permissions::cascade::CascadeRequest;
use workforce_permissions::types::PermissionState;

/// TTL for the per-user team-membership cache inside
/// [`CascadePermissionGate`]. Short enough that an operator-side
/// "add user to team" propagates within a minute; long enough that a
/// multi-turn conversation doesn't re-query the identity store on
/// every tool call.
const TEAM_CACHE_TTL: Duration = Duration::from_secs(60);

/// Wraps a shared [`PermissionEvaluator`] so the runner can persist
/// User-scope rules without depending on `workforce-permissions`.
pub struct CascadePermissionWriter {
    evaluator: Arc<PermissionEvaluator>,
}

impl CascadePermissionWriter {
    pub fn new(evaluator: Arc<PermissionEvaluator>) -> Arc<Self> {
        Arc::new(Self { evaluator })
    }
}

#[async_trait]
impl PermissionWriter for CascadePermissionWriter {
    async fn persist_user_choice(
        &self,
        user_id: &str,
        tool_name: &str,
        choice: PermissionChoice,
    ) -> Result<()> {
        let Some(state) = choice_to_state(choice) else {
            return Ok(());
        };
        let evaluator = Arc::clone(&self.evaluator);
        let user_id = user_id.to_string();
        let tool_name = tool_name.to_string();
        // Store calls are synchronous (rusqlite / tokio_postgres
        // `block_in_place`) — hop to a blocking thread so we don't
        // stall the runner's async runtime.
        task::spawn_blocking(move || {
            evaluator.persist_user_choice(&user_id, &tool_name, state, None)
        })
        .await
        .map_err(|e| eyre::eyre!("permission writer join failed: {e}"))?
    }
}

fn choice_to_state(choice: PermissionChoice) -> Option<PermissionState> {
    match choice {
        PermissionChoice::AllowAlways => Some(PermissionState::Allow),
        PermissionChoice::DenyAlways => Some(PermissionState::Deny),
        PermissionChoice::AllowOnce | PermissionChoice::DenyOnce => None,
    }
}

/// Adapter that lets the runner consult a [`PermissionEvaluator`]
/// without depending on `workforce-permissions`. Mirrors the writer
/// adapter shape so both live in one place.
///
/// Carries an optional [`TeamStore`] handle so the cascade runs with
/// the caller's team memberships in scope — Team-scope rules then
/// apply at the runner gate the same way Agent and User rules do.
/// Memberships are cached per-user with a short TTL
/// ([`TEAM_CACHE_TTL`]) so a multi-turn conversation doesn't hammer
/// the identity store; the TTL is short enough that an operator-side
/// "add user to team" propagates within a minute. When the team store
/// is unwired or the `user_id` isn't a UUID, the gate falls back to
/// an empty `team_ids` slice — Agent + User rules still apply.
pub struct CascadePermissionGate {
    evaluator: Arc<PermissionEvaluator>,
    team_store: Option<Arc<dyn TeamStore>>,
    team_cache: Mutex<HashMap<Uuid, (Vec<String>, Instant)>>,
}

impl CascadePermissionGate {
    pub fn new(evaluator: Arc<PermissionEvaluator>) -> Self {
        Self {
            evaluator,
            team_store: None,
            team_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_team_store(mut self, team_store: Arc<dyn TeamStore>) -> Self {
        self.team_store = Some(team_store);
        self
    }

    /// Resolve the caller's team UUIDs as strings — the shape the
    /// cascade evaluator expects. Returns an empty vec when the team
    /// store is unwired, the user isn't a UUID, or the lookup fails:
    /// the gate prefers conservative (no Team rules apply) over a hard
    /// runtime error, since the cascade is itself an additive
    /// restriction over PolicyEngine.
    fn team_ids_for(&self, user_id: &str) -> Vec<String> {
        let Some(store) = self.team_store.as_ref() else {
            return Vec::new();
        };
        let Ok(uid) = Uuid::from_str(user_id) else {
            return Vec::new();
        };
        // Fast path: cached entry within TTL.
        {
            let cache = self.team_cache.lock();
            if let Some((teams, deadline)) = cache.get(&uid) {
                if Instant::now() < *deadline {
                    return teams.clone();
                }
            }
        }
        // Cache miss / expired: re-query. Tolerate failure with an
        // empty vec rather than upgrading the gate to Deny.
        let teams = match store.teams_for_member(uid) {
            Ok(rows) => rows.into_iter().map(|t| t.id.to_string()).collect(),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    user = %user_id,
                    "team-membership lookup failed; cascade gate proceeding without Team-scope rules",
                );
                Vec::new()
            }
        };
        let deadline = Instant::now() + TEAM_CACHE_TTL;
        self.team_cache
            .lock()
            .insert(uid, (teams.clone(), deadline));
        teams
    }
}

impl PermissionGate for CascadePermissionGate {
    fn evaluate(&self, user_id: &str, agent_id: &str, tool_name: &str) -> CascadeState {
        let teams = self.team_ids_for(user_id);
        let request = CascadeRequest {
            user_id,
            agent_id,
            team_ids: &teams,
            tool_name,
        };
        match self.evaluator.evaluate(&request) {
            Ok(state) => permission_state_to_cascade(state),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    tool = %tool_name,
                    agent = %agent_id,
                    user = %user_id,
                    "Cascade gate evaluation failed; falling back to Allow",
                );
                CascadeState::Allow
            }
        }
    }
}

fn permission_state_to_cascade(state: PermissionState) -> CascadeState {
    match state {
        PermissionState::Allow => CascadeState::Allow,
        PermissionState::Ask => CascadeState::Ask,
        PermissionState::AlwaysAsk => CascadeState::AlwaysAsk,
        PermissionState::Deny => CascadeState::Deny,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_permissions::sqlite::SqlitePermissionStore;
    use workforce_permissions::store::PermissionStore;
    use workforce_permissions::types::{PermissionFilter, PermissionScope};

    #[tokio::test]
    async fn always_allow_persists_user_scope_row() {
        let store: Arc<dyn PermissionStore> = Arc::new(SqlitePermissionStore::in_memory().unwrap());
        let evaluator = Arc::new(PermissionEvaluator::new(Arc::clone(&store)));
        let writer = CascadePermissionWriter::new(Arc::clone(&evaluator));

        writer
            .persist_user_choice("user-x", "memory.recall", PermissionChoice::AllowAlways)
            .await
            .unwrap();

        let rules = store
            .list_rules(&PermissionFilter {
                scope: Some(PermissionScope::User),
                scope_id: Some("user-x".to_string()),
                ..PermissionFilter::default()
            })
            .unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].state, PermissionState::Allow);
    }

    #[derive(Clone, Default)]
    struct CountingTeamStore {
        calls: Arc<Mutex<u32>>,
        teams: Vec<workforce_identity::team::Team>,
    }

    impl workforce_identity::team::TeamStore for CountingTeamStore {
        fn create_team(&self, _: &workforce_identity::team::Team) -> Result<()> {
            unreachable!()
        }
        fn get_team(&self, _: Uuid) -> Result<Option<workforce_identity::team::Team>> {
            unreachable!()
        }
        fn update_team(&self, _: &workforce_identity::team::Team) -> Result<()> {
            unreachable!()
        }
        fn delete_team(&self, _: Uuid) -> Result<()> {
            unreachable!()
        }
        fn list_teams(&self) -> Result<Vec<workforce_identity::team::Team>> {
            unreachable!()
        }
        fn teams_for_member(&self, _: Uuid) -> Result<Vec<workforce_identity::team::Team>> {
            *self.calls.lock() += 1;
            Ok(self.teams.clone())
        }
    }

    fn evaluator_for(store: Arc<dyn PermissionStore>) -> Arc<PermissionEvaluator> {
        Arc::new(PermissionEvaluator::new(store))
    }

    #[test]
    fn team_lookup_caches_within_ttl_window() {
        let store: Arc<dyn PermissionStore> = Arc::new(SqlitePermissionStore::in_memory().unwrap());
        let evaluator = evaluator_for(Arc::clone(&store));
        let team_id = Uuid::new_v4();
        let counter = Arc::new(Mutex::new(0u32));
        let ts = Arc::new(CountingTeamStore {
            calls: Arc::clone(&counter),
            teams: vec![workforce_identity::team::Team {
                id: team_id,
                name: "alpha".into(),
                creator_id: Uuid::new_v4(),
                members: Vec::new(),
                join_requests: Vec::new(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            }],
        });
        let gate = CascadePermissionGate::new(evaluator).with_team_store(ts);
        let user = Uuid::new_v4().to_string();
        // First call hits the team store.
        let teams = gate.team_ids_for(&user);
        assert_eq!(teams, vec![team_id.to_string()]);
        assert_eq!(*counter.lock(), 1);
        // Second call within the TTL window stays cached — no extra hit.
        let teams_again = gate.team_ids_for(&user);
        assert_eq!(teams_again, vec![team_id.to_string()]);
        assert_eq!(*counter.lock(), 1);
    }

    #[test]
    fn team_lookup_skips_store_when_user_id_is_not_uuid() {
        let store: Arc<dyn PermissionStore> = Arc::new(SqlitePermissionStore::in_memory().unwrap());
        let evaluator = evaluator_for(store);
        let counter = Arc::new(Mutex::new(0u32));
        let ts = Arc::new(CountingTeamStore {
            calls: Arc::clone(&counter),
            teams: vec![],
        });
        let gate = CascadePermissionGate::new(evaluator).with_team_store(ts);
        let teams = gate.team_ids_for("not-a-uuid");
        assert!(teams.is_empty());
        assert_eq!(*counter.lock(), 0);
    }

    #[test]
    fn team_lookup_skips_store_when_unwired() {
        let store: Arc<dyn PermissionStore> = Arc::new(SqlitePermissionStore::in_memory().unwrap());
        let evaluator = evaluator_for(store);
        let gate = CascadePermissionGate::new(evaluator);
        let teams = gate.team_ids_for(&Uuid::new_v4().to_string());
        assert!(teams.is_empty());
    }

    #[tokio::test]
    async fn once_choices_are_noop() {
        let store: Arc<dyn PermissionStore> = Arc::new(SqlitePermissionStore::in_memory().unwrap());
        let evaluator = Arc::new(PermissionEvaluator::new(Arc::clone(&store)));
        let writer = CascadePermissionWriter::new(Arc::clone(&evaluator));

        writer
            .persist_user_choice("user-y", "vault.read", PermissionChoice::AllowOnce)
            .await
            .unwrap();
        writer
            .persist_user_choice("user-y", "vault.read", PermissionChoice::DenyOnce)
            .await
            .unwrap();

        let rules = store.list_rules(&PermissionFilter::default()).unwrap();
        assert!(rules.is_empty(), "once choices must not persist a rule");
    }
}
