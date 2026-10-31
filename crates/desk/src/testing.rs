//! Stand-ins for the hub's side of the desk routes.

use async_trait::async_trait;
use axum::http::{HeaderMap, StatusCode};

use crate::api::Authorize;
use crate::realtime::{KeySource, Provider};
use crate::speech::{Audio, Speak, SpeakError, Speech};

/// Lets in `Bearer hub-token` alone.
pub(crate) struct Bearer;

#[async_trait]
impl Authorize for Bearer {
    async fn authorize(
        &self,
        h: &HeaderMap,
        _: Option<&str>,
    ) -> Result<Option<String>, StatusCode> {
        match h.get("authorization").and_then(|v| v.to_str().ok()) {
            Some("Bearer hub-token") => Ok(None),
            _ => Err(StatusCode::UNAUTHORIZED),
        }
    }
}

/// Holds no voice keys.
pub(crate) struct NoKeys;

impl KeySource for NoKeys {
    fn api_key(&self, _: Provider) -> Option<String> {
        None
    }
    fn how_to_add(&self, provider: Provider) -> String {
        format!("add speech/{}/api_key", provider.name())
    }
}

/// Speaks nothing.
pub(crate) struct Mute;

#[async_trait]
impl Speech for Mute {
    async fn speak(&self, request: &Speak) -> Result<Audio, SpeakError> {
        Err(SpeakError::NoKey(request.provider))
    }
}

struct Fixed(Result<Option<String>, StatusCode>);

#[async_trait]
impl Authorize for Fixed {
    async fn authorize(
        &self,
        _: &HeaderMap,
        _: Option<&str>,
    ) -> Result<Option<String>, StatusCode> {
        self.0.clone()
    }
}

/// Lets everyone in.
pub(crate) fn allow_all() -> std::sync::Arc<dyn Authorize> {
    std::sync::Arc::new(Fixed(Ok(None)))
}

/// Lets no one in.
pub(crate) fn deny_all() -> std::sync::Arc<dyn Authorize> {
    std::sync::Arc::new(Fixed(Err(StatusCode::UNAUTHORIZED)))
}

/// Options for a desk whose CLIs never run.
pub(crate) fn options(dir: &std::path::Path) -> crate::service::Options {
    crate::service::Options {
        programs: crate::testing::programs("/bin/false".into(), "/bin/false".into()),
        env: Default::default(),
        run_dir: dir.join("run"),
        scrollback_bytes: 1024,
        approval_timeout: std::time::Duration::from_secs(30),
        restore: crate::service::RestorePolicy::Ask,
        adopt_every: None,
        holder: None,
        hub_internal: Default::default(),
    }
}

/// A desk on `pool`, its migrations applied.
pub(crate) async fn desk_on(
    pool: workforce_postgres::PostgresPool,
    dir: &std::path::Path,
) -> crate::service::Desk {
    crate::store::DeskStore::migrate(&pool).await.unwrap();
    crate::service::Desk::start(
        crate::store::DeskStore::from_pool(pool),
        options(dir),
        Default::default(),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap()
}

/// Programs for Claude Code and Codex, as a test's desk runs them.
pub(crate) fn programs(
    claude: std::path::PathBuf,
    codex: std::path::PathBuf,
) -> std::collections::BTreeMap<crate::model::Engine, std::path::PathBuf> {
    [
        (crate::model::Engine::Claude, claude),
        (crate::model::Engine::Codex, codex),
    ]
    .into()
}
