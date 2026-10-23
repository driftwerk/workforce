//! Who may reach an owned resource, in one vocabulary.
//!
//! Every owned thing in the hub — a definition row, a WASM skill record, an
//! external skill directory — records the same three facts: an owner, a
//! visibility, and a set of grants. [`access_with`] turns those into the one
//! standing every surface reads. Storage stays each model's own business; the
//! rule does not. A `manage` grant lets its holder change the resource, never
//! who else reaches it — only the owner (or an administrator overriding them)
//! decides that, so a delegated edit can never become a delegated grant.

use serde::{Deserialize, Serialize};

/// Who reaches a resource beyond its owner and its grantees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    #[default]
    Private,
    /// Everyone who shares a team with the owner, at [`AccessLevel::Run`].
    Team,
    /// Everyone on the install, at [`AccessLevel::Run`]. Stored as `global`,
    /// which is the word the records and the `visibility` column already hold.
    Global,
}

impl Visibility {
    /// The word the records and the `visibility` column hold.
    pub const fn as_str(self) -> &'static str {
        match self {
            Visibility::Private => "private",
            Visibility::Team => "team",
            Visibility::Global => "global",
        }
    }

    /// A stored value, exactly as written. Anything else is unreadable, which
    /// every reader treats as the closed default.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "private" => Some(Visibility::Private),
            "team" => Some(Visibility::Team),
            "global" => Some(Visibility::Global),
            _ => None,
        }
    }

    /// The word for a visibility in the surfaces people read.
    pub const fn word(self) -> &'static str {
        match self {
            Visibility::Private => "private",
            Visibility::Team => "team",
            Visibility::Global => "everyone",
        }
    }

    /// The visibility a person typed. `everyone` is the word chat and the UI
    /// use for the stored `global`; both are accepted so the API's vocabulary
    /// and the human one never diverge into an error.
    pub fn parse_word(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "private" | "me" => Some(Visibility::Private),
            "team" | "teams" | "my-team" => Some(Visibility::Team),
            "everyone" | "global" | "all" => Some(Visibility::Global),
            _ => None,
        }
    }
}

/// A stored record is read the way a row is: a value this hub cannot read
/// leaves the resource at the closed default instead of making the whole record
/// unparseable, which would hide it from its owner as well as from everyone
/// else.
impl<'de> Deserialize<'de> for Visibility {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let stored = String::deserialize(deserializer)?;
        Ok(Visibility::parse(&stored).unwrap_or_default())
    }
}

/// A caller's standing on one resource. `Manage` subsumes `Run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AccessLevel {
    None,
    Run,
    Manage,
}

impl AccessLevel {
    /// Whether this standing satisfies `needed`.
    pub fn allows(self, needed: AccessLevel) -> bool {
        self >= needed
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            AccessLevel::None => "none",
            AccessLevel::Run => "run",
            AccessLevel::Manage => "manage",
        }
    }
}

/// What a grantee may do. `Manage` confers everything `Run` does, so its
/// ordering (`Run < Manage`) makes the satisfies-check a comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantLevel {
    Run,
    Manage,
}

impl GrantLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            GrantLevel::Run => "run",
            GrantLevel::Manage => "manage",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "run" => Some(GrantLevel::Run),
            "manage" => Some(GrantLevel::Manage),
            _ => None,
        }
    }

    /// Whether this grant satisfies a requirement for `needed`.
    pub fn allows(self, needed: GrantLevel) -> bool {
        self >= needed
    }

    /// The standing this grant confers.
    pub const fn level(self) -> AccessLevel {
        match self {
            GrantLevel::Run => AccessLevel::Run,
            GrantLevel::Manage => AccessLevel::Manage,
        }
    }
}

/// Whether a grantee is one identity or a whole team.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    #[default]
    User,
    Team,
}

impl PrincipalKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            PrincipalKind::User => "user",
            PrincipalKind::Team => "team",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "user" => Some(PrincipalKind::User),
            "team" => Some(PrincipalKind::Team),
            _ => None,
        }
    }

    pub const fn is_user(&self) -> bool {
        matches!(self, PrincipalKind::User)
    }
}

/// One grantee of one resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    #[serde(default, skip_serializing_if = "PrincipalKind::is_user")]
    pub principal_type: PrincipalKind,
    /// `user_id` is the name the skill records have always used; keeping it
    /// means a hub older than this one still reads the records this one writes.
    #[serde(rename = "user_id", alias = "principal_id")]
    pub principal_id: String,
    pub permission: GrantLevel,
}

impl Grant {
    pub fn user(id: impl Into<String>, permission: GrantLevel) -> Self {
        Self {
            principal_type: PrincipalKind::User,
            principal_id: id.into(),
            permission,
        }
    }

    pub fn team(id: impl Into<String>, permission: GrantLevel) -> Self {
        Self {
            principal_type: PrincipalKind::Team,
            principal_id: id.into(),
            permission,
        }
    }
}

/// One grantee as the HTTP surfaces spell it — the shape the UI already speaks
/// and the only place a client's spelling is parsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantEntry {
    pub principal_type: String,
    pub principal_id: String,
    pub permission: String,
}

impl From<&Grant> for GrantEntry {
    fn from(grant: &Grant) -> Self {
        Self {
            principal_type: grant.principal_type.as_str().to_string(),
            principal_id: grant.principal_id.clone(),
            permission: grant.permission.as_str().to_string(),
        }
    }
}

impl TryFrom<GrantEntry> for Grant {
    type Error = String;

    fn try_from(entry: GrantEntry) -> Result<Self, Self::Error> {
        let principal_type = PrincipalKind::parse(&entry.principal_type).ok_or_else(|| {
            format!(
                "`{}` is not a principal type — use `user` or `team`",
                entry.principal_type
            )
        })?;
        let permission = GrantLevel::parse(&entry.permission)
            .ok_or_else(|| format!("unknown share permission `{}`", entry.permission))?;
        // The id is compared — against the owner, against the other grantees and
        // against a caller — so it is trimmed here, once, rather than at each
        // comparison, where one omission would let `" alice "` past all three.
        Ok(Self {
            principal_type,
            principal_id: entry.principal_id.trim().to_string(),
            permission,
        })
    }
}

/// The teams one caller stands in, asked lazily: nothing resolves a team for a
/// private or global resource. Shared across threads, because a listing is
/// filtered inside the async handler that read it.
pub trait TeamMembership: Send + Sync {
    /// Whether the caller belongs to the team a grant names.
    fn in_team(&self, team_id: &str) -> bool;
    /// Whether the caller shares at least one team with `owner`.
    fn shares_team_with(&self, owner: &str) -> bool;
}

/// A caller with no teams: a hub with no team store, the CLI, and every test
/// that does not exercise team reach.
pub struct NoTeams;

impl TeamMembership for NoTeams {
    fn in_team(&self, _team_id: &str) -> bool {
        false
    }

    fn shares_team_with(&self, _owner: &str) -> bool {
        false
    }
}

/// The best grant reaching `caller` — the answer a skill record gives from
/// memory, and a definition store gives with one indexed query.
pub fn granted_to(
    grants: &[Grant],
    caller: Option<&str>,
    teams: &dyn TeamMembership,
) -> Option<GrantLevel> {
    grants
        .iter()
        .filter(|g| match g.principal_type {
            PrincipalKind::User => caller == Some(g.principal_id.as_str()),
            PrincipalKind::Team => teams.in_team(&g.principal_id),
        })
        .map(|g| g.permission)
        .max()
}

/// The standing a caller holds once their grant is known. `in_owner_team` is
/// called only for a team-visible resource, so nothing resolves teams for a
/// private or global one.
pub fn access_with(
    owner: Option<&str>,
    visibility: Visibility,
    caller: Option<&str>,
    granted: Option<GrantLevel>,
    in_owner_team: impl FnOnce() -> bool,
) -> AccessLevel {
    match (owner, caller) {
        (Some(owner), Some(caller)) if owner == caller => AccessLevel::Manage,
        _ => granted
            .map(GrantLevel::level)
            .into_iter()
            .chain(
                open_to_all(owner, visibility, caller, in_owner_team).then_some(AccessLevel::Run),
            )
            .max()
            .unwrap_or(AccessLevel::None),
    }
}

fn open_to_all(
    owner: Option<&str>,
    visibility: Visibility,
    caller: Option<&str>,
    in_owner_team: impl FnOnce() -> bool,
) -> bool {
    match (owner, visibility) {
        (None, _) => true,
        (Some(_), Visibility::Global) => true,
        (Some(_), Visibility::Team) => caller.is_some() && in_owner_team(),
        (Some(_), Visibility::Private) => false,
    }
}

/// What a write to who-may-reach does to the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerWrite {
    Unchanged,
    To(String),
}

/// One replacement of who reaches a resource: the set is authoritative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessWrite {
    pub owner: OwnerWrite,
    pub visibility: Visibility,
    pub grants: Vec<Grant>,
}

impl AccessWrite {
    /// A change of who reaches a resource that leaves its owner alone.
    pub fn kept(visibility: Visibility, grants: Vec<Grant>) -> Self {
        Self {
            owner: OwnerWrite::Unchanged,
            visibility,
            grants,
        }
    }

    /// The owner after this write, given the one it replaces.
    pub fn owner_after(&self, current: Option<&str>) -> Option<String> {
        match &self.owner {
            OwnerWrite::Unchanged => current.map(str::to_string),
            OwnerWrite::To(next) => Some(next.clone()),
        }
    }
}

/// What the outgoing owner is left with when a resource changes hands.
///
/// Ownership subsumes every grant, so until the handover lands they cannot be
/// their own grantee — [`crate::access`]' rule, enforced wherever a grantee set
/// is validated. The choice is therefore part of the transfer and is written in
/// the same breath as it, never as a share beforehand: a share that lands ahead
/// of a transfer that then fails leaves standing nobody asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutgoingAccess {
    /// Nothing at all: ownership was their only standing. The default, and what
    /// every surface that does not offer the choice passes.
    #[default]
    Nothing,
    /// A `manage` grant — they go on editing it, but never re-share it and
    /// never take it back.
    Manage,
}

impl OutgoingAccess {
    /// A request's `keep_manage` flag as the choice it names.
    pub const fn keeping_manage(keep_manage: bool) -> Self {
        match keep_manage {
            true => Self::Manage,
            false => Self::Nothing,
        }
    }

    pub const fn keeps_manage(self) -> bool {
        matches!(self, Self::Manage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A caller in the named teams, whose teammates are the named owners.
    struct Teams {
        teams: &'static [&'static str],
        peers: &'static [&'static str],
    }

    impl TeamMembership for Teams {
        fn in_team(&self, team_id: &str) -> bool {
            self.teams.contains(&team_id)
        }

        fn shares_team_with(&self, owner: &str) -> bool {
            self.peers.contains(&owner)
        }
    }

    const PLATFORM: Teams = Teams {
        teams: &["platform"],
        peers: &["alice"],
    };

    fn access(
        owner: Option<&str>,
        visibility: Visibility,
        caller: Option<&str>,
        grants: &[Grant],
        teams: &dyn TeamMembership,
    ) -> AccessLevel {
        access_with(
            owner,
            visibility,
            caller,
            granted_to(grants, caller, teams),
            || owner.is_some_and(|owner| teams.shares_team_with(owner)),
        )
    }

    #[test]
    fn the_owner_manages_and_everyone_else_stands_where_they_were_put() {
        let run = Grant::user("bob", GrantLevel::Run);
        let manage = Grant::user("carol", GrantLevel::Manage);
        let team_run = Grant::team("platform", GrantLevel::Run);
        let team_manage = Grant::team("triage", GrantLevel::Manage);
        let grants = [run, manage, team_run, team_manage];
        let teams = Teams {
            teams: &["platform", "triage"],
            peers: &["alice"],
        };

        for visibility in [Visibility::Private, Visibility::Team, Visibility::Global] {
            assert_eq!(
                access(Some("alice"), visibility, Some("alice"), &grants, &NoTeams),
                AccessLevel::Manage
            );
            assert_eq!(
                access(Some("alice"), visibility, Some("bob"), &grants, &NoTeams),
                AccessLevel::Run
            );
            assert_eq!(
                access(Some("alice"), visibility, Some("carol"), &grants, &NoTeams),
                AccessLevel::Manage
            );
            // A team grant reaches a member at the level it names, whatever the
            // visibility, and nobody outside it.
            assert_eq!(
                access(Some("alice"), visibility, Some("dave"), &grants, &teams),
                AccessLevel::Manage
            );
        }

        // Without a grant, only the visibility is left.
        assert_eq!(
            access(
                Some("alice"),
                Visibility::Private,
                Some("dave"),
                &[],
                &NoTeams
            ),
            AccessLevel::None
        );
        assert_eq!(
            access(
                Some("alice"),
                Visibility::Global,
                Some("dave"),
                &[],
                &NoTeams
            ),
            AccessLevel::Run
        );
        assert_eq!(
            access(
                Some("alice"),
                Visibility::Team,
                Some("dave"),
                &[],
                &PLATFORM
            ),
            AccessLevel::Run
        );
    }

    #[test]
    fn team_visibility_grants_run_and_never_manage() {
        // A teammate of the owner runs it; a member of no team of theirs does
        // not; and nobody reaches it through a team the owner is not in.
        assert_eq!(
            access(Some("alice"), Visibility::Team, Some("bob"), &[], &PLATFORM),
            AccessLevel::Run
        );
        let no_peers = Teams {
            teams: &["platform"],
            peers: &[],
        };
        assert_eq!(
            access(Some("alice"), Visibility::Team, Some("bob"), &[], &no_peers),
            AccessLevel::None
        );
        // An owner in no team keeps it to themselves, and a deleted owner (an
        // id nobody holds) is nobody's teammate.
        assert_eq!(
            access(Some("gone"), Visibility::Team, Some("bob"), &[], &PLATFORM),
            AccessLevel::None
        );
    }

    #[test]
    fn a_caller_with_no_identity_never_reaches_a_team() {
        assert_eq!(
            access(Some("alice"), Visibility::Team, None, &[], &PLATFORM),
            AccessLevel::None
        );
        assert_eq!(
            access(Some("alice"), Visibility::Global, None, &[], &NoTeams),
            AccessLevel::Run
        );
        assert_eq!(
            access(Some("alice"), Visibility::Private, None, &[], &NoTeams),
            AccessLevel::None
        );
    }

    #[test]
    fn a_resource_nobody_owns_is_everyones_to_run() {
        for visibility in [Visibility::Private, Visibility::Team, Visibility::Global] {
            assert_eq!(
                access(None, visibility, Some("bob"), &[], &NoTeams),
                AccessLevel::Run
            );
            assert_eq!(
                access(None, visibility, None, &[], &NoTeams),
                AccessLevel::Run
            );
        }
    }

    #[test]
    fn teams_are_resolved_only_for_a_team_visible_resource() {
        let panics = || -> bool { panic!("teams resolved for a resource that has no team reach") };
        for visibility in [Visibility::Private, Visibility::Global] {
            access_with(Some("alice"), visibility, Some("bob"), None, panics);
        }
        // Nor for the owner, nor for a caller with no identity at all.
        access_with(Some("alice"), Visibility::Team, Some("alice"), None, panics);
        access_with(Some("alice"), Visibility::Team, None, None, panics);
    }

    #[test]
    fn the_highest_grant_reaching_a_caller_is_the_one_that_counts() {
        let grants = [
            Grant::user("bob", GrantLevel::Run),
            Grant::team("platform", GrantLevel::Manage),
            Grant::team("triage", GrantLevel::Run),
        ];
        assert_eq!(
            granted_to(&grants, Some("bob"), &PLATFORM),
            Some(GrantLevel::Manage)
        );
        assert_eq!(
            granted_to(&grants, Some("bob"), &NoTeams),
            Some(GrantLevel::Run)
        );
        assert_eq!(granted_to(&grants, Some("dave"), &NoTeams), None);
        assert_eq!(
            granted_to(&grants, None, &PLATFORM),
            Some(GrantLevel::Manage)
        );
        assert_eq!(granted_to(&[], Some("bob"), &PLATFORM), None);
    }

    #[test]
    fn a_grant_reads_and_writes_the_record_shape_it_always_had() {
        let user: Grant = serde_json::from_value(json!({"user_id": "alice", "permission": "run"}))
            .expect("legacy record");
        assert_eq!(user, Grant::user("alice", GrantLevel::Run));
        assert_eq!(
            serde_json::to_value(&user).unwrap(),
            json!({"user_id": "alice", "permission": "run"})
        );

        let team: Grant = serde_json::from_value(
            json!({"principal_id": "platform", "principal_type": "team", "permission": "manage"}),
        )
        .expect("wire spelling");
        assert_eq!(team, Grant::team("platform", GrantLevel::Manage));
        assert_eq!(
            serde_json::to_value(&team).unwrap(),
            json!({"principal_type": "team", "user_id": "platform", "permission": "manage"})
        );
        assert_eq!(
            serde_json::from_value::<Grant>(serde_json::to_value(&team).unwrap()).unwrap(),
            team
        );
    }

    #[test]
    fn a_visibility_is_stored_one_way_and_spoken_another() {
        for (visibility, stored, word) in [
            (Visibility::Private, "private", "private"),
            (Visibility::Team, "team", "team"),
            (Visibility::Global, "global", "everyone"),
        ] {
            assert_eq!(visibility.as_str(), stored);
            assert_eq!(visibility.word(), word);
            assert_eq!(Visibility::parse(stored), Some(visibility));
            assert_eq!(Visibility::parse_word(word), Some(visibility));
        }
        for (raw, visibility) in [
            ("private", Visibility::Private),
            ("ME", Visibility::Private),
            (" team ", Visibility::Team),
            ("teams", Visibility::Team),
            ("my-team", Visibility::Team),
            ("Everyone", Visibility::Global),
            ("global", Visibility::Global),
            ("all", Visibility::Global),
        ] {
            assert_eq!(Visibility::parse_word(raw), Some(visibility), "{raw}");
        }
        // The stored form is exact: a word nobody stores is unreadable, and an
        // unreadable value leaves a reader with the closed default.
        for raw in ["everyone", "PRIVATE", " team", "public", ""] {
            assert_eq!(Visibility::parse(raw), None, "{raw}");
        }
        assert_eq!(Visibility::parse_word("public"), None);
        assert_eq!(
            Visibility::parse("public").unwrap_or_default(),
            Visibility::Private
        );
        assert_eq!(
            serde_json::to_value(Visibility::Team).unwrap(),
            json!("team")
        );
    }

    /// A record holding a reach this hub does not know — one a newer hub wrote
    /// — still reads, at the closed default. The record stays legible; only the
    /// value nobody here can honour is dropped.
    #[test]
    fn an_unreadable_stored_visibility_is_the_closed_one() {
        for (stored, visibility) in [
            (json!("private"), Visibility::Private),
            (json!("team"), Visibility::Team),
            (json!("global"), Visibility::Global),
            (json!("everyone"), Visibility::Private),
            (json!("whoever-comes-next"), Visibility::Private),
        ] {
            assert_eq!(
                serde_json::from_value::<Visibility>(stored.clone()).unwrap(),
                visibility,
                "{stored}"
            );
        }
        assert!(serde_json::from_value::<Visibility>(json!(7)).is_err());
    }

    #[test]
    fn levels_compare_and_read_as_their_stored_word() {
        assert!(GrantLevel::Manage.allows(GrantLevel::Run));
        assert!(!GrantLevel::Run.allows(GrantLevel::Manage));
        assert_eq!(GrantLevel::parse("manage"), Some(GrantLevel::Manage));
        assert_eq!(GrantLevel::parse("run"), Some(GrantLevel::Run));
        assert_eq!(GrantLevel::parse("bogus"), None);
        assert_eq!(GrantLevel::Run.level(), AccessLevel::Run);
        assert_eq!(GrantLevel::Manage.level(), AccessLevel::Manage);
        assert!(AccessLevel::Manage.allows(AccessLevel::Run));
        assert!(!AccessLevel::None.allows(AccessLevel::Run));
        assert_eq!(AccessLevel::None.as_str(), "none");
        assert_eq!(
            serde_json::to_value(GrantLevel::Manage).unwrap(),
            json!("manage")
        );
    }

    #[test]
    fn a_wire_grantee_converts_both_ways_or_says_why_not() {
        let entry = GrantEntry {
            principal_type: "team".into(),
            principal_id: "platform".into(),
            permission: "manage".into(),
        };
        let grant = Grant::try_from(entry.clone()).expect("valid");
        assert_eq!(grant, Grant::team("platform", GrantLevel::Manage));
        assert_eq!(GrantEntry::from(&grant), entry);
        assert_eq!(
            GrantEntry::from(&Grant::user("alice", GrantLevel::Run)),
            GrantEntry {
                principal_type: "user".into(),
                principal_id: "alice".into(),
                permission: "run".into(),
            }
        );
        assert_eq!(
            Grant::try_from(GrantEntry {
                principal_type: "group".into(),
                principal_id: "platform".into(),
                permission: "run".into(),
            })
            .unwrap_err(),
            "`group` is not a principal type — use `user` or `team`"
        );
        assert_eq!(
            Grant::try_from(GrantEntry {
                principal_type: "user".into(),
                principal_id: "alice".into(),
                permission: "own".into(),
            })
            .unwrap_err(),
            "unknown share permission `own`"
        );
        // A padded id is the same grantee, so it cannot slip past the checks
        // that read one — the owner, the duplicate and the caller.
        assert_eq!(
            Grant::try_from(GrantEntry {
                principal_type: "user".into(),
                principal_id: "  alice\n".into(),
                permission: "run".into(),
            }),
            Ok(Grant::user("alice", GrantLevel::Run))
        );
    }

    #[test]
    fn an_access_write_says_who_owns_the_resource_afterwards() {
        let kept = AccessWrite::kept(Visibility::Team, vec![Grant::user("bob", GrantLevel::Run)]);
        assert_eq!(kept.owner, OwnerWrite::Unchanged);
        assert_eq!(kept.owner_after(Some("alice")), Some("alice".to_string()));
        assert_eq!(kept.owner_after(None), None);
        let moved = AccessWrite {
            owner: OwnerWrite::To("bob".into()),
            ..kept
        };
        assert_eq!(moved.owner_after(Some("alice")), Some("bob".to_string()));
        assert_eq!(moved.owner_after(None), Some("bob".to_string()));
    }
}
