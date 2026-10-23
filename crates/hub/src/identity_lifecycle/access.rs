//! The identity itself and what grants it access or authority.

use async_trait::async_trait;
use chrono::Utc;
use eyre::Result;
use workforce_identity::types::IdentityStatus;
use workforce_permissions::store::PermissionStore;
use workforce_permissions::types::{ApproverRef, PermissionFilter, PermissionScope};

use super::{Context, Holding, Owners, Person, Tally};
use crate::budget_overrides::OverrideScope;

/// Who a budget override removal is recorded against.
const LIFECYCLE_ACTOR: &str = "identity-lifecycle";

pub struct IdentityRecord;

#[async_trait]
impl Holding for IdentityRecord {
    /// The record is what a missing identity no longer has.
    async fn owners(&self, _cx: &Context<'_>) -> Result<Owners> {
        Ok(Owners::default())
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        let record = cx
            .identities
            .get_by_id(who.id)?
            .map(|_| "identity".to_string());
        Ok(Tally::of(
            record.into_iter().chain(
                who.linked()
                    .into_iter()
                    .map(|(platform, id)| format!("{platform}:{id}")),
            ),
        ))
    }

    /// The source is suspended and loses its linked ids and email before
    /// the target gains them, so no lookup ever finds both; a platform the
    /// target already has keeps the target's.
    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        let (Some(mut source), Some(mut target)) = (
            cx.identities.get_by_id(from.id)?,
            cx.identities.get_by_id(into.id)?,
        ) else {
            eyre::bail!("an identity of the merge is gone");
        };
        let mut tally = Tally::default();
        for (platform, id) in from.linked() {
            match target.linked_ids.get(platform) {
                Some(kept) if kept != id => tally
                    .conflicts
                    .push(format!("{platform}: kept {}'s own link", into.display)),
                Some(_) => {}
                None => {
                    tally.item(format!("{platform}:{id}"));
                    target
                        .linked_ids
                        .insert(platform.to_string(), id.to_string());
                }
            }
        }
        let email = source.email.take().filter(|_| target.email.is_none());
        if let Some(email) = &email {
            tally.item(format!("email {email}"));
        }
        if apply {
            source.linked_ids.clear();
            source.status = IdentityStatus::Suspended;
            source.updated_at = Utc::now();
            cx.identities.update(&source)?;
            crate::standing::forget(source.id);
            target.email = target.email.or(email);
            target.updated_at = Utc::now();
            cx.identities.update(&target)?;
        }
        Ok(tally)
    }

    /// The record itself is removed by the operation once every other
    /// category is done, so a failure part-way leaves it to resume from.
    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _to: Option<&Person>,
        _apply: bool,
    ) -> Result<Tally> {
        self.held(cx, who).await
    }
}

pub struct RemoteMcpTokens;

impl RemoteMcpTokens {
    fn revoke(cx: &Context<'_>, who: &Person, apply: bool) -> Result<Tally> {
        let tokens = cx.identities.list_tokens_for_identity(who.id)?;
        if apply {
            for token in &tokens {
                cx.identities
                    .revoke_token(token.id, Some(who.id), Utc::now())?;
            }
        }
        Ok(Tally::of(tokens.into_iter().map(|t| t.label)))
    }
}

#[async_trait]
impl Holding for RemoteMcpTokens {
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        Ok(Owners::of(
            cx.identities
                .list_all_tokens()?
                .into_iter()
                .map(|token| token.identity_id.to_string()),
        )
        .of_identities())
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        Self::revoke(cx, who, false)
    }

    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        _: &Person,
        apply: bool,
    ) -> Result<Tally> {
        Self::revoke(cx, from, apply)
    }

    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        Self::revoke(cx, who, apply)
    }
}

pub struct Devices;

impl Devices {
    fn paired(cx: &Context<'_>, who: &Person) -> Result<Vec<workforce_pairing::ApprovedDevice>> {
        let Some(devices) = &cx.devices else {
            return Ok(Vec::new());
        };
        let linked: Vec<&str> = who.linked().into_iter().map(|(_, id)| id).collect();
        Ok(devices
            .list_devices()?
            .into_iter()
            .filter(|device| !device.revoked && linked.contains(&device.id.as_str()))
            .collect())
    }
}

#[async_trait]
impl Holding for Devices {
    /// A device is held under its own id, which its person's linked ids
    /// name.
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        let Some(devices) = &cx.devices else {
            return Ok(Owners::default());
        };
        Ok(Owners::of(
            devices
                .list_devices()?
                .into_iter()
                .filter(|device| !device.revoked)
                .map(|device| device.id),
        ))
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        Ok(Tally::of(
            Self::paired(cx, who)?.into_iter().map(|d| d.device_name),
        ))
    }

    /// A device is the person's own, so it follows its link to the
    /// identity merged into. Where that identity already links a device of
    /// the same kind, it keeps its own and the source's is revoked: a token
    /// linked to nobody must not stay live.
    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        let Some(target) = cx.identities.get_by_id(into.id)? else {
            eyre::bail!("the identity merged into is gone");
        };
        let displaced: Vec<&str> = from
            .linked()
            .into_iter()
            .filter(|(platform, id)| {
                target
                    .linked_ids
                    .get(*platform)
                    .is_some_and(|kept| kept != id)
            })
            .map(|(_, id)| id)
            .collect();
        let mut tally = Tally::default();
        for device in Self::paired(cx, from)? {
            if displaced.contains(&device.id.as_str()) {
                tally.conflicts.push(format!(
                    "{}: {} keeps its own device of this kind, so this one is revoked",
                    device.device_name, into.display
                ));
                if let (true, Some(devices)) = (apply, &cx.devices) {
                    devices.revoke(&device.id)?;
                }
            }
            tally.item(device.device_name);
        }
        Ok(tally)
    }

    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        let paired = Self::paired(cx, who)?;
        if let (true, Some(devices)) = (apply, &cx.devices) {
            for device in &paired {
                devices.revoke(&device.id)?;
            }
        }
        Ok(Tally::of(paired.into_iter().map(|d| d.device_name)))
    }
}

pub struct ChannelAllowlist;

impl ChannelAllowlist {
    fn entries(
        cx: &Context<'_>,
        who: &Person,
    ) -> Result<Vec<workforce_pairing::ChannelAllowlistEntry>> {
        let Some(channels) = &cx.channels else {
            return Ok(Vec::new());
        };
        let linked = who.linked();
        Ok(channels
            .list_allowlist(None)?
            .into_iter()
            .filter(|entry| linked.contains(&(entry.platform.as_str(), entry.sender_id.as_str())))
            .collect())
    }
}

#[async_trait]
impl Holding for ChannelAllowlist {
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        let Some(channels) = &cx.channels else {
            return Ok(Owners::default());
        };
        Ok(Owners::of(channels.list_allowlist(None)?.into_iter().map(
            |entry| format!("{}:{}", entry.platform, entry.sender_id),
        )))
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        Ok(Tally::of(
            Self::entries(cx, who)?
                .into_iter()
                .map(|e| format!("{}:{}", e.platform, e.sender_id)),
        ))
    }

    /// Keyed by the platform id, which resolves to the identity merged into
    /// once its link has moved.
    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        _: &Person,
        _apply: bool,
    ) -> Result<Tally> {
        self.held(cx, from).await
    }

    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        let entries = Self::entries(cx, who)?;
        if let (true, Some(channels)) = (apply, &cx.channels) {
            for entry in &entries {
                channels.remove_sender(&entry.platform, &entry.sender_id)?;
            }
        }
        Ok(Tally::of(
            entries
                .into_iter()
                .map(|e| format!("{}:{}", e.platform, e.sender_id)),
        ))
    }
}

pub struct Teams;

impl Teams {
    fn rewrite(
        cx: &Context<'_>,
        who: &Person,
        into: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        let mut tally = Tally::default();
        for mut team in cx.identities.list_teams()? {
            let member = team.has_member(who.id);
            let requested = team.join_requests.iter().any(|r| r.identity_id == who.id);
            let created = team.creator_id == who.id;
            if !(member || requested || created) {
                continue;
            }
            tally.item(team.name.clone());
            team.remove_member(who.id);
            team.reject_join(who.id);
            if let Some(into) = into {
                if member {
                    team.add_member(into.id);
                } else if requested {
                    team.request_join(into.id);
                }
                if created {
                    team.creator_id = into.id;
                }
            }
            if apply {
                cx.identities.update_team(&team)?;
            }
        }
        Ok(tally)
    }
}

#[async_trait]
impl Holding for Teams {
    /// Memberships and requests; who created a team is a record kept
    /// through a delete, and proves only that they were an identity.
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        let teams = cx.identities.list_teams()?;
        let mut owners = Owners::of(teams.iter().flat_map(|team| {
            team.members
                .iter()
                .copied()
                .chain(team.join_requests.iter().map(|r| r.identity_id))
                .map(|id| id.to_string())
        }))
        .of_identities();
        owners.absorb(Owners::proving(
            teams.iter().map(|team| team.creator_id.to_string()),
        ));
        Ok(owners)
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        Self::rewrite(cx, who, None, false)
    }

    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        Self::rewrite(cx, from, Some(into), apply)
    }

    /// Their membership and requests go; a team they created keeps naming
    /// them as its creator, as a record of who made it.
    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        Self::rewrite(cx, who, None, apply)
    }
}

fn permission_store(cx: &Context<'_>) -> Result<Option<std::sync::Arc<dyn PermissionStore>>> {
    Ok(cx
        .hub()?
        .permission_evaluator
        .read()
        .as_ref()
        .map(|evaluator| std::sync::Arc::clone(evaluator.store())))
}

/// Roles, personal rules, approver slots and budget overrides are authority,
/// not belongings: a merge drops them with the identity rather than raising
/// the target's privileges as a side effect, and reports them so an
/// administrator can grant the target what it should have.
pub struct RoleGrants;

impl RoleGrants {
    fn unassign(cx: &Context<'_>, who: &Person, apply: bool) -> Result<Tally> {
        let Some(store) = permission_store(cx)? else {
            return Ok(Tally::default());
        };
        let roles = store.roles_for_identity(who.id)?;
        if apply {
            for role in &roles {
                store.unassign_role_from_identity(who.id, role.id)?;
            }
        }
        Ok(Tally::of(roles.into_iter().map(|r| r.name)))
    }
}

#[async_trait]
impl Holding for RoleGrants {
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        let Some(store) = permission_store(cx)? else {
            return Ok(Owners::default());
        };
        Ok(Owners::counted(
            store
                .role_holders()?
                .into_iter()
                .map(|(id, roles)| (id.to_string(), roles)),
        )
        .of_identities())
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        Self::unassign(cx, who, false)
    }

    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        _: &Person,
        apply: bool,
    ) -> Result<Tally> {
        Self::unassign(cx, from, apply)
    }

    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        Self::unassign(cx, who, apply)
    }
}

pub struct PermissionRules;

impl PermissionRules {
    fn remove(cx: &Context<'_>, who: &Person, apply: bool) -> Result<Tally> {
        let Some(store) = permission_store(cx)? else {
            return Ok(Tally::default());
        };
        let rules = store.list_rules(&PermissionFilter {
            scope: Some(PermissionScope::User),
            scope_id: Some(who.key()),
            ..PermissionFilter::default()
        })?;
        if apply {
            for rule in &rules {
                store.delete_rule(rule.id)?;
            }
        }
        Ok(Tally::of(rules.into_iter().map(|r| r.tool_pattern)))
    }
}

#[async_trait]
impl Holding for PermissionRules {
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        let Some(store) = permission_store(cx)? else {
            return Ok(Owners::default());
        };
        Ok(Owners::of(
            store
                .list_rules(&PermissionFilter {
                    scope: Some(PermissionScope::User),
                    ..PermissionFilter::default()
                })?
                .into_iter()
                .map(|rule| rule.scope_id),
        ))
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        Self::remove(cx, who, false)
    }

    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        _: &Person,
        apply: bool,
    ) -> Result<Tally> {
        Self::remove(cx, from, apply)
    }

    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        Self::remove(cx, who, apply)
    }
}

pub struct ApproverSlots;

impl ApproverSlots {
    /// The person's slot goes from every rule naming them beside someone
    /// else. Where they are a rule's only approver the slot stays: a rule
    /// with no approvers stops routing at all, so the calls it gates would
    /// fall to self-approval, while a slot naming nobody keeps them refused
    /// as unreachable, as they already were once the person had gone.
    fn remove(cx: &Context<'_>, who: &Person, apply: bool) -> Result<Tally> {
        let Some(store) = permission_store(cx)? else {
            return Ok(Tally::default());
        };
        let named =
            |r: &ApproverRef| matches!(r, ApproverRef::User { id } if who.names.contains(id));
        let mut tally = Tally::default();
        for mut rule in store.list_rules(&PermissionFilter::default())? {
            if !rule.approvers.iter().any(named) {
                continue;
            }
            tally.item(rule.tool_pattern.clone());
            if rule.approvers.iter().all(named) {
                tally.conflicts.push(format!(
                    "{}: they are its only approver, so the calls it gates stay refused until an administrator names another",
                    rule.tool_pattern
                ));
                continue;
            }
            rule.approvers.retain(|r| !named(r));
            if apply {
                store.upsert_rule(&rule)?;
            }
        }
        Ok(tally)
    }
}

#[async_trait]
impl Holding for ApproverSlots {
    /// A slot naming someone gone is counted where a delete keeps it too:
    /// as a rule's only approver it waits for an administrator to name
    /// another.
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        let Some(store) = permission_store(cx)? else {
            return Ok(Owners::default());
        };
        Ok(Owners::of(
            store
                .list_rules(&PermissionFilter::default())?
                .into_iter()
                .flat_map(|rule| {
                    rule.approvers
                        .into_iter()
                        .filter_map(|approver| match approver {
                            ApproverRef::User { id } => Some(id),
                            _ => None,
                        })
                        .collect::<std::collections::BTreeSet<_>>()
                }),
        ))
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        Self::remove(cx, who, false)
    }

    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        _: &Person,
        apply: bool,
    ) -> Result<Tally> {
        Self::remove(cx, from, apply)
    }

    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        Self::remove(cx, who, apply)
    }
}

pub struct BudgetOverrides;

impl BudgetOverrides {
    fn remove(who: &Person, apply: bool) -> Result<Tally> {
        let Some(store) = crate::budget_overrides::global_store() else {
            return Ok(Tally::default());
        };
        let held: Vec<String> = store
            .list_active(Utc::now())?
            .into_iter()
            .filter(|o| o.scope == OverrideScope::Identity && who.names.contains(&o.key))
            .map(|o| o.key)
            .collect();
        if apply {
            for key in &held {
                store.remove(OverrideScope::Identity, key, LIFECYCLE_ACTOR)?;
            }
        }
        Ok(Tally::of(held))
    }
}

#[async_trait]
impl Holding for BudgetOverrides {
    async fn owners(&self, _cx: &Context<'_>) -> Result<Owners> {
        let Some(store) = crate::budget_overrides::global_store() else {
            return Ok(Owners::default());
        };
        Ok(identity_overrides(store.list_active(Utc::now())?))
    }

    async fn held(&self, _cx: &Context<'_>, who: &Person) -> Result<Tally> {
        Self::remove(who, false)
    }

    async fn merge(
        &self,
        _cx: &Context<'_>,
        from: &Person,
        _: &Person,
        apply: bool,
    ) -> Result<Tally> {
        Self::remove(from, apply)
    }

    async fn delete(
        &self,
        _cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        Self::remove(who, apply)
    }
}

/// The people budget overrides raise a ceiling for.
pub(super) fn identity_overrides(
    overrides: Vec<crate::budget_overrides::BudgetOverride>,
) -> Owners {
    Owners::of(
        overrides
            .into_iter()
            .filter(|o| o.scope == OverrideScope::Identity)
            .map(|o| o.key),
    )
}

pub struct Approvals;

impl Approvals {
    fn store(
        cx: &Context<'_>,
    ) -> Result<Option<std::sync::Arc<dyn crate::hub_core::approvals::PendingApprovalStore>>> {
        Ok(cx.hub()?.pending_approval_store.read().clone())
    }

    fn requested(
        store: &dyn crate::hub_core::approvals::PendingApprovalStore,
        who: &Person,
    ) -> Result<Tally> {
        let mut tally = Tally::default();
        for name in &who.names {
            for request in store.requested_by(name)? {
                tally.item(format!("{} ({})", request.tool_name, request.status));
            }
        }
        let mut targets = 0;
        for name in &who.names {
            targets += store.approver_targets(name)?;
        }
        if targets > 0 {
            tally.absorb(Tally::of([format!("approver on {targets} requests")]));
        }
        Ok(tally)
    }
}

#[async_trait]
impl Holding for Approvals {
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        match Self::store(cx)? {
            Some(store) => Ok(Owners::counted(store.identity_holders()?)),
            None => Ok(Owners::default()),
        }
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        match Self::store(cx)? {
            Some(store) => Self::requested(store.as_ref(), who),
            None => Ok(Tally::default()),
        }
    }

    /// Open requests are the person's own and follow them; resolved ones are
    /// a record of who asked. Their approver slots go with their authority.
    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        let Some(store) = Self::store(cx)? else {
            return Ok(Tally::default());
        };
        let tally = Self::requested(store.as_ref(), from)?;
        if apply {
            for name in &from.names {
                store.reassign_requester(name, &into.key())?;
                store.drop_approver(name)?;
            }
        }
        Ok(tally)
    }

    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        let Some(store) = Self::store(cx)? else {
            return Ok(Tally::default());
        };
        let tally = Self::requested(store.as_ref(), who)?;
        if apply {
            for name in &who.names {
                store.forget_requester(name)?;
                store.drop_approver(name)?;
            }
        }
        Ok(tally)
    }
}
