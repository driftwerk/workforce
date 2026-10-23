//! Who may write a definition row — workflow, playbook, monitor or
//! consolidation template — decided once, for the gateway's HTTP handlers and
//! the agent tools alike, so a chat turn can never change what a request
//! could not.
//!
//! A definition lives on one row: the system row (no owner, operator-managed)
//! or a member's row. An administrator writes the system row. A member's row
//! is written by that member, by someone they granted a `manage` share, or by
//! a system administrator — who may also share it, change its visibility,
//! transfer it and delete it, so an install can still be administered when its
//! owner is away, suspended or gone. Every such override is recorded against
//! the administrator ([`crate::resource_audit`]); an owner's own write records
//! nothing, so the log separates the two, and a read records nothing because
//! an administrator's listing already sees every row. A writer who names no
//! row still lands on their own (an administrator on the system row), so a
//! member re-saving the id of a shared definition gets a copy of their own and
//! the shared one stays as it was.

use std::sync::Arc;

use async_trait::async_trait;
use workforce_agent::runner::SYSTEM_USER_ID;
use workforce_core::access::{AccessLevel, GrantLevel, OutgoingAccess, TeamMembership, Visibility};
use workforce_identity::team::TeamStore;
use workforce_orchestrator::workflow_store::{
    def_access, RunAsWrite, TransferFailed, Transferred, WorkflowStore, SYSTEM_OWNER,
};

use crate::authz::{never_admin, IsAdminFn};
use crate::team_reach::TeamReach;

/// The principal attempting the write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DefWriter {
    /// Authenticated identity; `None` for the static gateway token and for a
    /// turn bound to no user.
    pub identity_id: Option<String>,
    /// Holds the active SystemAdmin role, or is the static gateway token.
    pub is_admin: bool,
}

/// How a write was authorised — which is also what decides whether it is an
/// override somebody has to be able to find in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefAuthority {
    /// The row is the writer's own.
    Owner,
    /// The system row, written by an administrator or the gateway token.
    System,
    /// A `manage` share on somebody else's row.
    ManageShare,
    /// A system administrator reaching past the row's owner.
    Admin,
}

impl DefAuthority {
    /// Whether the writer decides who reaches the row — everyone but a
    /// grantee, who may edit the content and nothing about its audience.
    pub const fn controls_access(self) -> bool {
        !matches!(self, Self::ManageShare)
    }

    /// Whether this write reaches past an owner, and so is audited.
    pub const fn is_override(self) -> bool {
        matches!(self, Self::Admin)
    }
}

/// What a write does to the row, so an audited override names the action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefOp {
    Save,
    Delete,
    Restore,
    Share,
    Visibility,
    Transfer,
}

impl DefOp {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Save => "save",
            Self::Delete => "delete",
            Self::Restore => "restore",
            Self::Share => "share",
            Self::Visibility => "visibility",
            Self::Transfer => "transfer",
        }
    }
}

/// A permitted write: the row it lands on and how much say the writer has.
///
/// Only [`authorize_def_write`] builds one, and it records an override on the
/// way out, so holding a grant means the record exists. `non_exhaustive` keeps
/// that true outside this crate, where no caller can write the literal.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DefWriteGrant {
    /// Owner namespace of the row (`None` = system).
    pub owner: Option<String>,
    /// Identity recorded as the revision's editor.
    pub actor: Option<String>,
    /// How the write was authorised.
    pub authority: DefAuthority,
}

/// Why a write was refused. The message is what the requester sees.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DefWriteDenied {
    #[error("not signed in as anyone who may change definitions")]
    Unauthenticated,
    #[error(
        "'{id}' is owned by {owner}; only its owner, or someone they granted manage access, may change it"
    )]
    NotOwner { id: String, owner: String },
    #[error("'{owner}' names no member; a shared definition is addressed by naming no owner")]
    ReservedOwner { owner: String },
}

/// `id` as a member's identity: the empty string is no identity, and the
/// hub's own pseudo-identities (`system`, the `__system__` owner sentinel)
/// are never a member — a turn running as the system is treated as bound
/// to nobody, and a row can only be addressed by a real member's id.
pub fn member_identity(id: Option<&str>) -> Option<&str> {
    id.filter(|id| !id.is_empty() && *id != SYSTEM_USER_ID && *id != SYSTEM_OWNER)
}

/// The one thing the rule reads beyond the writer: the share a member holds
/// on another member's definition.
#[async_trait]
pub trait ShareLookup: Send + Sync {
    /// The highest permission `identity_id` holds on `(kind, id)` owned by
    /// `owner`, through a share to them or to one of their teams.
    async fn share_permission(
        &self,
        identity_id: &str,
        kind: &str,
        id: &str,
        owner: &str,
    ) -> Option<GrantLevel>;
}

/// Shares as stored: `def_shares` joined with the caller's teams. Either
/// store may be absent (disk deployments, tests), in which case nobody holds
/// a share through it.
#[derive(Clone)]
pub struct StoredShares {
    def_store: Option<Arc<WorkflowStore>>,
    team_store: Option<Arc<dyn TeamStore>>,
}

impl StoredShares {
    pub fn new(
        def_store: Option<Arc<WorkflowStore>>,
        team_store: Option<Arc<dyn TeamStore>>,
    ) -> Self {
        Self {
            def_store,
            team_store,
        }
    }

    /// The lookup's own reach for one caller: a share query names the teams of
    /// whoever it is asked about, never of whoever built this.
    fn reach(&self, identity_id: &str) -> TeamReach {
        TeamReach::new(self.team_store.clone(), Some(identity_id.to_string()))
    }
}

#[async_trait]
impl ShareLookup for StoredShares {
    async fn share_permission(
        &self,
        identity_id: &str,
        kind: &str,
        id: &str,
        owner: &str,
    ) -> Option<GrantLevel> {
        let store = self.def_store.as_ref()?;
        store
            .caller_share_permission(
                kind,
                id,
                owner,
                Some(identity_id),
                self.reach(identity_id).team_ids(),
            )
            .await
            .ok()
            .flatten()
    }
}

/// The row a write addresses and the standing that permits it, decided and not
/// yet recorded. Private: an override is audited by whatever turns a standing
/// into a [`DefWriteGrant`], so no caller outside this module can hold one
/// without the record.
///
/// `requested_owner` names a member's row explicitly (the gateway's `?owner=`,
/// a tool's `owner`); absent, an administrator writes the system row and a
/// member their own. An administrator reaching a member's row holds it as an
/// administrator whatever else they were granted, so the grant they happen to
/// hold never decides less than their office does.
async fn write_authority(
    writer: &DefWriter,
    shares: &dyn ShareLookup,
    kind: &str,
    id: &str,
    requested_owner: Option<&str>,
) -> Result<(Option<String>, DefAuthority), DefWriteDenied> {
    let me = member_identity(writer.identity_id.as_deref());
    let row = |owner: Option<&str>, authority| Ok((owner.map(str::to_string), authority));
    let requested_owner = requested_owner.filter(|o| !o.is_empty());
    if let Some(reserved) = requested_owner.filter(|o| member_identity(Some(o)).is_none()) {
        return Err(DefWriteDenied::ReservedOwner {
            owner: reserved.to_string(),
        });
    }
    match (requested_owner, me) {
        (None, _) if writer.is_admin => row(None, DefAuthority::System),
        (None, None) => Err(DefWriteDenied::Unauthenticated),
        (None, Some(me)) => row(Some(me), DefAuthority::Owner),
        (Some(owner), Some(me)) if owner == me => row(Some(owner), DefAuthority::Owner),
        (Some(owner), _) if writer.is_admin => row(Some(owner), DefAuthority::Admin),
        (Some(owner), Some(me)) => match shares
            .share_permission(me, kind, id, owner)
            .await
            .is_some_and(|p| p.allows(GrantLevel::Manage))
        {
            true => row(Some(owner), DefAuthority::ManageShare),
            false => Err(DefWriteDenied::NotOwner {
                id: id.to_string(),
                owner: owner.to_string(),
            }),
        },
        (Some(_), None) => Err(DefWriteDenied::Unauthenticated),
    }
}

/// Resolve the row a write addresses, authorize it, and record it when it
/// reaches past an owner. `op` is what the write does, so the log names the
/// action — the audit is emitted here because this is the only way to obtain a
/// [`DefWriteGrant`].
pub async fn authorize_def_write(
    writer: &DefWriter,
    shares: &dyn ShareLookup,
    kind: &str,
    id: &str,
    requested_owner: Option<&str>,
    op: DefOp,
) -> Result<DefWriteGrant, DefWriteDenied> {
    let (owner, authority) = write_authority(writer, shares, kind, id, requested_owner).await?;
    let actor = member_identity(writer.identity_id.as_deref()).map(str::to_string);
    if authority.is_override() {
        crate::hub_core::resource_audit::override_granted(
            kind,
            id,
            owner.as_deref(),
            actor.as_deref(),
            op.as_str(),
        );
    }
    Ok(DefWriteGrant {
        owner,
        actor,
        authority,
    })
}

/// Who may change a definition's grants, visibility or owner: the owner, or a
/// system administrator overriding them. A `manage` grantee edits the
/// definition but never re-shares it — the one rule that keeps a delegated
/// edit from becoming a delegated grant.
pub async fn authorize_share_manage(
    writer: &DefWriter,
    shares: &dyn ShareLookup,
    kind: &str,
    id: &str,
    owner: &str,
    op: DefOp,
) -> Result<(), DefWriteDenied> {
    authorize_def_write(writer, shares, kind, id, Some(owner), op)
        .await
        .and_then(|grant| controls_access(grant.authority, id, owner))
}

/// The one function that hands a definition over, so the audit cannot be
/// forgotten by a surface. Four surfaces transfer; they all call this, and
/// `ResourceOwnerChange` is emitted here and nowhere else.
///
/// The standing it takes is not a new rule: [`authorize_share_manage`] with
/// [`DefOp::Transfer`] already excludes the `manage` grantee — the rule that
/// keeps a delegated edit from becoming a delegated give-away — resolves an
/// administrator live rather than from a token claim, and records the override
/// where the grant is made. Over that it adds only what transfer decides: the
/// target must be an active member ([`crate::hub_core::recipients::active_owner`]), and
/// must not already be the owner.
///
/// Two events can come out of one call, and they are deliberately not one:
/// `ResourceAccessOverride` answers *did anybody reach past an owner?* and is
/// silent for the common case of an owner giving their own definition away;
/// `ResourceOwnerChange` answers *who owns this now?* and fires every time.
///
/// The outgoing owner keeps nothing unless `ask.keep` says otherwise, and the
/// default is nothing: manufacturing a grant nobody asked for would surprise,
/// would hand a share to exactly the departed identity an administrator's
/// rescue exists to clean up, and would leave the previous editor with standing
/// the taint gate has just taken away.
///
/// A surface that offers the choice carries it *into* the transfer rather than
/// sharing beforehand. Until the handover lands they are still the owner, and
/// [`crate::resource_access::validate_grants`] refuses an owner as their own
/// grantee — correctly, so a pre-share is not merely clumsy but impossible. The
/// grant is written inside [`WorkflowStore::transfer`]'s own transaction, so it
/// lands with the move or not at all.
pub async fn transfer_definition(
    writer: &DefWriter,
    shares: &dyn ShareLookup,
    stores: &TransferStores,
    ask: DefTransfer<'_>,
) -> Result<DefTransferred, TransferDenied> {
    let DefTransfer {
        kind,
        id,
        from,
        to,
        keep,
    } = ask;
    let word = def_word(kind);
    let crate::hub_core::recipients::ActiveMember {
        id: to_id,
        display: to_display,
    } = crate::hub_core::recipients::active_owner(
        stores.identities.as_deref(),
        stores.teams.as_deref(),
        word,
        to,
    )
    .map_err(TransferDenied::Target)?;
    if to_id == from {
        return Err(TransferDenied::Target(format!(
            "{to_display} already owns `{id}`, so there is nothing to transfer."
        )));
    }
    authorize_share_manage(writer, shares, kind, id, from, DefOp::Transfer).await?;
    let actor = member_identity(writer.identity_id.as_deref());
    let moved = stores
        .defs
        .transfer(kind, id, from, &to_id, keep, actor.unwrap_or_default())
        .await
        .map_err(|failed| match failed {
            TransferFailed::NotFound => TransferDenied::NotFound {
                word: word.to_string(),
                id: id.to_string(),
            },
            TransferFailed::Occupied => TransferDenied::Occupied {
                word: word.to_string(),
                id: id.to_string(),
                holder: to_display.clone(),
            },
            TransferFailed::Storage(e) => TransferDenied::Storage(e.to_string()),
        })?;
    crate::hub_core::resource_audit::owner_changed(kind, id, Some(from), &to_id, actor);
    Ok(DefTransferred {
        from: from.to_string(),
        to: to_id,
        to_display,
        moved,
    })
}

/// The definition a transfer addresses and who it is going to. `to` is however
/// the requester named them — an identity id, an email, an @-mention — read by
/// the one resolver every surface shares.
#[derive(Debug, Clone, Copy)]
pub struct DefTransfer<'a> {
    pub kind: &'a str,
    pub id: &'a str,
    /// The row's current owner.
    pub from: &'a str,
    pub to: &'a str,
    /// What `from` is left with. Every surface states it; only those that
    /// offer the choice send anything but [`OutgoingAccess::Nothing`].
    pub keep: OutgoingAccess,
}

/// What a transfer reads beyond the rule: the rows it moves, and the stores
/// that say who a name belongs to.
#[derive(Clone)]
pub struct TransferStores {
    pub defs: Arc<WorkflowStore>,
    pub identities: Option<Arc<dyn workforce_identity::store::IdentityStore>>,
    pub teams: Option<Arc<dyn TeamStore>>,
}

/// A definition that changed hands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefTransferred {
    pub from: String,
    /// The identity that owns it now.
    pub to: String,
    /// How they read in a sentence.
    pub to_display: String,
    pub moved: Transferred,
}

/// Why a definition did not change hands. The message is what the requester
/// sees; the variant is what a surface maps to its own vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransferDenied {
    /// The requester may not transfer this row.
    #[error(transparent)]
    Refused(#[from] DefWriteDenied),
    /// Nobody may be given it: not a person, not active, or already the owner.
    #[error("{0}")]
    Target(String),
    #[error("No {word} called `{id}` that you can transfer.")]
    NotFound { word: String, id: String },
    #[error("{holder} already owns a {word} called `{id}`; rename or delete one of them first.")]
    Occupied {
        word: String,
        id: String,
        holder: String,
    },
    #[error("Could not transfer it: {0}")]
    Storage(String),
}

/// What a transfer changed that nobody can otherwise see, worded once so
/// every surface says the same thing. `by_owner` is whether the person who
/// asked was the row's owner, rather than an administrator reaching past them.
pub fn transfer_notes(id: &str, done: &DefTransferred, by_owner: bool) -> Vec<String> {
    let to = &done.to_display;
    [
        match (by_owner, done.moved.kept_manage) {
            (true, false) => format!(
                "You no longer have access to `{id}` — ask {to} to share it back if you \
                 still need it."
            ),
            (true, true) => format!(
                "You keep `manage` access to `{id}`, so you can still edit it — but not \
                 re-share it, and not take it back."
            ),
            (false, false) => format!("Its previous owner no longer has access to `{id}`."),
            (false, true) => {
                format!("Its previous owner keeps `manage` access to `{id}` and can still edit it.")
            }
        },
        format!("Automated runs of `{id}` are paused until {to} has saved it."),
    ]
    .into_iter()
    .chain((!done.moved.trigger_paths.is_empty()).then(|| {
        format!(
            "Its stable trigger address is unchanged ({}), so callers using it are \
                 unaffected. Anything still posting to the older owner-derived address \
                 (`/triggers/{}/…`) now gets a 404 and must be repointed to the stable one.",
            done.moved
                .trigger_paths
                .iter()
                .map(|path| format!("`/triggers/{path}`"))
                .collect::<Vec<_>>()
                .join(", "),
            done.from,
        )
    }))
    .collect()
}

/// The kind as it reads inside a sentence.
fn def_word(kind: &str) -> &str {
    match kind {
        workforce_orchestrator::workflow_store::KIND_CONSOLIDATION_TEMPLATE => {
            "consolidation template"
        }
        other => other,
    }
}

/// The same standing, for *reading* who a definition reaches: who it is shared
/// with is the owner's business, so it takes the authority to change it — and
/// records nothing, because a read is not an override.
pub async fn authorize_share_read(
    writer: &DefWriter,
    shares: &dyn ShareLookup,
    kind: &str,
    id: &str,
    owner: &str,
) -> Result<(), DefWriteDenied> {
    write_authority(writer, shares, kind, id, Some(owner))
        .await
        .and_then(|(_, authority)| controls_access(authority, id, owner))
}

fn controls_access(authority: DefAuthority, id: &str, owner: &str) -> Result<(), DefWriteDenied> {
    authority
        .controls_access()
        .then_some(())
        .ok_or_else(|| DefWriteDenied::NotOwner {
            id: id.to_string(),
            owner: owner.to_string(),
        })
}

/// Whether `reader` may read or run the row `(owner, visibility)`: an
/// administrator may read anything, anyone a global definition, a teammate of
/// the owner a team-visible one, an owner their own, and a share grantee (run
/// or manage) what was shared with them.
pub async fn may_read_def(
    reader: &DefWriter,
    shares: &dyn ShareLookup,
    kind: &str,
    id: &str,
    owner: Option<&str>,
    visibility: Visibility,
    teams: &dyn TeamMembership,
) -> bool {
    let me = member_identity(reader.identity_id.as_deref());
    if reader.is_admin || def_access(owner, visibility, me, None, teams).allows(AccessLevel::Run) {
        return true;
    }
    let (Some(owner), Some(me)) = (owner, me) else {
        return false;
    };
    shares
        .share_permission(me, kind, id, owner)
        .await
        .is_some_and(|p| p.allows(GrantLevel::Run))
}

/// Validate a requested visibility, defaulting by owner: system definitions
/// default to `global`, members' to `private`. A row nobody owns has no team
/// whose members could be resolved, so `team` is refused there.
pub fn resolve_visibility(
    requested: Option<&str>,
    owner: Option<&str>,
) -> Result<Visibility, String> {
    match (requested.map(Visibility::parse), owner) {
        (None, None) => Ok(Visibility::Global),
        (None, Some(_)) => Ok(Visibility::Private),
        (Some(Some(Visibility::Team)), None) => {
            Err("a system definition has no team; use 'private' or 'everyone'".to_string())
        }
        (Some(Some(visibility)), _) => Ok(visibility),
        (Some(None), _) => Err(format!(
            "invalid visibility '{}'",
            requested.unwrap_or_default()
        )),
    }
}

/// The visibility a save persists. A writer who decides who reaches the row
/// sets it from the request; a manage-share grantee may not change who a
/// definition is exposed to, so their write keeps the `stored` one.
///
/// A save that names no visibility is an edit, not a re-scope: an existing row
/// keeps the reach its owner last chose — which `cmd: … visibility` may have set
/// without a save ([`crate::def_cmd`]), and which the agent tools never send —
/// and only a row that does not exist yet takes the default for its owner.
pub fn write_visibility(
    grant: &DefWriteGrant,
    requested: Option<&str>,
    stored: Option<Visibility>,
) -> Result<Visibility, String> {
    let owner = grant.owner.as_deref();
    let chosen = match requested {
        Some(_) => resolve_visibility(requested, owner)?,
        None => stored.map_or_else(|| resolve_visibility(None, owner), Ok)?,
    };
    Ok(match grant.authority.controls_access() {
        true => chosen,
        false => stored.unwrap_or_default(),
    })
}

/// `run_as` values a save may carry: `owner` hands automation back to the
/// owner; `me` names the saver, the only identity that can consent to running
/// content unattended.
pub const RUN_AS_OWNER: &str = "owner";
pub const RUN_AS_ME: &str = "me";

/// The run identity a save persists. Absent means unchanged. Only a
/// member-owned definition has one; a system definition runs as its trigger
/// says. `me` from an owner is the owner, so the column stays clear and the
/// owner's own edits never taint their workflow.
pub fn run_as_write(grant: &DefWriteGrant, requested: Option<&str>) -> Result<RunAsWrite, String> {
    let Some(requested) = requested.filter(|r| !r.is_empty()) else {
        return Ok(RunAsWrite::Unchanged);
    };
    let Some(owner) = grant.owner.as_deref() else {
        return Err(
            "run_as applies to member-owned definitions; a system definition runs as its \
             trigger's run_as"
                .to_string(),
        );
    };
    match requested {
        RUN_AS_OWNER => Ok(RunAsWrite::Owner),
        RUN_AS_ME => match grant.actor.as_deref() {
            Some(me) if me == owner => Ok(RunAsWrite::Owner),
            Some(me) => Ok(RunAsWrite::Identity(me.to_string())),
            None => Err("run_as 'me' needs a signed-in identity to run as".to_string()),
        },
        other => Err(format!("invalid run_as '{other}': use 'owner' or 'me'")),
    }
}

/// Late-bound team store, filled by the boot path after the agent services
/// exist (the same lifecycle as the identity store behind `IsAdminFn`).
pub type TeamStoreHandle = Arc<parking_lot::RwLock<Option<Arc<dyn TeamStore>>>>;

/// What an agent tool carries to apply the rule to the turn it runs in: the
/// admin gate every privileged tool shares, and the team store behind share
/// lookups. Both resolve at call time; the definition store is the tool's
/// runtime's. Defaults to deny-all with no stores, so a tool built bare (CLI,
/// tests) treats every turn as a member's.
#[derive(Clone)]
pub struct TurnDefWriteAuth {
    is_admin: IsAdminFn,
    team_store: TeamStoreHandle,
}

impl Default for TurnDefWriteAuth {
    fn default() -> Self {
        Self {
            is_admin: never_admin(),
            team_store: Arc::new(parking_lot::RwLock::new(None)),
        }
    }
}

impl TurnDefWriteAuth {
    pub fn with_admin_policy(mut self, is_admin: IsAdminFn) -> Self {
        self.is_admin = is_admin;
        self
    }

    pub fn with_team_store(mut self, team_store: TeamStoreHandle) -> Self {
        self.team_store = team_store;
        self
    }

    /// Whether the turn's user is an administrator.
    pub fn is_admin(&self) -> bool {
        (self.is_admin)()
    }

    /// The team store behind share lookups, for the listing that unions a
    /// caller's shared rows in ([`crate::resource_access`]).
    pub fn team_store(&self) -> Option<Arc<dyn TeamStore>> {
        self.team_store.read().clone()
    }

    /// The turn's user's team standing, resolved at most once per use.
    pub fn teams(&self) -> TeamReach {
        TeamReach::new(self.team_store(), crate::user_config::current_identity_id())
    }

    /// The turn's user as a writer: the task-local identity, admin-ness from
    /// the shared gate.
    pub fn writer(&self) -> DefWriter {
        DefWriter {
            identity_id: crate::user_config::current_identity_id(),
            is_admin: self.is_admin(),
        }
    }

    /// [`authorize_def_write`] for the turn's user, with shares read from
    /// `def_store` and the late-bound team store.
    pub async fn authorize(
        &self,
        def_store: Option<Arc<WorkflowStore>>,
        kind: &str,
        id: &str,
        requested_owner: Option<&str>,
        op: DefOp,
    ) -> Result<DefWriteGrant, DefWriteDenied> {
        let shares = self.shares(def_store);
        authorize_def_write(&self.writer(), &shares, kind, id, requested_owner, op).await
    }

    /// [`may_read_def`] for the turn's user.
    pub async fn may_read(
        &self,
        def_store: Option<Arc<WorkflowStore>>,
        kind: &str,
        id: &str,
        owner: Option<&str>,
        visibility: Visibility,
    ) -> bool {
        let shares = self.shares(def_store);
        may_read_def(
            &self.writer(),
            &shares,
            kind,
            id,
            owner,
            visibility,
            &self.teams(),
        )
        .await
    }

    fn shares(&self, def_store: Option<Arc<WorkflowStore>>) -> StoredShares {
        StoredShares::new(def_store, self.team_store.read().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Shares keyed by `(grantee, kind, id, owner)`.
    struct Shares(HashMap<(String, String, String, String), GrantLevel>);

    impl Shares {
        fn none() -> Self {
            Self(HashMap::new())
        }

        fn granting(grantee: &str, id: &str, owner: &str, permission: GrantLevel) -> Self {
            let mut map = HashMap::new();
            map.insert(
                (grantee.into(), "workflow".into(), id.into(), owner.into()),
                permission,
            );
            Self(map)
        }
    }

    #[async_trait]
    impl ShareLookup for Shares {
        async fn share_permission(
            &self,
            identity_id: &str,
            kind: &str,
            id: &str,
            owner: &str,
        ) -> Option<GrantLevel> {
            self.0
                .get(&(identity_id.into(), kind.into(), id.into(), owner.into()))
                .copied()
        }
    }

    fn admin(identity: Option<&str>) -> DefWriter {
        DefWriter {
            identity_id: identity.map(str::to_string),
            is_admin: true,
        }
    }

    fn member(identity: &str) -> DefWriter {
        DefWriter {
            identity_id: Some(identity.to_string()),
            is_admin: false,
        }
    }

    fn grant(owner: Option<&str>, actor: Option<&str>, authority: DefAuthority) -> DefWriteGrant {
        DefWriteGrant {
            owner: owner.map(str::to_string),
            actor: actor.map(str::to_string),
            authority,
        }
    }

    async fn authorize(
        writer: &DefWriter,
        shares: &Shares,
        owner: Option<&str>,
    ) -> Result<DefWriteGrant, DefWriteDenied> {
        authorize_def_write(writer, shares, "workflow", "triage", owner, DefOp::Save).await
    }

    #[tokio::test]
    async fn admin_writes_the_system_row_when_naming_none() {
        assert_eq!(
            authorize(&admin(Some("root")), &Shares::none(), None).await,
            Ok(grant(None, Some("root"), DefAuthority::System))
        );
        // The static gateway token has no identity to attribute to.
        assert_eq!(
            authorize(&admin(None), &Shares::none(), None).await,
            Ok(grant(None, None, DefAuthority::System))
        );
    }

    #[tokio::test]
    async fn member_writes_their_own_row_when_naming_none() {
        // The same call an admin makes lands on the member's row: a member
        // re-saving a shared id gets their own copy, never the system row.
        assert_eq!(
            authorize(&member("alice"), &Shares::none(), None).await,
            Ok(grant(Some("alice"), Some("alice"), DefAuthority::Owner))
        );
    }

    #[tokio::test]
    async fn owner_writes_their_own_named_row() {
        assert_eq!(
            authorize(&member("alice"), &Shares::none(), Some("alice")).await,
            Ok(grant(Some("alice"), Some("alice"), DefAuthority::Owner))
        );
    }

    #[tokio::test]
    async fn manage_share_grantee_writes_the_owners_row_without_controlling_it() {
        let shares = Shares::granting("bob", "triage", "alice", GrantLevel::Manage);
        assert_eq!(
            authorize(&member("bob"), &shares, Some("alice")).await,
            Ok(grant(Some("alice"), Some("bob"), DefAuthority::ManageShare))
        );
    }

    #[tokio::test]
    async fn run_share_or_no_share_does_not_permit_a_write() {
        let denied = Err(DefWriteDenied::NotOwner {
            id: "triage".into(),
            owner: "alice".into(),
        });
        let run_only = Shares::granting("bob", "triage", "alice", GrantLevel::Run);
        assert_eq!(
            authorize(&member("bob"), &run_only, Some("alice")).await,
            denied
        );
        assert_eq!(
            authorize(&member("bob"), &Shares::none(), Some("alice")).await,
            denied
        );
        // A share on a different definition or for someone else is no share.
        let other_def = Shares::granting("bob", "other", "alice", GrantLevel::Manage);
        assert_eq!(
            authorize(&member("bob"), &other_def, Some("alice")).await,
            denied
        );
    }

    /// An administrator writes a member's row in place, so an install can be
    /// administered when its owner is away — and holds it as an administrator,
    /// so a `manage` grant they happen to have never decides less than their
    /// office does, and the write is recorded either way.
    #[tokio::test]
    async fn admin_overrides_a_members_row() {
        assert_eq!(
            authorize(&admin(Some("root")), &Shares::none(), Some("alice")).await,
            Ok(grant(Some("alice"), Some("root"), DefAuthority::Admin))
        );
        // The static gateway token is the operator, with nobody to attribute to.
        assert_eq!(
            authorize(&admin(None), &Shares::none(), Some("alice")).await,
            Ok(grant(Some("alice"), None, DefAuthority::Admin))
        );
        // Their own row is their own, not an override.
        assert_eq!(
            authorize(&admin(Some("alice")), &Shares::none(), Some("alice")).await,
            Ok(grant(Some("alice"), Some("alice"), DefAuthority::Owner))
        );
        let shares = Shares::granting("root", "triage", "alice", GrantLevel::Manage);
        assert_eq!(
            authorize(&admin(Some("root")), &shares, Some("alice")).await,
            Ok(grant(Some("alice"), Some("root"), DefAuthority::Admin))
        );
        // A member is refused, whatever the administrator may do.
        assert_eq!(
            authorize(&member("bob"), &Shares::none(), Some("alice")).await,
            Err(DefWriteDenied::NotOwner {
                id: "triage".into(),
                owner: "alice".into(),
            })
        );
    }

    /// What each standing decides, which is the whole of the manage-grantee
    /// rule: a grantee edits the content and nothing about the audience.
    #[test]
    fn only_a_grantee_is_barred_from_deciding_who_reaches_a_row() {
        for authority in [
            DefAuthority::Owner,
            DefAuthority::System,
            DefAuthority::Admin,
        ] {
            assert!(authority.controls_access(), "{authority:?}");
        }
        assert!(!DefAuthority::ManageShare.controls_access());
        assert!(DefAuthority::Admin.is_override());
        for authority in [
            DefAuthority::Owner,
            DefAuthority::System,
            DefAuthority::ManageShare,
        ] {
            assert!(!authority.is_override(), "{authority:?}");
        }
        assert_eq!(
            [
                DefOp::Save,
                DefOp::Delete,
                DefOp::Restore,
                DefOp::Share,
                DefOp::Visibility,
                DefOp::Transfer,
            ]
            .map(DefOp::as_str),
            [
                "save",
                "delete",
                "restore",
                "share",
                "visibility",
                "transfer"
            ]
        );
    }

    #[tokio::test]
    async fn unbound_writer_is_refused() {
        let nobody = DefWriter::default();
        assert_eq!(
            authorize(&nobody, &Shares::none(), None).await,
            Err(DefWriteDenied::Unauthenticated)
        );
        assert_eq!(
            authorize(&nobody, &Shares::none(), Some("alice")).await,
            Err(DefWriteDenied::Unauthenticated)
        );
    }

    #[tokio::test]
    async fn reserved_and_empty_owners_never_address_a_row() {
        // The empty owner is no owner: the writer's own row (or the system row).
        assert_eq!(
            authorize(&member("alice"), &Shares::none(), Some("")).await,
            Ok(grant(Some("alice"), Some("alice"), DefAuthority::Owner))
        );
        assert_eq!(
            authorize(&admin(Some("root")), &Shares::none(), Some("")).await,
            Ok(grant(None, Some("root"), DefAuthority::System))
        );
        // The system sentinel and pseudo-identity are not members' rows.
        for reserved in [SYSTEM_OWNER, SYSTEM_USER_ID] {
            let denied = Err(DefWriteDenied::ReservedOwner {
                owner: reserved.into(),
            });
            assert_eq!(
                authorize(&admin(Some("root")), &Shares::none(), Some(reserved)).await,
                denied
            );
            assert_eq!(
                authorize(&member("alice"), &Shares::none(), Some(reserved)).await,
                denied
            );
        }
    }

    #[tokio::test]
    async fn the_system_pseudo_identity_is_bound_to_nobody() {
        let system = member(SYSTEM_USER_ID);
        assert_eq!(
            authorize(&system, &Shares::none(), None).await,
            Err(DefWriteDenied::Unauthenticated)
        );
        // Admin-ness still reaches the system row, unattributed.
        let system_admin = DefWriter {
            identity_id: Some(SYSTEM_USER_ID.into()),
            is_admin: true,
        };
        assert_eq!(
            authorize(&system_admin, &Shares::none(), None).await,
            Ok(grant(None, None, DefAuthority::System))
        );
    }

    #[tokio::test]
    async fn the_owner_or_an_admin_manages_shares() {
        let manage = |writer: DefWriter, shares: Shares| async move {
            authorize_share_manage(
                &writer,
                &shares,
                "workflow",
                "triage",
                "alice",
                DefOp::Share,
            )
            .await
        };
        let read = |writer: DefWriter, shares: Shares| async move {
            authorize_share_read(&writer, &shares, "workflow", "triage", "alice").await
        };
        assert_eq!(manage(member("alice"), Shares::none()).await, Ok(()));
        // An administrator shares, re-scopes and transfers a member's row.
        assert_eq!(manage(admin(Some("root")), Shares::none()).await, Ok(()));
        assert_eq!(manage(admin(None), Shares::none()).await, Ok(()));

        let not_owner = Err(DefWriteDenied::NotOwner {
            id: "triage".into(),
            owner: "alice".into(),
        });
        // A manage grantee edits but never re-shares — the rule that keeps a
        // delegated edit from becoming a delegated grant.
        let granted = || Shares::granting("bob", "triage", "alice", GrantLevel::Manage);
        assert_eq!(manage(member("bob"), granted()).await, not_owner);
        assert_eq!(manage(member("carol"), Shares::none()).await, not_owner);

        // Reading who a definition reaches takes the same standing.
        assert_eq!(read(member("alice"), Shares::none()).await, Ok(()));
        assert_eq!(read(admin(Some("root")), Shares::none()).await, Ok(()));
        assert_eq!(read(member("bob"), granted()).await, not_owner);
    }

    /// The audit is emitted where the grant is made, so an override cannot be
    /// obtained without it — and nothing else emits, so the log tells an
    /// administrator's reach from an owner's own work.
    #[tokio::test]
    async fn an_override_is_recorded_and_ordinary_work_is_not() {
        use workforce_memory::access_audit::{
            self, AccessAuditEvent, AuditDecision, AuditOperation,
        };

        let buffer = access_audit::install_ring_buffer(256);
        // The hub lib test binary registers no other global sink, so routing
        // the global sink at the buffer makes the emit observable.
        let _ = access_audit::set_global_sink(buffer.clone());
        let id = format!("triage-{}", uuid::Uuid::new_v4());
        let events = |resource: &str| -> Vec<AccessAuditEvent> {
            let resource = format!("workflow:{resource}");
            buffer
                .snapshot()
                .into_iter()
                .filter(|event| event.resource_id.as_deref() == Some(resource.as_str()))
                .collect()
        };
        let write = |writer: DefWriter, shares: Shares, id: String, op| async move {
            authorize_def_write(&writer, &shares, "workflow", &id, Some("alice"), op).await
        };

        // An owner's own write, and a grantee's, record nothing.
        assert!(
            write(member("alice"), Shares::none(), id.clone(), DefOp::Save)
                .await
                .is_ok()
        );
        let granted = Shares::granting("bob", &id, "alice", GrantLevel::Manage);
        assert!(write(member("bob"), granted, id.clone(), DefOp::Save)
            .await
            .is_ok());
        assert!(events(&id).is_empty(), "only an override is recorded");

        // A refusal records nothing either: there was no override to record.
        assert!(
            write(member("carol"), Shares::none(), id.clone(), DefOp::Save)
                .await
                .is_err()
        );
        assert!(events(&id).is_empty());

        assert!(write(
            admin(Some("root")),
            Shares::none(),
            id.clone(),
            DefOp::Delete
        )
        .await
        .is_ok());
        let recorded = events(&id);
        assert_eq!(recorded.len(), 1, "exactly one event per override");
        let event = &recorded[0];
        assert_eq!(event.operation, AuditOperation::ResourceAccessOverride);
        assert_eq!(event.decision, AuditDecision::Allow);
        assert_eq!(event.requester_identity_id.as_deref(), Some("root"));
        assert_eq!(event.owner_identity_id.as_deref(), Some("alice"));

        // Reading who it reaches is not an override; changing that is.
        let quiet = format!("triage-{}", uuid::Uuid::new_v4());
        assert_eq!(
            authorize_share_read(
                &admin(Some("root")),
                &Shares::none(),
                "workflow",
                &quiet,
                "alice"
            )
            .await,
            Ok(())
        );
        assert!(events(&quiet).is_empty());
        assert_eq!(
            authorize_share_manage(
                &admin(Some("root")),
                &Shares::none(),
                "workflow",
                &quiet,
                "alice",
                DefOp::Visibility,
            )
            .await,
            Ok(())
        );
        assert_eq!(events(&quiet).len(), 1);
    }

    /// Who may give a definition away, which is the share rule and not a new
    /// one: the owner, an administrator over them, and nobody else — least of
    /// all the `manage` grantee, for whom a delegated edit must never become a
    /// delegated give-away.
    #[tokio::test]
    async fn only_an_owner_or_an_administrator_transfers() {
        let transfer = |writer: DefWriter, shares: Shares, owner: &'static str| async move {
            authorize_share_manage(
                &writer,
                &shares,
                "workflow",
                "triage",
                owner,
                DefOp::Transfer,
            )
            .await
        };
        let not_owner = Err(DefWriteDenied::NotOwner {
            id: "triage".into(),
            owner: "alice".into(),
        });

        assert_eq!(
            transfer(member("alice"), Shares::none(), "alice").await,
            Ok(())
        );
        assert_eq!(
            transfer(admin(Some("root")), Shares::none(), "alice").await,
            Ok(())
        );
        assert_eq!(transfer(admin(None), Shares::none(), "alice").await, Ok(()));
        let granted = Shares::granting("bob", "triage", "alice", GrantLevel::Manage);
        assert_eq!(transfer(member("bob"), granted, "alice").await, not_owner);
        assert_eq!(
            transfer(member("carol"), Shares::none(), "alice").await,
            not_owner
        );
        assert_eq!(
            transfer(DefWriter::default(), Shares::none(), "alice").await,
            Err(DefWriteDenied::Unauthenticated)
        );
        // Neither party may be the install itself: a definition becomes
        // system-owned by an administrator saving it with no owner, and a
        // system row is refused a sharing surface before it reaches here.
        for reserved in [SYSTEM_OWNER, SYSTEM_USER_ID] {
            assert_eq!(
                transfer(admin(Some("root")), Shares::none(), reserved).await,
                Err(DefWriteDenied::ReservedOwner {
                    owner: reserved.into()
                })
            );
        }
        // An empty owner is no owner, so it lands on the caller's own row and
        // never falls through to the system one.
        assert_eq!(
            authorize_def_write(
                &member("alice"),
                &Shares::none(),
                "workflow",
                "triage",
                Some(""),
                DefOp::Transfer,
            )
            .await
            .map(|grant| grant.owner),
            Ok(Some("alice".to_string()))
        );
    }

    /// The two events a transfer can emit are not one event. The override
    /// answers "did anybody reach past an owner?" and is silent for the
    /// common case; the owner change answers "who owns this now?" and fires
    /// every time, or the ownership history has holes exactly where nothing
    /// went wrong.
    #[tokio::test]
    async fn a_transfer_records_the_change_always_and_the_override_only_over_somebody_else() {
        use workforce_memory::access_audit::{self, AuditOperation};

        let buffer = access_audit::install_ring_buffer(256);
        let _ = access_audit::set_global_sink(buffer.clone());
        let counts = |resource: &str| -> (usize, usize) {
            let resource = format!("workflow:{resource}");
            buffer
                .snapshot()
                .into_iter()
                .filter(|event| event.resource_id.as_deref() == Some(resource.as_str()))
                .fold((0, 0), |(owner, override_), event| match event.operation {
                    AuditOperation::ResourceOwnerChange => (owner + 1, override_),
                    AuditOperation::ResourceAccessOverride => (owner, override_ + 1),
                    _ => (owner, override_),
                })
        };
        // The whole of what a surface does, minus the row move the store
        // performs between them: take the standing, then record the change.
        let transfer = |writer: DefWriter, shares: Shares, id: String, owner: String| async move {
            let allowed =
                authorize_share_manage(&writer, &shares, "workflow", &id, &owner, DefOp::Transfer)
                    .await;
            if allowed.is_ok() {
                crate::hub_core::resource_audit::owner_changed(
                    "workflow",
                    &id,
                    Some(&owner),
                    "bob",
                    member_identity(writer.identity_id.as_deref()),
                );
            }
            allowed
        };
        let fresh = || format!("triage-{}", uuid::Uuid::new_v4());

        let own = fresh();
        assert!(
            transfer(member("alice"), Shares::none(), own.clone(), "alice".into())
                .await
                .is_ok()
        );
        assert_eq!(
            counts(&own),
            (1, 0),
            "an owner's own gift is not an override"
        );

        let reached = fresh();
        assert!(transfer(
            admin(Some("root")),
            Shares::none(),
            reached.clone(),
            "alice".into()
        )
        .await
        .is_ok());
        assert_eq!(counts(&reached), (1, 1), "one of each, never collapsed");

        let theirs = fresh();
        assert!(transfer(
            admin(Some("root")),
            Shares::none(),
            theirs.clone(),
            "root".into()
        )
        .await
        .is_ok());
        assert_eq!(
            counts(&theirs),
            (1, 0),
            "their own row is not somebody else's"
        );

        let refused = fresh();
        let granted = Shares::granting("bob", &refused, "alice", GrantLevel::Manage);
        assert!(
            transfer(member("bob"), granted, refused.clone(), "alice".into())
                .await
                .is_err()
        );
        assert_eq!(counts(&refused), (0, 0), "a refusal records nothing");
    }

    /// The whole of a definition transfer, over a real store: the standing,
    /// the move and both records, and every way a target is refused. The event
    /// counts are taken through [`transfer_definition`] itself rather than
    /// through a stand-in for it, so a second authorisation inside it — which
    /// would record a second override for one administrator's transfer — fails
    /// here rather than in a log nobody reads.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_definition_is_handed_over_only_to_a_live_active_member() {
        use workforce_identity::store::IdentityStore;
        use workforce_identity::types::{Identity, IdentityRole, IdentityStatus};
        use workforce_memory::access_audit::{self, AuditOperation};
        use workforce_orchestrator::workflow_store::EditContext;

        workforce_postgres::test_support::with_test_pool(
            WorkflowStore::build_pool,
            |pool| async move {
                let defs = WorkflowStore::new(pool).await.expect("store");
                let identities = Arc::new(
                    workforce_identity::sqlite::SqliteIdentityStore::in_memory().expect("sqlite"),
                );
                let member_of = |name: &str, status| {
                    let identity = Identity::new(name, IdentityRole::Member).with_status(status);
                    identities.create(&identity).expect("identity");
                    identity.id.to_string()
                };
                let alice = member_of("alice", IdentityStatus::Active);
                let bob = member_of("bob", IdentityStatus::Active);
                let dormant = member_of("dormant", IdentityStatus::Suspended);
                let id = format!("triage-{}", uuid::Uuid::new_v4());
                let buffer = access_audit::install_ring_buffer(256);
                let _ = access_audit::set_global_sink(buffer.clone());
                let counts = || {
                    let resource = format!("workflow:{id}");
                    buffer
                        .snapshot()
                        .into_iter()
                        .filter(|event| event.resource_id.as_deref() == Some(resource.as_str()))
                        .fold((0, 0), |(owner, reached), event| match event.operation {
                            AuditOperation::ResourceOwnerChange => (owner + 1, reached),
                            AuditOperation::ResourceAccessOverride => (owner, reached + 1),
                            _ => (owner, reached),
                        })
                };
                defs.save(
                    &id,
                    "workflow",
                    "content",
                    None,
                    Some(&alice),
                    Visibility::Private,
                    RunAsWrite::Unchanged,
                    EditContext::new("test", Some(&alice)),
                )
                .await
                .expect("definition");

                let stores = TransferStores {
                    defs: Arc::clone(&defs),
                    identities: Some(identities.clone() as Arc<dyn IdentityStore>),
                    teams: None,
                };
                let shares = StoredShares::new(Some(Arc::clone(&defs)), None);
                let hand_to = |writer: DefWriter,
                               to: String,
                               id: String,
                               from: String,
                               keep: OutgoingAccess| {
                    let (stores, shares) = (stores.clone(), shares.clone());
                    async move {
                        transfer_definition(
                            &writer,
                            &shares,
                            &stores,
                            DefTransfer {
                                kind: "workflow",
                                id: &id,
                                from: &from,
                                to: &to,
                                keep,
                            },
                        )
                        .await
                    }
                };
                let refusal = |writer: DefWriter, to: String| {
                    let (id, alice) = (id.clone(), alice.clone());
                    async move {
                        hand_to(writer, to, id, alice, OutgoingAccess::Nothing)
                            .await
                            .expect_err("must be refused")
                    }
                };

                // Resolved live, and fail-closed: a suspended member, a name
                // nobody holds, a team, and the owner themselves are all
                // refused, and none of them moves the row.
                for target in [
                    dormant.clone(),
                    uuid::Uuid::new_v4().to_string(),
                    "team:ops".into(),
                    alice.clone(),
                ] {
                    assert!(
                        matches!(
                            refusal(member(&alice), target).await,
                            TransferDenied::Target(_)
                        ),
                        "a definition must not be strandable"
                    );
                }
                // A `manage` grantee edits but never gives away.
                assert!(matches!(
                    refusal(member("mallory"), bob.clone()).await,
                    TransferDenied::Refused(DefWriteDenied::NotOwner { .. })
                ));
                assert_eq!(
                    defs.list("workflow").await.expect("list").len(),
                    1,
                    "not one refusal moved anything"
                );
                assert_eq!(counts(), (0, 0), "a refusal records nothing");

                let done = hand_to(
                    member(&alice),
                    bob.clone(),
                    id.clone(),
                    alice.clone(),
                    OutgoingAccess::Nothing,
                )
                .await
                .expect("the owner may give their own definition away");
                assert_eq!(done.to, bob);
                assert_eq!(done.from, alice);
                assert_eq!(
                    defs.get(&id, "workflow", Some(&bob))
                        .await
                        .expect("get")
                        .as_deref(),
                    Some("content")
                );
                assert!(
                    transfer_notes(&id, &done, true)
                        .iter()
                        .any(|note| note.contains("paused until")),
                    "the response must say automation is paused"
                );
                assert_eq!(counts(), (1, 0), "an owner's own gift is not an override");

                // An administrator reaching past the new owner: one of each,
                // never two of either, for the single action they took.
                hand_to(
                    admin(Some("root")),
                    alice.clone(),
                    id.clone(),
                    bob.clone(),
                    OutgoingAccess::Nothing,
                )
                .await
                .expect("an administrator may reach past an owner");
                assert_eq!(counts(), (2, 1), "one of each, never collapsed nor doubled");

                // A hub with no identity store cannot name anybody, so it can
                // transfer to nobody — where `owner_is_active` deliberately
                // answers the opposite question fail-open.
                let storeless = TransferStores {
                    identities: None,
                    ..stores.clone()
                };
                assert!(matches!(
                    transfer_definition(
                        &member(&bob),
                        &shares,
                        &storeless,
                        DefTransfer {
                            kind: "workflow",
                            id: &id,
                            from: &bob,
                            to: &alice,
                            keep: OutgoingAccess::Nothing,
                        },
                    )
                    .await,
                    Err(TransferDenied::Target(_))
                ));
            },
        )
        .await;
    }

    /// What every surface tells the requester, pinned without a pool: the move
    /// needs Postgres, but the sentences are a total function over its result,
    /// and they are the only place a caller learns that their webhook address
    /// has stopped answering.
    #[test]
    fn the_notes_name_the_loss_the_pause_and_only_a_real_trigger() {
        let kept = |trigger_paths: Vec<String>, kept_manage: bool| DefTransferred {
            from: "alice".into(),
            to: "bob".into(),
            to_display: "Bob".into(),
            moved: Transferred {
                trigger_paths,
                shares: 0,
                revisions: 1,
                kept_manage,
            },
        };
        let done = |trigger_paths: Vec<String>| kept(trigger_paths, false);
        let mine = transfer_notes("triage", &done(Vec::new()), true);
        assert_eq!(mine.len(), 2, "no trigger, no address to warn about");
        assert!(mine[0].contains("You no longer have access"), "{mine:?}");
        assert!(
            mine[1].contains("paused until Bob has saved it"),
            "{mine:?}"
        );

        let theirs = transfer_notes("triage", &done(Vec::new()), false);
        assert!(
            theirs[0].contains("Its previous owner no longer has access"),
            "an administrator's rescue is not their own loss: {theirs:?}"
        );
        assert_eq!(theirs[1], mine[1], "the pause reads the same either way");

        let addressed = transfer_notes("triage", &done(vec!["t.abc".into(), "t.def".into()]), true);
        assert_eq!(addressed.len(), 3);
        for path in ["`/triggers/t.abc`", "`/triggers/t.def`"] {
            assert!(addressed[2].contains(path), "{addressed:?}");
        }
        assert!(
            addressed[2].contains("`/triggers/alice/…`"),
            "the address that stops answering is the outgoing owner's: {addressed:?}"
        );
    }

    /// A note may not contradict the choice the requester made. Where the
    /// transfer left the outgoing owner a `manage` grant, telling them to ask
    /// for it back is simply false, and the administrator's wording says who
    /// still reaches the row rather than that nobody does.
    #[test]
    fn a_kept_grant_is_what_the_notes_say_happened() {
        let done = |kept_manage| DefTransferred {
            from: "alice".into(),
            to: "bob".into(),
            to_display: "Bob".into(),
            moved: Transferred {
                trigger_paths: Vec::new(),
                shares: 0,
                revisions: 1,
                kept_manage,
            },
        };
        for by_owner in [true, false] {
            let kept = transfer_notes("triage", &done(true), by_owner);
            assert!(kept[0].contains("`manage` access"), "{kept:?}");
            assert!(
                !kept[0].contains("share it back") && !kept[0].contains("no longer"),
                "a kept grant must not read as a loss: {kept:?}"
            );
            assert_eq!(
                kept[1],
                transfer_notes("triage", &done(false), by_owner)[1],
                "the pause reads the same either way"
            );
        }
        assert!(
            transfer_notes("triage", &done(true), true)[0].starts_with("You keep"),
            "the owner's own choice is addressed to them"
        );
        assert!(
            transfer_notes("triage", &done(true), false)[0].starts_with("Its previous owner"),
            "an administrator is told whose access was kept, not theirs"
        );
    }

    /// Alice's teammate, and nobody else's.
    struct AliceTeam;

    impl TeamMembership for AliceTeam {
        fn in_team(&self, team_id: &str) -> bool {
            team_id == "platform"
        }

        fn shares_team_with(&self, owner: &str) -> bool {
            owner == "alice"
        }
    }

    #[tokio::test]
    async fn reads_are_open_to_admins_globals_teammates_owners_and_grantees() {
        use workforce_core::access::NoTeams;
        let read = |reader: DefWriter, shares: Shares, owner: Option<&str>, visibility| {
            let owner = owner.map(str::to_string);
            async move {
                may_read_def(
                    &reader,
                    &shares,
                    "workflow",
                    "triage",
                    owner.as_deref(),
                    visibility,
                    &NoTeams,
                )
                .await
            }
        };
        assert!(read(member("alice"), Shares::none(), None, Visibility::Global).await);
        assert!(
            read(
                member("alice"),
                Shares::none(),
                Some("alice"),
                Visibility::Private
            )
            .await
        );
        assert!(
            !read(
                member("bob"),
                Shares::none(),
                Some("alice"),
                Visibility::Private
            )
            .await
        );
        assert!(
            read(
                admin(None),
                Shares::none(),
                Some("alice"),
                Visibility::Private
            )
            .await
        );
        let run = Shares::granting("bob", "triage", "alice", GrantLevel::Run);
        assert!(read(member("bob"), run, Some("alice"), Visibility::Private).await);
        assert!(
            !read(
                DefWriter::default(),
                Shares::none(),
                Some("alice"),
                Visibility::Private
            )
            .await
        );
        assert!(
            !read(
                member(SYSTEM_USER_ID),
                Shares::none(),
                Some(SYSTEM_USER_ID),
                Visibility::Private
            )
            .await
        );
        // A row nobody owns keeps the reach the operator gave it, and a
        // team-visible one reaches the owner's teammates without a share.
        assert!(!read(member("bob"), Shares::none(), None, Visibility::Private).await);
        assert!(
            may_read_def(
                &member("bob"),
                &Shares::none(),
                "workflow",
                "triage",
                Some("alice"),
                Visibility::Team,
                &AliceTeam,
            )
            .await
        );
        assert!(
            !may_read_def(
                &member("bob"),
                &Shares::none(),
                "workflow",
                "triage",
                Some("carol"),
                Visibility::Team,
                &AliceTeam,
            )
            .await
        );
    }

    #[test]
    fn denial_names_the_owner() {
        let denied = DefWriteDenied::NotOwner {
            id: "triage".into(),
            owner: "alice".into(),
        };
        assert_eq!(
            denied.to_string(),
            "'triage' is owned by alice; only its owner, or someone they granted manage access, may change it"
        );
    }

    #[test]
    fn visibility_defaults_by_owner_and_validates() {
        assert_eq!(resolve_visibility(None, None), Ok(Visibility::Global));
        assert_eq!(
            resolve_visibility(None, Some("alice")),
            Ok(Visibility::Private)
        );
        assert_eq!(
            resolve_visibility(Some("global"), Some("alice")),
            Ok(Visibility::Global)
        );
        assert_eq!(
            resolve_visibility(Some("team"), Some("alice")),
            Ok(Visibility::Team)
        );
        assert_eq!(
            resolve_visibility(Some("private"), None),
            Ok(Visibility::Private)
        );
        assert_eq!(
            resolve_visibility(Some("team"), None),
            Err("a system definition has no team; use 'private' or 'everyone'".to_string())
        );
        assert_eq!(
            resolve_visibility(Some("public"), Some("alice")),
            Err("invalid visibility 'public'".to_string())
        );
        // Only the stored spelling is a request; the words people type are read
        // by the surfaces that take them.
        assert!(resolve_visibility(Some("everyone"), Some("alice")).is_err());
    }

    #[test]
    fn write_visibility_is_the_writers_choice_only_when_they_control_access() {
        let owner = grant(Some("alice"), Some("alice"), DefAuthority::Owner);
        // An edit that names no visibility is not a re-scope: the stored reach
        // survives it, so a `cmd: … visibility` (or a UI) choice is not undone
        // by the next agent save, which never sends one.
        assert_eq!(
            write_visibility(&owner, None, Some(Visibility::Global)),
            Ok(Visibility::Global)
        );
        assert_eq!(
            write_visibility(&owner, None, Some(Visibility::Team)),
            Ok(Visibility::Team)
        );
        assert_eq!(
            write_visibility(&owner, None, Some(Visibility::Private)),
            Ok(Visibility::Private)
        );
        // A row that does not exist yet takes the default for its owner.
        assert_eq!(
            write_visibility(&owner, None, None),
            Ok(Visibility::Private)
        );
        assert_eq!(
            write_visibility(&grant(None, Some("alice"), DefAuthority::Owner), None, None),
            Ok(Visibility::Global)
        );
        // An explicit request still wins, in every direction.
        assert_eq!(
            write_visibility(&owner, Some("global"), Some(Visibility::Private)),
            Ok(Visibility::Global)
        );
        assert_eq!(
            write_visibility(&owner, Some("team"), Some(Visibility::Global)),
            Ok(Visibility::Team)
        );
        assert_eq!(
            write_visibility(&owner, Some("private"), Some(Visibility::Global)),
            Ok(Visibility::Private)
        );
        assert!(write_visibility(&owner, Some("public"), None).is_err());
        assert!(write_visibility(
            &grant(None, Some("root"), DefAuthority::System),
            Some("team"),
            None
        )
        .is_err());

        let grantee = grant(Some("alice"), Some("bob"), DefAuthority::ManageShare);
        assert_eq!(
            write_visibility(&grantee, Some("private"), Some(Visibility::Global)),
            Ok(Visibility::Global)
        );
        assert_eq!(
            write_visibility(&grantee, Some("team"), Some(Visibility::Team)),
            Ok(Visibility::Team)
        );
        assert_eq!(
            write_visibility(&grantee, Some("global"), None),
            Ok(Visibility::Private)
        );
        assert!(write_visibility(&grantee, Some("public"), Some(Visibility::Global)).is_err());
    }

    /// The stored lookup joins `def_shares` with the caller's teams: a manage
    /// share granted to a team reaches its members, and a run share does not
    /// permit a write.
    #[tokio::test(flavor = "multi_thread")]
    async fn stored_shares_reach_a_member_through_their_team() {
        use workforce_core::access::Grant;
        use workforce_identity::store::IdentityAndTeamStore;
        use workforce_identity::team::Team;
        workforce_postgres::test_support::with_test_pool(
            WorkflowStore::build_pool,
            |pool| async move {
                let store = WorkflowStore::new(pool).await.expect("store");
                let identities = Arc::new(
                    workforce_identity::sqlite::SqliteIdentityStore::in_memory().expect("sqlite"),
                );
                let alice = uuid::Uuid::new_v4().to_string();
                let bob = uuid::Uuid::new_v4();
                let carol = uuid::Uuid::new_v4();
                let team = Team::new("ops", bob);
                identities.create_team(&team).expect("team");
                store
                    .save(
                        "triage",
                        "workflow",
                        "",
                        None,
                        Some(&alice),
                        Visibility::Private,
                        RunAsWrite::Unchanged,
                        workforce_orchestrator::workflow_store::EditContext::new(
                            "test",
                            Some(&alice),
                        ),
                    )
                    .await
                    .expect("definition");
                store
                    .set_shares(
                        "workflow",
                        "triage",
                        &alice,
                        &[
                            Grant::team(team.id.to_string(), GrantLevel::Manage),
                            Grant::user(carol.to_string(), GrantLevel::Run),
                        ],
                        &alice,
                    )
                    .await
                    .expect("shares");

                let shares = StoredShares::new(Some(store), Some(identities.as_team_store()));
                let write = |who: uuid::Uuid| {
                    let shares = shares.clone();
                    let alice = alice.clone();
                    async move {
                        authorize_def_write(
                            &member(&who.to_string()),
                            &shares,
                            "workflow",
                            "triage",
                            Some(&alice),
                            DefOp::Save,
                        )
                        .await
                    }
                };
                assert_eq!(
                    write(bob).await,
                    Ok(grant(
                        Some(&alice),
                        Some(&bob.to_string()),
                        DefAuthority::ManageShare
                    ))
                );
                assert_eq!(
                    write(carol).await,
                    Err(DefWriteDenied::NotOwner {
                        id: "triage".into(),
                        owner: alice.clone(),
                    })
                );
            },
        )
        .await;
    }

    #[test]
    fn run_as_write_names_only_the_saver_or_the_owner() {
        let owner = grant(Some("alice"), Some("alice"), DefAuthority::Owner);
        let grantee = grant(Some("alice"), Some("bob"), DefAuthority::ManageShare);
        let token = grant(Some("alice"), None, DefAuthority::Owner);
        let system = grant(None, Some("root"), DefAuthority::System);

        assert_eq!(run_as_write(&grantee, None), Ok(RunAsWrite::Unchanged));
        assert_eq!(run_as_write(&grantee, Some("")), Ok(RunAsWrite::Unchanged));
        assert_eq!(run_as_write(&grantee, Some("owner")), Ok(RunAsWrite::Owner));
        assert_eq!(
            run_as_write(&grantee, Some("me")),
            Ok(RunAsWrite::Identity("bob".into()))
        );
        assert_eq!(run_as_write(&owner, Some("me")), Ok(RunAsWrite::Owner));
        assert!(run_as_write(&token, Some("me")).is_err());
        assert!(run_as_write(&system, Some("owner")).is_err());
        assert!(run_as_write(&grantee, Some("carol")).is_err());

        // An administrator's override stamps them as the row's last editor, so
        // its automated runs stay paused until the identity they run as
        // re-approves them. `me` names the administrator, which is the save
        // that resumes them without waiting for an owner who is away — and
        // `owner` still hands automation back when they return.
        let overriding = grant(Some("alice"), Some("root"), DefAuthority::Admin);
        assert_eq!(
            run_as_write(&overriding, Some("me")),
            Ok(RunAsWrite::Identity("root".into()))
        );
        assert_eq!(
            run_as_write(&overriding, Some("owner")),
            Ok(RunAsWrite::Owner)
        );
    }

    #[tokio::test]
    async fn turn_auth_reads_the_task_local_identity_and_the_admin_gate() {
        use workforce_agent::runner::CURRENT_USER_ID;
        let auth = TurnDefWriteAuth::default();
        assert_eq!(auth.writer(), DefWriter::default());

        let as_alice = CURRENT_USER_ID.scope("alice".to_string(), async { auth.clone().writer() });
        assert_eq!(as_alice.await, member("alice"));

        let root = auth.with_admin_policy(crate::authz::always_admin());
        assert_eq!(
            root.authorize(None, "workflow", "triage", None, DefOp::Save)
                .await,
            Ok(grant(None, None, DefAuthority::System))
        );
    }
}
