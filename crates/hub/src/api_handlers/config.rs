//! Configuration management API endpoints (DW-425).
//!
//! - `GET /api/config` — Read effective merged configuration (with `?section=` filter).
//! - `PUT /api/config` — Update configuration fields (owner-only, TOML merge).
//! - `GET /api/config/schema` — Configuration schema for frontend form building.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
use tracing::info;
use workforce_common::api_types::ConfigSchemaResponse;

use crate::gateway::GatewayState;

use super::errors::ApiError;

#[derive(Debug, Deserialize)]
pub struct ConfigQuery {
    pub token: Option<String>,
    pub section: Option<String>,
}

/// GET /api/config — Read effective merged configuration.
pub async fn config_get(
    Query(query): Query<ConfigQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let _caller = crate::gateway::require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    let service = state
        .default_service()
        .ok_or_else(ApiError::service_unavailable)?;
    let config = &service.config;

    let section = query.section.as_deref();
    let result = match section {
        Some("llm") => serde_json::json!({ "llm": config.llm }),
        Some("compaction") => serde_json::json!({ "compaction": config.compaction }),
        Some("gateway") => serde_json::json!({ "gateway": config.gateway }),
        Some("hub") => serde_json::json!({
            "data_dir": config.hub.data_dir,
        }),
        Some("heartbeat") => serde_json::json!({ "heartbeat": config.heartbeat }),
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "unknown config section: '{other}'. Valid sections: llm, compaction, gateway, hub, heartbeat"
            )));
        }
        None => serde_json::json!({
            "llm": config.llm,
            "compaction": config.compaction,
            "gateway": config.gateway,
            "heartbeat": config.heartbeat,
        }),
    };

    Ok(Json(result))
}

/// PUT /api/config — Update configuration fields (owner-only, TOML merge).
pub async fn config_put(
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !super::check_owner_auth(&state, &headers, query.token.as_deref()) {
        return Err(ApiError::unauthorized());
    }
    let service = state
        .default_service()
        .ok_or_else(ApiError::service_unavailable)?;
    let config_path = service
        .config_path
        .as_ref()
        .ok_or_else(|| ApiError::internal("config path not available"))?;

    // Validate the body is a JSON object
    if !body.is_object() {
        return Err(ApiError::bad_request(
            "request body must be a JSON object with config fields to update",
        ));
    }

    // Read current config file, apply updates, and write back
    let current = std::fs::read_to_string(config_path)
        .map_err(|e| ApiError::internal("failed to read config file").with_detail(e.to_string()))?;
    let mut doc: toml::Value = current.parse().map_err(|e: toml::de::Error| {
        ApiError::internal("failed to parse config TOML").with_detail(e.to_string())
    })?;

    merge_json_into_toml(&mut doc, &body);

    let new_content = doc.to_string();
    std::fs::write(config_path, &new_content).map_err(|e| {
        ApiError::internal("failed to write config file").with_detail(e.to_string())
    })?;

    info!("Config updated via API");
    Ok(Json(serde_json::json!({
        "updated": true,
        "message": "Configuration saved. Restart may be required for some changes to take effect."
    })))
}

/// GET /api/config/schema — Configuration schema for frontend form building.
///
/// Returns a JSON schema describing all configurable sections and their fields,
/// including types, defaults, validation constraints, and select options.
/// The frontend uses this to dynamically build configuration forms.
pub async fn config_schema(
    Query(query): Query<crate::gateway::WsQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<ConfigSchemaResponse>, ApiError> {
    let _caller = crate::gateway::require_scope(
        &state,
        &headers,
        query.token.as_deref(),
        crate::jwt::SCOPE_ADMIN,
    )
    .await?;
    Ok(Json(ConfigSchemaResponse::build()))
}

/// Recursively merge a JSON value into a TOML document.
fn merge_json_into_toml(toml_val: &mut toml::Value, json_val: &serde_json::Value) {
    match (toml_val, json_val) {
        (toml::Value::Table(table), serde_json::Value::Object(obj)) => {
            for (k, v) in obj {
                if let Some(existing) = table.get_mut(k) {
                    merge_json_into_toml(existing, v);
                } else if let Ok(tv) = json_to_toml(v) {
                    table.insert(k.clone(), tv);
                }
            }
        }
        (dest, src) => {
            if let Ok(tv) = json_to_toml(src) {
                *dest = tv;
            }
        }
    }
}

/// Convert a serde_json::Value to a toml::Value.
fn json_to_toml(v: &serde_json::Value) -> Result<toml::Value, ()> {
    match v {
        serde_json::Value::Null => Err(()),
        serde_json::Value::Bool(b) => Ok(toml::Value::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(toml::Value::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(toml::Value::Float(f))
            } else {
                Err(())
            }
        }
        serde_json::Value::String(s) => Ok(toml::Value::String(s.clone())),
        serde_json::Value::Array(arr) => {
            let items: Result<Vec<_>, _> = arr.iter().map(json_to_toml).collect();
            Ok(toml::Value::Array(items?))
        }
        serde_json::Value::Object(obj) => {
            let mut table = toml::map::Map::new();
            for (k, v) in obj {
                table.insert(k.clone(), json_to_toml(v)?);
            }
            Ok(toml::Value::Table(table))
        }
    }
}
