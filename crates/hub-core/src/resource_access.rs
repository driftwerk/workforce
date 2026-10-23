//! What a caller reaches across a whole listing of definitions, and the
//! grantee set any owned resource may be saved with.
//!
//! The rule for one row lives in [`workforce_core::access`]; this is the one
//! place that turns it into the standing every `list_definitions_visible`
//! reads, so the gateway and the agent tools answer the same question the same
//! way — including the shares and the teams, which a listing would otherwise
//! silently drop.

use std::collections::HashSet;
use std::sync::Arc;

use workforce_core::access::{Grant, GrantEntry, Visibility};
use workforce_orchestrator::workflow_store::{member_def_key, visible_to, WorkflowStore};

use crate::def_write_auth::{DefWriter, TurnDefWriteAuth};
use crate::team_reach::TeamReach;

/// What `team` visibility reaches for someone who stands in no team: nobody
/// else, yet. The one sentence every surface appends when they choose it.
pub const NO_TEAM_YET: &str =
    "You're not in a team yet, so nobody else can run it until you join one.";

/// The `{owner}/{id}` keys of definitions of `kind` granted to `caller` through
/// a share, to them or to one of their teams. Empty for an administrator (who
/// sees every row anyway), for a caller bound to no identity, and where there
/// is no definition store.
pub async fn shared_def_keys(
    def_store: Option<&Arc<WorkflowStore>>,
    caller: &DefWriter,
    teams: &TeamReach,
    kind: &str,
) -> HashSet<String> {
    let (Some(store), Some(identity)) = (
        def_store.filter(|_| !caller.is_admin),
        caller.identity_id.as_deref().filter(|id| !id.is_empty()),
    ) else {
        return HashSet::new();
    };
    store
        .shares_for_caller(kind, Some(identity), teams.team_ids())
        .await
        .map(|grants| {
            grants
                .iter()
                .map(|grant| member_def_key(&grant.owner, &grant.def_id))
                .collect()
        })
        .unwrap_or_default()
}

/// One caller's standing over a listing of one kind: who is asking, whether
/// they see every row, the teams they stand in, and the shared rows they reach.
#[derive(Debug, Clone)]
pub struct DefListing {
    pub caller: Option<String>,
    pub is_admin: bool,
    pub teams: TeamReach,
    pub shared: HashSet<String>,
}

impl DefListing {
    /// One caller's standing, with their shared rows read from `def_store`.
    pub async fn resolve(
        def_store: Option<&Arc<WorkflowStore>>,
        caller: DefWriter,
        teams: TeamReach,
        kind: &str,
    ) -> Self {
        let shared = shared_def_keys(def_store, &caller, &teams, kind).await;
        Self {
            caller: caller.identity_id,
            is_admin: caller.is_admin,
            teams,
            shared,
        }
    }

    /// The turn's user, for an agent tool: the same writer the tool's write
    /// rule uses, so what it lists and what it may change cannot diverge.
    pub async fn for_turn(
        auth: &TurnDefWriteAuth,
        def_store: Option<&Arc<WorkflowStore>>,
        kind: &str,
    ) -> Self {
        Self::resolve(def_store, auth.writer(), auth.teams(), kind).await
    }

    /// A standing with no stores behind it: no shared rows and no teams. The
    /// single-tenant CLI, the disk-backed runtimes and the tests that exercise
    /// neither.
    pub fn local(caller: Option<&str>, is_admin: bool) -> Self {
        Self {
            caller: caller.map(str::to_string),
            is_admin,
            teams: TeamReach::none(),
            shared: HashSet::new(),
        }
    }

    /// A standing with no shared rows read: the prompt blocks, which name what
    /// a caller reaches by visibility alone and leave the rows shared with them
    /// to the surfaces that can address one by owner.
    pub fn unshared(caller: Option<&str>, teams: TeamReach) -> Self {
        Self {
            caller: caller.map(str::to_string),
            is_admin: false,
            teams,
            shared: HashSet::new(),
        }
    }

    pub fn caller(&self) -> Option<&str> {
        self.caller.as_deref()
    }

    /// Whether this caller sees the row `(owner, visibility, id)` in a listing.
    pub fn sees(&self, owner: Option<&str>, visibility: Visibility, id: &str) -> bool {
        visible_to(
            owner,
            visibility,
            id,
            self.caller(),
            self.is_admin,
            Some(&self.shared),
            &self.teams,
        )
    }
}

/// The grantee set as a client reads it. The owner is never their own grantee,
/// so a record written before that rule — a self-grant was storable and is
/// inert, the owner already manages it — is not handed back for a client to
/// send again and be refused for.
pub fn grant_entries(grants: &[Grant], owner: Option<&str>) -> Vec<GrantEntry> {
    grants
        .iter()
        .filter(|grant| !(grant.principal_type.is_user() && owner == Some(&*grant.principal_id)))
        .map(GrantEntry::from)
        .collect()
}

/// The grantee set a save may persist: every entry names a known principal and
/// level, nobody is listed twice, and the owner is not their own grantee.
pub fn validate_grants(
    entries: Vec<GrantEntry>,
    owner: Option<&str>,
) -> Result<Vec<Grant>, String> {
    entries
        .into_iter()
        .try_fold(Vec::new(), |mut grants: Vec<Grant>, entry| {
            let grant = Grant::try_from(entry)?;
            if grant.principal_id.is_empty() {
                return Err("a share needs a user".to_string());
            }
            if grants.iter().any(|held| {
                (held.principal_type, &held.principal_id)
                    == (grant.principal_type, &grant.principal_id)
            }) {
                return Err(format!("`{}` is listed twice", grant.principal_id));
            }
            if grant.principal_type.is_user() && owner == Some(grant.principal_id.as_str()) {
                return Err("the owner already has full access to it".to_string());
            }
            grants.push(grant);
            Ok(grants)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_core::access::GrantLevel;

    fn entry(principal_type: &str, principal_id: &str, permission: &str) -> GrantEntry {
        GrantEntry {
            principal_type: principal_type.into(),
            principal_id: principal_id.into(),
            permission: permission.into(),
        }
    }

    #[test]
    fn a_grantee_set_names_users_teams_or_nothing_at_all() {
        assert_eq!(
            validate_grants(
                vec![
                    entry("user", "alice", "run"),
                    entry("team", "platform", "manage"),
                ],
                Some("bob"),
            ),
            Ok(vec![
                Grant::user("alice", GrantLevel::Run),
                Grant::team("platform", GrantLevel::Manage),
            ])
        );
        assert_eq!(validate_grants(Vec::new(), Some("bob")), Ok(Vec::new()));
        // The same id as a user and as a team are two grantees, not a duplicate.
        assert!(validate_grants(
            vec![
                entry("user", "platform", "run"),
                entry("team", "platform", "run")
            ],
            None,
        )
        .is_ok());
    }

    #[test]
    fn a_malformed_grantee_set_says_what_is_wrong_with_it() {
        for (entries, message) in [
            (
                vec![entry("group", "platform", "run")],
                "`group` is not a principal type — use `user` or `team`",
            ),
            (
                vec![entry("user", "alice", "own")],
                "unknown share permission `own`",
            ),
            (vec![entry("user", " ", "run")], "a share needs a user"),
            (
                vec![
                    entry("user", "alice", "run"),
                    entry("user", "alice", "manage"),
                ],
                "`alice` is listed twice",
            ),
            (
                vec![entry("user", "bob", "run")],
                "the owner already has full access to it",
            ),
            // Padding names the same grantee, so it reaches the same refusals.
            (
                vec![entry("user", " bob ", "run")],
                "the owner already has full access to it",
            ),
            (
                vec![
                    entry("user", "alice", "run"),
                    entry("user", " alice", "run"),
                ],
                "`alice` is listed twice",
            ),
        ] {
            assert_eq!(
                validate_grants(entries, Some("bob")).unwrap_err(),
                message.to_string()
            );
        }
    }

    /// The owner's own grant is inert — they manage the resource anyway — and a
    /// record written before it was refused must not travel back to a client
    /// that would then be told its unchanged set is invalid.
    #[test]
    fn a_grantee_set_a_client_reads_never_names_the_owner() {
        let grants = [
            Grant::user("alice", GrantLevel::Manage),
            Grant::user("bob", GrantLevel::Run),
            Grant::team("alice", GrantLevel::Run),
        ];
        assert_eq!(
            grant_entries(&grants, Some("alice")),
            vec![
                GrantEntry::from(&grants[1]),
                // A team that happens to share the owner's id is a different
                // grantee, and stays.
                GrantEntry::from(&grants[2]),
            ]
        );
        assert_eq!(grant_entries(&grants, None).len(), 3);
        assert_eq!(
            validate_grants(grant_entries(&grants, Some("alice")), Some("alice")),
            Ok(vec![grants[1].clone(), grants[2].clone()])
        );
    }
}
