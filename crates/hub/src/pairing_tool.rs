//! Device pairing management tools — let agents list, approve, reject pending pairing
//! requests and manage paired devices.
//!
//! Tools:
//! - `pairing.pending`  — list pending device pairing requests
//! - `pairing.approve`  — approve a code into the account of the person asking
//!   (an admin may name someone else with `for`)
//! - `pairing.reject`   — reject a pending pairing request by code
//! - `pairing.devices`  — list all paired (approved) devices and whose they are
//! - `pairing.revoke`   — revoke an approved device's access
//! - `pairing.link`     — connect an already-approved device to a person (admin)

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;
use tracing::info;

use workforce_agent::tools::{Tool, ToolResult};
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityStatus};
use workforce_pairing::types::WhoCanConnect;
use workforce_pairing::DevicePairingService;

use crate::authz::IsAdminFn;
use crate::device_link::{self, LinkChoice};

/// The identity store, filled in once the hub has built it (after the agents
/// and their tools); the same handle the admin policy reads.
pub type IdentityStoreHandle = Arc<parking_lot::RwLock<Option<Arc<dyn IdentityStore>>>>;

// ─── Pairing Tool Context ────────────────────────────────────────────────────

/// Shared context for all pairing tools.
#[derive(Clone)]
pub struct PairingToolContext {
    pub device_pairing: Arc<DevicePairingService>,
    pub identity_store: IdentityStoreHandle,
    /// Whether the person this turn runs for is an active System Admin.
    pub is_admin: IsAdminFn,
}

impl PairingToolContext {
    fn identities(&self) -> Option<Arc<dyn IdentityStore>> {
        self.identity_store.read().clone()
    }
}

/// Create all device pairing management tools.
pub fn pairing_tools(ctx: PairingToolContext) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(PairingPendingTool::new(ctx.clone())),
        Arc::new(PairingApproveTool::new(ctx.clone())),
        Arc::new(PairingRejectTool::new(ctx.clone())),
        Arc::new(PairingDevicesTool::new(ctx.clone())),
        Arc::new(PairingRevokeTool::new(ctx.clone())),
        Arc::new(PairingLinkTool::new(ctx)),
    ]
}

// ─── pairing.pending ─────────────────────────────────────────────────────────

pub struct PairingPendingTool {
    ctx: PairingToolContext,
}

impl PairingPendingTool {
    pub fn new(ctx: PairingToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for PairingPendingTool {
    fn name(&self) -> &str {
        "pairing.pending"
    }

    fn description(&self) -> &str {
        "List all pending device pairing requests"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        ,
            "required": []})
    }

    async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();

        match self.ctx.device_pairing.list_pending_async().await {
            Ok(requests) => {
                let items: Vec<serde_json::Value> = requests
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "code": r.code,
                            "device_name": r.device_name,
                            "device_kind": r.device_kind,
                            // What the device says about itself; it proves nothing.
                            "claims_to_be": r.identity_hint,
                            "status": format!("{:?}", r.status),
                            "created_at": r.created_at.to_rfc3339(),
                            "expires_at": r.expires_at.to_rfc3339(),
                        })
                    })
                    .collect();

                Ok(ToolResult::success(
                    serde_json::json!({ "pending": items, "count": items.len() }),
                    start.elapsed().as_millis() as u64,
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to list pending pairing requests: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

// ─── pairing.approve ─────────────────────────────────────────────────────────

pub struct PairingApproveTool {
    ctx: PairingToolContext,
}

impl PairingApproveTool {
    pub fn new(ctx: PairingToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct ApproveInput {
    code: String,
    #[serde(default, rename = "for")]
    for_identity: Option<String>,
}

#[async_trait]
impl Tool for PairingApproveTool {
    fn name(&self) -> &str {
        "pairing.approve"
    }

    fn description(&self) -> &str {
        "Approve a device's pairing code, connecting the device to the account of the person \
         you are talking to. Only an admin may connect it to someone else's account, with `for`."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "The pairing code the device shows"
                },
                "for": {
                    "type": "string",
                    "description": "Admins only: the identity UUID or email of the person to connect the device to, instead of the person asking"
                }
            },
            "required": ["code"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: ApproveInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;
        let fail = |msg: String| Ok(ToolResult::failure(msg, duration()));

        let Some(store) = self.ctx.identities() else {
            return fail("This hub has no accounts to connect a device to.".to_string());
        };
        let caller = match asking_person(store.as_ref()) {
            Ok(caller) => caller,
            Err(why) => return fail(why),
        };
        let is_admin = (self.ctx.is_admin)();
        if self.ctx.device_pairing.config().who_can_connect == WhoCanConnect::Admins && !is_admin {
            return fail(crate::api_handlers::device_pairing::ADMINS_ONLY.to_string());
        }
        let (target, via) = match params.for_identity.as_deref().map(str::trim) {
            Some(named) if !named.is_empty() => {
                if !is_admin {
                    return fail(
                        "Only an admin can connect a device to someone else's account. \
                         Without `for`, the device is connected to the account of the person asking."
                            .to_string(),
                    );
                }
                match device_link::resolve_named_identity(store.as_ref(), named) {
                    Ok(identity) => (identity, "for"),
                    Err(why) => return fail(why),
                }
            }
            _ => (caller.clone(), "caller"),
        };

        let code = device_link::normalise_code(&params.code);
        let caller_id = caller.id.to_string();
        let approval = match device_link::approve_and_link(
            &self.ctx.device_pairing,
            Some(store.as_ref()),
            &code,
            &format!("agent:{caller_id}"),
            LinkChoice::Identity {
                id: target.id,
                by_admin: via == "for",
            },
        )
        .await
        {
            Ok(approval) => approval,
            Err(why) => return fail(why.message()),
        };
        device_link::audit_device_pair("agent", Some(&caller_id), &approval);
        info!(device = %approval.device.device_name, identity_id = %target.id, "Pairing approved via pairing.approve tool");

        let account = match &target.email {
            Some(email) => format!("{} ({email})", target.display_name),
            None => target.display_name.clone(),
        };
        Ok(ToolResult::success(
            serde_json::json!({
                "approved": true,
                "code": code,
                "device_name": approval.device.device_name,
                "device_id": approval.device.id,
                "linked_identity": device_link::identity_summary(&target),
                "linked_via": via,
                "message": format!(
                    "Connected {} to {account}'s account. It will finish signing in on its own.",
                    approval.device.device_name
                ),
            }),
            duration(),
        ))
    }
}

/// The person this turn runs for, as an account a device can be connected to.
fn asking_person(store: &dyn IdentityStore) -> Result<Identity, String> {
    let not_linked = "I can only connect a device to the account of the person asking, and this \
                      conversation isn't linked to a workforce account. Sign in on the hub's \
                      /pair page with the device's code instead.";
    let id = crate::user_config::current_identity_id()
        .and_then(|id| uuid::Uuid::parse_str(&id).ok())
        .ok_or_else(|| not_linked.to_string())?;
    let identity = store
        .get_by_id(id)
        .map_err(|e| format!("Looking up your account failed: {e}"))?
        .ok_or_else(|| not_linked.to_string())?;
    if identity.status == IdentityStatus::Suspended {
        return Err("Your account is suspended, so it can't connect a device.".to_string());
    }
    Ok(identity)
}

// ─── pairing.reject ──────────────────────────────────────────────────────────

pub struct PairingRejectTool {
    ctx: PairingToolContext,
}

impl PairingRejectTool {
    pub fn new(ctx: PairingToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct RejectInput {
    code: String,
}

#[async_trait]
impl Tool for PairingRejectTool {
    fn name(&self) -> &str {
        "pairing.reject"
    }

    fn description(&self) -> &str {
        "Reject a pending device pairing request by its 8-character code"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "The 8-character pairing code to reject"
                }
            },
            "required": ["code"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: RejectInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        match self
            .ctx
            .device_pairing
            .reject_async(params.code.clone())
            .await
        {
            Ok(()) => {
                info!(code = %params.code, "Pairing rejected via pairing.reject tool");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "rejected": true,
                        "code": params.code,
                    }),
                    duration(),
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to reject pairing code '{}': {}", params.code, e),
                duration(),
            )),
        }
    }
}

// ─── pairing.devices ─────────────────────────────────────────────────────────

pub struct PairingDevicesTool {
    ctx: PairingToolContext,
}

impl PairingDevicesTool {
    pub fn new(ctx: PairingToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for PairingDevicesTool {
    fn name(&self) -> &str {
        "pairing.devices"
    }

    fn description(&self) -> &str {
        "List all approved (paired) devices"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        ,
            "required": []})
    }

    async fn execute(&self, _input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();

        let identities = self.ctx.identities();
        match self.ctx.device_pairing.list_devices_async().await {
            Ok(devices) => {
                let items: Vec<serde_json::Value> = devices
                    .iter()
                    .map(|d| {
                        let owner = identities
                            .as_deref()
                            .and_then(|store| device_link::device_owner(store, &d.id));
                        serde_json::json!({
                            "id": d.id,
                            "owner": owner.as_ref().map(device_link::identity_summary),
                            "device_name": d.device_name,
                            "scopes": d.scopes,
                            "created_at": d.created_at.to_rfc3339(),
                            "last_used": d.last_used.map(|t| t.to_rfc3339()),
                            "revoked": d.revoked,
                        })
                    })
                    .collect();

                Ok(ToolResult::success(
                    serde_json::json!({ "devices": items, "count": items.len() }),
                    start.elapsed().as_millis() as u64,
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to list devices: {}", e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

// ─── pairing.revoke ──────────────────────────────────────────────────────────

pub struct PairingRevokeTool {
    ctx: PairingToolContext,
}

impl PairingRevokeTool {
    pub fn new(ctx: PairingToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct RevokeInput {
    device_id: String,
}

#[async_trait]
impl Tool for PairingRevokeTool {
    fn name(&self) -> &str {
        "pairing.revoke"
    }

    fn description(&self) -> &str {
        "Revoke a device pairing"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "device_id": {
                    "type": "string",
                    "description": "The device ID to revoke (from pairing.devices)"
                }
            },
            "required": ["device_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: RevokeInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        match self
            .ctx
            .device_pairing
            .revoke_async(params.device_id.clone())
            .await
        {
            Ok(()) => {
                info!(device_id = %params.device_id, "Device revoked via pairing.revoke tool");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "revoked": true,
                        "device_id": params.device_id,
                    }),
                    duration(),
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to revoke device '{}': {}", params.device_id, e),
                duration(),
            )),
        }
    }
}

// ─── pairing.link ────────────────────────────────────────────────────────────

pub struct PairingLinkTool {
    ctx: PairingToolContext,
}

impl PairingLinkTool {
    pub fn new(ctx: PairingToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct LinkInput {
    device_id: String,
    identity: String,
    device_kind: String,
}

#[async_trait]
impl Tool for PairingLinkTool {
    fn name(&self) -> &str {
        "pairing.link"
    }

    fn description(&self) -> &str {
        "Admins only: connect an already-approved device (from pairing.devices) to a person's \
         account, for a device that was approved without one and acts anonymously."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "device_id": {
                    "type": "string",
                    "description": "The device id from pairing.devices"
                },
                "identity": {
                    "type": "string",
                    "description": "The person's identity UUID or email"
                },
                "device_kind": {
                    "type": "string",
                    "enum": workforce_pairing::types::DEVICE_KINDS,
                    "description": "ios_device for an iPhone or iPad, desktop_device for Desk, voice_device for a voice box"
                }
            },
            "required": ["device_id", "identity", "device_kind"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let params: LinkInput = serde_json::from_value(input)?;
        let duration = || start.elapsed().as_millis() as u64;

        if !(self.ctx.is_admin)() {
            return Ok(ToolResult::failure(
                "Only an admin can connect an existing device to an account.".to_string(),
                duration(),
            ));
        }
        let Some(store) = self.ctx.identities() else {
            return Ok(ToolResult::failure(
                "This hub has no accounts to connect a device to.".to_string(),
                duration(),
            ));
        };
        match device_link::link_existing_device(
            &self.ctx.device_pairing,
            store.as_ref(),
            &params.device_id,
            &params.identity,
            &params.device_kind,
        )
        .await
        {
            Ok(linked) => {
                let actor = crate::user_config::current_identity_id();
                let resource = format!("device:{}", params.device_id);
                let owner = linked.identity.id.to_string();
                workforce_memory::access_audit::emit(
                    workforce_memory::access_audit::AccessAuditEvent::new(
                        workforce_memory::access_audit::AuditOperation::DevicePair,
                        workforce_memory::access_audit::AuditDecision::Allow,
                        actor.as_deref(),
                        Some(&owner),
                        Some(&resource),
                    ),
                );
                info!(
                    target: "audit.device_pair",
                    via = "agent_link",
                    approver = actor.as_deref().unwrap_or("-"),
                    device_id = %params.device_id,
                    identity_id = %owner,
                    "Existing device connected to an account"
                );
                Ok(ToolResult::success(
                    serde_json::json!({
                        "linked": true,
                        "already_linked": linked.already_linked,
                        "device_id": params.device_id,
                        "device_kind": linked.device_kind,
                        "linked_identity": device_link::identity_summary(&linked.identity),
                    }),
                    duration(),
                ))
            }
            Err(why) => Ok(ToolResult::failure(why, duration())),
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_identity::sqlite::SqliteIdentityStore;
    use workforce_identity::types::IdentityRole;
    use workforce_pairing::{store::SqlitePairingStore, PairingConfig};

    struct Fixture {
        ctx: PairingToolContext,
        store: Arc<SqliteIdentityStore>,
    }

    fn fixture_with(admin: bool, who_can_connect: WhoCanConnect) -> Fixture {
        let pairing_store = Arc::new(SqlitePairingStore::in_memory().unwrap());
        let config = PairingConfig {
            device_pairing_enabled: true,
            who_can_connect,
            ..Default::default()
        };
        let store = Arc::new(SqliteIdentityStore::in_memory().unwrap());
        let handle: IdentityStoreHandle = Arc::new(parking_lot::RwLock::new(Some(
            Arc::clone(&store) as Arc<dyn IdentityStore>,
        )));
        Fixture {
            ctx: PairingToolContext {
                device_pairing: Arc::new(DevicePairingService::new(pairing_store, config)),
                identity_store: handle,
                is_admin: if admin {
                    crate::authz::always_admin()
                } else {
                    crate::authz::never_admin()
                },
            },
            store,
        }
    }

    fn fixture(admin: bool) -> Fixture {
        fixture_with(admin, WhoCanConnect::Members)
    }

    fn person(f: &Fixture, email: &str) -> Identity {
        let identity = Identity::new(email, IdentityRole::Member).with_email(email);
        f.store.create(&identity).unwrap();
        identity
    }

    fn code(f: &Fixture, hint: Option<&str>) -> String {
        f.ctx
            .device_pairing
            .initiate(
                "Kings's Mac",
                vec!["chat".into()],
                None,
                None,
                hint.map(str::to_string),
                Some("desktop_device".into()),
            )
            .unwrap()
            .code
    }

    async fn as_person(
        who: Option<&Identity>,
        tool: &dyn Tool,
        input: serde_json::Value,
    ) -> ToolResult {
        match who {
            Some(who) => {
                workforce_agent::runner::CURRENT_USER_ID
                    .scope(who.id.to_string(), tool.execute(input))
                    .await
            }
            None => tool.execute(input).await,
        }
        .unwrap()
    }

    fn owner(f: &Fixture, device_id: &serde_json::Value) -> Option<String> {
        device_link::identity_for_device(f.store.as_ref(), device_id.as_str().unwrap())
    }

    #[tokio::test]
    async fn pending_empty_returns_empty() {
        let f = fixture(true);
        let tool = PairingPendingTool::new(f.ctx);
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output["count"], 0);
        assert!(result.output["pending"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pending_shows_what_the_device_claims() {
        let f = fixture(true);
        code(&f, Some("kings@example.com"));
        let result = PairingPendingTool::new(f.ctx)
            .execute(serde_json::json!({}))
            .await
            .unwrap();
        let pending = &result.output["pending"][0];
        assert_eq!(pending["claims_to_be"], "kings@example.com");
        assert_eq!(pending["device_kind"], "desktop_device");
    }

    #[tokio::test]
    async fn approve_connects_the_device_to_the_person_asking() {
        let f = fixture(false);
        let kings = person(&f, "kings@example.com");
        let other = person(&f, "other@example.com");
        // The device claims to be someone else; the person asking gets it.
        let code = code(&f, Some("other@example.com"));

        let result = as_person(
            Some(&kings),
            &PairingApproveTool::new(f.ctx.clone()),
            serde_json::json!({ "code": code.to_lowercase() }),
        )
        .await;

        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.output["linked_via"], "caller");
        assert_eq!(
            result.output["linked_identity"]["email"],
            "kings@example.com"
        );
        assert!(result.output["message"]
            .as_str()
            .unwrap()
            .contains("kings@example.com"));
        assert!(
            result.output.get("token").is_none(),
            "the token stays out of chat"
        );
        assert_eq!(
            owner(&f, &result.output["device_id"]),
            Some(kings.id.to_string())
        );
        assert!(f
            .store
            .get_by_id(other.id)
            .unwrap()
            .unwrap()
            .linked_ids
            .is_empty());

        let devices = PairingDevicesTool::new(f.ctx)
            .execute(serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            devices.output["devices"][0]["owner"]["email"],
            "kings@example.com"
        );
    }

    #[tokio::test]
    async fn approve_needs_a_person() {
        let f = fixture(true);
        let code = code(&f, Some("kings@example.com"));
        let result = as_person(
            None,
            &PairingApproveTool::new(f.ctx.clone()),
            serde_json::json!({ "code": code }),
        )
        .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("isn't linked"));
        assert!(
            f.ctx.device_pairing.list_devices().unwrap().is_empty(),
            "nothing was approved"
        );
    }

    #[tokio::test]
    async fn only_an_admin_approves_for_someone_else() {
        let member = fixture(false);
        let kings = person(&member, "kings@example.com");
        person(&member, "other@example.com");
        let code_m = code(&member, None);
        let refused = as_person(
            Some(&kings),
            &PairingApproveTool::new(member.ctx.clone()),
            serde_json::json!({ "code": code_m, "for": "other@example.com" }),
        )
        .await;
        assert!(!refused.success);
        assert!(refused.error.unwrap().contains("Only an admin"));
        assert!(member.ctx.device_pairing.list_devices().unwrap().is_empty());

        let admin = fixture(true);
        let boss = person(&admin, "boss@example.com");
        let other = person(&admin, "other@example.com");
        let code_a = code(&admin, None);
        let done = as_person(
            Some(&boss),
            &PairingApproveTool::new(admin.ctx.clone()),
            serde_json::json!({ "code": code_a, "for": "other@example.com" }),
        )
        .await;
        assert!(done.success, "{:?}", done.error);
        assert_eq!(done.output["linked_via"], "for");
        assert_eq!(
            owner(&admin, &done.output["device_id"]),
            Some(other.id.to_string())
        );
    }

    #[tokio::test]
    async fn admins_mode_refuses_members() {
        let f = fixture_with(false, WhoCanConnect::Admins);
        let kings = person(&f, "kings@example.com");
        let code = code(&f, None);
        let refused = as_person(
            Some(&kings),
            &PairingApproveTool::new(f.ctx.clone()),
            serde_json::json!({ "code": code }),
        )
        .await;
        assert!(!refused.success);
        assert!(refused.error.unwrap().contains("Only admins"));

        let admins = fixture_with(true, WhoCanConnect::Admins);
        let boss = person(&admins, "boss@example.com");
        let code = self::code(&admins, None);
        let done = as_person(
            Some(&boss),
            &PairingApproveTool::new(admins.ctx.clone()),
            serde_json::json!({ "code": code }),
        )
        .await;
        assert!(done.success, "{:?}", done.error);
    }

    #[tokio::test]
    async fn link_connects_an_existing_device_for_admins_only() {
        for admin in [false, true] {
            let f = fixture(admin);
            let kings = person(&f, "kings@example.com");
            let req = f
                .ctx
                .device_pairing
                .initiate("Desk", vec![], None, None, None, None)
                .unwrap();
            let (device, _) = f.ctx.device_pairing.approve(&req.code, "test").unwrap();

            let result = as_person(
                Some(&kings),
                &PairingLinkTool::new(f.ctx.clone()),
                serde_json::json!({
                    "device_id": device.id,
                    "identity": kings.id.to_string(),
                    "device_kind": "desktop_device",
                }),
            )
            .await;

            assert_eq!(result.success, admin, "{:?}", result.error);
            let linked = device_link::identity_for_device(f.store.as_ref(), &device.id);
            assert_eq!(linked.is_some(), admin);
        }
    }

    #[tokio::test]
    async fn reject_pairing() {
        let f = fixture(true);
        let req = f
            .ctx
            .device_pairing
            .initiate("rejected-device", vec![], None, None, None, None)
            .unwrap();

        let result = PairingRejectTool::new(f.ctx.clone())
            .execute(serde_json::json!({ "code": req.code }))
            .await
            .unwrap();
        assert!(result.success, "reject failed: {:?}", result.error);
        assert_eq!(result.output["rejected"], true);

        let result = PairingPendingTool::new(f.ctx)
            .execute(serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result.output["count"], 0);
    }

    #[tokio::test]
    async fn revoke_device() {
        let f = fixture(true);
        let req = f
            .ctx
            .device_pairing
            .initiate("revoke-me", vec![], None, None, None, None)
            .unwrap();
        let (device, _token) = f.ctx.device_pairing.approve(&req.code, "test").unwrap();

        let result = PairingRevokeTool::new(f.ctx.clone())
            .execute(serde_json::json!({ "device_id": device.id }))
            .await
            .unwrap();
        assert!(result.success, "revoke failed: {:?}", result.error);
        assert_eq!(result.output["revoked"], true);

        let result = PairingDevicesTool::new(f.ctx)
            .execute(serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result.output["devices"][0]["revoked"], true);
    }
}
