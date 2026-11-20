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

    fn is_hub_token(&self, h: &HeaderMap) -> bool {
        h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer hub-token")
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

    /// Everyone let in without a name stands for the hub's own token.
    fn is_hub_token(&self, _: &HeaderMap) -> bool {
        matches!(self.0, Ok(None))
    }
}

/// Lets in a signed-in administrator who isn't the person holding the hub:
/// what only the holder may do is refused.
struct NotHolder;

#[async_trait]
impl Authorize for NotHolder {
    async fn authorize(
        &self,
        _: &HeaderMap,
        _: Option<&str>,
    ) -> Result<Option<String>, StatusCode> {
        Ok(Some("admin".into()))
    }

    async fn authorize_holder(
        &self,
        _: &HeaderMap,
        _: Option<&str>,
    ) -> Result<Option<String>, StatusCode> {
        Err(StatusCode::FORBIDDEN)
    }
}

pub(crate) fn not_holder() -> std::sync::Arc<dyn Authorize> {
    std::sync::Arc::new(NotHolder)
}

/// Lets everyone in.
pub(crate) fn allow_all() -> std::sync::Arc<dyn Authorize> {
    std::sync::Arc::new(Fixed(Ok(None)))
}

/// Lets in as the person `name`, a signed-in administrator (not the hub's
/// own token).
pub(crate) fn identified(name: &str) -> std::sync::Arc<dyn Authorize> {
    std::sync::Arc::new(Fixed(Ok(Some(name.to_string()))))
}

/// A signed-in person who isn't an administrator: refused, as the hub
/// refuses them (403).
pub(crate) fn not_an_admin() -> std::sync::Arc<dyn Authorize> {
    std::sync::Arc::new(Fixed(Err(StatusCode::FORBIDDEN)))
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
        account_keys: None,
        line_reach: crate::line::Reach::Group,
        memory: Default::default(),
        bosses: Default::default(),
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

/// Everything logged, as one string.
#[derive(Clone, Default)]
pub(crate) struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl Captured {
    /// What this thread logs, at every level, while the guard lives: a
    /// desk on a current-thread runtime logs its tasks' here.
    pub(crate) fn on_this_thread() -> (Self, (tracing::subscriber::DefaultGuard, tracing::Dispatch))
    {
        let log = Self::default();
        let logging = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_ansi(false)
                .with_writer(log.clone())
                .finish(),
        );
        // Under `cargo test` other tests' threads share these callsites. A
        // second live subscriber keeps tracing from caching "off" for them
        // on a thread that listens to nothing; what is caught is still only
        // what this thread logs.
        let second = tracing::Dispatch::new(tracing_subscriber::registry());
        tracing::callsite::rebuild_interest_cache();
        (log, (logging, second))
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Captured {
        self.clone()
    }
}
