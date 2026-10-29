//! Connecting a device to an account from the browser.
//!
//! A device asks for a code (`POST /api/pair/device/request`) and shows its
//! person the `approve_url`. They sign in on the hub's `/pair` page — through
//! the hub's own login or its SSO — and approve the code into **their own**
//! account; the device collects its token from the status poll. Who the device
//! belongs to is decided by who signed in, never by the email or id the device
//! claimed when it asked.
//!
//! - `GET  /api/pair/device/pending/{code}` — what the page shows: the device's
//!   name and kind, and when it asked. Nothing about who asked.
//! - `POST /api/pair/device/approve-self` — approve into the signed-in person.
//! - `POST /api/pair/device/reject-self` — turn the code down.
//! - `POST /api/pair/device/approve` — an admin assigns a waiting device to a
//!   person they choose.
//!
//! `[pairing] who_can_connect = "admins"` limits approving to admins.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use tracing::info;
use workforce_identity::types::{Identity, IdentityStatus};
use workforce_pairing::types::{WhoCanConnect, DEFAULT_DEVICE_KIND};
use workforce_pairing::DevicePairingService;

use crate::device_link::{self, ApproveError, LinkChoice};
use crate::gateway::{CallerContext, GatewayState};

type Rejection = (StatusCode, Json<serde_json::Value>);
type ApiResult<T> = Result<Json<T>, Rejection>;

fn err(status: StatusCode, msg: impl Into<String>) -> Rejection {
    (status, Json(serde_json::json!({ "error": msg.into() })))
}

fn refused(why: ApproveError) -> Rejection {
    err(why.status(), why.message())
}

/// Approvals and rejections a person may make: ten in ten minutes is far more
/// than connecting their own devices needs, and far too few to sweep codes.
static CONNECT_LIMITER: LazyLock<crate::rate_limit::IdentityActionLimiter> =
    LazyLock::new(|| crate::rate_limit::IdentityActionLimiter::new(10, Duration::from_secs(600)));

fn connect_rate_limit(identity: &Identity) -> Result<(), Rejection> {
    CONNECT_LIMITER
        .check_and_record(&identity.id.to_string())
        .map_err(|retry_after| {
            (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({
                    "error": format!(
                        "Too many pairing attempts. Try again in {retry_after} seconds."
                    ),
                    "retry_after_secs": retry_after,
                })),
            )
        })
}

fn device_pairing(state: &GatewayState) -> Result<&Arc<DevicePairingService>, Rejection> {
    state.device_pairing.as_ref().ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            "Device pairing is not enabled on this hub.",
        )
    })
}

/// The person signed in to the browser: a JWT session for an identity that
/// can hold a device. The gateway's admin token and a paired device's own
/// token are not a person.
struct SignedIn {
    caller: CallerContext,
    identity: Identity,
    is_admin: bool,
}

async fn signed_in_person(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
) -> Result<SignedIn, Rejection> {
    let caller = crate::gateway::resolve_caller_context(state, headers, None).await;
    if !caller.is_authenticated() {
        return Err(err(
            StatusCode::UNAUTHORIZED,
            "Sign in to connect a device.",
        ));
    }
    let not_a_person = || {
        err(
            StatusCode::FORBIDDEN,
            "Sign in with your own account to connect a device.",
        )
    };
    if caller.device_id.is_some() {
        return Err(not_a_person());
    }
    let id = caller
        .identity_id
        .as_deref()
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
        .ok_or_else(not_a_person)?;
    let store = state.identity_store.as_ref().ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "This hub has no accounts to connect a device to.",
        )
    })?;
    let identity = store
        .get_by_id(id)
        .map_err(|e| {
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Looking up your account failed: {e}"),
            )
        })?
        .ok_or_else(not_a_person)?;
    if identity.status == IdentityStatus::Suspended {
        return Err(err(
            StatusCode::FORBIDDEN,
            "Your account is suspended, so it can't connect a device.",
        ));
    }
    let principal = crate::gateway::gateway_principal(&caller, state.identity_store.as_ref());
    let is_admin = crate::authz::is_effective_admin(principal.role, principal.status);
    Ok(SignedIn {
        caller,
        identity,
        is_admin,
    })
}

/// `[pairing] who_can_connect = "admins"` keeps approving to admins.
pub(crate) const ADMINS_ONLY: &str =
    "Only admins can connect devices on this workspace. Ask an admin to connect it for you.";

fn may_connect(pairing: &DevicePairingService, is_admin: bool) -> Result<(), Rejection> {
    if pairing.config().who_can_connect == WhoCanConnect::Admins && !is_admin {
        return Err(err(StatusCode::FORBIDDEN, ADMINS_ONLY));
    }
    Ok(())
}

/// `GET /api/pair/device/pending/{code}` — what the `/pair` page shows about a
/// waiting code. Deliberately not the hint, client id or key: nothing about
/// who asked beyond the name the device gave itself.
pub async fn pending(
    Path(code): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult<serde_json::Value> {
    let signed = signed_in_person(&state, &headers).await?;
    if let Err(retry_after) =
        crate::gateway::check_action_rate_limit(state.action_limiter.as_ref(), &signed.caller)
    {
        return Err(crate::gateway::too_many_requests_err(retry_after));
    }
    let pairing = device_pairing(&state)?;
    let req = device_link::pending_request(pairing, &device_link::normalise_code(&code))
        .await
        .map_err(refused)?;
    Ok(Json(serde_json::json!({
        "code": req.code,
        "device_name": req.device_name,
        "device_kind": req.device_kind.as_deref().unwrap_or(DEFAULT_DEVICE_KIND),
        "requested_at": req.created_at.to_rfc3339(),
        "expires_at": req.expires_at.to_rfc3339(),
    })))
}

#[derive(Deserialize)]
pub struct CodeBody {
    pub code: String,
}

/// `POST /api/pair/device/approve-self {code}` — connect the device to the
/// signed-in person. The device's own `identity_hint` plays no part.
pub async fn approve_self(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<CodeBody>,
) -> ApiResult<serde_json::Value> {
    let signed = signed_in_person(&state, &headers).await?;
    let pairing = device_pairing(&state)?;
    may_connect(pairing, signed.is_admin)?;
    connect_rate_limit(&signed.identity)?;

    let id = signed.identity.id.to_string();
    let approval = device_link::approve_and_link(
        pairing,
        state.identity_store.as_deref(),
        &device_link::normalise_code(&body.code),
        &format!("self:{id}"),
        LinkChoice::Identity {
            id: signed.identity.id,
            by_admin: false,
        },
    )
    .await
    .map_err(refused)?;
    device_link::audit_device_pair("self", Some(&id), &approval);

    Ok(Json(serde_json::json!({
        "approved": true,
        "device_id": approval.device.id,
        "device_name": approval.device.device_name,
        "identity": device_link::identity_summary(&signed.identity),
    })))
}

/// `POST /api/pair/device/reject-self {code}` — turn a code down from the
/// `/pair` page.
pub async fn reject_self(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<CodeBody>,
) -> ApiResult<serde_json::Value> {
    let signed = signed_in_person(&state, &headers).await?;
    let pairing = device_pairing(&state)?;
    connect_rate_limit(&signed.identity)?;
    let code = device_link::normalise_code(&body.code);
    device_link::pending_request(pairing, &code)
        .await
        .map_err(refused)?;
    pairing
        .reject_async(code)
        .await
        .map_err(|_| refused(ApproveError::AlreadyUsed))?;
    info!(identity_id = %signed.identity.id, "Device pairing rejected from the pair page");
    Ok(Json(serde_json::json!({ "rejected": true })))
}

#[derive(Deserialize)]
pub struct AssignBody {
    pub code: String,
    /// The person to connect the device to: an identity UUID, or an email.
    pub identity_id: String,
}

/// `POST /api/pair/device/approve {code, identity_id}` — an admin connects a
/// waiting device to the person they chose. Admin-only, like every other
/// pairing administration route.
pub async fn assign(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<AssignBody>,
) -> ApiResult<serde_json::Value> {
    let caller =
        crate::gateway::require_scope(&state, &headers, None, crate::jwt::SCOPE_ADMIN).await?;
    let pairing = device_pairing(&state)?;
    let store = state.identity_store.as_ref().ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "This hub has no accounts to connect a device to.",
        )
    })?;
    let identity = device_link::resolve_named_identity(store.as_ref(), &body.identity_id)
        .map_err(|why| err(StatusCode::NOT_FOUND, why))?;

    let assigner = caller.identity_id.clone();
    let approval = device_link::approve_and_link(
        pairing,
        Some(store.as_ref()),
        &device_link::normalise_code(&body.code),
        &format!("admin:{}", assigner.as_deref().unwrap_or("gateway-token")),
        LinkChoice::Identity {
            id: identity.id,
            by_admin: true,
        },
    )
    .await
    .map_err(refused)?;
    device_link::audit_device_pair("admin_assign", assigner.as_deref(), &approval);

    Ok(Json(serde_json::json!({
        "approved": true,
        "device_id": approval.device.id,
        "device_name": approval.device.device_name,
        "identity": device_link::identity_summary(&identity),
    })))
}
