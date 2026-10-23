//! Core types for the WASM skill system.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use workforce_core::access::{Grant, Visibility};

use crate::manifest::SkillManifest;

/// Capabilities a WASM skill can request. Each capability gates access to a
/// set of host functions — if a skill doesn't declare a capability, calls to
/// the corresponding host functions return permission errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Outbound HTTP requests via `http_fetch`.
    Http,
    /// Read/write access to the vault (scoped to `wasm-skills/{skill_name}/*`).
    Vault,
    /// Read/write access to agent memory store.
    Memory,
    /// Publish/request on the NATS message bus.
    Nats,
    /// Read skill-scoped configuration values.
    Config,
    /// Read staged artifacts by id and stage new ones for delivery.
    Artifacts,
    /// Invoke declared platform tools via `wf_tool_invoke` (each tool must
    /// also be listed in `[[permissions]]`).
    Tools,
    /// Read and write the hub's document store via `wf_store_call`, in the
    /// calling agent's scope (and the calling user's, when one is set).
    Store,
}

impl Capability {
    /// All known capabilities for iteration.
    pub const ALL: &'static [Capability] = &[
        Capability::Http,
        Capability::Vault,
        Capability::Memory,
        Capability::Nats,
        Capability::Config,
        Capability::Artifacts,
        Capability::Tools,
        Capability::Store,
    ];
}

/// Identity scope a credential is bound to. Determines whether two users
/// invoking the same skill share a single vault entry (`Shared`) or each
/// have their own (`User`).
///
/// `User` is the secure default. `Shared` is opt-in per credential in the
/// manifest, for genuinely cross-user secrets like an org-wide service-
/// account token. With `User`, the host computes the vault path as
/// `wasm-skills/{skill}/users/{uid}/{key}`; with `Shared`, as
/// `wasm-skills/{skill}/shared/{key}`. A `User`-scoped credential cannot
/// be read when `CURRENT_USER_ID` is unset — the host returns a hard
/// error rather than silently falling through to a shared bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CredentialScope {
    /// Per-user — path includes the active `CURRENT_USER_ID`. Default.
    #[default]
    User,
    /// Cross-user — single vault entry shared by every caller of the skill.
    Shared,
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Capability::Http => write!(f, "http"),
            Capability::Vault => write!(f, "vault"),
            Capability::Memory => write!(f, "memory"),
            Capability::Nats => write!(f, "nats"),
            Capability::Config => write!(f, "config"),
            Capability::Artifacts => write!(f, "artifacts"),
            Capability::Tools => write!(f, "tools"),
            Capability::Store => write!(f, "store"),
        }
    }
}

/// Bridge from `wf_store_call` into the hub's document store. The request
/// is the same JSON the `store.*` platform tools take plus an `op`; the
/// reply is the tool's output, or `{"ok": false, "code", "error"}` for a
/// store error. `Err` is a policy denial (becomes the call's notice).
/// Synchronous because host functions run on a blocking thread.
pub trait SkillDocumentStore: Send + Sync {
    fn call(
        &self,
        ctx: &SkillCallContext,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, String>;
}

static SKILL_DOCUMENT_STORE: parking_lot::RwLock<Option<Arc<dyn SkillDocumentStore>>> =
    parking_lot::RwLock::new(None);

/// Install the process-wide store bridge; the engine hands it to every
/// execution's [`crate::host::HostState`].
pub fn set_skill_document_store(store: Arc<dyn SkillDocumentStore>) {
    *SKILL_DOCUMENT_STORE.write() = Some(store);
}

pub fn skill_document_store() -> Option<Arc<dyn SkillDocumentStore>> {
    SKILL_DOCUMENT_STORE.read().clone()
}

/// Identity and build context for a skill-originated tool invocation,
/// handed to the [`SkillToolInvoker`] alongside the declared permission.
#[derive(Debug, Clone)]
pub struct SkillCallContext {
    pub skill_name: String,
    pub grant_fingerprint: String,
    pub agent_id: String,
    pub user_id: Option<String>,
}

/// Late-binding slot for an agent's [`SkillToolInvoker`]: skill tools
/// register before the policy engine (which the invoker needs) exists, so
/// each tool holds the slot and reads it at execute time.
pub type SkillInvokerSlot = Arc<parking_lot::RwLock<Option<Arc<dyn SkillToolInvoker>>>>;

/// Decides whether the acting identity may call a skill's tools at all. The
/// hub implements it over the install record's owner/visibility/shares plus
/// the caller's role; the runtime only asks.
pub trait SkillUseGate: Send + Sync {
    /// `Err` is the user-facing denial text, surfaced verbatim as the tool
    /// failure.
    fn authorize_use(&self, skill: &str, user: Option<&str>) -> Result<(), String>;
}

/// Late-binding slot for the process-wide [`SkillUseGate`], filled once the
/// hub's store and identity store exist. Unfilled is permissive: single-tenant
/// CLI has no identities to scope against.
pub type SkillUseGateSlot = Arc<parking_lot::RwLock<Option<Arc<dyn SkillUseGate>>>>;

/// The late-bound bridges a registered skill tool reads at execute time: the
/// per-agent tool invoker and the process-wide use gate.
#[derive(Clone)]
pub struct SkillRuntimeBindings {
    pub invoker: SkillInvokerSlot,
    pub use_gate: SkillUseGateSlot,
}

impl SkillRuntimeBindings {
    pub fn new(invoker: SkillInvokerSlot, use_gate: SkillUseGateSlot) -> Self {
        Self { invoker, use_gate }
    }

    /// Bindings that gate nothing and invoke nothing — tests and the paths
    /// that register tools before either bridge exists.
    pub fn unbound() -> Self {
        Self::new(Arc::default(), Arc::default())
    }

    /// Deny with the gate's own message when one is wired; allow otherwise.
    pub fn authorize_use(&self, skill: &str, user: Option<&str>) -> Result<(), String> {
        match self.use_gate.read().clone() {
            Some(gate) => gate.authorize_use(skill, user),
            None => Ok(()),
        }
    }
}

/// Bridge from `wf_tool_invoke` into the platform's governed tool dispatch.
/// Implementations run the full gauntlet — capability gate under the calling
/// user, policy engine, audit — before executing; the host has already
/// checked the manifest declaration, budget, and secret-marker rules.
/// Synchronous because host functions run on a blocking thread.
pub trait SkillToolInvoker: Send + Sync {
    /// `Err` is the user-facing denial or failure text (becomes the call's
    /// policy notice).
    fn invoke(
        &self,
        ctx: &SkillCallContext,
        permission: &crate::manifest::ManifestPermission,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, String>;

    /// Invoke several declared tools for one guest batch call, preserving
    /// element order; each `Err` is that call's denial text. The default
    /// runs sequentially — implementations may dispatch concurrently.
    fn invoke_many(
        &self,
        ctx: &SkillCallContext,
        calls: Vec<(crate::manifest::ManifestPermission, serde_json::Value)>,
    ) -> Vec<Result<serde_json::Value, String>> {
        calls
            .into_iter()
            .map(|(permission, args)| self.invoke(ctx, &permission, &args))
            .collect()
    }
}

/// A tool definition declared in a WASM skill's manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmToolDefinition {
    /// Tool name as the agent sees it (e.g., `weather.get_forecast`).
    /// Constructed as `{skill_name}.{tool_name}` at registration time.
    pub name: String,
    /// Human-readable description shown to the LLM.
    pub description: String,
    /// JSON Schema for the tool's input parameters.
    pub input_schema: serde_json::Value,
    /// Name of the WASM handler function to call (e.g., `get_forecast`).
    pub handler: String,
}

/// Lifecycle states for an installed WASM skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WasmSkillState {
    /// Binary downloaded and compiled but not yet configured.
    Installed,
    /// Configuration and credentials provided.
    Configured,
    /// Tools registered and available to agents.
    Active,
    /// Temporarily disabled.
    Disabled,
    /// Failed to load or compile.
    Error,
}

/// Fingerprint of a specific installed build — binary plus canonical
/// manifest. Recorded on grants for audit (which exact build was live at
/// approval time) and used to validate legacy single-field records; consent
/// validity itself pins [`contract_fingerprint`].
pub fn grant_fingerprint(binary_sha256: &str, manifest: &SkillManifest) -> eyre::Result<String> {
    Ok(crate::store::sha256_hex(
        format!("{binary_sha256}\n{}", manifest.to_toml()?).as_bytes(),
    ))
}

/// Fingerprint of the manifest's consent-relevant subset: what the skill can
/// reach (`[network] allowed_hosts`), read (`[[credentials]]` names +
/// scopes), and invoke (`[[permissions]]` tools + host limits), plus the
/// capability set gating each host-API family and the tools that hold
/// themselves behind a human decision. Grants pin this, so a pure-logic
/// rebuild keeps them valid while any consent-surface edit — including a
/// `user`→`shared` scope flip, or a rebuild that drops an approval gate —
/// invalidates them. Version, description, author, and the skill's own
/// ungated tools are excluded: changing them changes nothing the user
/// approved. The summary template is excluded too — it is a legibility aid,
/// so a typo fix must not cost every grant. Canonicalised as sorted JSON so
/// the digest is order-stable and delimiter-injection-proof.
///
/// Pinning makes a dropped gate *visible*, forcing re-consent wherever
/// grants are load-bearing; it is not itself the stop. What stops a call is
/// that `requires_approval` is read from the live installed manifest on
/// every dispatch.
pub fn contract_fingerprint(manifest: &SkillManifest) -> String {
    let mut capabilities: Vec<String> = manifest
        .capabilities
        .iter()
        .map(Capability::to_string)
        .collect();
    capabilities.sort();
    let mut network: Vec<&str> = manifest.allowed_hosts.iter().map(String::as_str).collect();
    network.sort();
    let mut credentials: Vec<(&str, CredentialScope)> = manifest
        .credentials
        .iter()
        .map(|c| (c.name.as_str(), c.scope))
        .collect();
    credentials.sort_by_key(|&(name, scope)| (name, scope == CredentialScope::Shared));
    let mut permissions: Vec<(&str, Vec<&str>)> = manifest
        .permissions
        .iter()
        .map(|p| {
            let mut hosts: Vec<&str> = p.allowed_hosts.iter().map(String::as_str).collect();
            hosts.sort();
            (p.tool.as_str(), hosts)
        })
        .collect();
    permissions.sort();
    let mut approvals: Vec<&str> = manifest
        .tools
        .iter()
        .filter(|tool| tool.requires_approval)
        .map(|tool| tool.name.as_str())
        .collect();
    approvals.sort();
    let doc = serde_json::json!({
        "approvals": approvals,
        "capabilities": capabilities,
        "network": network,
        "credentials": credentials
            .iter()
            .map(|(name, scope)| serde_json::json!({"name": name, "scope": scope}))
            .collect::<Vec<_>>(),
        "permissions": permissions
            .iter()
            .map(|(tool, hosts)| serde_json::json!({"tool": tool, "allowed_hosts": hosts}))
            .collect::<Vec<_>>(),
    });
    crate::store::sha256_hex(format!("wasm-skill-contract-v1\n{doc}").as_bytes())
}

/// The dual-field upgrade of a legacy grant record that has just validated
/// by exact build match. Such a record is PROVABLY consent-equivalent to
/// the current contract — the build fingerprint covers binary + full
/// manifest, so a match means the granted manifest IS the installed one —
/// which makes adding its contract fingerprint a pure re-encoding of the
/// consent already given, never a widening. Returns `None` for records
/// that are already dual-field or unreadable; callers write the result
/// back so future pure-logic rebuilds no longer stale the grant.
pub fn upgraded_legacy_record(raw: &str, contract_fingerprint: &str) -> Option<String> {
    let mut record: serde_json::Value = serde_json::from_str(raw).ok()?;
    let fields = record.as_object_mut()?;
    // Mirror the validator's notion of "legacy" exactly: a null or
    // non-string contract field validates by build match, so it upgrades.
    if fields
        .get("contract_fingerprint")
        .and_then(serde_json::Value::as_str)
        .is_some()
    {
        return None;
    }
    fields.insert(
        "contract_fingerprint".to_string(),
        contract_fingerprint.into(),
    );
    fields.insert("migrated_at".to_string(), Utc::now().to_rfc3339().into());
    Some(record.to_string())
}

/// Whether a stored grant record still covers the installed build. A
/// dual-field record is judged by its contract alone; a legacy record (no
/// `contract_fingerprint`) by exact build match, which migrates it on the
/// next re-pin. Unparseable records and empty fingerprints fail closed.
pub fn grant_record_is_current(
    raw: &str,
    build_fingerprint: &str,
    contract_fingerprint: &str,
) -> bool {
    let Ok(record) = serde_json::from_str::<serde_json::Value>(raw) else {
        return false;
    };
    match record.get("contract_fingerprint").and_then(|v| v.as_str()) {
        Some(contract) => !contract_fingerprint.is_empty() && contract == contract_fingerprint,
        None => {
            !build_fingerprint.is_empty()
                && record.get("fingerprint").and_then(|v| v.as_str()) == Some(build_fingerprint)
        }
    }
}

/// Who a skill belongs to and who may use it — the ownership stamped on an
/// install, kept together so every install site names both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillOwnership {
    pub owner: Option<String>,
    pub visibility: Visibility,
}

impl SkillOwnership {
    /// A skill owned by `user`, visible per `visibility`.
    pub fn owned(user: impl Into<String>, visibility: Visibility) -> Self {
        Self {
            owner: Some(user.into()),
            visibility,
        }
    }

    /// An operator install with no per-user owner: usable by everyone,
    /// manageable only by an admin.
    pub fn operator() -> Self {
        Self {
            owner: None,
            visibility: Visibility::Global,
        }
    }
}

/// A skill whose owner, visibility and grantees decide who reaches it, whatever
/// kind of skill it is and wherever that record is kept.
pub trait OwnedSkill {
    fn owner(&self) -> Option<&str>;
    fn visibility(&self) -> Visibility;
    fn shares(&self) -> &[Grant];
}

impl OwnedSkill for InstalledWasmSkill {
    fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    fn visibility(&self) -> Visibility {
        self.visibility
    }

    fn shares(&self) -> &[Grant] {
        &self.shares
    }
}

/// An installed WASM skill with runtime metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledWasmSkill {
    /// The parsed manifest.
    pub manifest: SkillManifest,
    /// Current lifecycle state.
    pub state: WasmSkillState,
    /// When the skill was installed.
    pub installed_at: DateTime<Utc>,
    /// Source of the installation (e.g., `github.com/user/repo@v1.0.0` or local path).
    pub source: String,
    /// Non-secret configuration values.
    #[serde(default)]
    pub config: HashMap<String, String>,
    /// Version of the compiled WASM binary on disk.
    pub binary_version: u32,
    /// Hex-encoded SHA-256 of the installed `.wasm` bytes. Captured at
    /// install time and persisted with the install record. Callers that
    /// supply an expected `sha256` to the install flow get it verified
    /// before activation; subsequent upgrades record a new digest. Empty
    /// for legacy install records written before this field existed.
    #[serde(default)]
    pub sha256: String,
    /// Identity that installed the skill. `None` for operator installs and for
    /// records written before ownership existed.
    #[serde(default)]
    pub owner: Option<String>,
    /// Who may use it beyond the owner and grantees. Ignored while `owner` is
    /// `None` — an ownerless skill is everyone's.
    #[serde(default)]
    pub visibility: Visibility,
    /// Explicit grantees, each a user or a team.
    #[serde(default)]
    pub shares: Vec<Grant>,
}

impl InstalledWasmSkill {
    /// The consent fingerprint of this exact build — see [`grant_fingerprint`].
    pub fn grant_fingerprint(&self) -> eyre::Result<String> {
        grant_fingerprint(&self.sha256, &self.manifest)
    }

    /// The consent-contract fingerprint of the installed manifest — see
    /// [`contract_fingerprint`].
    pub fn contract_fingerprint(&self) -> String {
        contract_fingerprint(&self.manifest)
    }
}

/// Request context passed to a WASM tool handler.
///
/// Serialised to JSON and written into WASM linear memory before calling
/// the handler function.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmToolRequest {
    /// Name of the tool being invoked.
    pub tool_name: String,
    /// Name of the handler function in the WASM module.
    pub handler: String,
    /// Input parameters from the agent.
    pub input: serde_json::Value,
    /// Agent ID making the request.
    pub agent_id: String,
    /// User ID making the request. `None` for system-initiated work that
    /// hasn't opted into a synthetic identity via
    /// `workforce_agent::runner::with_system_identity`. The host hard-fails
    /// any `CredentialScope::User` vault read when this is `None`.
    #[serde(default)]
    pub user_id: Option<String>,
    /// Task ID context (if any).
    #[serde(default)]
    pub task_id: Option<String>,
}

/// Response from a WASM tool handler.
///
/// The guest writes this as JSON to its response buffer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmToolResponse {
    /// Whether the tool execution succeeded.
    pub success: bool,
    /// Result data (arbitrary JSON).
    pub result: serde_json::Value,
    /// Error message if the tool failed.
    #[serde(default)]
    pub error: Option<String>,
    /// Host-authored remedy for a policy denial during the call (ungranted
    /// credential, disallowed host). The engine overwrites this after parsing
    /// the guest response, so guest code can never spoof a platform notice.
    #[serde(default)]
    pub policy_notice: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_core::access::{
        access_with, granted_to, AccessLevel, GrantLevel, NoTeams, TeamMembership,
    };

    #[test]
    fn capability_display() {
        assert_eq!(Capability::Http.to_string(), "http");
        assert_eq!(Capability::Vault.to_string(), "vault");
        assert_eq!(Capability::Memory.to_string(), "memory");
        assert_eq!(Capability::Nats.to_string(), "nats");
        assert_eq!(Capability::Config.to_string(), "config");
        assert_eq!(Capability::Artifacts.to_string(), "artifacts");
    }

    #[test]
    fn capability_serde_round_trip() {
        for (cap, expected) in [
            (Capability::Http, "\"http\""),
            (Capability::Artifacts, "\"artifacts\""),
        ] {
            let json = serde_json::to_string(&cap).unwrap();
            assert_eq!(json, expected);
            let parsed: Capability = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, cap);
        }
    }

    #[test]
    fn wasm_tool_response_success() {
        let resp = WasmToolResponse {
            success: true,
            result: serde_json::json!({"temp": 22}),
            error: None,
            policy_notice: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: WasmToolResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.success);
        assert_eq!(parsed.result["temp"], 22);
    }

    #[test]
    fn wasm_tool_response_failure() {
        let resp = WasmToolResponse {
            success: false,
            result: serde_json::Value::Null,
            error: Some("timeout".into()),
            policy_notice: None,
        };
        assert!(!resp.success);
        assert_eq!(resp.error.as_deref(), Some("timeout"));
    }

    #[test]
    fn grant_fingerprint_pins_binary_and_manifest() {
        let manifest = SkillManifest {
            name: "test".into(),
            version: "1.0.0".into(),
            description: "test skill".into(),
            author: String::new(),
            license: String::new(),
            min_workforce_version: None,
            capabilities: std::collections::HashSet::new(),
            tools: vec![],
            credentials: vec![],
            config_schema: vec![],
            allowed_hosts: vec![],
            permissions: vec![],
        };
        let base = grant_fingerprint("aa11", &manifest).unwrap();
        assert_eq!(base, grant_fingerprint("aa11", &manifest).unwrap());
        assert_ne!(base, grant_fingerprint("bb22", &manifest).unwrap());

        let mut widened = manifest.clone();
        widened.allowed_hosts = vec!["evil.example.com".into()];
        assert_ne!(base, grant_fingerprint("aa11", &widened).unwrap());
    }

    fn consent_manifest() -> SkillManifest {
        use crate::manifest::{ManifestCredential, ManifestPermission, ManifestTool};
        SkillManifest {
            name: "test".into(),
            version: "1.0.0".into(),
            description: "test skill".into(),
            author: String::new(),
            license: String::new(),
            min_workforce_version: None,
            capabilities: [Capability::Http, Capability::Tools, Capability::Vault]
                .into_iter()
                .collect(),
            tools: vec![ManifestTool {
                name: "ping".into(),
                description: "ping".into(),
                requires_approval: false,
                approval_summary: None,
                input_schema: serde_json::json!({"type": "object"}),
                handler: None,
            }],
            credentials: vec![
                ManifestCredential {
                    name: "api_key".into(),
                    description: "key".into(),
                    required: true,
                    scope: CredentialScope::User,
                },
                ManifestCredential {
                    name: "token".into(),
                    description: "token".into(),
                    required: false,
                    scope: CredentialScope::Shared,
                },
            ],
            config_schema: vec![],
            allowed_hosts: vec!["b.example.com".into(), "a.example.com".into()],
            permissions: vec![ManifestPermission {
                tool: "slack.post_message".into(),
                description: "post".into(),
                allowed_hosts: vec!["hooks.slack.com".into()],
            }],
        }
    }

    #[test]
    fn contract_fingerprint_ignores_non_consent_edits() {
        let manifest = consent_manifest();
        let base = contract_fingerprint(&manifest);

        let mut edited = manifest.clone();
        edited.version = "9.9.9".into();
        edited.description = "totally different".into();
        edited.author = "someone".into();
        edited.tools.push(crate::manifest::ManifestTool {
            name: "extra".into(),
            description: "extra".into(),
            requires_approval: false,
            approval_summary: None,
            input_schema: serde_json::json!({"type": "object"}),
            handler: None,
        });
        edited.credentials[0].description = "reworded".into();
        edited.credentials[0].required = false;
        edited.permissions[0].description = "reworded".into();
        assert_eq!(base, contract_fingerprint(&edited));
    }

    #[test]
    fn contract_fingerprint_is_order_stable() {
        let manifest = consent_manifest();
        let mut shuffled = manifest.clone();
        shuffled.credentials.reverse();
        shuffled.allowed_hosts.reverse();
        shuffled
            .permissions
            .push(crate::manifest::ManifestPermission {
                tool: "aa.first".into(),
                description: "sorts first".into(),
                allowed_hosts: vec!["b.com".into(), "a.com".into()],
            });
        let mut with_perm = manifest.clone();
        with_perm.permissions.insert(
            0,
            crate::manifest::ManifestPermission {
                tool: "aa.first".into(),
                description: "different words, same contract".into(),
                allowed_hosts: vec!["a.com".into(), "b.com".into()],
            },
        );
        assert_eq!(
            contract_fingerprint(&with_perm),
            contract_fingerprint(&shuffled)
        );
    }

    /// Freezes the canonicalisation: any drift — a serde_json feature change
    /// reordering keys, an accidental field addition — silently invalidates
    /// every grant in every deployment, so it must fail a test first.
    #[test]
    fn contract_fingerprint_digest_is_pinned() {
        assert_eq!(
            contract_fingerprint(&consent_manifest()),
            "c4a658e431a171d190dbd62cff7ca9944b95b59f41899d0fec2c506f12c62af9"
        );
    }

    /// `grant_fingerprint` hashes `to_toml()`, and a legacy grant record is
    /// validated by exact build match, so the canonical TOML of an ungated
    /// skill is itself a compatibility surface: a stray line in it stales
    /// every legacy grant on every hub at once.
    #[test]
    fn legacy_grant_fingerprint_digest_is_pinned() {
        assert_eq!(
            grant_fingerprint("aa11", &consent_manifest()).unwrap(),
            "39b2a759d6ea3f8f4e68f8692c88f63edfc48a375f5f4239b8807223ade3a38e"
        );
    }

    /// Adding or removing a gate re-pins consent; retitling the prompt does
    /// not, so a typo fix never costs a grant.
    #[test]
    fn contract_fingerprint_tracks_gates_but_not_their_wording() {
        let base = contract_fingerprint(&consent_manifest());

        let mut gated = consent_manifest();
        gated.tools[0].requires_approval = true;
        gated.tools[0].approval_summary = Some("ping {{host}}".into());
        let gated_digest = contract_fingerprint(&gated);
        assert_ne!(base, gated_digest, "adding a gate must re-pin");

        let mut reworded = gated.clone();
        reworded.tools[0].approval_summary = Some("ping host {{host}}".into());
        assert_eq!(gated_digest, contract_fingerprint(&reworded));

        let mut ungated = gated.clone();
        ungated.tools[0].requires_approval = false;
        assert_eq!(base, contract_fingerprint(&ungated), "removing it re-pins");

        let mut second = gated.clone();
        second.tools.push(crate::manifest::ManifestTool {
            name: "aa_first".into(),
            description: "sorts first".into(),
            requires_approval: true,
            approval_summary: Some("do it".into()),
            input_schema: serde_json::json!({"type": "object"}),
            handler: None,
        });
        let mut reordered = second.clone();
        reordered.tools.reverse();
        assert_eq!(
            contract_fingerprint(&second),
            contract_fingerprint(&reordered)
        );
        assert_ne!(gated_digest, contract_fingerprint(&second));
    }

    #[test]
    fn contract_fingerprint_changes_on_every_consent_surface() {
        let base = contract_fingerprint(&consent_manifest());
        type ManifestEdit = Box<dyn Fn(&mut SkillManifest)>;
        let edits: Vec<ManifestEdit> = vec![
            Box::new(|m| {
                m.capabilities.insert(Capability::Artifacts);
            }),
            Box::new(|m| m.allowed_hosts.push("c.example.com".into())),
            Box::new(|m| m.credentials[0].name = "renamed".into()),
            Box::new(|m| m.credentials[0].scope = CredentialScope::Shared),
            Box::new(|m| {
                m.credentials.pop();
            }),
            Box::new(|m| m.permissions[0].tool = "slack.list_users".into()),
            Box::new(|m| m.permissions[0].allowed_hosts.push("evil.com".into())),
            Box::new(|m| {
                m.permissions.push(crate::manifest::ManifestPermission {
                    tool: "jira.search".into(),
                    description: "search".into(),
                    allowed_hosts: vec![],
                })
            }),
        ];
        for (i, edit) in edits.iter().enumerate() {
            let mut edited = consent_manifest();
            edit(&mut edited);
            assert_ne!(base, contract_fingerprint(&edited), "edit {i} must re-pin");
        }
    }

    #[test]
    fn legacy_record_upgrade_preserves_fields_and_survives_rebuilds() {
        let legacy = serde_json::json!({
            "fingerprint": "b1",
            "granted_at": "2026-08-01T00:00:00Z",
            "granted_by": "alice",
        })
        .to_string();
        let upgraded = upgraded_legacy_record(&legacy, "c1").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&upgraded).unwrap();
        assert_eq!(parsed["fingerprint"], "b1");
        assert_eq!(parsed["granted_at"], "2026-08-01T00:00:00Z");
        assert_eq!(parsed["granted_by"], "alice");
        assert_eq!(parsed["contract_fingerprint"], "c1");
        assert!(parsed["migrated_at"].is_string());

        // The upgraded record now validates by contract across a rebuild.
        assert!(grant_record_is_current(&upgraded, "b2-new-build", "c1"));
        assert!(!grant_record_is_current(&upgraded, "b1", "c2"));

        // Already-dual-field and unreadable records are never rewritten.
        assert_eq!(upgraded_legacy_record(&upgraded, "c1"), None);
        assert_eq!(upgraded_legacy_record("not-json", "c1"), None);
        assert_eq!(upgraded_legacy_record("[1,2]", "c1"), None);

        // A null contract field validates as legacy, so it upgrades too.
        let null_contract =
            serde_json::json!({"fingerprint": "b1", "contract_fingerprint": null}).to_string();
        let repaired = upgraded_legacy_record(&null_contract, "c1").unwrap();
        assert!(grant_record_is_current(&repaired, "other-build", "c1"));
    }

    #[test]
    fn grant_record_validity_rule() {
        let dual = serde_json::json!({"fingerprint": "b1", "contract_fingerprint": "c1"});
        let legacy = serde_json::json!({"fingerprint": "b1"});

        // Dual-field records are judged by contract alone.
        assert!(grant_record_is_current(&dual.to_string(), "b2", "c1"));
        assert!(!grant_record_is_current(&dual.to_string(), "b1", "c2"));

        // Legacy records need an exact build match.
        assert!(grant_record_is_current(&legacy.to_string(), "b1", "c1"));
        assert!(!grant_record_is_current(&legacy.to_string(), "b2", "c1"));

        // Corrupt or empty inputs fail closed.
        assert!(!grant_record_is_current("not-json", "b1", "c1"));
        assert!(!grant_record_is_current(&legacy.to_string(), "", "c1"));
        assert!(!grant_record_is_current(&dual.to_string(), "b1", ""));
    }

    #[test]
    fn installed_wasm_skill_serde() {
        let skill = InstalledWasmSkill {
            manifest: SkillManifest {
                name: "test".into(),
                version: "1.0.0".into(),
                description: "test skill".into(),
                author: String::new(),
                license: String::new(),
                min_workforce_version: None,
                capabilities: std::collections::HashSet::new(),
                tools: vec![],
                credentials: vec![],
                config_schema: vec![],
                allowed_hosts: vec![],
                permissions: vec![],
            },
            state: WasmSkillState::Installed,
            installed_at: Utc::now(),
            source: "local".into(),
            config: HashMap::new(),
            binary_version: 1,
            sha256: "abcd0123".into(),
            owner: Some("alice".into()),
            visibility: Visibility::Global,
            shares: vec![Grant::user("bob", GrantLevel::Manage)],
        };
        let json = serde_json::to_string(&skill).unwrap();
        let parsed: InstalledWasmSkill = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.manifest.name, "test");
        assert_eq!(parsed.state, WasmSkillState::Installed);
        assert_eq!(parsed.owner.as_deref(), Some("alice"));
        assert_eq!(parsed.visibility, Visibility::Global);
        assert_eq!(parsed.shares, skill.shares);
    }

    /// Records written before ownership existed must load, and must read as
    /// the pre-ownership world did: everyone may run them.
    #[test]
    fn legacy_record_without_ownership_is_everyones() {
        let legacy = serde_json::json!({
            "manifest": {
                "name": "legacy",
                "version": "1.0.0",
                "description": "d",
                "author": "",
                "license": "",
                "capabilities": [],
                "tools": [],
                "credentials": [],
                "config_schema": [],
                "allowed_hosts": [],
                "permissions": []
            },
            "state": "installed",
            "installed_at": Utc::now().to_rfc3339(),
            "source": "local",
            "config": {},
            "binary_version": 1
        });
        let parsed: InstalledWasmSkill = serde_json::from_value(legacy.clone()).unwrap();
        assert!(parsed.owner.is_none());
        assert!(parsed.shares.is_empty());
        assert_eq!(parsed.visibility, Visibility::Private);
        for user in [Some("anyone"), None] {
            assert_eq!(reach(&parsed, user, &NoTeams), AccessLevel::Run);
        }

        // A record written before teams existed: an owner, a stored visibility,
        // and grantees spelled `user_id` with no principal type at all.
        let mut owned = legacy;
        let fields = owned.as_object_mut().unwrap();
        fields.insert("owner".into(), "alice".into());
        fields.insert("visibility".into(), "global".into());
        fields.insert(
            "shares".into(),
            serde_json::json!([
                {"user_id": "bob", "permission": "manage"},
                {"user_id": "carol", "permission": "run"},
            ]),
        );
        let parsed: InstalledWasmSkill = serde_json::from_value(owned).unwrap();
        assert_eq!(parsed.owner.as_deref(), Some("alice"));
        assert_eq!(parsed.visibility, Visibility::Global);
        assert_eq!(
            parsed.shares,
            vec![
                Grant::user("bob", GrantLevel::Manage),
                Grant::user("carol", GrantLevel::Run),
            ]
        );
        assert_eq!(reach(&parsed, Some("bob"), &NoTeams), AccessLevel::Manage);
        assert_eq!(reach(&parsed, Some("carol"), &NoTeams), AccessLevel::Run);
        assert_eq!(reach(&parsed, Some("dave"), &NoTeams), AccessLevel::Run);
    }

    /// The shared rule over one record, as every skill surface asks it.
    fn reach(
        skill: &(impl OwnedSkill + ?Sized),
        user: Option<&str>,
        teams: &dyn TeamMembership,
    ) -> AccessLevel {
        access_with(
            skill.owner(),
            skill.visibility(),
            user,
            granted_to(skill.shares(), user, teams),
            || {
                skill
                    .owner()
                    .is_some_and(|owner| teams.shares_team_with(owner))
            },
        )
    }

    fn owned_skill(owner: Option<&str>, visibility: Visibility) -> InstalledWasmSkill {
        InstalledWasmSkill {
            manifest: SkillManifest {
                name: "s".into(),
                version: "1.0.0".into(),
                description: "d".into(),
                author: String::new(),
                license: String::new(),
                min_workforce_version: None,
                capabilities: std::collections::HashSet::new(),
                tools: vec![],
                credentials: vec![],
                config_schema: vec![],
                allowed_hosts: vec![],
                permissions: vec![],
            },
            state: WasmSkillState::Active,
            installed_at: Utc::now(),
            source: "local".into(),
            config: HashMap::new(),
            binary_version: 1,
            sha256: String::new(),
            owner: owner.map(str::to_string),
            visibility,
            shares: Vec::new(),
        }
    }

    /// A record's own fields are what the shared rule reads, teams included:
    /// the team a grant names and the team its owner shares with are new
    /// reaches a record written before this release simply never held.
    #[test]
    fn a_record_stands_where_its_own_fields_put_it() {
        struct Platform;
        impl TeamMembership for Platform {
            fn in_team(&self, team_id: &str) -> bool {
                team_id == "platform"
            }
            fn shares_team_with(&self, owner: &str) -> bool {
                owner == "alice"
            }
        }

        let private = owned_skill(Some("alice"), Visibility::Private);
        assert_eq!(private.owner(), Some("alice"));
        assert_eq!(private.visibility(), Visibility::Private);
        assert!(private.shares().is_empty());
        assert_eq!(
            reach(&private, Some("alice"), &NoTeams),
            AccessLevel::Manage
        );
        assert_eq!(reach(&private, Some("bob"), &Platform), AccessLevel::None);

        let team = owned_skill(Some("alice"), Visibility::Team);
        assert_eq!(reach(&team, Some("bob"), &Platform), AccessLevel::Run);
        assert_eq!(reach(&team, Some("bob"), &NoTeams), AccessLevel::None);

        let granted = InstalledWasmSkill {
            shares: vec![
                Grant::user("bob", GrantLevel::Run),
                Grant::team("platform", GrantLevel::Manage),
            ],
            ..owned_skill(Some("alice"), Visibility::Private)
        };
        assert_eq!(reach(&granted, Some("bob"), &NoTeams), AccessLevel::Run);
        assert_eq!(reach(&granted, Some("bob"), &Platform), AccessLevel::Manage);
        assert_eq!(reach(&granted, Some("dave"), &NoTeams), AccessLevel::None);
    }

    #[test]
    fn unbound_bindings_authorize_everything() {
        assert!(SkillRuntimeBindings::unbound()
            .authorize_use("s", Some("bob"))
            .is_ok());
    }

    #[test]
    fn a_wired_gate_decides() {
        struct Deny;
        impl SkillUseGate for Deny {
            fn authorize_use(&self, skill: &str, _user: Option<&str>) -> Result<(), String> {
                Err(format!("no `{skill}` for you"))
            }
        }
        let bindings = SkillRuntimeBindings::unbound();
        *bindings.use_gate.write() = Some(Arc::new(Deny));
        assert_eq!(
            bindings.authorize_use("s", Some("bob")),
            Err("no `s` for you".to_string())
        );
    }
}
