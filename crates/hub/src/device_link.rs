//! Approving a device pairing and connecting the device to its person.
//!
//! A device asks for a code; someone approves the code into an account. Every
//! approval goes through [`approve_and_link`]:
//!
//! - the person signs in on the hub's `/pair` page and approves the code into
//!   their own account (`POST /api/pair/device/approve-self`);
//! - an admin assigns a waiting device to a person they choose
//!   (`POST /api/pair/device/approve`);
//! - an agent's `pairing.approve` approves it into the account of the person
//!   who asked, or, for an admin, of the person named;
//! - the older admin route (`POST /api/pair/approve/{code}`) links by the
//!   `identity_hint` the device sent, as it always has.
//!
//! [`link_existing_device`] connects a device approved without one.

use tracing::{info, warn};
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole, IdentityStatus, ProvisioningSource};
use workforce_pairing::types::{
    ApprovedDevice, DevicePairingRequest, PairingStatus, DEFAULT_DEVICE_KIND, DEVICE_KINDS,
};
use workforce_pairing::DevicePairingService;

/// Whom an approval connects the new device to.
#[derive(Debug, Clone, Copy)]
pub(crate) enum LinkChoice {
    /// The pending request's self-asserted `identity_hint`: a UUID or a known
    /// email links, an unknown email auto-provisions, no hint stays anonymous.
    /// Only the older admin route approves this way.
    FromHint,
    /// An identity the approver established — the signed-in person, the
    /// person an agent is talking to, or one an admin chose.
    Identity {
        id: uuid::Uuid,
        /// An admin chose this person. Only an admin may move a device that
        /// already belongs to someone else (a re-pair carrying its
        /// `client_id`); anyone else approving such a code is refused, so a
        /// member can't take over — or revoke — another person's device.
        by_admin: bool,
    },
}

/// Why a code could not be approved.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ApproveError {
    /// No such code, or it has expired — the same answer for both, so a probe
    /// can't tell an issued code from one that never existed.
    NotFound,
    /// The code was already approved or rejected.
    AlreadyUsed,
    /// The identity to connect to can't take a device.
    Identity(String),
    Failed(String),
}

impl ApproveError {
    /// The plain sentence a person is shown.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::NotFound => "This pairing code has expired or doesn't exist. \
                               Ask the device for a new code."
                .to_string(),
            Self::AlreadyUsed => "This pairing code has already been used.".to_string(),
            Self::Identity(why) | Self::Failed(why) => why.clone(),
        }
    }

    pub(crate) fn status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::AlreadyUsed => StatusCode::CONFLICT,
            Self::Identity(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// The `linked_ids` key one device is recorded under: `{kind}:{device_id}`.
///
/// `linked_ids` maps a key to one id, and pairing used to key a device by its
/// kind alone — so a person's second Mac or iPad, or a re-pair, silently
/// replaced the first, whose token went on working anonymously. Keying each
/// device by itself lets a person hold any number of each kind, keeps every
/// link findable by `get_by_linked_id`, and lets identity merges move them
/// like any other link.
pub(crate) fn device_link_key(kind: &str, device_id: &str) -> String {
    format!("{kind}:{device_id}")
}

/// Every kind a device may have been linked under, aliases included.
fn lookup_kinds() -> impl Iterator<Item = &'static str> {
    DEVICE_KINDS.iter().copied().chain(
        workforce_pairing::types::DEVICE_KIND_ALIASES
            .iter()
            .map(|(alias, _)| *alias),
    )
}

/// The identity holding `device_id`, under whichever key it was linked:
/// its own `{kind}:{device_id}` key, or one kind's single slot as pairing
/// used to record it (including the `desk` alias Desk once paired under).
fn holder_of(store: &dyn IdentityStore, device_id: &str) -> Option<Identity> {
    lookup_kinds()
        .map(|kind| device_link_key(kind, device_id))
        .chain(lookup_kinds().map(str::to_string))
        .find_map(|key| store.get_by_linked_id(&key, device_id).ok().flatten())
}

/// Resolve the identity that owns `device_id`, whatever kind it paired as and
/// however it was recorded.
pub(crate) fn identity_for_device(store: &dyn IdentityStore, device_id: &str) -> Option<String> {
    holder_of(store, device_id).map(|identity| identity.id.to_string())
}

/// The identity holding `device_id`, for listing who each device belongs to.
pub(crate) fn device_owner(store: &dyn IdentityStore, device_id: &str) -> Option<Identity> {
    holder_of(store, device_id)
}

/// Record `device_id` on `identity` under its own key, dropping any old
/// single-slot entry for the same device.
fn record_device(identity: &mut Identity, kind: &str, device_id: &str) {
    identity
        .linked_ids
        .retain(|key, id| !(id == device_id && lookup_kinds().any(|k| k == key)));
    identity.link_id(device_link_key(kind, device_id), device_id);
}

/// Take `device_id` off every identity but `keep`, so it resolves to one
/// person only. A re-pair reuses a device's id; whoever held it before must
/// not go on owning the new token.
fn release_device(
    store: &dyn IdentityStore,
    device_id: &str,
    keep: uuid::Uuid,
) -> eyre::Result<()> {
    // `get_by_linked_id` answers one holder at a time; a device is held by
    // at most a handful, so this ends after a few rounds.
    for _ in 0..16 {
        let Some(mut holder) = holder_of(store, device_id).filter(|h| h.id != keep) else {
            return Ok(());
        };
        holder.linked_ids.retain(|_, id| id != device_id);
        holder.updated_at = chrono::Utc::now();
        store.update(&holder)?;
        info!(identity_id = %holder.id, device_id = device_id, "Released a device from its earlier holder");
    }
    eyre::bail!("device {device_id} is still held after releasing it repeatedly")
}

/// A code as a person typed or pasted it: spaces and dashes dropped,
/// upper-cased (codes are drawn from upper-case letters and digits only).
pub(crate) fn normalise_code(code: &str) -> String {
    code.chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_uppercase()
}

/// The still-pending request behind `code`. An expired code answers
/// [`ApproveError::NotFound`], as an unknown one does.
pub(crate) async fn pending_request(
    pairing: &DevicePairingService,
    code: &str,
) -> Result<DevicePairingRequest, ApproveError> {
    let req = pairing
        .check_status_async(code.to_string())
        .await
        .map_err(|e| ApproveError::Failed(format!("Reading the pairing code failed: {e}")))?
        .ok_or(ApproveError::NotFound)?;
    match req.status {
        PairingStatus::Pending if req.expires_at < chrono::Utc::now() => {
            Err(ApproveError::NotFound)
        }
        PairingStatus::Pending => Ok(req),
        PairingStatus::Expired => Err(ApproveError::NotFound),
        PairingStatus::Approved | PairingStatus::Rejected => Err(ApproveError::AlreadyUsed),
    }
}

/// What an approval did.
pub(crate) struct DeviceApproval {
    pub device: ApprovedDevice,
    /// The device's bearer token. Only the older admin route returns it; every
    /// other path leaves it for the device to collect from the status poll,
    /// and it never goes into an agent's context.
    pub token: String,
    pub device_kind: String,
    /// `(identity_id, how)` where `how` is `id`, `email` or
    /// `auto_provisioned` (from a hint) or `identity` (named by the
    /// approver). `None` means the device is anonymous.
    pub link: Option<(String, &'static str)>,
}

/// Approve the pending device request `code` and connect the device as
/// `choice` says.
///
/// The request is read first, so its kind (and hint) are known whatever
/// approval does with the row, and an unknown, expired or used code is
/// refused before anything changes. Approval itself is one conditional write,
/// so a code is approved once however many approvers race.
///
/// A hint that matches nothing leaves the device paired and anonymous, as it
/// always has. A device approved into a named identity is never left
/// anonymous: if that link can't be saved the device is revoked again and the
/// approval fails, rather than the person being told it is theirs.
pub(crate) async fn approve_and_link(
    pairing: &DevicePairingService,
    identity_store: Option<&dyn IdentityStore>,
    code: &str,
    approved_by: &str,
    choice: LinkChoice,
) -> Result<DeviceApproval, ApproveError> {
    let pending = pending_request(pairing, code).await?;
    let identity_hint = pending.identity_hint.clone();
    let device_kind = pending
        .device_kind
        .clone()
        .unwrap_or_else(|| DEFAULT_DEVICE_KIND.to_string());

    if let LinkChoice::Identity { id, by_admin } = choice {
        let store = identity_store.ok_or_else(|| {
            ApproveError::Failed("No identity store is configured on this hub.".to_string())
        })?;
        connectable(store, id)?;
        if !by_admin {
            refuse_taking_anothers_device(pairing, store, &pending, id).await?;
        }
    }

    let (device, token) = match pairing
        .approve_async(code.to_string(), approved_by.to_string())
        .await
    {
        Ok(approved) => approved,
        // Lost a race, or expired between the read and the write.
        Err(e) => {
            return Err(match pending_request(pairing, code).await {
                Err(why) => why,
                Ok(_) => ApproveError::Failed(format!("Approving the pairing code failed: {e}")),
            })
        }
    };

    let link = match choice {
        LinkChoice::FromHint => {
            let link = identity_hint.as_deref().and_then(|hint| {
                let store = identity_store?;
                match auto_link_device_to_identity(store, hint, &device.id, &device_kind) {
                    Ok(linked) => linked,
                    Err(e) => {
                        warn!(
                            device_id = %device.id,
                            hint = %hint,
                            error = %e,
                            "Auto-link failed during device pairing approval"
                        );
                        None
                    }
                }
            });
            if let (None, Some(hint)) = (&link, identity_hint.as_deref()) {
                warn!(
                    device_id = %device.id,
                    hint = %hint,
                    "Pairing approved but identity hint did not match any identity — device is unlinked"
                );
            }
            link
        }
        LinkChoice::Identity {
            id: identity_id, ..
        } => {
            // Checked above, so a store is present.
            let store = identity_store.expect("identity store checked before approval");
            if let Err(e) = link_to_identity(store, identity_id, &device.id, &device_kind) {
                warn!(
                    device_id = %device.id,
                    identity_id = %identity_id,
                    error = %e,
                    "Connecting an approved device to its person failed — revoking it"
                );
                if let Err(revoke) = pairing.revoke_async(device.id.clone()).await {
                    warn!(device_id = %device.id, error = %revoke, "Revoking the unconnected device failed");
                }
                return Err(ApproveError::Failed(format!(
                    "The device couldn't be connected to the account: {e}"
                )));
            }
            Some((identity_id.to_string(), "identity"))
        }
    };

    Ok(DeviceApproval {
        device,
        token,
        device_kind,
        link,
    })
}

/// A request carrying the `client_id` of a device already approved re-pairs
/// that device: approval revokes it and reissues its id. When the device
/// belongs to someone other than `approver`, only an admin may do that.
async fn refuse_taking_anothers_device(
    pairing: &DevicePairingService,
    store: &dyn IdentityStore,
    pending: &DevicePairingRequest,
    approver: uuid::Uuid,
) -> Result<(), ApproveError> {
    if pending.client_id.is_none() {
        return Ok(());
    }
    let earlier = pairing
        .device_for_request_async(pending)
        .await
        .map_err(|e| ApproveError::Failed(format!("Looking up the device failed: {e}")))?;
    let Some(earlier) = earlier.filter(|d| !d.revoked) else {
        return Ok(());
    };
    match identity_for_device(store, &earlier.id) {
        Some(owner) if owner != approver.to_string() => Err(ApproveError::Identity(
            "This device is already connected to another account. Ask an admin to move it."
                .to_string(),
        )),
        _ => Ok(()),
    }
}

/// Refuse an identity a device can't be connected to.
fn connectable(
    store: &dyn IdentityStore,
    identity_id: uuid::Uuid,
) -> Result<Identity, ApproveError> {
    let identity = store
        .get_by_id(identity_id)
        .map_err(|e| ApproveError::Failed(format!("Looking up the account failed: {e}")))?
        .ok_or_else(|| ApproveError::Identity(format!("No account has id {identity_id}.")))?;
    if identity.status == IdentityStatus::Suspended {
        return Err(ApproveError::Identity(
            "This account is suspended, so no device can be connected to it.".to_string(),
        ));
    }
    Ok(identity)
}

fn link_to_identity(
    store: &dyn IdentityStore,
    identity_id: uuid::Uuid,
    device_id: &str,
    device_kind: &str,
) -> eyre::Result<()> {
    release_device(store, device_id, identity_id)?;
    let mut identity = store
        .get_by_id(identity_id)?
        .ok_or_else(|| eyre::eyre!("identity {identity_id} not found"))?;
    record_device(&mut identity, device_kind, device_id);
    store.update(&identity)?;
    info!(
        identity_id = %identity.id,
        device_id = device_id,
        device_kind = device_kind,
        "Connected an approved device to its person"
    );
    Ok(())
}

/// Resolve `hint` to an identity and link `device_id` to it under
/// `device_kind`. Mirrors the auto-match flow already used by Slack
/// ([`crate::slack_manager::resolve_slack_identity`]) and Keycloak
/// ([`crate::oidc`]).
///
/// Resolution order:
///  1. `hint` parses as a UUID → look up by `Identity.id`.
///  2. `hint` looks like an email → look up by email; if missing,
///     auto-provision a new identity (status=AutoProvisioned) under the
///     provisioning source `IosAuto`. This matches Slack's behaviour:
///     trust the email at the auth boundary, create the row.
///
/// Returns `Ok(Some((identity_id, kind)))` on success where `kind` is
/// `"id"` / `"email"` / `"auto_provisioned"`. `Ok(None)` means the hint
/// did not resolve and no identity was created (e.g. a malformed hint).
pub(crate) fn auto_link_device_to_identity(
    store: &dyn IdentityStore,
    hint: &str,
    device_id: &str,
    device_kind: &str,
) -> eyre::Result<Option<(String, &'static str)>> {
    if let Ok(uuid) = uuid::Uuid::parse_str(hint) {
        let Some(identity) = store.get_by_id(uuid)? else {
            return Ok(None);
        };
        if identity.status == IdentityStatus::Suspended {
            return Ok(None);
        }
        release_device(store, device_id, identity.id)?;
        let mut identity = store.get_by_id(identity.id)?.unwrap_or(identity);
        record_device(&mut identity, device_kind, device_id);
        store.update(&identity)?;
        info!(
            identity_id = %identity.id,
            device_id = device_id,
            "Linked device to identity by UUID"
        );
        return Ok(Some((identity.id.to_string(), "id")));
    }

    if !hint.contains('@') {
        return Ok(None);
    }

    if let Some(existing) = store.get_by_email(hint)? {
        if existing.status == IdentityStatus::Suspended {
            return Ok(None);
        }
        release_device(store, device_id, existing.id)?;
        let mut existing = store.get_by_id(existing.id)?.unwrap_or(existing);
        record_device(&mut existing, device_kind, device_id);
        store.update(&existing)?;
        info!(
            identity_id = %existing.id,
            email = hint,
            device_id = device_id,
            "Linked device to existing identity by email"
        );
        return Ok(Some((existing.id.to_string(), "email")));
    }

    let identity = Identity::new(hint, IdentityRole::Member)
        .with_email(hint)
        .with_status(IdentityStatus::AutoProvisioned)
        .with_provisioning_source(ProvisioningSource::IosAuto)
        .with_linked_id(device_link_key(device_kind, device_id), device_id);
    release_device(store, device_id, identity.id)?;
    let id_str = identity.id.to_string();
    store.create(&identity)?;
    info!(
        identity_id = %id_str,
        email = hint,
        device_id = device_id,
        "Auto-provisioned identity for device pairing by email"
    );
    Ok(Some((id_str, "auto_provisioned")))
}

/// Resolve an identity an admin named (a UUID or an email) to an existing,
/// non-suspended identity. Unlike a pairing hint this never provisions: a
/// typo must not create an account.
pub(crate) fn resolve_named_identity(
    store: &dyn IdentityStore,
    named: &str,
) -> Result<Identity, String> {
    let named = named.trim();
    let found = if let Ok(uuid) = uuid::Uuid::parse_str(named) {
        store.get_by_id(uuid)
    } else if named.contains('@') {
        store.get_by_email(&named.to_ascii_lowercase())
    } else {
        return Err(format!(
            "'{named}' is neither an identity UUID nor an email address"
        ));
    };
    let identity = found
        .map_err(|e| format!("looking up '{named}' failed: {e}"))?
        .ok_or_else(|| format!("no identity matches '{named}'"))?;
    if identity.status == IdentityStatus::Suspended {
        return Err(format!(
            "identity {} is suspended; a device cannot be connected to it",
            identity.id
        ));
    }
    Ok(identity)
}

/// The account a device was connected to, as a person or a device is shown
/// it: no role, no linked ids.
pub(crate) fn identity_summary(identity: &Identity) -> serde_json::Value {
    serde_json::json!({
        "id": identity.id.to_string(),
        "display_name": identity.display_name,
        "email": identity.email,
    })
}

/// Record an approval on the access audit trail and the `audit.device_pair`
/// log target: who approved, how, which device, into whose account.
pub(crate) fn audit_device_pair(via: &str, approver: Option<&str>, approval: &DeviceApproval) {
    let owner = approval.link.as_ref().map(|(id, _)| id.as_str());
    let resource = format!("device:{}", approval.device.id);
    workforce_memory::access_audit::emit(workforce_memory::access_audit::AccessAuditEvent::new(
        workforce_memory::access_audit::AuditOperation::DevicePair,
        workforce_memory::access_audit::AuditDecision::Allow,
        approver,
        owner,
        Some(&resource),
    ));
    info!(
        target: "audit.device_pair",
        via = via,
        approver = approver.unwrap_or("gateway-token"),
        device_id = %approval.device.id,
        device_name = %approval.device.device_name,
        device_kind = %approval.device_kind,
        identity_id = owner.unwrap_or("-"),
        "Device pairing approved"
    );
}

/// What [`link_existing_device`] did.
#[derive(Debug)]
pub(crate) struct ExistingLink {
    pub identity: Identity,
    pub device_kind: String,
    /// The device was already linked to this person.
    pub already_linked: bool,
}

/// Link an already-approved device to an existing identity under
/// `device_kind`. Refuses an unknown or revoked device, a kind nothing looks
/// up, and a device already linked to someone else — moving a device off a
/// person is a deliberate act, not a side effect of linking it elsewhere.
pub(crate) async fn link_existing_device(
    pairing: &DevicePairingService,
    store: &dyn IdentityStore,
    device_id: &str,
    identity: &str,
    device_kind: &str,
) -> Result<ExistingLink, String> {
    let kind = workforce_pairing::types::normalise_device_kind(device_kind)
        .filter(|k| DEVICE_KINDS.contains(&k.as_str()))
        .ok_or_else(|| {
            format!(
                "device_kind '{device_kind}' is not one of {}",
                DEVICE_KINDS.join(", ")
            )
        })?;

    let devices = pairing
        .list_devices_async()
        .await
        .map_err(|e| format!("listing devices failed: {e}"))?;
    let device = devices
        .iter()
        .find(|d| d.id == device_id)
        .ok_or_else(|| format!("no approved device has id '{device_id}'"))?;
    if device.revoked {
        return Err(format!("device '{device_id}' is revoked"));
    }

    let mut target = resolve_named_identity(store, identity)?;
    let target_id = target.id.to_string();

    if let Some(owner) = identity_for_device(store, device_id) {
        if owner != target_id {
            return Err(format!(
                "device '{device_id}' is already linked to identity {owner}; \
                 unlink it there before linking it to {target_id}"
            ));
        }
        if target
            .linked_ids
            .get(&device_link_key(&kind, device_id))
            .map(String::as_str)
            == Some(device_id)
        {
            return Ok(ExistingLink {
                identity: target,
                device_kind: kind,
                already_linked: true,
            });
        }
    }

    record_device(&mut target, &kind, device_id);
    store
        .update(&target)
        .map_err(|e| format!("saving the link failed: {e}"))?;
    info!(
        identity_id = %target_id,
        device_id = device_id,
        device_kind = %kind,
        "Linked an approved device to an identity"
    );
    Ok(ExistingLink {
        identity: target,
        device_kind: kind,
        already_linked: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use workforce_identity::sqlite::SqliteIdentityStore;
    use workforce_pairing::{PairingConfig, SqlitePairingStore};

    fn pairing_with_ttl(code_ttl_secs: u64) -> DevicePairingService {
        DevicePairingService::new(
            Arc::new(SqlitePairingStore::in_memory().unwrap()),
            PairingConfig {
                device_pairing_enabled: true,
                code_ttl_secs,
                ..Default::default()
            },
        )
    }

    fn pairing() -> DevicePairingService {
        pairing_with_ttl(900)
    }

    async fn request(
        pairing: &DevicePairingService,
        hint: Option<&str>,
        kind: Option<&str>,
        client_id: Option<&str>,
    ) -> String {
        pairing
            .initiate_async(
                "Test device".to_string(),
                vec!["chat".to_string()],
                None,
                client_id.map(str::to_string),
                hint.map(str::to_string),
                kind.map(str::to_string),
            )
            .await
            .unwrap()
            .code
    }

    async fn pending(
        pairing: &DevicePairingService,
        hint: Option<&str>,
        kind: Option<&str>,
    ) -> String {
        request(pairing, hint, kind, None).await
    }

    fn person(store: &SqliteIdentityStore, email: &str) -> Identity {
        let identity = Identity::new(email, IdentityRole::Member).with_email(email);
        store.create(&identity).unwrap();
        identity
    }

    fn signed_in(id: uuid::Uuid) -> LinkChoice {
        LinkChoice::Identity {
            id,
            by_admin: false,
        }
    }

    fn holds(store: &SqliteIdentityStore, who: &Identity, kind: &str, device: &str) -> bool {
        store
            .get_by_id(who.id)
            .unwrap()
            .unwrap()
            .linked_ids
            .get(&device_link_key(kind, device))
            == Some(&device.to_string())
    }

    #[tokio::test]
    async fn a_hinted_email_links_its_person() {
        let (pairing, store) = (pairing(), SqliteIdentityStore::in_memory().unwrap());
        let kings = person(&store, "kings@example.com");
        let code = pending(&pairing, Some("kings@example.com"), Some("desktop_device")).await;

        let approval =
            approve_and_link(&pairing, Some(&store), &code, "admin", LinkChoice::FromHint)
                .await
                .unwrap();

        assert_eq!(approval.link, Some((kings.id.to_string(), "email")));
        assert!(holds(&store, &kings, "desktop_device", &approval.device.id));
        assert_eq!(
            identity_for_device(&store, &approval.device.id),
            Some(kings.id.to_string())
        );
    }

    #[tokio::test]
    async fn a_hinted_uuid_links_its_person() {
        let (pairing, store) = (pairing(), SqliteIdentityStore::in_memory().unwrap());
        let kings = person(&store, "kings@example.com");
        let code = pending(&pairing, Some(&kings.id.to_string()), None).await;

        let approval =
            approve_and_link(&pairing, Some(&store), &code, "admin", LinkChoice::FromHint)
                .await
                .unwrap();

        assert_eq!(approval.link, Some((kings.id.to_string(), "id")));
        assert_eq!(approval.device_kind, DEFAULT_DEVICE_KIND);
        assert!(holds(
            &store,
            &kings,
            DEFAULT_DEVICE_KIND,
            &approval.device.id
        ));
    }

    #[tokio::test]
    async fn an_unknown_hinted_email_provisions_its_person() {
        let (pairing, store) = (pairing(), SqliteIdentityStore::in_memory().unwrap());
        let code = pending(&pairing, Some("new@example.com"), None).await;

        let approval =
            approve_and_link(&pairing, Some(&store), &code, "admin", LinkChoice::FromHint)
                .await
                .unwrap();

        let (id, how) = approval.link.unwrap();
        assert_eq!(how, "auto_provisioned");
        let created = store.get_by_email("new@example.com").unwrap().unwrap();
        assert_eq!(created.id.to_string(), id);
        assert_eq!(created.status, IdentityStatus::AutoProvisioned);
        assert!(holds(
            &store,
            &created,
            DEFAULT_DEVICE_KIND,
            &approval.device.id
        ));
    }

    #[tokio::test]
    async fn no_hint_links_nobody() {
        let (pairing, store) = (pairing(), SqliteIdentityStore::in_memory().unwrap());
        person(&store, "kings@example.com");
        let code = pending(&pairing, None, None).await;

        let approval =
            approve_and_link(&pairing, Some(&store), &code, "admin", LinkChoice::FromHint)
                .await
                .unwrap();

        assert!(approval.link.is_none());
        assert!(identity_for_device(&store, &approval.device.id).is_none());
    }

    #[tokio::test]
    async fn the_signed_in_person_wins_over_the_hint() {
        let (pairing, store) = (pairing(), SqliteIdentityStore::in_memory().unwrap());
        let kings = person(&store, "kings@example.com");
        let other = person(&store, "other@example.com");
        let code = pending(&pairing, Some("other@example.com"), Some("desktop_device")).await;

        let approval = approve_and_link(&pairing, Some(&store), &code, "self", signed_in(kings.id))
            .await
            .unwrap();

        assert_eq!(approval.link, Some((kings.id.to_string(), "identity")));
        assert!(holds(&store, &kings, "desktop_device", &approval.device.id));
        assert!(store
            .get_by_id(other.id)
            .unwrap()
            .unwrap()
            .linked_ids
            .is_empty());
    }

    #[tokio::test]
    async fn a_person_holds_several_devices_of_one_kind() {
        let (pairing, store) = (pairing(), SqliteIdentityStore::in_memory().unwrap());
        let kings = person(&store, "kings@example.com");
        let mut devices = Vec::new();
        for _ in 0..2 {
            let code = pending(&pairing, None, Some("desktop_device")).await;
            let approval =
                approve_and_link(&pairing, Some(&store), &code, "self", signed_in(kings.id))
                    .await
                    .unwrap();
            devices.push(approval.device.id);
        }

        for device in &devices {
            assert_eq!(
                identity_for_device(&store, device),
                Some(kings.id.to_string()),
                "every Mac stays theirs"
            );
        }
    }

    #[tokio::test]
    async fn desk_pairs_as_a_desktop_device_and_old_desk_links_resolve() {
        assert_eq!(
            workforce_pairing::types::normalise_device_kind("desk").as_deref(),
            Some("desktop_device")
        );
        let store = SqliteIdentityStore::in_memory().unwrap();
        let kings = Identity::new("Kings", IdentityRole::Member).with_linked_id("desk", "old-desk");
        store.create(&kings).unwrap();

        assert_eq!(
            identity_for_device(&store, "old-desk"),
            Some(kings.id.to_string())
        );
    }

    #[tokio::test]
    async fn a_member_cannot_take_over_anothers_device_by_repairing_it() {
        let (pairing, store) = (pairing(), SqliteIdentityStore::in_memory().unwrap());
        let owner = person(&store, "owner@example.com");
        let thief = person(&store, "thief@example.com");
        let first = request(&pairing, None, Some("desktop_device"), Some("client-1")).await;
        let device = approve_and_link(&pairing, Some(&store), &first, "self", signed_in(owner.id))
            .await
            .unwrap()
            .device;

        let again = request(&pairing, None, Some("desktop_device"), Some("client-1")).await;
        let refused = approve_and_link(&pairing, Some(&store), &again, "self", signed_in(thief.id))
            .await
            .err();

        assert!(matches!(refused, Some(ApproveError::Identity(_))));
        assert_eq!(
            identity_for_device(&store, &device.id),
            Some(owner.id.to_string())
        );
        assert!(
            !pairing.list_devices_async().await.unwrap()[0].revoked,
            "the owner's device keeps working"
        );
    }

    #[tokio::test]
    async fn an_admin_moves_a_repaired_device_and_its_old_holder_lets_go() {
        let (pairing, store) = (pairing(), SqliteIdentityStore::in_memory().unwrap());
        let before = person(&store, "before@example.com");
        let after = person(&store, "after@example.com");
        let first = request(&pairing, None, None, Some("client-1")).await;
        let device = approve_and_link(&pairing, Some(&store), &first, "self", signed_in(before.id))
            .await
            .unwrap()
            .device;

        let again = request(&pairing, None, None, Some("client-1")).await;
        let moved = approve_and_link(
            &pairing,
            Some(&store),
            &again,
            "admin",
            LinkChoice::Identity {
                id: after.id,
                by_admin: true,
            },
        )
        .await
        .unwrap();

        assert_eq!(moved.device.id, device.id, "a re-pair keeps the device id");
        assert_eq!(
            identity_for_device(&store, &device.id),
            Some(after.id.to_string())
        );
        assert!(store
            .get_by_id(before.id)
            .unwrap()
            .unwrap()
            .linked_ids
            .is_empty());
    }

    #[tokio::test]
    async fn a_person_repairs_their_own_device() {
        let (pairing, store) = (pairing(), SqliteIdentityStore::in_memory().unwrap());
        let kings = person(&store, "kings@example.com");
        for _ in 0..2 {
            let code = request(&pairing, None, None, Some("client-1")).await;
            approve_and_link(&pairing, Some(&store), &code, "self", signed_in(kings.id))
                .await
                .unwrap();
        }
        assert_eq!(
            store.get_by_id(kings.id).unwrap().unwrap().linked_ids.len(),
            1
        );
    }

    #[tokio::test]
    async fn a_code_is_approved_once() {
        let (pairing, store) = (pairing(), SqliteIdentityStore::in_memory().unwrap());
        let kings = person(&store, "kings@example.com");
        let other = person(&store, "other@example.com");
        let code = pending(&pairing, None, None).await;

        approve_and_link(&pairing, Some(&store), &code, "a", signed_in(kings.id))
            .await
            .unwrap();
        let again = approve_and_link(&pairing, Some(&store), &code, "b", signed_in(other.id))
            .await
            .err();

        assert_eq!(again, Some(ApproveError::AlreadyUsed));
        assert!(store
            .get_by_id(other.id)
            .unwrap()
            .unwrap()
            .linked_ids
            .is_empty());
    }

    #[tokio::test]
    async fn an_expired_or_unknown_code_is_not_found() {
        let (pairing, store) = (
            pairing_with_ttl(0),
            SqliteIdentityStore::in_memory().unwrap(),
        );
        let kings = person(&store, "kings@example.com");
        let code = pending(&pairing, None, None).await;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        for code in [code.as_str(), "NOSUCHCODE01"] {
            let refused = approve_and_link(&pairing, Some(&store), code, "a", signed_in(kings.id))
                .await
                .err();
            assert_eq!(refused, Some(ApproveError::NotFound), "{code}");
        }
        assert!(store
            .get_by_id(kings.id)
            .unwrap()
            .unwrap()
            .linked_ids
            .is_empty());
    }

    #[tokio::test]
    async fn a_suspended_identity_takes_no_device() {
        let (pairing, store) = (pairing(), SqliteIdentityStore::in_memory().unwrap());
        let mut kings = person(&store, "kings@example.com");
        kings.status = IdentityStatus::Suspended;
        store.update(&kings).unwrap();
        let code = pending(&pairing, None, None).await;

        let refused = approve_and_link(&pairing, Some(&store), &code, "a", signed_in(kings.id))
            .await
            .err();

        assert!(matches!(refused, Some(ApproveError::Identity(_))));
        assert!(
            pending_request(&pairing, &code).await.is_ok(),
            "the code stays pending"
        );
    }

    #[test]
    fn a_typed_code_is_normalised() {
        assert_eq!(normalise_code(" abcd-efgh 2345 "), "ABCDEFGH2345");
    }

    #[tokio::test]
    async fn an_existing_device_links_to_a_named_person() {
        let (pairing, store) = (pairing(), SqliteIdentityStore::in_memory().unwrap());
        let kings = person(&store, "kings@example.com");
        let code = pending(&pairing, None, Some("desktop_device")).await;
        let device = approve_and_link(&pairing, Some(&store), &code, "admin", LinkChoice::FromHint)
            .await
            .unwrap()
            .device;

        let linked =
            link_existing_device(&pairing, &store, &device.id, &kings.id.to_string(), "desk")
                .await
                .unwrap();
        assert_eq!(linked.identity.id, kings.id);
        assert_eq!(linked.device_kind, "desktop_device");
        assert!(!linked.already_linked);
        assert_eq!(
            identity_for_device(&store, &device.id),
            Some(kings.id.to_string())
        );

        let again = link_existing_device(
            &pairing,
            &store,
            &device.id,
            "KINGS@example.com",
            "desktop_device",
        )
        .await
        .unwrap();
        assert!(again.already_linked, "linking twice is a no-op");
    }

    #[tokio::test]
    async fn an_existing_device_is_not_taken_from_its_person() {
        let (pairing, store) = (pairing(), SqliteIdentityStore::in_memory().unwrap());
        person(&store, "kings@example.com");
        let other = person(&store, "other@example.com");
        let code = pending(&pairing, Some("kings@example.com"), None).await;
        let device = approve_and_link(&pairing, Some(&store), &code, "admin", LinkChoice::FromHint)
            .await
            .unwrap()
            .device;

        let refused = link_existing_device(
            &pairing,
            &store,
            &device.id,
            &other.id.to_string(),
            DEFAULT_DEVICE_KIND,
        )
        .await
        .unwrap_err();
        assert!(refused.contains("already linked"), "{refused}");
    }

    #[tokio::test]
    async fn an_existing_device_link_refuses_bad_input() {
        let (pairing, store) = (pairing(), SqliteIdentityStore::in_memory().unwrap());
        let kings = person(&store, "kings@example.com");
        let code = pending(&pairing, None, None).await;
        let device = approve_and_link(&pairing, Some(&store), &code, "admin", LinkChoice::FromHint)
            .await
            .unwrap()
            .device;
        let kings_id = kings.id.to_string();

        for (device_id, identity, kind, says) in [
            (
                device.id.as_str(),
                kings_id.as_str(),
                "laptop",
                "not one of",
            ),
            (
                "no-such-device",
                kings_id.as_str(),
                "desktop_device",
                "no approved device",
            ),
            (
                device.id.as_str(),
                "nobody@example.com",
                "desktop_device",
                "no identity matches",
            ),
            (device.id.as_str(), "kings", "desktop_device", "neither"),
        ] {
            let refused = link_existing_device(&pairing, &store, device_id, identity, kind)
                .await
                .unwrap_err();
            assert!(refused.contains(says), "{refused}");
        }
        assert!(
            store.get_by_email("nobody@example.com").unwrap().is_none(),
            "naming an unknown email never provisions"
        );
    }
}
