//! The consent core behind `cmd: skill` and the remote `skill.*` tools: one
//! person approving one credential, tool set, or audience for one skill's
//! *consent contract*.
//!
//! A skill's vault locker (`wasm-skills/{skill}/…`) is writable by no agent
//! tool: the agent authors both the skill's code and its `[network]`
//! allowed_hosts, so letting it also place secrets in the skill's reach would
//! complete a prompt-injection exfiltration chain. The two surfaces that
//! reach this module are therefore both driven by a person, never by an
//! agent's model: `cmd:` commands run on user-typed chat text only
//! (`commands.rs`), and the remote MCP tools exist only in the remote surface
//! the user's own client calls under their token (`remote_mcp/skill_tools.rs`)
//! — no agent registry ever contains them.
//!
//! Grants pin the contract — capabilities, network hosts, credentials,
//! permissions ([`workforce_wasm_runtime::types::contract_fingerprint`]).
//! Editing any part of it requires re-approval before the skill can read the
//! value again; pure-logic rebuilds keep grants valid. A `shared`-scoped
//! credential is one value for every caller, so the skill **owner** provides
//! it. A `tools shared` grant consents on other users' behalf and stays
//! admin-only.
//!
//! Sharing and visibility also reach external (SKILL.md) skills, with the
//! caller's standing over those; a name a WASM skill holds stays the WASM
//! skill's.

use std::fmt;
use std::sync::Arc;

use chrono::Utc;
use workforce_common::settings::{SecretStore, VaultActor};
use workforce_core::access::{
    AccessLevel, AccessWrite, Grant, GrantLevel, OutgoingAccess, OwnerWrite, Visibility,
};
use workforce_identity::store::IdentityStore;
use workforce_identity::team::TeamStore;
use workforce_skills::catalog::SkillKind;
use workforce_wasm_runtime::{CredentialScope, InstalledWasmSkill, OwnedSkill, WasmSkillStore};

use crate::authz::Capability;
use crate::external_skills::ExternalSkills;
pub use crate::hub_core::grants::GrantStatus;
use crate::recipients::Recipient;
use crate::serve::AgentService;
use crate::skill_access::{kind_word, SkillCaller, SkillRegistry};
use crate::team_reach::TeamReach;

/// Which person-driven surface is acting — recorded on every audit line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Chat,
    RemoteMcp,
    Web,
}

impl Surface {
    fn as_str(self) -> &'static str {
        match self {
            Surface::Chat => "chat",
            Surface::RemoteMcp => "remote_mcp",
            Surface::Web => "web",
        }
    }
}

/// Why a consent operation did not happen. Every variant but `NoValue`
/// renders the same on both surfaces; `NoValue` needs the surface's own
/// instructions for where the value comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    NoStore,
    VaultUnavailable,
    NoValue {
        skill: String,
        credential: String,
    },
    /// The action consents on other users' behalf, which only an active
    /// SystemAdmin may do; the caller can still act for themselves alone.
    AdminOnly(String),
    /// The person named cannot be given the skill: not a member of this
    /// install, not active, or already its owner. Told apart from
    /// [`Refusal::Denied`] because it is the request that is wrong, not the
    /// requester's standing.
    NoSuchTarget(String),
    Denied(String),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::NoStore => f.write_str("No WASM skill runtime is active on this hub."),
            Refusal::VaultUnavailable => f.write_str(
                "The vault is not available on this hub, so skill credentials cannot be stored.",
            ),
            Refusal::NoValue { skill, credential } => {
                write!(
                    f,
                    "No value to grant for `{credential}` of skill `{skill}`."
                )
            }
            Refusal::AdminOnly(why) | Refusal::NoSuchTarget(why) | Refusal::Denied(why) => {
                f.write_str(why)
            }
        }
    }
}

impl From<String> for Refusal {
    fn from(why: String) -> Self {
        Refusal::Denied(why)
    }
}

/// What a grant or revocation is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Credential(String),
    Tools { shared: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueAction {
    Stored,
    Kept,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialGrant {
    pub skill: String,
    pub credential: String,
    pub scope: CredentialScope,
    pub value: ValueAction,
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantedPermission {
    pub tool: String,
    pub description: String,
    pub allowed_hosts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolsGrant {
    pub skill: String,
    pub shared: bool,
    pub permissions: Vec<GrantedPermission>,
    /// A self-heal must not hide that everyone else is still broken: set on
    /// a per-user grant when the org-wide shared record exists but is stale.
    pub shared_stale: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revocation {
    pub skill: String,
    pub target: Target,
    pub removed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolsStatus {
    pub declared: usize,
    pub own: GrantStatus,
    pub shared: GrantStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialStatus {
    pub name: String,
    pub scope: CredentialScope,
    pub status: GrantStatus,
    pub value_stored: bool,
}

#[derive(Debug, Clone)]
pub struct SkillGrants {
    pub skill: InstalledWasmSkill,
    pub tools: Option<ToolsStatus>,
    pub credentials: Vec<CredentialStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareChange {
    Added(GrantLevel),
    Removed,
    NotShared,
    SelfShare,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareOutcome {
    pub skill: String,
    pub recipient: Recipient,
    pub change: ShareChange,
    pub kind: SkillKind,
}

/// A skill that changed hands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferOutcome {
    pub skill: String,
    pub recipient: crate::recipients::ActiveMember,
    /// Who held it before; `None` for a skill nobody owned, which a transfer
    /// is the natural way to give an owner.
    pub previous_owner: Option<String>,
    pub kind: SkillKind,
    /// `shared`-scope credentials still holding the outgoing owner's own
    /// secret. The transfer does not touch them and must not stay silent about
    /// them: they go on serving a skill their owner no longer controls.
    pub shared_credentials: Vec<String>,
    /// Whether the outgoing owner was left a `manage` grant. What the record
    /// now holds rather than what was asked for — a skill nobody owned has no
    /// outgoing owner to leave anything to — so a note cannot contradict it.
    pub kept_manage: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibilityChange {
    pub skill: String,
    pub visibility: Visibility,
    pub shares: usize,
    pub kind: SkillKind,
    /// `team` was chosen by an owner who stands in no team, so it reaches
    /// nobody but them until they join one. Never true of the other two.
    pub owner_has_no_team: bool,
}

/// One grantee of a skill, as the person who controls it reads them: their
/// name where it resolves, their bare id where the account has since gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grantee {
    pub display: String,
    pub permission: GrantLevel,
}

/// Whether a registry of one kind holds a skill name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Holding {
    NoRegistry,
    Missing,
    Held,
}

impl Holding {
    fn of<R: SkillRegistry + ?Sized>(registry: Option<&R>, name: &str) -> Self {
        match registry.map(|registry| registry.installed(name).is_some()) {
            None => Holding::NoRegistry,
            Some(false) => Holding::Missing,
            Some(true) => Holding::Held,
        }
    }
}

/// The kind a sharing command acts on: a name a WASM skill holds is the WASM
/// skill's, whatever else holds it, so every command that reached a WASM skill
/// still does; otherwise the external skill's.
fn sharing_kind(wasm: Holding, external: Holding) -> SkillKind {
    match (wasm, external) {
        (Holding::Held, _)
        | (Holding::Missing, Holding::Missing)
        | (Holding::Missing, Holding::NoRegistry) => SkillKind::Wasm,
        _ => SkillKind::External,
    }
}

/// One identified person's standing to approve, on one surface.
pub struct Consent {
    store: Option<Arc<WasmSkillStore>>,
    external: Option<(Arc<ExternalSkills>, SkillCaller)>,
    vault: Option<Arc<dyn SecretStore>>,
    caller: SkillCaller,
    uid: String,
    identities: Option<Arc<dyn IdentityStore>>,
    teams: Option<Arc<dyn TeamStore>>,
    surface: Surface,
}

impl Consent {
    pub fn new(
        store: Arc<WasmSkillStore>,
        vault: Option<Arc<dyn SecretStore>>,
        uid: impl Into<String>,
        is_admin: bool,
        identities: Option<Arc<dyn IdentityStore>>,
        teams: Option<Arc<dyn TeamStore>>,
        surface: Surface,
    ) -> Self {
        Self::from_parts(
            Some(store),
            vault,
            uid.into(),
            is_admin,
            identities,
            teams,
            surface,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        store: Option<Arc<WasmSkillStore>>,
        vault: Option<Arc<dyn SecretStore>>,
        uid: String,
        is_admin: bool,
        identities: Option<Arc<dyn IdentityStore>>,
        teams: Option<Arc<dyn TeamStore>>,
        surface: Surface,
    ) -> Self {
        Self {
            store,
            external: None,
            vault,
            caller: SkillCaller::new(
                Some(uid.clone()),
                is_admin,
                TeamReach::new(teams.clone(), Some(uid.clone())),
            ),
            uid,
            identities,
            teams,
            surface,
        }
    }

    /// Let the sharing commands reach external skills too, with the caller's
    /// standing over them: `manages_skills` is holding skills_manage.
    pub fn with_external(self, skills: Arc<ExternalSkills>, manages_skills: bool) -> Self {
        let caller = SkillCaller::new(
            Some(self.uid.clone()),
            manages_skills,
            TeamReach::new(self.teams.clone(), Some(self.uid.clone())),
        );
        Self {
            external: Some((skills, caller)),
            ..self
        }
    }

    pub fn uid(&self) -> &str {
        &self.uid
    }

    /// The hub-wide skill stores and the exact locker store every registered
    /// `WasmSkillTool` reads (global vault primary, agent vault as legacy read
    /// fallback), acting as `uid` through `service`. A hub without a WASM
    /// runtime refuses the WASM operations when they are asked for.
    pub fn for_service(service: &Arc<AgentService>, uid: &str, surface: Surface) -> Self {
        let identities = service.identity_store.read().clone();
        let is_admin = crate::authz::is_admin(service, uid);
        // `skills_manage` is delegable through the capability matrix, so a
        // system administrator's standing over external skills cannot be an
        // edit of it: they hold the bypass either way.
        let manages_skills = is_admin
            || crate::authz::holds_capability(identities.as_ref(), uid, Capability::SkillsManage);
        Self::from_parts(
            crate::wasm_skill_tools::global_skill_store(),
            crate::wasm_skill_tools::SkillLockerVault::compose(
                service.global_vault.clone(),
                service.vault.clone(),
            ),
            uid.to_string(),
            is_admin,
            identities,
            service.team_store.read().clone(),
            surface,
        )
        .with_external(crate::external_skills::of(service), manages_skills)
    }

    fn wasm(&self) -> Result<&WasmSkillStore, Refusal> {
        self.store.as_deref().ok_or(Refusal::NoStore)
    }

    fn external(&self) -> Result<(&ExternalSkills, &SkillCaller), Refusal> {
        self.external
            .as_ref()
            .map(|(skills, caller)| (skills.as_ref(), caller))
            .ok_or(Refusal::NoStore)
    }

    fn sharing_kind(&self, skill_name: &str) -> SkillKind {
        sharing_kind(
            Holding::of(self.store.as_deref(), skill_name),
            Holding::of(
                self.external.as_ref().map(|(skills, _)| skills.as_ref()),
                skill_name,
            ),
        )
    }

    fn vault(&self) -> Result<&dyn SecretStore, Refusal> {
        self.vault.as_deref().ok_or(Refusal::VaultUnavailable)
    }

    /// The org-wide tools grant consents on every user's behalf, so making
    /// or withdrawing it takes an active SystemAdmin.
    fn require_admin(&self, skill_name: &str, attempt: &str, why: &str) -> Result<(), Refusal> {
        if self.caller.is_admin {
            return Ok(());
        }
        tracing::warn!(
            target: "sentinel.audit",
            skill = %skill_name, actor = %self.uid, surface = %self.surface.as_str(),
            "non-admin attempted to {attempt} the shared tools grant"
        );
        Err(Refusal::AdminOnly(why.to_string()))
    }

    /// A secret the caller placed in their own per-user vault subtree — the
    /// remote surface's value source, where nothing typed reaches the hub.
    pub fn user_secret(&self, path: &str) -> Result<Option<String>, Refusal> {
        self.wasm()?;
        let path = path.trim().trim_matches('/');
        if path.is_empty() || path.split('/').any(|segment| segment == "..") {
            return Ok(None);
        }
        Ok(self.vault()?.get(&user_path(&self.uid, path)))
    }

    /// Approve `credential` for `skill` and provision it: `provided` is
    /// stored, else an already-stored value is kept, else there is nothing
    /// to grant.
    pub fn grant_credential(
        &self,
        skill_name: &str,
        credential: &str,
        provided: Option<String>,
    ) -> Result<CredentialGrant, Refusal> {
        let (installed, scope) = self.declared_credential(skill_name, credential)?;
        if scope == CredentialScope::Shared {
            // A shared credential is the author's own secret, reachable only by
            // the code they wrote, over the hosts they declared, through the
            // tools they declared — all pinned by the consent contract.
            // Providing it is the owner configuring their own skill, so it
            // needs their standing, not an admin's.
            if let Err(refusal) = self
                .caller
                .require(self.wasm()?, skill_name, AccessLevel::Manage)
            {
                tracing::warn!(
                    target: "sentinel.audit",
                    skill = %skill_name, credential = %credential, actor = %self.uid,
                    surface = %self.surface.as_str(),
                    "shared skill-credential grant refused: caller does not own the skill"
                );
                return Err(Refusal::Denied(format!(
                    "`{credential}` is a `shared` credential — every caller of the skill uses \
                     the one value — so only the skill's owner can provide it. {refusal}"
                )));
            }
        }
        let record = grant_record(&installed, &self.uid)?;
        let vault = self.vault()?;
        let (value_path, grant_path) = locker_paths(skill_name, credential, scope, &self.uid);
        let value = match provided.filter(|v| !v.is_empty()) {
            Some(value) => {
                vault
                    .set_with_actor(&value_path, &value, VaultActor::User(&self.uid))
                    .map_err(|e| Refusal::Denied(format!("Could not store the credential: {e}")))?;
                ValueAction::Stored
            }
            None if vault.get(&value_path).is_some() => ValueAction::Kept,
            None => {
                return Err(Refusal::NoValue {
                    skill: skill_name.to_string(),
                    credential: credential.to_string(),
                })
            }
        };
        vault
            .set_with_actor(&grant_path, &record, VaultActor::User(&self.uid))
            .map_err(|e| Refusal::Denied(format!("Could not record the grant: {e}")))?;
        tracing::info!(
            target: "sentinel.audit",
            skill = %skill_name, credential = %credential, actor = %self.uid,
            scope = ?scope, contract = %installed.contract_fingerprint(),
            surface = %self.surface.as_str(),
            "wasm skill credential granted"
        );
        Ok(CredentialGrant {
            skill: skill_name.to_string(),
            credential: credential.to_string(),
            scope,
            value,
            hosts: installed.manifest.allowed_hosts.clone(),
        })
    }

    /// Approve the skill's declared `[[permissions]]` for the caller alone,
    /// or (admin only) for every user at once.
    pub fn grant_tools(&self, skill_name: &str, shared: bool) -> Result<ToolsGrant, Refusal> {
        let installed = self
            .caller
            .require(self.wasm()?, skill_name, AccessLevel::Run)?;
        if installed.manifest.permissions.is_empty() {
            return Err(Refusal::Denied(format!(
                "Skill `{skill_name}` declares no [[permissions]] — there is no tool access \
                 to grant."
            )));
        }
        if shared {
            self.require_admin(
                skill_name,
                "make",
                "A shared tools grant covers every user of the skill, so only an active \
                 SystemAdmin can make it.",
            )?;
        }
        let record = grant_record(&installed, &self.uid)?;
        let vault = self.vault()?;
        let grant_path = match shared {
            true => shared_tools_grant_path(skill_name),
            false => tools_grant_path(skill_name, &self.uid),
        };
        vault
            .set_with_actor(&grant_path, &record, VaultActor::User(&self.uid))
            .map_err(|e| Refusal::Denied(format!("Could not record the grant: {e}")))?;
        tracing::info!(
            target: "sentinel.audit",
            skill = %skill_name, actor = %self.uid,
            contract = %installed.contract_fingerprint(), shared,
            surface = %self.surface.as_str(),
            "wasm skill tools grant recorded"
        );
        let shared_stale = !shared
            && grant_status(
                vault,
                &shared_tools_grant_path(skill_name),
                installed.grant_fingerprint().ok().as_deref(),
                &installed.contract_fingerprint(),
            ) == GrantStatus::Stale;
        Ok(ToolsGrant {
            skill: skill_name.to_string(),
            shared,
            permissions: installed
                .manifest
                .permissions
                .iter()
                .map(|p| GrantedPermission {
                    tool: p.tool.clone(),
                    description: p.description.clone(),
                    allowed_hosts: p.allowed_hosts.clone(),
                })
                .collect(),
            shared_stale,
        })
    }

    /// Remove a grant, and for a credential its stored value too.
    pub fn revoke(&self, skill_name: &str, target: Target) -> Result<Revocation, Refusal> {
        let store = self.wasm()?;
        let removed = match &target {
            Target::Tools { shared } => {
                if *shared {
                    self.require_admin(
                        skill_name,
                        "revoke",
                        "Only an active SystemAdmin can revoke the shared tools grant.",
                    )?;
                }
                let grant_path = match shared {
                    true => shared_tools_grant_path(skill_name),
                    false => tools_grant_path(skill_name, &self.uid),
                };
                let removed = self.vault()?.delete(&grant_path).unwrap_or(false);
                if removed {
                    tracing::info!(
                        target: "sentinel.audit",
                        skill = %skill_name, actor = %self.uid, shared,
                        surface = %self.surface.as_str(),
                        "wasm skill tools grant revoked"
                    );
                }
                removed
            }
            Target::Credential(credential) => {
                let (_, scope) = self.declared_credential(skill_name, credential)?;
                if scope == CredentialScope::Shared {
                    // Mirrors the grant side: the shared value is the owner's, so
                    // the owner takes it back.
                    self.caller
                        .require(store, skill_name, AccessLevel::Manage)
                        .map_err(|refusal| {
                            Refusal::Denied(format!(
                                "`{credential}` is a `shared` credential, so only the skill's \
                                 owner can revoke it. {refusal}"
                            ))
                        })?;
                }
                let vault = self.vault()?;
                let (value_path, grant_path) =
                    locker_paths(skill_name, credential, scope, &self.uid);
                let removed_value = vault.delete(&value_path).unwrap_or(false);
                let removed_grant = vault.delete(&grant_path).unwrap_or(false);
                let removed = removed_value || removed_grant;
                if removed {
                    tracing::info!(
                        target: "sentinel.audit",
                        skill = %skill_name, credential = %credential, actor = %self.uid,
                        scope = ?scope, surface = %self.surface.as_str(),
                        "wasm skill credential revoked"
                    );
                }
                removed
            }
        };
        Ok(Revocation {
            skill: skill_name.to_string(),
            target,
            removed,
        })
    }

    /// The caller's grants on every skill they may use, with validity.
    /// Only the caller's own standing: a listing over someone else's grants
    /// would name their skills and the credentials those declare.
    pub fn grants(&self, skill_filter: Option<&str>) -> Result<Vec<SkillGrants>, Refusal> {
        let store = self.wasm()?;
        let vault = self.vault()?;
        Ok(self
            .caller
            .visible(store)
            .into_iter()
            .map(|(skill, _)| skill)
            .filter(|s| skill_filter.is_none_or(|wanted| s.manifest.name == wanted))
            .map(|skill| self.skill_grants(vault, skill))
            .collect())
    }

    fn skill_grants(&self, vault: &dyn SecretStore, skill: InstalledWasmSkill) -> SkillGrants {
        let name = skill.manifest.name.as_str();
        let status = grant_status_of(vault, &skill);
        let tools = (!skill.manifest.permissions.is_empty()).then(|| ToolsStatus {
            declared: skill.manifest.permissions.len(),
            own: status(&tools_grant_path(name, &self.uid)),
            shared: status(&shared_tools_grant_path(name)),
        });
        let credentials = skill
            .manifest
            .credentials
            .iter()
            .map(|cred| {
                let (value_path, grant_path) =
                    locker_paths(name, &cred.name, cred.scope, &self.uid);
                CredentialStatus {
                    name: cred.name.clone(),
                    scope: cred.scope,
                    status: status(&grant_path),
                    value_stored: vault.get(&value_path).is_some(),
                }
            })
            .collect();
        SkillGrants {
            skill,
            tools,
            credentials,
        }
    }

    /// Let `recipient` use (`Some(Run)`), use and edit (`Some(Manage)`), or
    /// no longer reach (`None`) a skill the caller owns, of either kind.
    pub fn share(
        &self,
        skill_name: &str,
        recipient: &str,
        permission: Option<GrantLevel>,
    ) -> Result<ShareOutcome, Refusal> {
        match self.sharing_kind(skill_name) {
            SkillKind::Wasm => self.share_in(
                self.wasm()?,
                &self.caller,
                skill_name,
                recipient,
                permission,
            ),
            SkillKind::External => {
                let (skills, caller) = self.external()?;
                self.share_in(skills, caller, skill_name, recipient, permission)
            }
        }
    }

    fn share_in<R: SkillRegistry + ?Sized>(
        &self,
        registry: &R,
        caller: &SkillCaller,
        skill_name: &str,
        recipient: &str,
        permission: Option<GrantLevel>,
    ) -> Result<ShareOutcome, Refusal> {
        let skill = caller.require_owner(registry, skill_name)?;
        registry
            .fixed_reach(&skill, skill.visibility())
            .map_or(Ok(()), Err)?;
        let recipient = self.resolve_recipient(recipient)?;
        let outcome = |change| ShareOutcome {
            skill: skill_name.to_string(),
            recipient: recipient.clone(),
            change,
            kind: R::KIND,
        };
        if matches!(&recipient, Recipient::User { id, .. } if *id == self.uid) {
            return Ok(outcome(ShareChange::SelfShare));
        }
        let shares = apply_share(skill.shares(), &recipient, permission);
        if permission.is_none() && shares.len() == skill.shares().len() {
            return Ok(outcome(ShareChange::NotShared));
        }
        registry
            .set_access(skill_name, AccessWrite::kept(skill.visibility(), shares))
            .map_err(|e| Refusal::Denied(format!("Could not update sharing: {e}")))?;
        tracing::info!(
            target: "sentinel.audit",
            skill = %skill_name, actor = %self.uid, recipient = %recipient.id(),
            principal = recipient.principal().as_str(),
            added = permission.is_some(), permission = ?permission,
            surface = %self.surface.as_str(),
            "{} skill sharing changed", kind_word(R::KIND).to_ascii_lowercase()
        );
        Ok(outcome(
            permission.map_or(ShareChange::Removed, ShareChange::Added),
        ))
    }

    /// Hand a skill the caller owns, of either kind, to another member.
    ///
    /// The one function that changes a skill's owner, so the audit cannot be
    /// forgotten by a surface: `ResourceOwnerChange` is emitted here and
    /// nowhere else on this side, beside the override `require_owner` records
    /// when somebody reaches past an owner. A skill nobody owns has no owner
    /// to reach past, so an administrator giving a legacy or catalog skill an
    /// owner records only the change — which is the point of the bypass.
    ///
    /// Nothing else about the skill moves, and nothing needs to: grant records
    /// and credential values are keyed by skill name, and a grant pins the
    /// consent contract rather than the owner, so a transfer invalidates no
    /// grant and nobody re-consents. The one thing it cannot fix is a
    /// `shared`-scope credential, which is the outgoing owner's own secret and
    /// stays readable by code the incoming owner now controls; the outcome
    /// names every one of them rather than letting the transfer be silent.
    ///
    /// `keep` is what the outgoing owner is left with, written into the same
    /// access record as the new owner.
    pub fn transfer(
        &self,
        skill_name: &str,
        recipient: &str,
        keep: OutgoingAccess,
    ) -> Result<TransferOutcome, Refusal> {
        let by = TransferBy {
            actor: &self.uid,
            surface: self.surface,
            identities: self.identities.as_deref(),
            teams: self.teams.as_deref(),
        };
        match self.sharing_kind(skill_name) {
            SkillKind::Wasm => {
                let store = self.wasm()?;
                let outcome = transfer_skill(store, &self.caller, by, skill_name, recipient, keep)?;
                Ok(TransferOutcome {
                    shared_credentials: shared_credentials(store, skill_name),
                    ..outcome
                })
            }
            SkillKind::External => {
                let (skills, caller) = self.external()?;
                transfer_skill(skills, caller, by, skill_name, recipient, keep)
            }
        }
    }

    /// Who else reaches a skill the caller controls, of either kind — the set
    /// `share`/`unshare` edit, named rather than numbered.
    pub fn shares(&self, skill_name: &str) -> Result<Vec<Grantee>, Refusal> {
        let shares = match self.sharing_kind(skill_name) {
            SkillKind::Wasm => self
                .caller
                .require_owner_to_read(self.wasm()?, skill_name)?
                .shares()
                .to_vec(),
            SkillKind::External => {
                let (skills, caller) = self.external()?;
                caller
                    .require_owner_to_read(skills, skill_name)?
                    .shares()
                    .to_vec()
            }
        };
        Ok(shares
            .into_iter()
            .map(|grant| Grantee {
                display: match grant.principal_type.is_user() {
                    true => crate::recipients::display_name(
                        self.identities.as_deref(),
                        &grant.principal_id,
                    ),
                    false => format!(
                        "the {} team",
                        crate::recipients::team_name(self.teams.as_deref(), &grant.principal_id)
                            .unwrap_or_else(|| grant.principal_id.clone())
                    ),
                },
                permission: grant.permission,
            })
            .collect())
    }

    /// Publish a skill the caller owns, of either kind, to everyone, or make it
    /// theirs alone.
    pub fn set_visibility(
        &self,
        skill_name: &str,
        visibility: Visibility,
    ) -> Result<VisibilityChange, Refusal> {
        match self.sharing_kind(skill_name) {
            SkillKind::Wasm => {
                self.set_visibility_in(self.wasm()?, &self.caller, skill_name, visibility)
            }
            SkillKind::External => {
                let (skills, caller) = self.external()?;
                self.set_visibility_in(skills, caller, skill_name, visibility)
            }
        }
    }

    fn set_visibility_in<R: SkillRegistry + ?Sized>(
        &self,
        registry: &R,
        caller: &SkillCaller,
        skill_name: &str,
        visibility: Visibility,
    ) -> Result<VisibilityChange, Refusal> {
        let skill = caller.require_owner(registry, skill_name)?;
        registry
            .fixed_reach(&skill, visibility)
            .map_or(Ok(()), Err)?;
        registry
            .set_access(
                skill_name,
                AccessWrite::kept(visibility, skill.shares().to_vec()),
            )
            .map_err(|e| Refusal::Denied(format!("Could not update visibility: {e}")))?;
        tracing::info!(
            target: "sentinel.audit",
            skill = %skill_name, actor = %self.uid, visibility = ?visibility,
            surface = %self.surface.as_str(),
            "{} skill visibility changed", kind_word(R::KIND).to_ascii_lowercase()
        );
        Ok(VisibilityChange {
            skill: skill_name.to_string(),
            visibility,
            shares: skill.shares().len(),
            kind: R::KIND,
            owner_has_no_team: visibility == Visibility::Team
                && skill.owner() == Some(self.uid.as_str())
                && caller.teams().team_ids().is_empty(),
        })
    }

    /// A skill the caller may run and one of its declared credentials, or
    /// the message explaining what exists instead. Standing comes first so a
    /// typo never lists skills, or credential names, the caller cannot see.
    fn declared_credential(
        &self,
        skill_name: &str,
        credential: &str,
    ) -> Result<(InstalledWasmSkill, CredentialScope), Refusal> {
        let installed = self
            .caller
            .require(self.wasm()?, skill_name, AccessLevel::Run)?;
        let declared = &installed.manifest.credentials;
        match declared
            .iter()
            .find(|c| c.name == credential)
            .map(|c| c.scope)
        {
            Some(scope) => Ok((installed, scope)),
            None => {
                let names: Vec<&str> = declared.iter().map(|c| c.name.as_str()).collect();
                Err(Refusal::Denied(format!(
                    "Skill `{skill_name}` declares no credential `{credential}`.{}",
                    match names.is_empty() {
                        true =>
                            " It declares no credentials at all — nothing to grant.".to_string(),
                        false => format!(" Declared: {}", names.join(", ")),
                    }
                )))
            }
        }
    }

    /// Resolve the person or team the caller named.
    fn resolve_recipient(&self, token: &str) -> Result<Recipient, Refusal> {
        let store = self
            .identities
            .as_deref()
            .ok_or_else(|| Refusal::Denied(crate::recipients::NO_IDENTITY_STORE.to_string()))?;
        crate::recipients::resolve(store, self.teams.as_deref(), token).map_err(Refusal::Denied)
    }
}

/// Who a skill transfer is by, and what says who it is going to.
#[derive(Clone, Copy)]
pub struct TransferBy<'a> {
    /// The identity acting, recorded on the audit line.
    pub actor: &'a str,
    pub surface: Surface,
    pub identities: Option<&'a dyn IdentityStore>,
    pub teams: Option<&'a dyn TeamStore>,
}

/// The one function that changes a skill's owner, so no surface can forget
/// the audit: `ResourceOwnerChange` is emitted here and nowhere else on the
/// skills side, beside the override [`SkillCaller::require_owner`] records
/// when somebody reaches past an owner. A skill nobody owns has no owner to
/// reach past, so an administrator giving a legacy or catalog skill an owner
/// records only the change — which is what the bypass is for.
///
/// Nothing else about the skill moves, and nothing needs to: grant records and
/// credential values are keyed by skill name, and a grant pins the consent
/// contract rather than the owner, so a transfer invalidates no grant and
/// nobody re-consents. The one thing it cannot fix is a `shared`-scope
/// credential — see [`shared_credentials`].
///
/// It takes the registry rather than resolving one, because the kind is the
/// caller's to decide: chat and the remote surface route a name to whichever
/// registry holds it, while the REST routes are per kind and must stay there.
///
/// `keep` is what the outgoing owner is left with. A record holds one owner and
/// one grantee set, so the choice is part of the same [`SkillRegistry::set_access`]
/// that moves ownership — never a share beforehand, which the owner-is-never-a-
/// grantee rule refuses while they still own it. A skill nobody owned has no
/// outgoing owner, so there is nothing to keep and the outcome says so.
pub(crate) fn transfer_skill<R: SkillRegistry + ?Sized>(
    registry: &R,
    caller: &SkillCaller,
    by: TransferBy<'_>,
    skill_name: &str,
    recipient: &str,
    keep: OutgoingAccess,
) -> Result<TransferOutcome, Refusal> {
    let skill = caller.require_owner(registry, skill_name)?;
    registry
        .fixed_reach(&skill, skill.visibility())
        .map_or(Ok(()), Err)?;
    let recipient = crate::recipients::active_owner(by.identities, by.teams, "skill", recipient)
        .map_err(Refusal::NoSuchTarget)?;
    if skill.owner() == Some(recipient.id.as_str()) {
        return Err(Refusal::NoSuchTarget(format!(
            "{} already owns `{skill_name}`, so there is nothing to transfer.",
            recipient.display
        )));
    }
    let previous_owner = skill.owner().map(str::to_string);
    let kept = keep
        .keeps_manage()
        .then_some(previous_owner.as_deref())
        .flatten();
    let kept_manage = kept.is_some();
    // Ownership subsumes every grant, so one naming the new owner would only
    // show them sharing with themselves. A team grant that happens to contain
    // them stays: it reaches everyone else on that team. The outgoing owner's
    // own `user` entry is dropped for the same reason and rebuilt at `manage`
    // where they keep it, so the record never names them twice.
    let grants = skill
        .shares()
        .iter()
        .filter(|g| {
            !(g.principal_type.is_user()
                && (g.principal_id == recipient.id || Some(&*g.principal_id) == kept))
        })
        .cloned()
        .chain(kept.map(|held| Grant::user(held, GrantLevel::Manage)))
        .collect();
    registry
        .set_access(
            skill_name,
            AccessWrite {
                owner: OwnerWrite::To(recipient.id.clone()),
                visibility: skill.visibility(),
                grants,
            },
        )
        .map_err(|e| Refusal::Denied(format!("Could not transfer it: {e}")))?;
    crate::resource_audit::owner_changed(
        crate::skill_access::audit_kind(R::KIND),
        skill_name,
        previous_owner.as_deref(),
        &recipient.id,
        Some(by.actor),
    );
    tracing::info!(
        target: "sentinel.audit",
        skill = %skill_name, actor = %by.actor, owner = %recipient.id,
        from_owner = ?previous_owner, surface = %by.surface.as_str(),
        "{} skill owner changed", kind_word(R::KIND).to_ascii_lowercase()
    );
    Ok(TransferOutcome {
        skill: skill_name.to_string(),
        recipient,
        previous_owner,
        kind: R::KIND,
        shared_credentials: Vec::new(),
        kept_manage,
    })
}

/// What a skill transfer changed that nobody can otherwise see, worded once
/// for every surface that reports one.
pub fn skill_transfer_notes(outcome: &TransferOutcome) -> Vec<String> {
    let TransferOutcome {
        skill,
        recipient,
        shared_credentials,
        kept_manage,
        ..
    } = outcome;
    [
        match kept_manage {
            false => format!(
                "`{skill}` is {}'s now — you can no longer edit it or change who reaches it.",
                recipient.display
            ),
            true => format!(
                "`{skill}` is {}'s now. You keep `manage` access, so you can still edit it — \
                 but not change who reaches it, and not take it back.",
                recipient.display
            ),
        },
        "Nobody has to re-consent: a grant pins the skill's contract, not its owner.".to_string(),
    ]
    .into_iter()
    .chain((!shared_credentials.is_empty()).then(|| {
        format!(
            "It declares the shared credential(s) {}, whose value only its owner could place. \
             Any value you did place stays in the vault and stays readable by code {display} \
             now controls, and clearing it takes the ownership you have just given up — so \
             rotate the secret at its source, or ask {display} to remove it with `cmd: skill \
             revoke {skill} <credential>`.",
            shared_credentials
                .iter()
                .map(|c| format!("`{c}`"))
                .collect::<Vec<_>>()
                .join(", "),
            display = recipient.display
        )
    }))
    .collect()
}

/// The `shared`-scope credentials a WASM skill declares. Only the owner may
/// place one — it is their own secret, served to every caller — so a transfer
/// leaves it in the vault at `wasm-skills/{skill}/shared/{credential}`,
/// readable by code the incoming owner now controls. Blocking a transfer over
/// it would defeat the departed-owner rescue this whole model exists for;
/// staying silent would leave the outgoing owner with nothing to decide on.
///
/// Read from the manifest rather than the vault, so every surface names the
/// same set whether or not it holds a vault handle. An external skill declares
/// no credentials, so the question is the WASM side's alone.
pub(crate) fn shared_credentials(store: &WasmSkillStore, skill_name: &str) -> Vec<String> {
    store
        .get_installed(skill_name)
        .into_iter()
        .flat_map(|skill| skill.manifest.credentials)
        .filter(|cred| cred.scope == CredentialScope::Shared)
        .map(|cred| cred.name)
        .collect()
}

fn user_path(uid: &str, path: &str) -> String {
    format!("users/{uid}/{path}")
}

/// The credential value's vault path and its grant record's path, per the
/// manifest-declared scope. Mirrors `host.rs::scoped_vault_path` /
/// `grant_record_path` — `grants/` is a sibling of `users/`/`shared/`, so
/// guest keys can never resolve to it.
pub fn locker_paths(
    skill: &str,
    credential: &str,
    scope: CredentialScope,
    uid: &str,
) -> (String, String) {
    match scope {
        CredentialScope::User => (
            format!("wasm-skills/{skill}/users/{uid}/{credential}"),
            format!("wasm-skills/{skill}/grants/users/{uid}/{credential}"),
        ),
        CredentialScope::Shared => (
            format!("wasm-skills/{skill}/shared/{credential}"),
            format!("wasm-skills/{skill}/grants/shared/{credential}"),
        ),
    }
}

pub fn tools_grant_path(skill: &str, uid: &str) -> String {
    format!(
        "wasm-skills/{skill}/grants/users/{uid}/{}",
        workforce_wasm_runtime::host::TOOLS_GRANT_KEY
    )
}

pub fn shared_tools_grant_path(skill: &str) -> String {
    format!(
        "wasm-skills/{skill}/grants/shared/{}",
        workforce_wasm_runtime::host::TOOLS_GRANT_KEY
    )
}

/// The dual-fingerprint consent record: validated by contract, with the
/// build fingerprint kept for audit — see
/// [`workforce_wasm_runtime::types::grant_record_is_current`].
fn grant_record(installed: &InstalledWasmSkill, uid: &str) -> Result<String, Refusal> {
    let fingerprint = installed
        .grant_fingerprint()
        .map_err(|e| Refusal::Denied(format!("Could not fingerprint the build: {e}")))?;
    Ok(serde_json::json!({
        "fingerprint": fingerprint,
        "contract_fingerprint": installed.contract_fingerprint(),
        "granted_by": uid,
        "granted_at": Utc::now().to_rfc3339(),
    })
    .to_string())
}

/// The standing of the grant record at a path against `skill` as installed:
/// its build fingerprint and its consent contract.
pub fn grant_status_of<'a>(
    vault: &'a dyn SecretStore,
    skill: &InstalledWasmSkill,
) -> impl Fn(&str) -> GrantStatus + 'a {
    let fingerprint = skill.grant_fingerprint().ok();
    let contract = skill.contract_fingerprint();
    move |grant_path| grant_status(vault, grant_path, fingerprint.as_deref(), &contract)
}

fn grant_status(
    vault: &dyn SecretStore,
    grant_path: &str,
    build_fingerprint: Option<&str>,
    contract_fingerprint: &str,
) -> GrantStatus {
    match vault.get(grant_path) {
        Some(raw)
            if workforce_wasm_runtime::types::grant_record_is_current(
                &raw,
                build_fingerprint.unwrap_or(""),
                contract_fingerprint,
            ) =>
        {
            GrantStatus::Valid
        }
        Some(_) => GrantStatus::Stale,
        None => GrantStatus::Missing,
    }
}

/// The grantee set after adding (`Some(permission)`) or removing (`None`) one
/// grantee — the selection is replaced wholesale, so this is the whole edit. It
/// keys on the principal the record keys on, so a team and a user of the same
/// id are two grantees.
fn apply_share(
    existing: &[Grant],
    recipient: &Recipient,
    permission: Option<GrantLevel>,
) -> Vec<Grant> {
    existing
        .iter()
        .filter(|held| {
            (held.principal_type, held.principal_id.as_str())
                != (recipient.principal(), recipient.id())
        })
        .cloned()
        .chain(permission.map(|permission| recipient.grant(permission)))
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;
    use workforce_wasm_runtime::{SkillManifest, SkillOwnership};

    pub(crate) struct MapVault(parking_lot::Mutex<HashMap<String, String>>);

    impl Default for MapVault {
        fn default() -> Self {
            Self(parking_lot::Mutex::new(HashMap::new()))
        }
    }

    impl SecretStore for MapVault {
        fn get(&self, path: &str) -> Option<String> {
            self.0.lock().get(path).cloned()
        }
        fn set(&self, path: &str, value: &str) -> eyre::Result<()> {
            self.0.lock().insert(path.to_string(), value.to_string());
            Ok(())
        }
        fn delete(&self, path: &str) -> eyre::Result<bool> {
            Ok(self.0.lock().remove(path).is_some())
        }
        fn list(&self, _prefix: Option<&str>) -> Vec<String> {
            self.0.lock().keys().cloned().collect()
        }
    }

    pub(crate) const ENABLER: &str = r#"
[skill]
name = "enabler"
version = "0.1.0"
description = "test skill"

[capabilities]
http = true
vault = true
tools = true

[network]
allowed_hosts = ["api.example.com"]

[[credentials]]
name = "api_token"
description = "per-user token"
scope = "user"

[[credentials]]
name = "shared_key"
description = "one key for everyone"
scope = "shared"

[[permissions]]
tool = "slack.post_message"
description = "post"

[[tools]]
name = "ping"
description = "ping"
"#;

    pub(crate) fn installed_fixture(name: &str, owner: Option<&str>) -> InstalledWasmSkill {
        let manifest = SkillManifest::from_toml(&format!(
            "[skill]\nname = \"{name}\"\nversion = \"0.1.0\"\n\
             description = \"record test\"\n\n[[tools]]\nname = \"ping\"\n\
             description = \"ping\"\n"
        ))
        .unwrap();
        InstalledWasmSkill {
            manifest,
            state: workforce_wasm_runtime::WasmSkillState::Active,
            installed_at: chrono::Utc::now(),
            source: "test".into(),
            config: Default::default(),
            binary_version: 1,
            sha256: "abcd".into(),
            owner: owner.map(str::to_string),
            visibility: Visibility::Private,
            shares: Vec::new(),
        }
    }

    /// A temp store holding `enabler` owned by `alice`, plus a consent for
    /// `uid` with the given standing over a fresh map vault.
    pub(crate) struct Harness {
        pub store: Arc<WasmSkillStore>,
        pub vault: Arc<MapVault>,
        _dir: tempfile::TempDir,
    }

    impl Harness {
        pub(crate) fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(WasmSkillStore::open(dir.path()).unwrap());
            store
                .install_prebuilt(
                    SkillManifest::from_toml(ENABLER).unwrap(),
                    b"wasm",
                    None,
                    "test".into(),
                    HashMap::new(),
                    SkillOwnership::owned("alice", Visibility::Private),
                )
                .unwrap();
            Self {
                store,
                vault: Arc::new(MapVault::default()),
                _dir: dir,
            }
        }

        pub(crate) fn consent(&self, uid: &str, admin: bool) -> Consent {
            self.consent_with(uid, admin, None)
        }

        pub(crate) fn consent_with(
            &self,
            uid: &str,
            admin: bool,
            identities: Option<Arc<dyn IdentityStore>>,
        ) -> Consent {
            Consent::new(
                Arc::clone(&self.store),
                Some(Arc::clone(&self.vault) as Arc<dyn SecretStore>),
                uid,
                admin,
                identities,
                None,
                Surface::RemoteMcp,
            )
        }

        /// Make `enabler` reachable by every user, as its owner would.
        pub(crate) fn publish(&self) {
            self.consent("alice", false)
                .set_visibility("enabler", Visibility::Global)
                .unwrap();
        }
    }

    #[test]
    fn locker_paths_by_scope() {
        assert_eq!(
            locker_paths("enabler", "pw", CredentialScope::User, "alice"),
            (
                "wasm-skills/enabler/users/alice/pw".to_string(),
                "wasm-skills/enabler/grants/users/alice/pw".to_string()
            )
        );
        assert_eq!(
            locker_paths("enabler", "pw", CredentialScope::Shared, "alice"),
            (
                "wasm-skills/enabler/shared/pw".to_string(),
                "wasm-skills/enabler/grants/shared/pw".to_string()
            )
        );
    }

    #[test]
    fn tools_grant_path_uses_the_reserved_record_key() {
        assert_eq!(
            tools_grant_path("enabler", "alice"),
            "wasm-skills/enabler/grants/users/alice/_tools"
        );
        assert_eq!(
            shared_tools_grant_path("enabler"),
            "wasm-skills/enabler/grants/shared/_tools"
        );
    }

    #[test]
    fn grant_record_carries_both_fingerprints_and_validates_by_contract() {
        let installed = installed_fixture("rec", None);
        let record = grant_record(&installed, "alice").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&record).unwrap();
        assert_eq!(
            parsed["fingerprint"].as_str().unwrap(),
            installed.grant_fingerprint().unwrap()
        );
        assert_eq!(
            parsed["contract_fingerprint"].as_str().unwrap(),
            installed.contract_fingerprint()
        );
        assert_eq!(parsed["granted_by"], "alice");
        assert!(workforce_wasm_runtime::types::grant_record_is_current(
            &record,
            "a-different-build",
            &installed.contract_fingerprint(),
        ));
    }

    #[test]
    fn grant_status_distinguishes_valid_stale_missing() {
        let vault = MapVault::default();
        assert_eq!(
            grant_status(&vault, "p", Some("fp"), "c1"),
            GrantStatus::Missing
        );

        // Legacy single-field records validate by exact build match.
        vault
            .set("p", &serde_json::json!({"fingerprint": "fp"}).to_string())
            .unwrap();
        assert_eq!(
            grant_status(&vault, "p", Some("fp"), "c1"),
            GrantStatus::Valid
        );
        assert_eq!(
            grant_status(&vault, "p", Some("other"), "c1"),
            GrantStatus::Stale
        );

        // Dual-field records validate by contract even across rebuilds.
        vault
            .set(
                "p",
                &serde_json::json!({"fingerprint": "old-build", "contract_fingerprint": "c1"})
                    .to_string(),
            )
            .unwrap();
        assert_eq!(
            grant_status(&vault, "p", Some("new-build"), "c1"),
            GrantStatus::Valid
        );
        assert_eq!(
            grant_status(&vault, "p", Some("new-build"), "c2"),
            GrantStatus::Stale
        );

        // Corrupt records fail closed as stale (a re-grant repairs them).
        vault.set("p", "corrupt").unwrap();
        assert_eq!(
            grant_status(&vault, "p", Some("fp"), "c1"),
            GrantStatus::Stale
        );
    }

    fn person(id: &str) -> Recipient {
        Recipient::User {
            id: id.to_string(),
            display: id.to_string(),
        }
    }

    #[test]
    fn sharing_edits_replace_one_grantee_at_a_time() {
        let team = Recipient::Team {
            id: "bob".into(),
            name: "platform".into(),
        };
        let existing = vec![
            Grant::user("bob", GrantLevel::Run),
            Grant::user("carol", GrantLevel::Manage),
        ];
        let upgraded = apply_share(&existing, &person("bob"), Some(GrantLevel::Manage));
        assert_eq!(upgraded.len(), 2, "an upgrade replaces, never duplicates");
        assert_eq!(
            upgraded
                .iter()
                .find(|g| g.principal_id == "bob")
                .map(|g| g.permission),
            Some(GrantLevel::Manage)
        );

        let removed = apply_share(&existing, &person("bob"), None);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].principal_id, "carol");

        // Removing someone who was never a grantee leaves the set untouched,
        // which is how `unshare` reports "was not shared".
        assert_eq!(
            apply_share(&existing, &person("dave"), None).len(),
            existing.len()
        );

        // A team of the same id as a user is a different grantee, both ways.
        let with_team = apply_share(&existing, &team, Some(GrantLevel::Run));
        assert_eq!(with_team.len(), 3);
        assert_eq!(apply_share(&with_team, &team, None).len(), 2);
    }

    #[test]
    fn a_provided_value_is_stored_in_the_locker_and_the_grant_pinned() {
        let h = Harness::new();
        h.publish();
        let grant = h
            .consent("bob", false)
            .grant_credential("enabler", "api_token", Some("s3cret".into()))
            .unwrap();
        assert_eq!(grant.value, ValueAction::Stored);
        assert_eq!(grant.scope, CredentialScope::User);
        assert_eq!(grant.hosts, vec!["api.example.com".to_string()]);
        assert_eq!(
            h.vault
                .get("wasm-skills/enabler/users/bob/api_token")
                .as_deref(),
            Some("s3cret")
        );
        assert!(h
            .vault
            .get("wasm-skills/enabler/grants/users/bob/api_token")
            .is_some());
    }

    #[test]
    fn a_regrant_without_a_value_keeps_the_stored_one_and_needs_one_otherwise() {
        let h = Harness::new();
        h.publish();
        let consent = h.consent("bob", false);
        assert_eq!(
            consent.grant_credential("enabler", "api_token", None),
            Err(Refusal::NoValue {
                skill: "enabler".into(),
                credential: "api_token".into()
            })
        );
        assert_eq!(
            consent.grant_credential("enabler", "api_token", Some(String::new())),
            Err(Refusal::NoValue {
                skill: "enabler".into(),
                credential: "api_token".into()
            }),
            "an empty value is no value"
        );
        consent
            .grant_credential("enabler", "api_token", Some("s3cret".into()))
            .unwrap();
        let again = consent
            .grant_credential("enabler", "api_token", None)
            .unwrap();
        assert_eq!(again.value, ValueAction::Kept);
    }

    #[test]
    fn undeclared_credentials_and_unknown_skills_are_named() {
        let h = Harness::new();
        let consent = h.consent("alice", false);
        let Err(Refusal::Denied(msg)) =
            consent.grant_credential("enabler", "nope", Some("v".into()))
        else {
            panic!("undeclared credential must be refused");
        };
        assert!(msg.contains("Declared: api_token, shared_key"), "{msg}");
        let Err(Refusal::Denied(msg)) = consent.grant_credential("ghost", "x", Some("v".into()))
        else {
            panic!("unknown skill must be refused");
        };
        assert!(msg.contains("Available to you: enabler"), "{msg}");
    }

    #[test]
    fn a_shared_credential_is_the_owners_to_provide_and_revoke() {
        let h = Harness::new();
        h.publish();
        let Err(Refusal::Denied(msg)) =
            h.consent("bob", false)
                .grant_credential("enabler", "shared_key", Some("v".into()))
        else {
            panic!("a non-owner must not provide a shared credential");
        };
        assert!(msg.contains("only the skill's owner"), "{msg}");

        let owner = h.consent("alice", false);
        let grant = owner
            .grant_credential("enabler", "shared_key", Some("v".into()))
            .unwrap();
        assert_eq!(grant.scope, CredentialScope::Shared);
        assert!(h
            .vault
            .get("wasm-skills/enabler/shared/shared_key")
            .is_some());

        assert!(matches!(
            h.consent("bob", false)
                .revoke("enabler", Target::Credential("shared_key".into())),
            Err(Refusal::Denied(_))
        ));
        let revoked = owner
            .revoke("enabler", Target::Credential("shared_key".into()))
            .unwrap();
        assert!(revoked.removed);
        assert!(h
            .vault
            .get("wasm-skills/enabler/shared/shared_key")
            .is_none());
        assert!(h
            .vault
            .get("wasm-skills/enabler/grants/shared/shared_key")
            .is_none());
    }

    #[test]
    fn an_admin_may_provide_a_shared_credential_without_owning_the_skill() {
        let h = Harness::new();
        h.consent("root", true)
            .grant_credential("enabler", "shared_key", Some("v".into()))
            .unwrap();
    }

    #[test]
    fn tools_grants_are_per_user_unless_an_admin_covers_everyone() {
        let h = Harness::new();
        h.publish();
        let bob = h.consent("bob", false);
        let Err(Refusal::AdminOnly(msg)) = bob.grant_tools("enabler", true) else {
            panic!("a shared tools grant is admin-only");
        };
        assert!(msg.contains("SystemAdmin"), "{msg}");
        assert!(h.vault.get(&shared_tools_grant_path("enabler")).is_none());

        let own = bob.grant_tools("enabler", false).unwrap();
        assert!(!own.shared);
        assert_eq!(own.permissions[0].tool, "slack.post_message");
        assert!(
            !own.shared_stale,
            "no shared record means nothing to warn about"
        );
        assert!(h.vault.get(&tools_grant_path("enabler", "bob")).is_some());

        let everyone = h
            .consent("root", true)
            .grant_tools("enabler", true)
            .unwrap();
        assert!(everyone.shared);
        assert!(h.vault.get(&shared_tools_grant_path("enabler")).is_some());

        assert!(matches!(
            bob.revoke("enabler", Target::Tools { shared: true }),
            Err(Refusal::AdminOnly(_))
        ));
        let revoked = bob
            .revoke("enabler", Target::Tools { shared: false })
            .unwrap();
        assert!(revoked.removed);
        let nothing = bob
            .revoke("enabler", Target::Tools { shared: false })
            .unwrap();
        assert!(!nothing.removed, "a second revoke has nothing to remove");
    }

    #[test]
    fn a_stale_shared_record_is_flagged_on_a_per_user_grant() {
        let h = Harness::new();
        h.publish();
        h.vault
            .set(
                &shared_tools_grant_path("enabler"),
                &serde_json::json!({"fingerprint": "old-build"}).to_string(),
            )
            .unwrap();
        let own = h
            .consent("bob", false)
            .grant_tools("enabler", false)
            .unwrap();
        assert!(own.shared_stale);
    }

    #[test]
    fn grants_report_validity_and_stored_values_for_visible_skills() {
        let h = Harness::new();
        let alice = h.consent("alice", false);
        alice
            .grant_credential("enabler", "api_token", Some("v".into()))
            .unwrap();
        let listed = alice.grants(None).unwrap();
        assert_eq!(listed.len(), 1);
        let grants = &listed[0];
        assert_eq!(grants.skill.manifest.name, "enabler");
        let tools = grants.tools.as_ref().unwrap();
        assert_eq!(tools.declared, 1);
        assert_eq!(tools.own, GrantStatus::Missing);
        assert_eq!(tools.shared, GrantStatus::Missing);
        let token = grants
            .credentials
            .iter()
            .find(|c| c.name == "api_token")
            .unwrap();
        assert_eq!(token.status, GrantStatus::Valid);
        assert!(token.value_stored);
        let shared = grants
            .credentials
            .iter()
            .find(|c| c.name == "shared_key")
            .unwrap();
        assert_eq!(shared.status, GrantStatus::Missing);
        assert!(!shared.value_stored);

        assert!(alice.grants(Some("other")).unwrap().is_empty());
    }

    #[test]
    fn visibility_and_sharing_are_the_owners_call() {
        let h = Harness::new();
        assert!(matches!(
            h.consent("bob", false)
                .set_visibility("enabler", Visibility::Global),
            Err(Refusal::Denied(_))
        ));
        let change = h
            .consent("alice", false)
            .set_visibility("enabler", Visibility::Global)
            .unwrap();
        assert_eq!(change.visibility, Visibility::Global);
        assert_eq!(
            h.store.get_installed("enabler").unwrap().visibility,
            Visibility::Global
        );
        assert!(
            h.consent("bob", false)
                .grants(Some("enabler"))
                .unwrap()
                .len()
                == 1,
            "a published skill's grants are listable by any user"
        );

        let Err(Refusal::Denied(msg)) =
            h.consent("alice", false)
                .share("enabler", "bob@example.com", Some(GrantLevel::Run))
        else {
            panic!("without an identity store nobody can be looked up");
        };
        assert!(msg.contains("identity store"), "{msg}");
    }

    #[test]
    fn user_secrets_come_only_from_the_callers_own_subtree() {
        let h = Harness::new();
        h.vault.set("users/bob/api_token", "mine").unwrap();
        h.vault.set("users/alice/api_token", "hers").unwrap();
        let bob = h.consent("bob", false);
        assert_eq!(
            bob.user_secret("api_token").unwrap().as_deref(),
            Some("mine")
        );
        assert_eq!(
            bob.user_secret("/api_token/").unwrap().as_deref(),
            Some("mine")
        );
        assert_eq!(bob.user_secret("../alice/api_token").unwrap(), None);
        assert_eq!(bob.user_secret("").unwrap(), None);
        assert_eq!(bob.user_secret("missing").unwrap(), None);
    }

    #[test]
    fn a_skill_the_caller_cannot_run_is_neither_grantable_nor_named() {
        let h = Harness::new();
        let bob = h.consent("bob", false);
        for refusal in [
            bob.grant_credential("enabler", "api_token", Some("v".into()))
                .unwrap_err(),
            bob.grant_tools("enabler", false).unwrap_err(),
            bob.revoke("enabler", Target::Credential("api_token".into()))
                .unwrap_err(),
            bob.grant_credential("ghost", "x", Some("v".into()))
                .unwrap_err(),
        ] {
            let Refusal::Denied(msg) = refusal else {
                panic!("expected a denial, got {refusal:?}");
            };
            assert!(
                !msg.contains("Available to you")
                    && !msg.contains("api_token")
                    && !msg.contains("shared_key"),
                "neither other skills nor a private skill's credentials are listed: {msg}"
            );
        }
        assert!(
            h.vault
                .get("wasm-skills/enabler/users/bob/api_token")
                .is_none(),
            "nothing was written"
        );
        assert!(bob.grants(Some("enabler")).unwrap().is_empty());
    }

    #[test]
    fn the_value_can_come_from_the_callers_own_vault_subtree() {
        let h = Harness::new();
        h.publish();
        h.vault
            .set("users/bob/enabler/token", "from-vault")
            .unwrap();
        let bob = h.consent("bob", false);
        let value = bob.user_secret("enabler/token").unwrap();
        let grant = bob.grant_credential("enabler", "api_token", value).unwrap();
        assert_eq!(grant.value, ValueAction::Stored);
        assert_eq!(
            h.vault
                .get("wasm-skills/enabler/users/bob/api_token")
                .as_deref(),
            Some("from-vault")
        );
        assert_eq!(
            h.vault.get("users/bob/enabler/token").as_deref(),
            Some("from-vault"),
            "the caller's own copy is theirs to keep or delete"
        );
    }

    #[test]
    fn sharing_resolves_the_person_and_reports_each_kind_of_change() {
        use workforce_identity::sqlite::SqliteIdentityStore;
        use workforce_identity::types::{Identity, IdentityRole};

        let identities = Arc::new(SqliteIdentityStore::in_memory().unwrap());
        let mut alice = Identity::new("Alice", IdentityRole::Member);
        alice.email = Some("alice@example.com".into());
        let mut bob = Identity::new("Bob", IdentityRole::Member);
        bob.email = Some("bob@example.com".into());
        identities.create(&alice).unwrap();
        identities.create(&bob).unwrap();
        let (alice, bob) = (alice.id.to_string(), bob.id.to_string());

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(WasmSkillStore::open(dir.path()).unwrap());
        store
            .install_prebuilt(
                SkillManifest::from_toml(ENABLER).unwrap(),
                b"wasm",
                None,
                "test".into(),
                HashMap::new(),
                SkillOwnership::owned(&alice, Visibility::Private),
            )
            .unwrap();
        let identities = identities as Arc<dyn IdentityStore>;
        let owner = Consent::new(
            Arc::clone(&store),
            None,
            &alice,
            false,
            Some(Arc::clone(&identities)),
            None,
            Surface::Chat,
        );

        let shared = owner
            .share("enabler", "bob@example.com", Some(GrantLevel::Run))
            .unwrap();
        assert_eq!(shared.change, ShareChange::Added(GrantLevel::Run));
        assert_eq!(shared.recipient.id(), bob);
        assert_eq!(shared.recipient.named(), "Bob");
        assert_eq!(store.get_installed("enabler").unwrap().shares.len(), 1);

        let upgraded = owner
            .share("enabler", &bob, Some(GrantLevel::Manage))
            .unwrap();
        assert_eq!(upgraded.change, ShareChange::Added(GrantLevel::Manage));
        assert_eq!(store.get_installed("enabler").unwrap().shares.len(), 1);

        let me = owner.share("enabler", "alice@example.com", None).unwrap();
        assert_eq!(me.change, ShareChange::SelfShare);

        let removed = owner.share("enabler", &bob, None).unwrap();
        assert_eq!(removed.change, ShareChange::Removed);
        let again = owner.share("enabler", &bob, None).unwrap();
        assert_eq!(again.change, ShareChange::NotShared);
        assert!(store.get_installed("enabler").unwrap().shares.is_empty());

        let grantee = Consent::new(
            Arc::clone(&store),
            None,
            &bob,
            false,
            Some(Arc::clone(&identities)),
            None,
            Surface::Chat,
        );
        assert!(matches!(
            grantee.share("enabler", "alice@example.com", None),
            Err(Refusal::Denied(_))
        ));
        assert!(matches!(
            owner.share("enabler", "nobody@example.com", None),
            Err(Refusal::Denied(msg)) if msg.contains("can't tell who")
        ));
        // A team is a grantee only where there is a team store to name one in.
        assert!(matches!(
            owner.share("enabler", "team:platform", Some(GrantLevel::Run)),
            Err(Refusal::Denied(msg)) if msg.contains("can't tell who")
        ));
    }

    /// A team is granted, listed and revoked exactly as a person is — the
    /// record keeps the principal, so a team and a user of the same id never
    /// collide.
    #[test]
    fn a_skill_is_shared_with_a_team_by_name() {
        use workforce_identity::sqlite::SqliteIdentityStore;
        use workforce_identity::store::IdentityAndTeamStore;
        use workforce_identity::team::Team;
        use workforce_identity::types::{Identity, IdentityRole};

        let identities = Arc::new(SqliteIdentityStore::in_memory().unwrap());
        let alice = Identity::new("Alice", IdentityRole::Member);
        identities.create(&alice).unwrap();
        let platform = Team::new("platform", alice.id);
        identities.create_team(&platform).unwrap();
        let alice = alice.id.to_string();

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(WasmSkillStore::open(dir.path()).unwrap());
        store
            .install_prebuilt(
                SkillManifest::from_toml(ENABLER).unwrap(),
                b"wasm",
                None,
                "test".into(),
                HashMap::new(),
                SkillOwnership::owned(&alice, Visibility::Private),
            )
            .unwrap();
        let owner = Consent::new(
            Arc::clone(&store),
            None,
            &alice,
            false,
            Some(Arc::clone(&identities) as Arc<dyn IdentityStore>),
            Some(Arc::clone(&identities).as_team_store()),
            Surface::Chat,
        );

        let shared = owner
            .share("enabler", "team:PLATFORM", Some(GrantLevel::Manage))
            .unwrap();
        assert_eq!(shared.change, ShareChange::Added(GrantLevel::Manage));
        assert_eq!(shared.recipient.named(), "the platform team");
        assert_eq!(
            store.get_installed("enabler").unwrap().shares,
            vec![Grant::team(platform.id.to_string(), GrantLevel::Manage)]
        );
        assert_eq!(
            owner.shares("enabler").unwrap(),
            vec![Grantee {
                display: "the platform team".into(),
                permission: GrantLevel::Manage,
            }]
        );

        assert_eq!(
            owner
                .share("enabler", "team:platform", None)
                .unwrap()
                .change,
            ShareChange::Removed
        );
        assert!(store.get_installed("enabler").unwrap().shares.is_empty());
    }

    /// An identity store holding alice and bob, with their ids.
    pub(crate) fn alice_and_bob() -> (Arc<dyn IdentityStore>, String, String) {
        use workforce_identity::sqlite::SqliteIdentityStore;
        use workforce_identity::types::{Identity, IdentityRole};

        let identities = SqliteIdentityStore::in_memory().unwrap();
        let mut alice = Identity::new("Alice", IdentityRole::Member);
        alice.email = Some("alice@example.com".into());
        let mut bob = Identity::new("Bob", IdentityRole::Member);
        bob.email = Some("bob@example.com".into());
        identities.create(&alice).unwrap();
        identities.create(&bob).unwrap();
        (
            Arc::new(identities),
            alice.id.to_string(),
            bob.id.to_string(),
        )
    }

    /// An external `hello-world` owned by `owner` in a fresh skills directory.
    pub(crate) fn external_owned_by(owner: &str) -> (tempfile::TempDir, Arc<ExternalSkills>) {
        use crate::external_skills::tests::{owned, record_in, write_skill};
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        write_skill(&skills_dir, "hello-world", "hello-world").unwrap();
        record_in(
            &skills_dir.join("hello-world"),
            &owned(Some(owner), Visibility::Private, vec![]),
        );
        let skills = Arc::new(ExternalSkills::new(skills_dir.clone(), vec![skills_dir]));
        (dir, skills)
    }

    #[test]
    fn sharing_kind_resolves_every_holding() {
        use Holding::{Held, Missing, NoRegistry};
        for (wasm, external, kind) in [
            (Held, Held, SkillKind::Wasm),
            (Held, Missing, SkillKind::Wasm),
            (Held, NoRegistry, SkillKind::Wasm),
            (Missing, Held, SkillKind::External),
            (Missing, Missing, SkillKind::Wasm),
            (Missing, NoRegistry, SkillKind::Wasm),
            (NoRegistry, Held, SkillKind::External),
            (NoRegistry, Missing, SkillKind::External),
            (NoRegistry, NoRegistry, SkillKind::External),
        ] {
            assert_eq!(sharing_kind(wasm, external), kind, "{wasm:?} {external:?}");
        }
    }

    #[test]
    fn an_external_skill_is_shared_through_the_same_commands() {
        let (identities, alice, bob) = alice_and_bob();
        let (_dir, skills) = external_owned_by(&alice);
        let h = Harness::new();
        let consent = |uid: &str| {
            Consent::new(
                Arc::clone(&h.store),
                None,
                uid,
                false,
                Some(Arc::clone(&identities)),
                None,
                Surface::Chat,
            )
            .with_external(Arc::clone(&skills), false)
        };

        let shared = consent(&alice)
            .share("hello-world", "bob@example.com", Some(GrantLevel::Run))
            .unwrap();
        assert_eq!(shared.kind, SkillKind::External);
        assert_eq!(shared.change, ShareChange::Added(GrantLevel::Run));
        let live = skills.installed("hello-world").unwrap();
        assert_eq!(
            live.access.shares,
            vec![Grant::user(bob.clone(), GrantLevel::Run)]
        );
        assert_eq!(
            crate::external_skills::read_access(&live.discovered.path).unwrap(),
            live.access
        );

        let Err(Refusal::Denied(refused)) =
            consent(&bob).share("hello-world", "alice@example.com", None)
        else {
            panic!("a grantee must not re-share");
        };
        assert!(refused.contains("belongs to another user"), "{refused}");

        let published = consent(&alice)
            .set_visibility("hello-world", Visibility::Global)
            .unwrap();
        assert_eq!(published.kind, SkillKind::External);
        assert_eq!(published.shares, 1);
        assert_eq!(
            skills.installed("hello-world").unwrap().access.visibility,
            Visibility::Global
        );

        // And handed over through the same one, writing the record on disk.
        let given = consent(&alice)
            .transfer("hello-world", &bob, OutgoingAccess::Nothing)
            .unwrap();
        assert_eq!(given.kind, SkillKind::External);
        assert_eq!(given.previous_owner.as_deref(), Some(alice.as_str()));
        assert!(
            given.shared_credentials.is_empty(),
            "an external skill declares no credential to leave behind"
        );
        let live = skills.installed("hello-world").unwrap();
        assert_eq!(live.access.owner.as_deref(), Some(bob.as_str()));
        assert_eq!(live.access.visibility, Visibility::Global);
        assert!(
            live.access.shares.is_empty(),
            "the grant naming the new owner is subsumed by owning it"
        );
        assert_eq!(
            crate::external_skills::read_access(&live.discovered.path).unwrap(),
            live.access
        );
        assert!(consent(&alice)
            .transfer("hello-world", &bob, OutgoingAccess::Nothing)
            .is_err());
    }

    #[test]
    fn a_skills_manager_cannot_make_an_ownerless_external_skill_private() {
        use crate::external_skills::tests::write_skill;
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        write_skill(&skills_dir, "org", "org").unwrap();
        let skills = Arc::new(ExternalSkills::new(skills_dir.clone(), vec![skills_dir]));
        let h = Harness::new();
        let consent = Consent::new(
            Arc::clone(&h.store),
            None,
            "root",
            true,
            None,
            None,
            Surface::Chat,
        )
        .with_external(Arc::clone(&skills), true);

        assert_eq!(
            consent.set_visibility("org", Visibility::Private),
            Err(Refusal::Denied(
                "Skill `org` has no owner, so everyone can use it whatever its visibility. \
                 Install it again with `visibility = \"private\"` to make it a private skill of \
                 your own."
                    .to_string()
            ))
        );
        assert_eq!(
            skills.installed("org").unwrap().access.visibility,
            Visibility::Global
        );
    }

    #[test]
    fn a_name_both_kinds_hold_reaches_the_wasm_skill() {
        let (identities, alice, _bob) = alice_and_bob();
        let (_dir, skills) = external_owned_by(&alice);
        let wasm_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(WasmSkillStore::open(wasm_dir.path()).unwrap());
        store
            .install_prebuilt(
                SkillManifest::from_toml(
                    "[skill]\nname = \"hello-world\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\
                     [[tools]]\nname = \"ping\"\ndescription = \"p\"\n",
                )
                .unwrap(),
                b"wasm",
                None,
                "test".into(),
                HashMap::new(),
                SkillOwnership::owned("carol", Visibility::Private),
            )
            .unwrap();
        let consent = Consent::new(
            Arc::clone(&store),
            None,
            &alice,
            false,
            Some(identities),
            None,
            Surface::Chat,
        )
        .with_external(Arc::clone(&skills), false);

        let Err(Refusal::Denied(refused)) =
            consent.set_visibility("hello-world", Visibility::Global)
        else {
            panic!("the WASM skill is carol's");
        };
        assert_eq!(
            refused,
            "Skill `hello-world` belongs to another user, so only they can change who reaches it."
        );
        assert!(consent
            .share("hello-world", "bob@example.com", Some(GrantLevel::Run))
            .is_err());
        assert_eq!(
            skills.installed("hello-world").unwrap().access.visibility,
            Visibility::Private
        );
        assert!(skills
            .installed("hello-world")
            .unwrap()
            .access
            .shares
            .is_empty());
    }

    #[test]
    fn grants_without_a_wasm_store_refuse_with_no_store() {
        let (_dir, skills) = external_owned_by("alice");
        let h = Harness::new();
        let consent = Consent::from_parts(
            None,
            Some(Arc::clone(&h.vault) as Arc<dyn SecretStore>),
            "alice".to_string(),
            false,
            None,
            None,
            Surface::Chat,
        )
        .with_external(Arc::clone(&skills), false);

        assert_eq!(
            consent.grant_credential("enabler", "api_token", Some("v".into())),
            Err(Refusal::NoStore)
        );
        assert_eq!(consent.grant_tools("enabler", false), Err(Refusal::NoStore));
        assert_eq!(
            consent.revoke("enabler", Target::Tools { shared: false }),
            Err(Refusal::NoStore)
        );
        assert_eq!(consent.grants(None).unwrap_err(), Refusal::NoStore);
        assert_eq!(consent.user_secret("api_token"), Err(Refusal::NoStore));
        assert_eq!(
            Refusal::NoStore.to_string(),
            "No WASM skill runtime is active on this hub."
        );
        assert_eq!(
            consent
                .set_visibility("hello-world", Visibility::Global)
                .unwrap()
                .kind,
            SkillKind::External
        );
    }

    #[test]
    fn without_a_vault_only_sharing_still_works() {
        let h = Harness::new();
        let consent = Consent::new(
            Arc::clone(&h.store),
            None,
            "alice",
            false,
            None,
            None,
            Surface::Chat,
        );
        assert_eq!(
            consent.grant_credential("enabler", "api_token", Some("v".into())),
            Err(Refusal::VaultUnavailable)
        );
        assert_eq!(
            consent.grant_tools("enabler", false),
            Err(Refusal::VaultUnavailable)
        );
        assert_eq!(consent.user_secret("x"), Err(Refusal::VaultUnavailable));
        consent
            .set_visibility("enabler", Visibility::Global)
            .unwrap();
    }
    /// Three members and a temp store holding `enabler` owned by the first,
    /// which is what a transfer needs and the shared `Harness` does not carry.
    struct Members {
        identities: Arc<dyn IdentityStore>,
        alice: String,
        bob: String,
        dormant: String,
    }

    fn members() -> Members {
        use workforce_identity::sqlite::SqliteIdentityStore;
        use workforce_identity::types::{Identity, IdentityRole, IdentityStatus};

        let store = Arc::new(SqliteIdentityStore::in_memory().unwrap());
        let made = |name: &str, status| {
            let mut identity = Identity::new(name, IdentityRole::Member).with_status(status);
            identity.email = Some(format!("{}@example.test", name.to_ascii_lowercase()));
            store.create(&identity).unwrap();
            identity.id.to_string()
        };
        let (alice, bob, dormant) = (
            made("Alice", IdentityStatus::Active),
            made("Bob", IdentityStatus::Active),
            made("Dormant", IdentityStatus::Suspended),
        );
        Members {
            identities: store as Arc<dyn IdentityStore>,
            alice,
            bob,
            dormant,
        }
    }

    impl Harness {
        fn consent_for(&self, who: &Members, uid: &str, admin: bool) -> Consent {
            self.consent_with(uid, admin, Some(Arc::clone(&who.identities)))
        }

        fn own(&self, owner: &str) {
            self.store
                .set_access(
                    "enabler",
                    AccessWrite {
                        owner: OwnerWrite::To(owner.to_string()),
                        visibility: Visibility::Private,
                        grants: Vec::new(),
                    },
                )
                .unwrap();
        }
    }

    /// The transfer rule, which is the sharing rule: only ownership earns it,
    /// never a `manage` grant, and the target must be a live active member.
    #[test]
    fn a_skill_is_handed_over_by_its_owner_and_by_nobody_they_shared_it_with() {
        let who = members();
        let harness = Harness::new();
        harness.own(&who.alice);
        harness
            .store
            .set_access(
                "enabler",
                AccessWrite::kept(
                    Visibility::Private,
                    vec![Grant::user(&who.bob, GrantLevel::Manage)],
                ),
            )
            .unwrap();

        let grantee = harness.consent_for(&who, &who.bob, false);
        assert!(matches!(
            grantee.transfer("enabler", &who.alice, OutgoingAccess::Nothing),
            Err(Refusal::Denied(_))
        ));

        let owner = harness.consent_for(&who, &who.alice, false);
        for refused in [
            &who.dormant,
            &uuid::Uuid::new_v4().to_string(),
            "team:ops",
            &who.alice,
        ] {
            assert!(
                matches!(
                    owner.transfer("enabler", refused, OutgoingAccess::Nothing),
                    Err(Refusal::NoSuchTarget(_))
                ),
                "a skill must not be strandable: {refused}"
            );
        }
        assert_eq!(
            harness
                .store
                .get_installed("enabler")
                .unwrap()
                .owner
                .as_deref(),
            Some(who.alice.as_str()),
            "not one refusal moved it"
        );

        let done = owner
            .transfer("enabler", "bob@example.test", OutgoingAccess::Nothing)
            .unwrap();
        assert_eq!(done.recipient.id, who.bob);
        assert_eq!(done.previous_owner.as_deref(), Some(who.alice.as_str()));
        // A shared-scope credential goes on serving a skill its owner no
        // longer controls, so the outcome says so rather than staying silent.
        assert_eq!(done.shared_credentials, vec!["shared_key".to_string()]);
        assert!(skill_transfer_notes(&done)
            .iter()
            .any(|note| note.contains("shared_key")));
        assert!(
            owner
                .transfer("enabler", &who.alice, OutgoingAccess::Nothing)
                .is_err(),
            "alice keeps nothing"
        );
    }

    /// The regression guard for the production incident: a transfer must not
    /// disturb the consent contract, so nobody re-consents and no grant is
    /// invalidated. Asserted on the fingerprints themselves, not on a
    /// behaviour derived from them.
    #[test]
    fn a_transfer_leaves_the_consent_contract_and_the_locker_byte_identical() {
        let who = members();
        let harness = Harness::new();
        harness.own(&who.alice);
        let installed = harness.store.get_installed("enabler").unwrap();
        let (contract, grant) = (
            installed.contract_fingerprint(),
            installed.grant_fingerprint().unwrap(),
        );
        let locker = [
            "wasm-skills/enabler/users/alice/api_token",
            "wasm-skills/enabler/shared/shared_key",
            "wasm-skills/enabler/grants/users/alice/api_token",
            "wasm-skills/enabler/grants/shared/shared_key",
        ];
        for (n, path) in locker.iter().enumerate() {
            harness.vault.set(path, &format!("value-{n}")).unwrap();
        }

        harness
            .consent_for(&who, &who.alice, false)
            .transfer("enabler", &who.bob, OutgoingAccess::Nothing)
            .unwrap();

        let after = harness.store.get_installed("enabler").unwrap();
        assert_eq!(after.owner.as_deref(), Some(who.bob.as_str()));
        assert_eq!(after.contract_fingerprint(), contract);
        assert_eq!(after.grant_fingerprint().unwrap(), grant);
        for (n, path) in locker.iter().enumerate() {
            assert_eq!(
                harness.vault.get(path).as_deref(),
                Some(format!("value-{n}").as_str()),
                "{path} must be untouched by a transfer"
            );
        }
    }

    /// Only the owner changes. A grant naming the new owner is dead weight
    /// once they own it; a team grant that happens to contain them reaches
    /// everyone else on that team and must survive.
    #[test]
    fn a_transfer_carries_the_visibility_and_every_grant_but_the_new_owners() {
        let who = members();
        let harness = Harness::new();
        harness.own(&who.alice);
        harness
            .store
            .set_access(
                "enabler",
                AccessWrite::kept(
                    Visibility::Global,
                    vec![
                        Grant::user(&who.bob, GrantLevel::Run),
                        Grant::user(&who.alice, GrantLevel::Manage),
                        Grant::team("platform", GrantLevel::Run),
                    ],
                ),
            )
            .unwrap();

        harness
            .consent_for(&who, &who.alice, false)
            .transfer("enabler", &who.bob, OutgoingAccess::Nothing)
            .unwrap();

        let after = harness.store.get_installed("enabler").unwrap();
        assert_eq!(after.visibility, Visibility::Global);
        assert_eq!(
            after.shares,
            vec![
                Grant::user(&who.alice, GrantLevel::Manage),
                Grant::team("platform", GrantLevel::Run),
            ]
        );
    }

    /// A skill installed under a name nothing else in the process holds, so an
    /// audit assertion can filter the shared ring buffer — which every test in
    /// this binary writes to — by resource id.
    fn unique_skill(store: &WasmSkillStore, owner: Option<&str>) -> String {
        let name = format!("enabler-{}", uuid::Uuid::new_v4().simple());
        store
            .install_prebuilt(
                SkillManifest::from_toml(
                    &ENABLER.replace("name = \"enabler\"", &format!("name = \"{name}\"")),
                )
                .unwrap(),
                b"wasm",
                None,
                "test".into(),
                HashMap::new(),
                owner.map_or_else(SkillOwnership::operator, |owner| {
                    SkillOwnership::owned(owner, Visibility::Private)
                }),
            )
            .unwrap();
        name
    }

    /// The keep-access choice is part of the single write that moves
    /// ownership, so there is no share before the handover to be refused — the
    /// owner cannot be their own grantee while they still own it — and none
    /// after it that they would no longer have the standing to make. Ticked,
    /// the outgoing owner is left exactly one `manage` grant, the inert
    /// self-grant that predates the rule is raised rather than doubled, and
    /// every other grantee is untouched — including the new owner's own, which
    /// ownership still subsumes.
    #[test]
    fn keeping_manage_leaves_one_grant_and_every_other_grantee_intact() {
        let who = members();
        let harness = Harness::new();
        harness.own(&who.alice);
        harness
            .store
            .set_access(
                "enabler",
                AccessWrite::kept(
                    Visibility::Private,
                    vec![
                        Grant::user(&who.bob, GrantLevel::Run),
                        Grant::team("platform", GrantLevel::Manage),
                        Grant::user(&who.alice, GrantLevel::Run),
                    ],
                ),
            )
            .unwrap();

        let done = harness
            .consent_for(&who, &who.alice, false)
            .transfer("enabler", &who.bob, OutgoingAccess::Manage)
            .unwrap();

        assert!(done.kept_manage);
        let live = harness.store.get_installed("enabler").unwrap();
        assert_eq!(live.owner.as_deref(), Some(who.bob.as_str()));
        assert_eq!(
            live.shares,
            vec![
                Grant::team("platform", GrantLevel::Manage),
                Grant::user(&who.alice, GrantLevel::Manage),
            ],
            "one manage grant for the outgoing owner, the team's untouched, \
             and the new owner's subsumed by owning it"
        );
        let notes = skill_transfer_notes(&done);
        assert!(notes[0].contains("You keep `manage` access"), "{notes:?}");
        assert!(!notes[0].contains("no longer edit it"), "{notes:?}");
    }

    /// A skill nobody owned has no outgoing owner, so there is nothing for the
    /// choice to keep: it is answered by the outcome rather than by a grant
    /// naming whoever happened to ask, and the note says what happened.
    #[test]
    fn an_ownerless_skill_keeps_nothing_for_anybody() {
        let who = members();
        let harness = Harness::new();
        let ownerless = unique_skill(&harness.store, None);

        let done = harness
            .consent_for(&who, "root", true)
            .transfer(&ownerless, &who.alice, OutgoingAccess::Manage)
            .unwrap();

        assert!(!done.kept_manage);
        assert!(harness
            .store
            .get_installed(&ownerless)
            .unwrap()
            .shares
            .is_empty());
        assert!(skill_transfer_notes(&done)[0].contains("no longer edit it"));
    }

    /// An administrator reaching past an owner is recorded as an override
    /// beside the owner change; giving a skill nobody owns an owner is not an
    /// override at all, because there was no owner to reach past — which is
    /// the case the bypass exists for.
    #[test]
    fn an_administrators_transfer_is_recorded_and_an_ownerless_one_is_only_a_change() {
        use workforce_memory::access_audit::{self, AuditOperation};

        let buffer = access_audit::install_ring_buffer(256);
        let _ = access_audit::set_global_sink(buffer.clone());
        let who = members();
        let counts = |name: &str| -> (usize, usize) {
            let resource = format!("wasm:{name}");
            buffer
                .snapshot()
                .into_iter()
                .filter(|e| e.resource_id.as_deref() == Some(resource.as_str()))
                .fold((0, 0), |(owned, reached), e| match e.operation {
                    AuditOperation::ResourceOwnerChange => (owned + 1, reached),
                    AuditOperation::ResourceAccessOverride => (owned, reached + 1),
                    _ => (owned, reached),
                })
        };
        let harness = Harness::new();
        let root = harness.consent_for(&who, "root", true);

        let owned = unique_skill(&harness.store, Some(&who.alice));
        root.transfer(&owned, &who.bob, OutgoingAccess::Nothing)
            .unwrap();
        assert_eq!(counts(&owned), (1, 1), "one of each, never collapsed");

        let ownerless = unique_skill(&harness.store, None);
        root.transfer(&ownerless, &who.alice, OutgoingAccess::Nothing)
            .unwrap();
        assert_eq!(
            counts(&ownerless),
            (1, 0),
            "giving an unowned skill an owner reaches past nobody"
        );

        // The grant a kept transfer writes is part of that same write, so it
        // adds no event of its own and duplicates neither of these.
        let kept = unique_skill(&harness.store, Some(&who.alice));
        root.transfer(&kept, &who.bob, OutgoingAccess::Manage)
            .unwrap();
        assert_eq!(counts(&kept), (1, 1), "a kept grant is not a second event");
    }

    #[test]
    fn sharing_hints_read_exactly_as_before() {
        let mut skill = installed_fixture("hello-counter", Some("alice"));
        assert_eq!(
            crate::skill_access::sharing_hint("hello-counter", &skill),
            "Only you can use `hello-counter`. Share it with `cmd: skill share hello-counter \
             <user>` (add `manage` to let them edit it), or publish it to everyone with \
             `cmd: skill visibility hello-counter everyone`."
        );
        skill.shares = vec![
            Grant::user("bob", GrantLevel::Run),
            Grant::user("carol", GrantLevel::Manage),
        ];
        assert_eq!(
            crate::skill_access::sharing_hint("hello-counter", &skill),
            "You and the 2 user(s) you shared it with can use `hello-counter`. Publish it to \
             everyone with `cmd: skill visibility hello-counter everyone`, or change who has it \
             with `cmd: skill share hello-counter <user>` / `cmd: skill unshare hello-counter \
             <user>`."
        );
        skill.visibility = Visibility::Global;
        assert_eq!(
            crate::skill_access::sharing_hint("hello-counter", &skill),
            "Everyone can use `hello-counter`. Make it yours alone again with \
             `cmd: skill visibility hello-counter private`."
        );
        skill.visibility = Visibility::Team;
        assert_eq!(
            crate::skill_access::sharing_hint("hello-counter", &skill),
            "Anyone in a team with you can use `hello-counter`. Make it yours alone again \
             with `cmd: skill visibility hello-counter private`."
        );
    }
}
