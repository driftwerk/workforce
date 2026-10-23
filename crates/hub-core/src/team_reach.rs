//! One caller's team standing, resolved once and reused: the team ids a grant
//! may name, and the people they share a team with — which is what team-visible
//! means. Resolution is lazy, so a private or global resource never asks.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use workforce_core::access::TeamMembership;
use workforce_identity::team::TeamStore;

/// What one identity's teams say, read from the store at most once.
#[derive(Debug, Default)]
struct Facts {
    team_ids: Vec<String>,
    peers: HashSet<String>,
}

pub struct TeamReach {
    identity: Option<String>,
    store: Option<Arc<dyn TeamStore>>,
    facts: Arc<OnceLock<Facts>>,
}

impl TeamReach {
    pub fn new(store: Option<Arc<dyn TeamStore>>, identity: Option<String>) -> Self {
        Self {
            identity: identity.filter(|id| !id.is_empty()),
            store,
            facts: Arc::new(OnceLock::new()),
        }
    }

    /// A caller who stands in no team: a hub with no team store, the CLI, and
    /// every surface that has no identity to resolve.
    pub fn none() -> Self {
        Self::new(None, None)
    }

    /// The reach of `identity` on a running hub, whose team store is late-bound.
    pub fn for_service(service: &crate::serve::AgentService, identity: Option<String>) -> Self {
        Self::new(service.team_store.read().clone(), identity)
    }

    pub fn identity(&self) -> Option<&str> {
        self.identity.as_deref()
    }

    /// The teams this caller belongs to — the ids a grant of theirs may name.
    pub fn team_ids(&self) -> &[String] {
        &self.facts().team_ids
    }

    /// One store read, whatever is asked of it. An identity that is not a UUID,
    /// an absent store and a store that errors all stand in no team: reach
    /// fails closed, exactly as a share lookup does.
    fn facts(&self) -> &Facts {
        self.facts.get_or_init(|| {
            let Some((store, id)) = self.store.as_deref().zip(
                self.identity
                    .as_deref()
                    .and_then(|id| uuid::Uuid::parse_str(id).ok()),
            ) else {
                return Facts::default();
            };
            store
                .teams_for_member(id)
                .map(|teams| Facts {
                    team_ids: teams.iter().map(|team| team.id.to_string()).collect(),
                    peers: teams
                        .iter()
                        .flat_map(|team| team.members.iter())
                        .map(uuid::Uuid::to_string)
                        .collect(),
                })
                .unwrap_or_default()
        })
    }
}

impl TeamMembership for TeamReach {
    fn in_team(&self, team_id: &str) -> bool {
        self.facts().team_ids.iter().any(|id| id == team_id)
    }

    fn shares_team_with(&self, owner: &str) -> bool {
        self.facts().peers.contains(owner)
    }
}

impl Clone for TeamReach {
    /// Shares the memo: a clone asks the store no more often than its original.
    fn clone(&self) -> Self {
        Self {
            identity: self.identity.clone(),
            store: self.store.clone(),
            facts: Arc::clone(&self.facts),
        }
    }
}

impl std::fmt::Debug for TeamReach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeamReach")
            .field("identity", &self.identity)
            .field("teams", &self.facts.get().map(|facts| facts.team_ids.len()))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_identity::sqlite::SqliteIdentityStore;
    use workforce_identity::team::Team;

    /// A store that counts its reads and can be made to fail.
    struct Counting {
        inner: Arc<SqliteIdentityStore>,
        reads: Arc<std::sync::atomic::AtomicUsize>,
        fails: bool,
    }

    impl TeamStore for Counting {
        fn create_team(&self, team: &Team) -> eyre::Result<()> {
            self.inner.create_team(team)
        }

        fn get_team(&self, id: uuid::Uuid) -> eyre::Result<Option<Team>> {
            self.inner.get_team(id)
        }

        fn update_team(&self, team: &Team) -> eyre::Result<()> {
            self.inner.update_team(team)
        }

        fn delete_team(&self, id: uuid::Uuid) -> eyre::Result<()> {
            self.inner.delete_team(id)
        }

        fn list_teams(&self) -> eyre::Result<Vec<Team>> {
            self.inner.list_teams()
        }

        fn teams_for_member(&self, identity_id: uuid::Uuid) -> eyre::Result<Vec<Team>> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match self.fails {
                true => Err(eyre::eyre!("the team store is unavailable")),
                false => self.inner.teams_for_member(identity_id),
            }
        }
    }

    fn store(fails: bool) -> (Arc<Counting>, Arc<std::sync::atomic::AtomicUsize>) {
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let inner = Arc::new(SqliteIdentityStore::in_memory().expect("sqlite"));
        (
            Arc::new(Counting {
                inner,
                reads: Arc::clone(&reads),
                fails,
            }),
            reads,
        )
    }

    #[test]
    fn peers_are_every_member_of_every_team_the_caller_is_in() {
        let (teams, reads) = store(false);
        let (alice, bob, carol) = (
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
        );
        let mut platform = Team::new("platform", alice);
        platform.add_member(bob);
        let triage = Team::new("triage", carol);
        teams.create_team(&platform).expect("platform");
        teams.create_team(&triage).expect("triage");

        let reach = TeamReach::new(Some(teams.clone()), Some(bob.to_string()));
        assert_eq!(reach.identity(), Some(bob.to_string().as_str()));
        assert_eq!(reach.team_ids(), [platform.id.to_string()]);
        assert!(reach.in_team(&platform.id.to_string()));
        assert!(!reach.in_team(&triage.id.to_string()));
        assert!(reach.shares_team_with(&alice.to_string()));
        assert!(reach.shares_team_with(&bob.to_string()));
        assert!(!reach.shares_team_with(&carol.to_string()));

        // Many questions, and a clone of the reach, still cost one read.
        assert!(!reach.clone().shares_team_with(&carol.to_string()));
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn a_reach_with_nothing_to_resolve_stands_in_no_team() {
        let (teams, reads) = store(false);
        let member = uuid::Uuid::new_v4();
        let empty = [
            TeamReach::none(),
            TeamReach::new(None, Some(member.to_string())),
            TeamReach::new(Some(teams.clone()), None),
            TeamReach::new(Some(teams.clone()), Some(String::new())),
            TeamReach::new(Some(teams.clone()), Some("not-a-uuid".into())),
        ];
        for reach in &empty {
            assert!(reach.team_ids().is_empty());
            assert!(!reach.shares_team_with(&member.to_string()));
        }
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 0);

        // A store that cannot answer leaves the caller in no team, not in every
        // team: reach fails closed.
        let (broken, _) = store(true);
        let reach = TeamReach::new(Some(broken), Some(member.to_string()));
        assert!(reach.team_ids().is_empty());
        assert!(!reach.shares_team_with(&member.to_string()));
    }
}
