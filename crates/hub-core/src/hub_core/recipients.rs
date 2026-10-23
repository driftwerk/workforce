//! Naming a grantee in chat: a person by an @-mention, an email address or a
//! Workforce user id, or a whole team by `team:<name>`. Every `cmd:` surface
//! that hands something to somebody — a skill ([`crate::skill_consent`]), a
//! definition ([`crate::def_cmd`]) — reads the name through here, so one typo
//! is answered with one sentence wherever it was typed, and the reply names the
//! person or the team rather than their id.

use workforce_core::access::{Grant, GrantLevel, PrincipalKind};
use workforce_identity::store::IdentityStore;
use workforce_identity::team::TeamStore;

/// The sentence a surface answers with when there is nobody to look a name up
/// in — a hub with no identity store cannot name anyone on any surface.
pub const NO_IDENTITY_STORE: &str = "No identity store is configured, so users can't be looked up.";

/// The prefix that names a team rather than a person.
const TEAM_PREFIX: &str = "team:";

/// How a caller named someone. The Slack mention grammar identifies its own
/// platform, so no surface context is needed to read one.
#[derive(Debug, PartialEq, Eq)]
pub enum RecipientRef<'a> {
    SlackUser(&'a str),
    Email(&'a str),
    IdentityId(&'a str),
    Team(&'a str),
}

/// Who a grantee turned out to be, and how they read in a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recipient {
    User { id: String, display: String },
    Team { id: String, name: String },
}

impl Recipient {
    pub fn id(&self) -> &str {
        match self {
            Recipient::User { id, .. } | Recipient::Team { id, .. } => id,
        }
    }

    pub const fn principal(&self) -> PrincipalKind {
        match self {
            Recipient::User { .. } => PrincipalKind::User,
            Recipient::Team { .. } => PrincipalKind::Team,
        }
    }

    /// The grantee as the record keeps them.
    pub fn grant(&self, permission: GrantLevel) -> Grant {
        Grant {
            principal_type: self.principal(),
            principal_id: self.id().to_string(),
            permission,
        }
    }

    /// How they read inside a sentence.
    pub fn named(&self) -> String {
        match self {
            Recipient::User { display, .. } => display.clone(),
            Recipient::Team { name, .. } => format!("the {name} team"),
        }
    }

    /// How they read opening one.
    pub fn opening(&self) -> String {
        match self {
            Recipient::User { display, .. } => display.clone(),
            Recipient::Team { name, .. } => format!("The {name} team"),
        }
    }

    /// Who acts, once the grant is theirs: a person, or a team's members.
    pub const fn who(&self) -> &'static str {
        match self {
            Recipient::User { .. } => "they",
            Recipient::Team { .. } => "its members",
        }
    }
}

pub fn parse(token: &str) -> RecipientRef<'_> {
    let token = token.trim();
    if let Some(team) = token.strip_prefix(TEAM_PREFIX) {
        return RecipientRef::Team(team);
    }
    match token
        .strip_prefix("<@")
        .and_then(|rest| rest.strip_suffix('>'))
        .map(|inner| inner.split('|').next().unwrap_or(inner))
    {
        Some(platform_id) => RecipientRef::SlackUser(platform_id),
        None if token.contains('@') => RecipientRef::Email(token),
        None => RecipientRef::IdentityId(token),
    }
}

/// Resolve a grantee the caller named. Teams are matched by name, ignoring
/// case, over the teams this hub knows.
pub fn resolve(
    identities: &dyn IdentityStore,
    teams: Option<&dyn TeamStore>,
    token: &str,
) -> Result<Recipient, String> {
    match parse(token) {
        RecipientRef::Team(name) => {
            resolve_team(teams, name).unwrap_or_else(|| Err(unknown(token)))
        }
        RecipientRef::SlackUser(id) => named(identities.get_by_linked_id("slack", id), token),
        RecipientRef::Email(email) => named(identities.get_by_email(email), token),
        RecipientRef::IdentityId(id) => named(
            uuid::Uuid::parse_str(id).map_or(Ok(None), |u| identities.get_by_id(u)),
            token,
        ),
    }
}

/// One identity lookup as a recipient, or the sentence for a name nobody holds.
fn named(
    found: eyre::Result<Option<workforce_identity::types::Identity>>,
    token: &str,
) -> Result<Recipient, String> {
    found
        .ok()
        .flatten()
        .map(|i| Recipient::User {
            id: i.id.to_string(),
            display: i.display_name,
        })
        .ok_or_else(|| unknown(token))
}

/// The one team called `name`. `None` is "no team of that name", which the
/// caller words with every other way of naming a grantee.
fn resolve_team(teams: Option<&dyn TeamStore>, name: &str) -> Option<Result<Recipient, String>> {
    let name = name.trim();
    let mut matched: Vec<workforce_identity::team::Team> = teams?
        .list_teams()
        .ok()?
        .into_iter()
        .filter(|team| team.name.eq_ignore_ascii_case(name))
        .collect();
    match matched.len() {
        0 => None,
        1 => {
            let team = matched.swap_remove(0);
            Some(Ok(Recipient::Team {
                id: team.id.to_string(),
                name: team.name,
            }))
        }
        _ => {
            let mut ids: Vec<String> = matched.iter().map(|team| team.id.to_string()).collect();
            ids.sort();
            Some(Err(format!(
                "More than one team is called `{name}`. Name it by id: {}.",
                ids.join(", ")
            )))
        }
    }
}

/// The member a resource may be handed to: whoever `token` names, resolved
/// from the identity store at the moment of the transfer and required to be
/// active *now* rather than at whatever moment a token was minted.
///
/// Deliberately **fail-closed**, where
/// [`crate::workflow_runtime::WorkflowRuntime`]'s `owner_is_active` is
/// fail-open. That one answers `true` when no identity store is wired, which
/// is right for its job — not killing a single-operator deployment's runs.
/// Here the same answer would accept any string as a new owner and strand the
/// resource under an id that names nobody, which nobody could then undo. No
/// identity store, no transfer.
///
/// [`resolve`] answers "who is this?", not "may they own something?" — a
/// suspended identity resolves perfectly well — so the standing check is this
/// function's own. Ownership is a single identity everywhere in the model, and
/// a team-owned resource has no coherent run identity, so a team is refused in
/// the sentence every surface already uses for a person-shaped slot.
pub fn active_owner(
    identities: Option<&dyn IdentityStore>,
    teams: Option<&dyn TeamStore>,
    noun: &str,
    token: &str,
) -> Result<ActiveMember, String> {
    let store = identities.ok_or_else(|| NO_IDENTITY_STORE.to_string())?;
    let Recipient::User { id, display } = resolve(store, teams, token)? else {
        return Err(person_not_team(noun));
    };
    let active = uuid::Uuid::parse_str(&id)
        .ok()
        .and_then(|uuid| store.get_by_id(uuid).ok().flatten())
        .is_some_and(|i| i.status == workforce_identity::types::IdentityStatus::Active);
    match active {
        true => Ok(ActiveMember { id, display }),
        false => Err(format!(
            "{display} is not an active member of this install, so a {noun} cannot be \
             transferred to them."
        )),
    }
}

/// The sentence a person-shaped slot gets when it is handed a team. Worded
/// here so the slot that names a row's owner and the slot that names its next
/// one read identically.
pub fn person_not_team(noun: &str) -> String {
    format!("A {noun} belongs to a person, not a team.")
}

/// A member who may be given something: narrower than [`Recipient`] by
/// construction, so no caller has an impossible team case to answer for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveMember {
    pub id: String,
    /// How they read in a sentence.
    pub display: String,
}

/// The sentence a name nobody answers to gets, on every surface.
fn unknown(token: &str) -> String {
    format!(
        "I can't tell who `{}` is. Name them with an @-mention, their email address, or \
         their Workforce user id, or a team with `team:<name>`.",
        token.trim()
    )
}

/// How an identity reads to a person: their display name, or the bare id when
/// nothing resolves it — an account since deleted still has to be listable.
pub fn display_name(store: Option<&dyn IdentityStore>, identity_id: &str) -> String {
    store
        .and_then(|store| {
            uuid::Uuid::parse_str(identity_id)
                .ok()
                .and_then(|u| store.get_by_id(u).ok().flatten())
        })
        .map(|i| i.display_name)
        .unwrap_or_else(|| identity_id.to_string())
}

/// How a team reads to a person: its name, while the team exists. A grant
/// or a cost row outlives the team it names, so each caller says how a team
/// since gone reads.
pub fn team_name(store: Option<&dyn TeamStore>, team_id: &str) -> Option<String> {
    store
        .and_then(|store| {
            uuid::Uuid::parse_str(team_id)
                .ok()
                .and_then(|id| store.get_team(id).ok().flatten())
        })
        .map(|team| team.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use workforce_identity::sqlite::SqliteIdentityStore;
    use workforce_identity::store::IdentityAndTeamStore;
    use workforce_identity::team::Team;
    use workforce_identity::types::{Identity, IdentityRole};

    fn store_with_alice() -> (Arc<SqliteIdentityStore>, Identity) {
        let store = Arc::new(SqliteIdentityStore::in_memory().unwrap());
        let mut alice = Identity::new("Alice", IdentityRole::Member);
        alice.email = Some("alice@example.com".into());
        alice.linked_ids.insert("slack".into(), "U1".into());
        store.create(&alice).unwrap();
        (store, alice)
    }

    #[test]
    fn a_token_names_its_own_grammar() {
        assert_eq!(parse("<@U1>"), RecipientRef::SlackUser("U1"));
        assert_eq!(parse("<@U1|alice>"), RecipientRef::SlackUser("U1"));
        assert_eq!(
            parse(" alice@example.com "),
            RecipientRef::Email("alice@example.com")
        );
        assert_eq!(parse("not-a-uuid"), RecipientRef::IdentityId("not-a-uuid"));
        assert_eq!(parse(" team:platform "), RecipientRef::Team("platform"));
    }

    #[test]
    fn every_spelling_of_one_person_resolves_to_them() {
        let (store, alice) = store_with_alice();
        let id = alice.id.to_string();
        for token in ["<@U1>", "alice@example.com", &id] {
            assert_eq!(
                resolve(store.as_ref(), None, token).unwrap(),
                Recipient::User {
                    id: id.clone(),
                    display: "Alice".to_string()
                }
            );
        }
    }

    #[test]
    fn a_team_resolves_by_name_whatever_its_case() {
        let (store, alice) = store_with_alice();
        let platform = Team::new("platform", alice.id);
        store.create_team(&platform).unwrap();
        let teams = Arc::clone(&store).as_team_store();
        let resolved = resolve(store.as_ref(), Some(teams.as_ref()), "team:PLATFORM").unwrap();
        assert_eq!(
            resolved,
            Recipient::Team {
                id: platform.id.to_string(),
                name: "platform".to_string()
            }
        );
        assert_eq!(resolved.named(), "the platform team");
        assert_eq!(resolved.opening(), "The platform team");
        assert_eq!(resolved.who(), "its members");
        assert_eq!(
            resolved.grant(GrantLevel::Run),
            Grant::team(platform.id.to_string(), GrantLevel::Run)
        );

        // Without a team store, and for a name nobody answers to, there is
        // nothing to hand anything to.
        assert!(resolve(store.as_ref(), None, "team:platform").is_err());
        assert!(resolve(store.as_ref(), Some(teams.as_ref()), "team:ops").is_err());
    }

    #[test]
    fn two_teams_of_one_name_are_named_by_id() {
        let (store, alice) = store_with_alice();
        let (first, second) = (
            Team::new("platform", alice.id),
            Team::new("platform", alice.id),
        );
        store.create_team(&first).unwrap();
        store.create_team(&second).unwrap();
        let teams = Arc::clone(&store).as_team_store();
        let mut ids = [first.id.to_string(), second.id.to_string()];
        ids.sort();
        assert_eq!(
            resolve(store.as_ref(), Some(teams.as_ref()), "team:platform").unwrap_err(),
            format!(
                "More than one team is called `platform`. Name it by id: {}.",
                ids.join(", ")
            )
        );
    }

    #[test]
    fn an_unknown_name_names_every_way_of_spelling_one() {
        let (store, _alice) = store_with_alice();
        assert_eq!(
            resolve(store.as_ref(), None, " ghost ").unwrap_err(),
            "I can't tell who `ghost` is. Name them with an @-mention, their email address, \
             or their Workforce user id, or a team with `team:<name>`."
        );
    }

    #[test]
    fn a_grantee_reads_as_their_name_or_their_bare_id() {
        let (store, alice) = store_with_alice();
        let platform = Team::new("platform", alice.id);
        store.create_team(&platform).unwrap();
        let teams = Arc::clone(&store).as_team_store();
        let identities: Option<&dyn IdentityStore> = Some(store.as_ref());
        assert_eq!(display_name(identities, &alice.id.to_string()), "Alice");
        assert_eq!(display_name(identities, "gone"), "gone");
        assert_eq!(
            display_name(None, &alice.id.to_string()),
            alice.id.to_string()
        );
        assert_eq!(
            team_name(Some(teams.as_ref()), &platform.id.to_string()).as_deref(),
            Some("platform")
        );
        assert_eq!(team_name(Some(teams.as_ref()), "gone"), None);
        assert_eq!(team_name(None, &platform.id.to_string()), None);
    }
}
