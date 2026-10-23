//! Admin API for identity lifecycle operations: what an identity holds,
//! suspending and reactivating it, merging it into another, transferring
//! one item, deleting it, and
//! cleaning up after identities that no longer exist. Every route needs an
//! active System Admin; A dry run
//! answers with its report; a real merge, delete or clean-up answers `202`
//! with its operation record as soon as it is claimed, and runs on in the
//! hub: `GET /api/identity-lifecycle/operations/{id}` reads its progress and
//! then its report.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use crate::gateway::GatewayState;
use crate::hub_core::roles::Actor;
use crate::identity_lifecycle::{DeletePlan, Item, Launched, Lifecycle, LifecycleError, Request};

type ApiError = (StatusCode, Json<serde_json::Value>);
type ApiResult = Result<Json<serde_json::Value>, ApiError>;
type LaunchResult = Result<(StatusCode, Json<serde_json::Value>), ApiError>;

fn err(status: StatusCode, msg: &str) -> ApiError {
    (status, Json(serde_json::json!({ "error": msg })))
}

/// An active System Admin, resolved live so a suspended one is refused, and
/// the installed lifecycle.
async fn admin(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
) -> Result<(Actor, Arc<Lifecycle>), ApiError> {
    let principal = super::identity::require_roles_assign(state, headers).await?;
    if !crate::authz::is_effective_admin(principal.role, principal.status) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "Requires an active System Admin",
        ));
    }
    let lifecycle = crate::identity_lifecycle::installed().ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "Identity service not configured",
        )
    })?;
    Ok((Actor(principal.identity_id), lifecycle))
}

fn subject(id: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(id).map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid identity ID"))
}

fn refusal(e: LifecycleError) -> ApiError {
    let status = match &e {
        LifecycleError::NotFound(_) => StatusCode::NOT_FOUND,
        LifecycleError::Refused(_) => StatusCode::BAD_REQUEST,
        LifecycleError::Busy(_) => StatusCode::CONFLICT,
        LifecycleError::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    err(status, &format!("{e:#}"))
}

pub(crate) fn render(outcome: Result<impl serde::Serialize, LifecycleError>) -> ApiResult {
    serde_json::to_value(outcome.map_err(refusal)?)
        .map(Json)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))
}

/// A dry run's report, or `202` with the record of the run it started.
async fn launch(
    lifecycle: &Arc<Lifecycle>,
    request: Request,
    actor: Actor,
    dry_run: bool,
) -> LaunchResult {
    match lifecycle.launch(request, actor, dry_run).await {
        Ok(Launched::Report(report)) => render(Ok(report)).map(|json| (StatusCode::OK, json)),
        Ok(Launched::Started(op)) => render(Ok(serde_json::json!({ "operation": op })))
            .map(|json| (StatusCode::ACCEPTED, json)),
        Err(e) => Err(refusal(e)),
    }
}

pub async fn holdings(
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    let (actor, lifecycle) = admin(&state, &headers).await?;
    render(lifecycle.holdings(subject(&id)?, &actor).await)
}

pub async fn suspend(
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    let (actor, lifecycle) = admin(&state, &headers).await?;
    render(lifecycle.suspend(subject(&id)?, &actor).await)
}

pub async fn resume(
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    let (actor, lifecycle) = admin(&state, &headers).await?;
    render(lifecycle.resume(subject(&id)?, &actor).await)
}

fn dry_run_default() -> bool {
    true
}

#[derive(Deserialize)]
pub struct MergeRequest {
    pub into: Uuid,
    #[serde(default = "dry_run_default")]
    pub dry_run: bool,
}

pub async fn merge(
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<MergeRequest>,
) -> LaunchResult {
    let (actor, lifecycle) = admin(&state, &headers).await?;
    let request = Request::Merge {
        from: subject(&id)?,
        into: req.into,
    };
    launch(&lifecycle, request, actor, req.dry_run).await
}

#[derive(Deserialize)]
pub struct TransferRequest {
    pub item: Item,
    pub to: Uuid,
}

pub async fn transfer(
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<TransferRequest>,
) -> ApiResult {
    let (actor, lifecycle) = admin(&state, &headers).await?;
    render(
        lifecycle
            .transfer(subject(&id)?, &req.item, req.to, &actor)
            .await,
    )
}

#[derive(Deserialize)]
pub struct DeleteRequest {
    #[serde(flatten)]
    pub plan: DeletePlan,
    #[serde(default = "dry_run_default")]
    pub dry_run: bool,
}

pub async fn delete(
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<DeleteRequest>,
) -> LaunchResult {
    let (actor, lifecycle) = admin(&state, &headers).await?;
    let request = Request::Delete {
        subject: subject(&id)?,
        plan: req.plan,
    };
    launch(&lifecycle, request, actor, req.dry_run).await
}

/// `DELETE /api/identities/{id}`: a delete with the default for every
/// category, for real.
pub async fn delete_with_defaults(
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> LaunchResult {
    let (actor, lifecycle) = admin(&state, &headers).await?;
    let request = Request::Delete {
        subject: subject(&id)?,
        plan: DeletePlan::default(),
    };
    launch(&lifecycle, request, actor, false).await
}

/// `GET /api/identity-lifecycle/orphans`: what identities that no longer
/// exist left behind. Reads only.
pub async fn orphans(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    let (actor, lifecycle) = admin(&state, &headers).await?;
    render(lifecycle.orphans(&actor).await)
}

#[derive(Deserialize)]
pub struct OrphanTransferRequest {
    pub to: Uuid,
    /// Move the missing identity's conversations, memories, notes and
    /// documents to `to` as well; by default they stay where they are.
    #[serde(default)]
    pub move_personal: bool,
    #[serde(default = "dry_run_default")]
    pub dry_run: bool,
}

pub async fn transfer_orphan(
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<OrphanTransferRequest>,
) -> LaunchResult {
    let (actor, lifecycle) = admin(&state, &headers).await?;
    let request = Request::TransferOrphan {
        id: subject(&id)?,
        to: req.to,
        personal: req.move_personal,
    };
    launch(&lifecycle, request, actor, req.dry_run).await
}

pub async fn delete_orphan(
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<DeleteRequest>,
) -> LaunchResult {
    let (actor, lifecycle) = admin(&state, &headers).await?;
    let request = Request::DeleteOrphan {
        id: subject(&id)?,
        plan: req.plan,
    };
    launch(&lifecycle, request, actor, req.dry_run).await
}

#[derive(Deserialize)]
pub struct OperationsQuery {
    pub identity: Option<Uuid>,
    pub limit: Option<usize>,
}

pub async fn operations(
    headers: axum::http::HeaderMap,
    Query(query): Query<OperationsQuery>,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    let (_, lifecycle) = admin(&state, &headers).await?;
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    render(
        lifecycle
            .operations(query.identity, limit)
            .map(|ops| serde_json::json!({ "operations": ops }))
            .map_err(LifecycleError::Failed),
    )
}

/// `GET /api/identity-lifecycle/operations/{id}`: one operation's record,
/// with its progress while it runs and its report once it ends.
pub async fn operation(
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> ApiResult {
    let (_, lifecycle) = admin(&state, &headers).await?;
    let id =
        Uuid::parse_str(&id).map_err(|_| err(StatusCode::BAD_REQUEST, "Invalid operation ID"))?;
    render(
        lifecycle
            .operation(id)
            .map(|op| serde_json::json!({ "operation": op })),
    )
}
