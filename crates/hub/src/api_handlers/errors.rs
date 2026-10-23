//! Standardized API error types for the DW-425 management endpoints.
//!
//! Provides a consistent JSON error envelope `{ "error": "...", "detail": "..." }`
//! across all config, policy, engagement, and memory endpoints.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

/// Structured API error with status code and JSON body.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub error: String,
    pub detail: Option<String>,
}

/// JSON error body returned to clients.
#[derive(Serialize)]
struct ErrorBody {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl ApiError {
    /// Create a new API error with the given status and message.
    pub fn new(status: StatusCode, error: impl Into<String>) -> Self {
        Self {
            status,
            error: error.into(),
            detail: None,
        }
    }

    /// Add a detail message to the error.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// 400 Bad Request.
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, msg)
    }

    /// 401 Unauthorized.
    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized")
    }

    /// 403 Forbidden.
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, msg)
    }

    /// 404 Not Found.
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, msg)
    }

    /// 409 Conflict.
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, msg)
    }

    /// 500 Internal Server Error.
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, msg)
    }

    /// 503 Service Unavailable (no agent service running).
    pub fn service_unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "no agent service available",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: self.error,
            detail: self.detail,
        };
        (self.status, Json(body)).into_response()
    }
}

impl From<std::io::Error> for ApiError {
    fn from(err: std::io::Error) -> Self {
        match err.kind() {
            std::io::ErrorKind::NotFound => ApiError::not_found("file not found"),
            std::io::ErrorKind::PermissionDenied => ApiError::forbidden("permission denied"),
            _ => ApiError::internal("I/O error").with_detail(err.to_string()),
        }
    }
}

impl From<crate::gateway::ScopeRejection> for ApiError {
    fn from(r: crate::gateway::ScopeRejection) -> Self {
        match r {
            crate::gateway::ScopeRejection::Unauthenticated => ApiError::unauthorized(),
            crate::gateway::ScopeRejection::MissingScope => ApiError::forbidden("forbidden"),
        }
    }
}

/// Map the `(StatusCode, Json<Value>)` rejection the gateway capability guards
/// (`require_capability_read` / `require_capability_action`) return into the
/// `ApiError` envelope the management endpoints use, preserving the status and
/// the `error` message. One place, so every `/api/permissions/*` and
/// `/api/policy/*` site adapts with a bare `?`.
impl From<(StatusCode, Json<serde_json::Value>)> for ApiError {
    fn from((status, body): (StatusCode, Json<serde_json::Value>)) -> Self {
        let message = body
            .0
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("admin action denied")
            .to_string();
        ApiError::new(status, message)
    }
}
