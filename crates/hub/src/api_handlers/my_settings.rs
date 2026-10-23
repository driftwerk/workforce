//! Per-user settings API — `/api/me/` endpoints.
//!
//! Authenticated users manage their personal integration credentials without
//! admin access. All vault operations are scoped to `users/{identity_id}/` —
//! users cannot access other users' keys. The set of writable paths is gated
//! by [`ALLOWED_PREFIXES`] and [`crate::vault_policy::is_shared_only`] (deny wins).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use workforce_common::subscription::SubscriptionKind;

use crate::gateway::{login_refusal, login_vaults, vault_write_rejection, GatewayState};

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<serde_json::Value>)>;

fn err(status: StatusCode, msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({"error": msg})))
}

/// Extract the identity_id from the JWT claims (UUID-validated; static admin
/// tokens rejected). Thin alias over the shared per-user helper.
fn require_identity(
    state: &GatewayState,
    headers: &HeaderMap,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    super::identity::require_identity(state, headers)
}

pub(crate) const MAX_SECRET_SIZE: usize = 100_000;

/// Audit F4 (2026-05-15) — per-identity throttle on vault writes.
pub(crate) fn enforce_action_rate_limit(
    state: &GatewayState,
    identity_id: &str,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    state
        .action_limiter
        .as_ref()
        .map_or(Ok(()), |lim| lim.check_and_record(identity_id))
        .map_err(crate::gateway::too_many_requests_err)
}

// ── Vault path allow/deny lists ─────────────────────────────────────────────

/// Per-user-writable sub-paths of each skill. Narrow on purpose — `skills/google/`
/// is intentionally avoided so users cannot reach `skills/google/oauth_client/`.
const ALLOWED_PREFIXES: &[&str] = &[
    SubscriptionKind::Claude.vault_key(),
    SubscriptionKind::ChatGpt.vault_key(),
    "skills/datadog/",
    "skills/email/accounts/",
    "skills/freeagent/workspaces/",
    "skills/github/workspaces/",
    "skills/google/workspaces/",
    "skills/hubspot/",
    "skills/jira/",
    "skills/linear/",
    "skills/metabase/",
    "skills/notion/",
    "skills/postmark/",
    "skills/sendgrid/",
    "skills/slack/",
    // Speech (TTS): per-user provider API key + voice, e.g.
    // speech/fish_audio/api_key — each user's own account gets billed.
    "speech/",
];

/// Reject empty / `.` / `..` segments before prefix matching. Path traversal
/// is moot in a flat-key vault (keys are stored verbatim), but the validator
/// pins the canonical key shape so audit logs and prefix checks line up.
/// Mirrors `is_safe_path_segment` in `crates/wasm-runtime/src/host.rs`.
fn has_safe_segments(path: &str) -> bool {
    !path.is_empty()
        && path
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// Whether a non-admin user may write `path` under their own `users/{uid}/`
/// namespace. The authoritative server-side gate for per-user vault writes;
/// the credential catalog endpoint calls this so the editor cannot offer a
/// per-user option the API would then reject.
pub(crate) fn is_allowed_path(path: &str) -> bool {
    is_member_writable(path)
        && (ALLOWED_PREFIXES.iter().any(|p| path.starts_with(p))
            || crate::credentials::is_personal(path))
}

/// Whether `path` is well-formed and not shared-only — the gate every
/// per-user write shares, before any integration allowlist.
fn is_member_writable(path: &str) -> bool {
    has_safe_segments(path) && !crate::vault_policy::is_shared_only(path)
}

/// One representative key per skill — the flat path the adapter falls back
/// to when no workspace is configured. Drives [`get_integrations`].
const SCOPE_PROBES: &[(&str, &str)] = &[
    ("datadog", "skills/datadog/api_key"),
    ("email", "skills/email/accounts/default/username"),
    (
        "freeagent",
        "skills/freeagent/workspaces/default/refresh_token",
    ),
    ("github", "skills/github/token"),
    ("google", "skills/google/workspaces/default/refresh_token"),
    ("hubspot", "skills/hubspot/access_token"),
    ("jira", "skills/jira/api_token"),
    ("linear", "skills/linear/api_key"),
    ("metabase", "skills/metabase/api_key"),
    ("notion", "skills/notion/token"),
    ("postmark", "skills/postmark/server_token"),
    ("sendgrid", "skills/sendgrid/api_key"),
    ("slack", "skills/slack/user_token"),
    ("fish_audio", "speech/fish_audio/api_key"),
    ("elevenlabs", "speech/elevenlabs/api_key"),
    ("soniox", "speech/soniox/api_key"),
];

const GOOGLE_EMAIL_KEY: &str = "skills/google/workspaces/default/email";

#[derive(Serialize)]
pub struct IdentitySummary {
    id: String,
    display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
}

#[derive(Serialize)]
pub struct IntegrationsResponse {
    identity: IdentitySummary,
    integrations: serde_json::Map<String, serde_json::Value>,
}

/// `"personal"` (per-user entry exists), `"shared"` (only the bare path is
/// set), or `"none"`.
fn probe_scope(vault: &crate::vault::Vault, identity_id: &str, key: &str) -> &'static str {
    vault
        .get(&format!("users/{identity_id}/{key}"))
        .map(|_| "personal")
        .or_else(|| vault.get(key).map(|_| "shared"))
        .unwrap_or("none")
}

fn scope_value(scope: &'static str) -> serde_json::Value {
    serde_json::json!({ "configured": scope != "none", "scope": scope })
}

fn resolve_identity_summary(state: &GatewayState, identity_id: &str) -> IdentitySummary {
    state
        .identity_store
        .as_ref()
        .and_then(|store| {
            uuid::Uuid::parse_str(identity_id)
                .ok()
                .and_then(|uid| store.get_by_id(uid).ok().flatten())
        })
        .map(|identity| IdentitySummary {
            id: identity.id.to_string(),
            display_name: identity.display_name,
            email: identity.email,
        })
        .unwrap_or_else(|| IdentitySummary {
            id: identity_id.to_string(),
            display_name: identity_id.to_string(),
            email: None,
        })
}

pub async fn get_integrations(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> ApiResult<IntegrationsResponse> {
    let identity_id = require_identity(&state, &headers)?;

    let vault = state
        .vault
        .as_ref()
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "Vault not configured"))?;

    let identity_summary = resolve_identity_summary(&state, &identity_id);

    let mut integrations: serde_json::Map<String, serde_json::Value> = SCOPE_PROBES
        .iter()
        .map(|(name, key)| {
            (
                (*name).to_string(),
                scope_value(probe_scope(vault, &identity_id, key)),
            )
        })
        .collect();

    // Speech overrides come in two per-user parts (api_key + voice). A user
    // who saved ONLY a voice still has an active personal override — surface
    // it, so the Settings card shows it as configured and offers Remove.
    for provider in ["fish_audio", "elevenlabs", "soniox"] {
        let entry = integrations
            .get_mut(provider)
            .expect("speech providers are in SCOPE_PROBES");
        if entry["scope"] == "none" {
            let voice_scope = probe_scope(vault, &identity_id, &format!("speech/{provider}/voice"));
            if voice_scope == "personal" {
                *entry = scope_value("personal");
            }
        }
    }

    let google_email = vault
        .get(&format!("users/{identity_id}/{GOOGLE_EMAIL_KEY}"))
        .or_else(|| vault.get(GOOGLE_EMAIL_KEY));
    if let Some(entry) = integrations.get_mut("google") {
        entry["email"] = serde_json::json!(google_email);
    }

    Ok(Json(IntegrationsResponse {
        identity: identity_summary,
        integrations,
    }))
}

// ── GET /api/me/vault ───────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct VaultListResponse {
    /// All credentials addressable by the active identity, projected across
    /// the four-tier layered store. Tier `agent_per_user` and
    /// `global_per_user` are the user's own; `agent_specific` and `global`
    /// are shared but still visible from this endpoint so the UI can show
    /// which org/agent-wide defaults the user inherits.
    entries: Vec<crate::vault_view::VaultEntryView>,
    paths: Vec<String>,
}

pub async fn list_user_vault(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
) -> ApiResult<VaultListResponse> {
    let identity_id = require_identity(&state, &headers)?;
    let vault = state
        .vault
        .as_ref()
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "Vault not configured"))?;

    let resolve_display = crate::vault_view::actor_display(state.identity_store.clone());
    let mut entries = list_global_vault_entries_for_user(vault, &identity_id, &resolve_display);
    entries.extend(list_agent_per_user_entries(
        &state,
        &identity_id,
        &resolve_display,
    ));
    let paths = entries.iter().map(|e| e.path.clone()).collect();
    Ok(Json(VaultListResponse { entries, paths }))
}

/// Project the global vault's entries into the per-user vault list. Returns
/// the active user's T2 (`global_per_user`) entries plus every T4 (`global`)
/// shared entry, both with owner-display enrichment.
fn list_global_vault_entries_for_user(
    vault: &crate::vault::Vault,
    identity_id: &str,
    resolve_display: &(dyn Fn(&str) -> Option<String> + Send + Sync),
) -> Vec<crate::vault_view::VaultEntryView> {
    use workforce_common::settings::VaultSource;
    let user_prefix = format!("users/{identity_id}/");
    let mut entries: Vec<crate::vault_view::VaultEntryView> = vault
        .list(Some(&user_prefix))
        .into_iter()
        .filter_map(|raw| {
            let stripped = raw.strip_prefix(&user_prefix)?.to_string();
            let entry = vault.get_entry(&raw)?;
            Some(crate::vault_view::VaultEntryView::from_entry(
                stripped,
                VaultSource::GlobalPerUser,
                entry,
                resolve_display,
            ))
        })
        .collect();
    entries.extend(
        vault
            .list(None)
            .into_iter()
            .filter(|p| !p.starts_with("users/") && !p.starts_with("wasm-skills/"))
            .filter_map(|p| {
                let entry = vault.get_entry(&p)?;
                Some(crate::vault_view::VaultEntryView::from_entry(
                    p,
                    VaultSource::Global,
                    entry,
                    resolve_display,
                ))
            }),
    );
    entries
}

/// Walk every registered agent's vault for this user's T1 (`agent_per_user`)
/// entries. Each entry carries its `agent_id` so the UI can show "you have
/// X cred in agent Atlas" and offer to move it. Agents without a vault
/// are silently skipped.
pub(crate) fn list_agent_per_user_entries(
    state: &GatewayState,
    identity_id: &str,
    resolve_display: &(dyn Fn(&str) -> Option<String> + Send + Sync),
) -> Vec<crate::vault_view::VaultEntryView> {
    use workforce_common::settings::VaultSource;
    let user_prefix = format!("users/{identity_id}/");
    state
        .registry
        .agent_ids()
        .into_iter()
        .filter_map(|aid| {
            let agent_id = aid.as_str().to_string();
            let service = state.registry.get(&agent_id)?;
            let agent_vault = service.vault.as_ref()?;
            let entries: Vec<_> = agent_vault
                .list(Some(&user_prefix))
                .into_iter()
                .filter_map(|raw| {
                    let stripped = raw.strip_prefix(&user_prefix)?.to_string();
                    let entry = agent_vault.get_entry(&raw)?;
                    Some(crate::vault_view::VaultEntryView::from_entry_with_agent(
                        stripped,
                        VaultSource::AgentPerUser,
                        Some(agent_id.clone()),
                        entry,
                        resolve_display,
                    ))
                })
                .collect();
            Some(entries)
        })
        .flatten()
        .collect()
}

// ── POST /api/me/vault/move ────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct MoveUserVaultBody {
    pub path: String,
    pub from_tier: String,
    pub to_tier: String,
    /// Optional destination path (rename). When set and different from `path`,
    /// the entry is renamed; it must also pass the allowed-path check. Omitted
    /// keeps the path and only changes tier.
    #[serde(default)]
    pub to_path: Option<String>,
    /// Required when either `from_tier` or `to_tier` is a per-agent tier
    /// (`agent_per_user`). Identifies which agent's vault to source from /
    /// write to. Ignored when both tiers are global (T2 only).
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Serialize)]
pub struct MoveUserVaultResponse {
    pub moved: String,
    pub to_path: String,
    pub from: String,
    pub to: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

pub async fn move_user_vault(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<MoveUserVaultBody>,
) -> ApiResult<MoveUserVaultResponse> {
    let identity_id = require_identity(&state, &headers)?;
    enforce_action_rate_limit(&state, &identity_id)?;
    let dest_logical = body.to_path.clone().unwrap_or_else(|| body.path.clone());
    if !is_allowed_path(&body.path) || !is_allowed_path(&dest_logical) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "Path is not in the allowed integration paths",
        ));
    }
    let from = parse_tier(&body.from_tier)?;
    let to = parse_tier(&body.to_tier)?;
    if !from.is_per_user() || !to.is_per_user() {
        return Err(err(
            StatusCode::FORBIDDEN,
            "Only per-user tier moves (agent_per_user ↔ global_per_user) are allowed from this endpoint; shared-tier moves require admin via the per-agent vault page",
        ));
    }
    if from == to && dest_logical == body.path {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "no-op move: same tier and same path (set to_path to rename)",
        ));
    }

    let global_vault = state
        .vault
        .as_ref()
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "Vault not configured"))?;
    let agent_vault = match (from, to) {
        (workforce_common::settings::VaultSource::AgentPerUser, _)
        | (_, workforce_common::settings::VaultSource::AgentPerUser) => {
            let aid = body.agent_id.as_deref().ok_or_else(|| {
                err(
                    StatusCode::BAD_REQUEST,
                    "agent_id is required when moving to/from agent_per_user",
                )
            })?;
            Some(
                state
                    .registry
                    .get(aid)
                    .ok_or_else(|| err(StatusCode::NOT_FOUND, &format!("agent '{aid}' not found")))?
                    .vault
                    .as_ref()
                    .ok_or_else(|| {
                        err(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "agent vault not configured",
                        )
                    })?
                    .clone(),
            )
        }
        _ => None,
    };

    let source_path = format!("users/{identity_id}/{}", body.path);
    let dest_path = format!("users/{identity_id}/{dest_logical}");
    let source_vault = vault_for_tier(from, agent_vault.as_ref(), global_vault);
    let dest_vault = vault_for_tier(to, agent_vault.as_ref(), global_vault);

    source_vault
        .sync()
        .and_then(|()| dest_vault.sync())
        .map_err(|e| vault_write_rejection(&e))?;
    let source_value = source_vault
        .get(&source_path)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "No value at source tier"))?;
    if !body.overwrite && dest_vault.get(&dest_path).is_some() {
        return Err(err(
            StatusCode::CONFLICT,
            "Destination tier already has a value; pass overwrite=true to replace",
        ));
    }
    crate::hub_core::logins::store::relocate(
        (source_vault, &source_path),
        (dest_vault, &dest_path),
        &source_value,
        || login_vaults(&state),
        workforce_common::settings::VaultActor::User(&identity_id),
    )
    .await
    .map_err(login_refusal)?;

    tracing::info!(
        identity_id = %identity_id,
        vault_path = %body.path,
        to_path = %dest_logical,
        from = from.label(),
        to = to.label(),
        agent_id = body.agent_id.as_deref().unwrap_or("-"),
        "Per-user vault entry moved"
    );

    Ok(Json(MoveUserVaultResponse {
        moved: body.path,
        to_path: dest_logical,
        from: from.label().to_string(),
        to: to.label().to_string(),
        agent_id: body.agent_id,
    }))
}

fn vault_for_tier<'a>(
    tier: workforce_common::settings::VaultSource,
    agent_vault: Option<&'a Arc<crate::vault::Vault>>,
    global_vault: &'a Arc<crate::vault::Vault>,
) -> &'a Arc<crate::vault::Vault> {
    use workforce_common::settings::VaultSource;
    match tier {
        VaultSource::AgentPerUser => agent_vault.expect("checked above"),
        _ => global_vault,
    }
}

fn parse_tier(
    s: &str,
) -> Result<workforce_common::settings::VaultSource, (StatusCode, Json<serde_json::Value>)> {
    use workforce_common::settings::VaultSource;
    match s {
        "agent_per_user" | "agent_user_scoped" => Ok(VaultSource::AgentPerUser),
        "global_per_user" | "global_user_scoped" | "user" => Ok(VaultSource::GlobalPerUser),
        "agent_specific" | "agent_shared_bare" | "agent_shared" => Ok(VaultSource::AgentSpecific),
        "global" | "global_shared_bare" => Ok(VaultSource::Global),
        other => Err(err(
            StatusCode::BAD_REQUEST,
            &format!("Unknown vault tier: {other}"),
        )),
    }
}

// ── POST /api/me/vault/{*path} ──────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SetVaultBody {
    value: String,
}

#[derive(Serialize)]
pub struct SetVaultResponse {
    stored: String,
}

pub async fn set_user_vault(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(path): Path<String>,
    Json(body): Json<SetVaultBody>,
) -> ApiResult<SetVaultResponse> {
    let identity_id = require_identity(&state, &headers)?;
    enforce_action_rate_limit(&state, &identity_id)?;

    if body.value.len() > MAX_SECRET_SIZE {
        return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "Secret value too large"));
    }

    if !is_allowed_path(&path) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "Path is not in the allowed integration paths",
        ));
    }

    let vault = state
        .vault
        .as_ref()
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "Vault not configured"))?;

    let full_path = format!("users/{identity_id}/{path}");
    crate::hub_core::logins::store::set(
        vault,
        &full_path,
        &body.value,
        || login_vaults(&state),
        workforce_common::settings::VaultActor::User(&identity_id),
    )
    .await
    .map_err(login_refusal)?;

    tracing::info!(
        identity_id = %identity_id,
        vault_path = %path,
        "Per-user secret stored"
    );

    Ok(Json(SetVaultResponse { stored: path }))
}

#[derive(Deserialize)]
pub struct SetUserSecretBody {
    path: String,
    value: String,
    /// `"global_per_user"` or `"agent_per_user"`.
    tier: String,
    /// Required when `tier == agent_per_user`.
    #[serde(default)]
    agent_id: Option<String>,
}

/// Validate a per-user-secret request and resolve where it addresses. Shared by
/// [`set_user_secret`] and [`delete_user_secret`] so the two halves of the
/// secret lifecycle stay in lock-step: the key must be safe and non-reserved
/// (any key — not the integration allowlist — because the write is hard-locked
/// to the caller's own per-user namespace), the tier must be a per-user one
/// (never a shared, agent-specific, or infra tier), and `agent_per_user`
/// requires a resolvable agent vault. Returns the tier and, for `agent_per_user`,
/// that agent's vault.
fn resolve_per_user_secret_target(
    state: &GatewayState,
    path: &str,
    tier: &str,
    agent_id: Option<&str>,
) -> Result<
    (
        workforce_common::settings::VaultSource,
        Option<Arc<crate::vault::Vault>>,
    ),
    (StatusCode, Json<serde_json::Value>),
> {
    use workforce_common::settings::VaultSource;
    if !has_safe_segments(path) {
        return Err(err(StatusCode::FORBIDDEN, "Invalid secret path"));
    }
    if crate::vault_policy::is_shared_only(path) {
        return Err(err(
            StatusCode::FORBIDDEN,
            &crate::vault_policy::shared_only_rejection_message(path),
        ));
    }
    let tier = parse_tier(tier)?;
    if !tier.is_per_user() {
        return Err(err(
            StatusCode::FORBIDDEN,
            "Secrets are per-user only (global_per_user or agent_per_user)",
        ));
    }
    let agent_vault = if tier == VaultSource::AgentPerUser {
        let aid = agent_id.ok_or_else(|| {
            err(
                StatusCode::BAD_REQUEST,
                "agent_id is required for agent_per_user",
            )
        })?;
        Some(
            state
                .registry
                .get(aid)
                .ok_or_else(|| err(StatusCode::NOT_FOUND, &format!("agent '{aid}' not found")))?
                .vault
                .as_ref()
                .ok_or_else(|| {
                    err(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "agent vault not configured",
                    )
                })?
                .clone(),
        )
    } else {
        None
    };
    Ok((tier, agent_vault))
}

/// Store a member's own secret at an arbitrary key in a **per-user tier only**
/// (global or agent). Unlike [`set_user_vault`] this isn't restricted to the
/// integration allowlist — a member may name any key — because the write is hard
/// locked to their own per-user namespace (isolated per user; never a shared,
/// agent-specific, or infra tier) and reserved admin-managed prefixes stay
/// denied. This is what backs workflow secret variables.
pub async fn set_user_secret(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Json(body): Json<SetUserSecretBody>,
) -> ApiResult<SetVaultResponse> {
    let identity_id = require_identity(&state, &headers)?;
    enforce_action_rate_limit(&state, &identity_id)?;

    if body.value.len() > MAX_SECRET_SIZE {
        return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "Secret value too large"));
    }
    let (tier, agent_vault) =
        resolve_per_user_secret_target(&state, &body.path, &body.tier, body.agent_id.as_deref())?;

    let global_vault = state
        .vault
        .as_ref()
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "Vault not configured"))?;
    let full_path = format!("users/{identity_id}/{}", body.path);
    let vault = vault_for_tier(tier, agent_vault.as_ref(), global_vault);
    crate::hub_core::logins::store::set(
        vault,
        &full_path,
        &body.value,
        || login_vaults(&state),
        workforce_common::settings::VaultActor::User(&identity_id),
    )
    .await
    .map_err(login_refusal)?;

    tracing::info!(
        identity_id = %identity_id,
        vault_path = %body.path,
        tier = tier.label(),
        "Per-user secret stored"
    );
    Ok(Json(SetVaultResponse { stored: body.path }))
}

#[derive(Deserialize)]
pub struct DeleteUserSecretQuery {
    path: String,
    tier: String,
    #[serde(default)]
    agent_id: Option<String>,
}

/// Remove a member's own per-user secret, the mirror of [`set_user_secret`]:
/// it accepts any safe, non-reserved key (not just the integration allowlist)
/// but stays hard-locked to the caller's own per-user namespace and tier, so a
/// member can fully manage — add *and* delete — the workflow secrets they
/// create. Deleting a missing key succeeds (idempotent).
pub async fn delete_user_secret(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Query(q): Query<DeleteUserSecretQuery>,
) -> ApiResult<DeleteVaultResponse> {
    let identity_id = require_identity(&state, &headers)?;
    enforce_action_rate_limit(&state, &identity_id)?;

    let (tier, agent_vault) =
        resolve_per_user_secret_target(&state, &q.path, &q.tier, q.agent_id.as_deref())?;

    let global_vault = state
        .vault
        .as_ref()
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "Vault not configured"))?;
    let full_path = format!("users/{identity_id}/{}", q.path);
    let vault = vault_for_tier(tier, agent_vault.as_ref(), global_vault);
    crate::hub_core::logins::store::delete(
        vault,
        &full_path,
        workforce_common::settings::VaultActor::User(&identity_id),
    )
    .await
    .map_err(|e| vault_write_rejection(&e))?;

    tracing::info!(
        identity_id = %identity_id,
        vault_path = %q.path,
        tier = tier.label(),
        "Per-user secret deleted"
    );
    Ok(Json(DeleteVaultResponse { deleted: q.path }))
}

// ── DELETE /api/me/vault/{*path} ────────────────────────────────────────────

#[derive(Serialize)]
pub struct DeleteVaultResponse {
    deleted: String,
}

pub async fn delete_user_vault(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Path(path): Path<String>,
) -> ApiResult<DeleteVaultResponse> {
    let identity_id = require_identity(&state, &headers)?;
    enforce_action_rate_limit(&state, &identity_id)?;

    if !is_allowed_path(&path) {
        return Err(err(StatusCode::FORBIDDEN, "Path not allowed"));
    }

    let vault = state
        .vault
        .as_ref()
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "Vault not configured"))?;

    let full_path = format!("users/{identity_id}/{path}");
    crate::hub_core::logins::store::delete(
        vault,
        &full_path,
        workforce_common::settings::VaultActor::User(&identity_id),
    )
    .await
    .map_err(|e| vault_write_rejection(&e))?;

    tracing::info!(
        identity_id = %identity_id,
        vault_path = %path,
        "Per-user secret deleted"
    );

    Ok(Json(DeleteVaultResponse { deleted: path }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_accepts_every_declared_prefix() {
        for prefix in ALLOWED_PREFIXES {
            let probe = format!("{prefix}whatever/sub/path");
            assert!(
                is_allowed_path(&probe),
                "expected {probe:?} to be allowed by prefix {prefix:?}",
            );
        }
    }

    #[test]
    fn allowlist_accepts_exact_prefix_string() {
        assert!(is_allowed_path("claude_code/oauth_token"));
        assert!(is_allowed_path("codex/auth_json"));
    }

    #[test]
    fn allowlist_accepts_per_user_speech_credentials() {
        assert!(is_allowed_path("speech/fish_audio/api_key"));
        assert!(is_allowed_path("speech/fish_audio/voice"));
        assert!(is_allowed_path("speech/elevenlabs/api_key"));
        assert!(is_allowed_path("speech/soniox/api_key"));
        assert!(is_allowed_path("speech/soniox/voice"));
        // Traversal still rejected inside the allowed prefix.
        assert!(!is_allowed_path("speech/../llm/accounts/x"));
    }

    #[test]
    fn a_person_may_add_their_own_integration_keys_but_not_the_service_keys() {
        assert!(is_allowed_path("decision/jev/api_key"));
        assert!(is_allowed_path("tools/brave_api_key"));
        assert!(!is_allowed_path("decision/jev/other"));
        assert!(!is_allowed_path("decision/kev/api_key"));
        for hub_wide in ["embeddings/openai/api_key", "embeddings/memory/api_key"] {
            assert!(!is_allowed_path(hub_wide));
            assert!(!is_member_writable(hub_wide));
        }
    }

    #[test]
    fn user_secret_gate_allows_arbitrary_safe_keys_but_not_reserved() {
        // set_user_secret's gate: any safe, non-reserved key — unlike the
        // integration allowlist used by set_user_vault.
        let gate = is_member_writable;
        assert!(gate("portal_password"));
        assert!(!is_allowed_path("portal_password"));
        assert!(gate("my/nested/key"));
        assert!(!gate("claude_code/oauth_client/secret"));
        assert!(!gate("skills/jira/default_workspace"));
        assert!(!gate("../escape"));
        assert!(!gate(""));
        // Per-user tiers only.
        assert!(parse_tier("global_per_user").unwrap().is_per_user());
        assert!(parse_tier("agent_per_user").unwrap().is_per_user());
        assert!(!parse_tier("global").unwrap().is_per_user());
    }

    #[test]
    fn shared_only_wins_over_allowlist() {
        // Pins the precedence in case a future change widens the allowlist
        // to a parent prefix (e.g. `skills/google/`).
        assert!(!is_allowed_path("skills/google/oauth_client/client_secret"));
        assert!(!is_allowed_path(
            "skills/freeagent/oauth_client/client_secret"
        ));
        assert!(!is_allowed_path("claude_code/oauth_client/client_secret"));
        assert!(!is_allowed_path("skills/jira/default_workspace"));
        assert!(!is_allowed_path("skills/postmark/default_workspace"));
        assert!(is_allowed_path("skills/jira/site"));
        assert!(is_allowed_path("skills/postmark/base_url"));
    }

    #[test]
    fn unknown_paths_are_rejected() {
        for path in [
            "",
            "/",
            "skills/unknown/foo",
            "users/other/skills/jira/api_token",
            "system/secret",
            "../etc/passwd",
        ] {
            assert!(!is_allowed_path(path), "expected {path:?} to be rejected");
        }
    }

    #[test]
    fn unsafe_segments_are_rejected_even_when_prefix_matches() {
        for path in [
            "skills/google/workspaces/../oauth_client/client_id",
            "skills/jira/./api_token",
            "skills/jira//api_token",
            "skills/jira/api_token/",
        ] {
            assert!(
                !is_allowed_path(path),
                "expected unsafe-segment {path:?} to be rejected",
            );
        }
    }

    #[test]
    fn scope_probe_targets_are_well_formed() {
        for (name, key) in SCOPE_PROBES {
            assert!(
                has_safe_segments(key),
                "scope probe {name:?} has malformed key {key:?}",
            );
        }
        assert!(has_safe_segments(GOOGLE_EMAIL_KEY));
    }

    #[test]
    fn integrations_no_longer_probe_login_keys() {
        assert!(SCOPE_PROBES.iter().all(|(_, key)| {
            workforce_common::subscription::SubscriptionKind::of_vault_key(key).is_none()
        }));
    }

    #[test]
    fn scope_value_marks_configured_iff_not_none() {
        let v = scope_value("personal");
        assert_eq!(v["configured"], serde_json::Value::Bool(true));
        assert_eq!(v["scope"], "personal");

        let v = scope_value("shared");
        assert_eq!(v["configured"], serde_json::Value::Bool(true));

        let v = scope_value("none");
        assert_eq!(v["configured"], serde_json::Value::Bool(false));
        assert_eq!(v["scope"], "none");
    }

    #[test]
    fn parse_tier_accepts_new_and_legacy_labels() {
        use workforce_common::settings::VaultSource;
        assert_eq!(
            parse_tier("agent_per_user").unwrap(),
            VaultSource::AgentPerUser
        );
        assert_eq!(
            parse_tier("agent_user_scoped").unwrap(),
            VaultSource::AgentPerUser
        );
        assert_eq!(
            parse_tier("global_per_user").unwrap(),
            VaultSource::GlobalPerUser
        );
        assert_eq!(
            parse_tier("global_user_scoped").unwrap(),
            VaultSource::GlobalPerUser
        );
        assert_eq!(parse_tier("user").unwrap(), VaultSource::GlobalPerUser);
        assert_eq!(
            parse_tier("agent_specific").unwrap(),
            VaultSource::AgentSpecific
        );
        assert_eq!(
            parse_tier("agent_shared").unwrap(),
            VaultSource::AgentSpecific
        );
        assert_eq!(parse_tier("global").unwrap(), VaultSource::Global);
        assert!(parse_tier("nonsense").is_err());
    }

    #[test]
    fn vault_for_tier_routes_per_agent_to_agent_vault() {
        use crate::vault::Vault;
        use workforce_common::settings::VaultSource;
        let tmp = tempfile::tempdir().unwrap();
        let global = Arc::new(Vault::open(&tmp.path().join("g.enc"), "k").unwrap());
        let agent = Arc::new(Vault::open(&tmp.path().join("a.enc"), "k").unwrap());
        let chosen_t1 = vault_for_tier(VaultSource::AgentPerUser, Some(&agent), &global);
        assert!(Arc::ptr_eq(chosen_t1, &agent));
        let chosen_t2 = vault_for_tier(VaultSource::GlobalPerUser, Some(&agent), &global);
        assert!(Arc::ptr_eq(chosen_t2, &global));
    }
}
