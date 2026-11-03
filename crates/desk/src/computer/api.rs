//! Desk's routes for what agents may touch on this computer, under the
//! desk's own [`Authorize`].
//!
//! - `GET /api/desk/computer/disk-access`: whether macOS lets the hub read
//!   other apps' files (Full Disk Access), as the hub itself finds it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::Authorize;
use crate::service::Desk;

/// Whether the hub may read the files macOS keeps from apps without Full
/// Disk Access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskAccess {
    Granted,
    Missing,
    /// It couldn't be told: not macOS, or the probe failed some other way.
    Unknown,
}

/// The file only Full Disk Access opens: the privacy database itself.
fn tcc_db(home: &Path) -> PathBuf {
    home.join("Library/Application Support/com.apple.TCC/TCC.db")
}

/// [`DiskAccess`] as opening `path` read-only finds it.
pub fn probe(path: &Path) -> DiskAccess {
    match std::fs::File::open(path) {
        Ok(_) => DiskAccess::Granted,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => DiskAccess::Missing,
        Err(_) => DiskAccess::Unknown,
    }
}

/// [`probe`] of this computer.
pub fn disk_access() -> DiskAccess {
    if !cfg!(target_os = "macos") {
        return DiskAccess::Unknown;
    }
    match std::env::var_os("HOME") {
        Some(home) => probe(&tcc_db(Path::new(&home))),
        None => DiskAccess::Unknown,
    }
}

struct ComputerApi {
    #[allow(dead_code)]
    desk: Desk,
    auth: Arc<dyn Authorize>,
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

type S = State<Arc<ComputerApi>>;

async fn guard(s: &ComputerApi, h: &HeaderMap, q: &TokenQuery) -> Result<(), Response> {
    s.auth
        .authorize(h, q.token.as_deref())
        .await
        .map(drop)
        .map_err(|status| (status, Json(json!({ "error": "not allowed" }))).into_response())
}

async fn get_disk_access(State(s): S, h: HeaderMap, Query(q): Query<TokenQuery>) -> Response {
    if let Err(r) = guard(&s, &h, &q).await {
        return r;
    }
    let state = tokio::task::spawn_blocking(disk_access)
        .await
        .unwrap_or(DiskAccess::Unknown);
    (StatusCode::OK, Json(json!({ "state": state }))).into_response()
}

pub(crate) fn computer_router(desk: Desk, auth: Arc<dyn Authorize>) -> Router {
    Router::new()
        .route("/api/desk/computer/disk-access", get(get_disk_access))
        .with_state(Arc::new(ComputerApi { desk, auth }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_reads_granted_missing_and_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("TCC.db");
        std::fs::write(&db, b"x").unwrap();
        assert_eq!(probe(&db), DiskAccess::Granted);
        assert_eq!(probe(&dir.path().join("absent.db")), DiskAccess::Unknown);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o000)).unwrap();
            // Root reads it anyway; everyone else is refused, as without FDA.
            if std::fs::File::open(&db).is_err() {
                assert_eq!(probe(&db), DiskAccess::Missing);
            }
        }
        assert_eq!(
            tcc_db(Path::new("/Users/sarah")),
            PathBuf::from("/Users/sarah/Library/Application Support/com.apple.TCC/TCC.db")
        );
        assert_eq!(
            serde_json::to_value(DiskAccess::Missing).unwrap(),
            "missing"
        );
    }
}
