//! Shadow mode's admin routes (DW-676, PR 9c), an administrator's alone:
//! `GET shadow/status` and `GET shadow/report?store=&week=`, under
//! `/api/admin` on the gateway (`/api` on the standalone admin port).
//!
//! The report is read from Postgres alone, so whichever hub replica
//! answers gives the same one. The status's process counts are the
//! answering replica's, and say so.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{Duration as Days, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::json;

use crate::shadow_report;
use crate::shadow_status;
use crate::state::HubState;

/// The week a report is of when none is asked: the seven whole UTC days
/// before `today`.
pub fn default_week(today: NaiveDate) -> NaiveDate {
    today - Days::days(shadow_report::WEEK_DAYS)
}

fn said(status: StatusCode, error: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(json!({ "error": error.into() })))
}

/// This hub process's shadow counts, the mirror's backlog, and the weekly
/// verdicts recorded.
pub async fn status(State(state): State<Arc<HubState>>) -> (StatusCode, Json<serde_json::Value>) {
    let config = &state.config;
    let backlog = shadow_status::backlog(config).await;
    let verdicts = match crate::shadow::opened_store() {
        Some(tables) => match tables.verdicts(None, 8).await {
            Ok(v) => json!(v
                .iter()
                .map(|v| json!({
                    "store": v.store, "week_start": v.week_start, "week_end": v.week_end,
                    "verdict": v.verdict, "recorded_at": v.recorded_at,
                }))
                .collect::<Vec<_>>()),
            Err(e) => json!({ "error": format!("{e:#}") }),
        },
        None => serde_json::Value::Null,
    };
    (
        StatusCode::OK,
        Json(json!({
            "stores": config.shadowed(),
            "paused": config.vellev.shadow.paused,
            "paused_note": "read once at start-up: a change needs every hub replica restarted",
            "process": shadow_status::health_json(
                &shadow_status::process_state(),
                config.vellev.shadow.paused,
            ),
            "memory_backlog": shadow_status::backlog_json(backlog.as_ref()),
            "verdicts": verdicts,
        })),
    )
}

#[derive(Debug, Deserialize)]
pub struct ReportQuery {
    pub store: String,
    /// The week's first UTC day, `YYYY-MM-DD`; the seven days before today
    /// when left out.
    pub week: Option<NaiveDate>,
}

/// A store's week, judged from Postgres.
pub async fn report(
    State(state): State<Arc<HubState>>,
    Query(query): Query<ReportQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(store) = shadow_report::store_named(&query.store) else {
        return said(
            StatusCode::BAD_REQUEST,
            format!("no shadowed store {:?}: memories or artifacts", query.store),
        );
    };
    // Only the tables this process opened: a route builds and migrates
    // nothing.
    if state.config.shadowed().is_empty() {
        return said(StatusCode::NOT_FOUND, "nothing is shadowed on this hub");
    }
    let Some(tables) = crate::shadow::opened_store() else {
        return said(
            StatusCode::SERVICE_UNAVAILABLE,
            "the shadow tables are not open in this hub process yet",
        );
    };
    let from = query
        .week
        .unwrap_or_else(|| default_week(Utc::now().date_naive()));
    match shadow_report::gather(&tables, &state.config, store, from).await {
        Ok(input) => (
            StatusCode::OK,
            Json(shadow_report::to_json(&shadow_report::judge(&input))),
        ),
        Err(e) => said(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}
