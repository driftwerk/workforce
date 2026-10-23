use std::path::{Path, PathBuf};
use std::sync::Arc;

use eyre::{Result, WrapErr};
use workforce_common::settings::{KnowledgeConfig, WeaviateConfig};
use workforce_core::types::AgentId;
use workforce_knowledge_service::KnowledgeService;
use workforce_memory::embedding::EmbeddingBackend;
use workforce_memory::sqlite::{SqliteMemoryConfig, SqliteMemoryStore};

use crate::adapters::Adapters;
use crate::config::LocalMcpConfig;
use crate::embedding::FastembedBackend;
use crate::mirror::MirrorManager;
use crate::remote::{HttpConnector, RemoteHubs};
use crate::secret_store::LocalSecretStore;

/// Identity stamped onto memories created by this server. Fixed per-install so
/// `memory.list` / `memory.recall` can filter to the local user's own notes
/// even if other sources ever write to the same DB.
///
/// `local-mcp` is **single-user-per-data-dir** by design (DW-528). The
/// data dir is expected to live under the OS user's home directory and to
/// be readable only by that user. The `assert_single_user_data_dir` check
/// at startup fails loud if a different OS user later tries to share the
/// same data dir — preventing accidental cross-user memory bleed on a
/// multi-tenant host.
pub const LOCAL_OWNER: &str = "local-claude-code";

/// File written into the data dir on first init that records which OS
/// principal owns this dir. Subsequent starts compare to refuse takeover
/// by a different user.
const OWNER_MARKER: &str = ".workforce-local-owner";

/// Env var that disables local embeddings when set to "0", "false", "off",
/// or "no" (case-insensitive). Any other value — including unset — leaves
/// embeddings enabled.
const EMBEDDINGS_ENV: &str = "WORKFORCE_LOCAL_EMBEDDINGS";

/// Shared application state for all MCP tool handlers.
///
/// Holds the (cheap-to-clone) handles to the code graph and memory store
/// so every tool call shares one SQLite pool and one set of background
/// resources.
pub struct AppState {
    pub data_dir: PathBuf,
    pub workspace: PathBuf,
    pub projects_dir: PathBuf,
    pub knowledge: Arc<KnowledgeService>,
    pub memory: Arc<SqliteMemoryStore>,
    pub adapters: Adapters,
    pub owner: AgentId,
    pub mirror: Arc<MirrorManager>,
    pub remote: Arc<RemoteHubs>,
}

impl AppState {
    pub async fn new(cfg: LocalMcpConfig) -> Result<Self> {
        assert_single_user_data_dir(&cfg.data_dir)
            .wrap_err("local-mcp data dir is owned by a different OS user")?;

        let knowledge_config = KnowledgeConfig {
            generate_summaries: false,
            ..KnowledgeConfig::default()
        };
        let ks = KnowledgeService::new(
            cfg.workspace.clone(),
            cfg.data_dir.clone(),
            &knowledge_config,
            &WeaviateConfig::default(),
            None,
        )
        .wrap_err("initialising KnowledgeService")?;

        let embedding: Option<Arc<dyn EmbeddingBackend>> = if embeddings_enabled() {
            let cache_dir = cfg.data_dir.join("embeddings");
            match FastembedBackend::bge_small_en_v15(cache_dir).await {
                Ok(backend) => {
                    tracing::info!("local embeddings enabled (BGE-small-en-v1.5, 384d)");
                    Some(Arc::new(backend))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not load fastembed model; falling back to keyword-only memory recall");
                    None
                }
            }
        } else {
            tracing::info!(
                "local embeddings disabled by env ({}); using keyword-only memory recall",
                EMBEDDINGS_ENV,
            );
            None
        };

        let memory_db = cfg.data_dir.join("memory.db");
        let memory_config = SqliteMemoryConfig {
            db_path: memory_db.to_string_lossy().into_owned(),
            ..SqliteMemoryConfig::default()
        };
        let memory = SqliteMemoryStore::open_with_backend(&memory_config, embedding)
            .wrap_err("initialising SqliteMemoryStore")?;

        let knowledge = Arc::new(ks);
        let mirror = MirrorManager::new(
            Arc::clone(&knowledge),
            cfg.projects_dir.clone(),
            cfg.data_dir.clone(),
        );
        let remote = Arc::new(RemoteHubs::new(
            LocalSecretStore::new(),
            Arc::new(HttpConnector::default()),
        ));
        remote.spawn_connect_active();
        Ok(Self {
            data_dir: cfg.data_dir,
            workspace: cfg.workspace,
            projects_dir: cfg.projects_dir,
            knowledge,
            memory: Arc::new(memory),
            adapters: Adapters::load(),
            owner: AgentId::new(LOCAL_OWNER),
            mirror,
            remote,
        })
    }
}

fn embeddings_enabled() -> bool {
    !matches!(
        std::env::var(EMBEDDINGS_ENV)
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "false" | "off" | "no"
    )
}

/// Coarse OS-principal fingerprint: HOME path plus USER name. Captures
/// the cases that matter (different OS user, different home directory)
/// without pulling in a libc dependency for `getuid`.
fn current_principal_marker() -> String {
    let user = std::env::var("USER").unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{user}@{home}")
}

/// Refuse to start when the data dir was previously initialised by a
/// different OS principal. Writes the marker on first init.
fn assert_single_user_data_dir(data_dir: &Path) -> Result<()> {
    use std::fs;
    use std::io::ErrorKind;

    if !data_dir.exists() {
        fs::create_dir_all(data_dir).wrap_err("creating local-mcp data dir")?;
    }

    let marker_path = data_dir.join(OWNER_MARKER);
    let me = current_principal_marker();
    if me.is_empty() {
        return Ok(());
    }

    match fs::read_to_string(&marker_path) {
        Ok(existing) if existing.trim() == me => Ok(()),
        Ok(existing) => Err(eyre::eyre!(
            "local-mcp refusing to share data dir {} between OS principals: \
             marker says '{}', current is '{}'. Either run as the original \
             user or pick a different --data-dir (DW-528).",
            data_dir.display(),
            existing.trim(),
            me,
        )),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            fs::write(&marker_path, &me).wrap_err("writing owner marker")?;
            Ok(())
        }
        Err(e) => Err(eyre::Report::new(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path as StdPath;

    fn unique_dir(label: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(label)
            .tempdir()
            .expect("tempdir")
    }

    fn write_marker(dir: &StdPath, content: &str) {
        std::fs::write(dir.join(OWNER_MARKER), content).unwrap();
    }

    #[test]
    fn owner_marker_is_written_on_first_init() {
        let dir = unique_dir("local-mcp-init");
        assert_single_user_data_dir(dir.path()).expect("first init should succeed");
        let marker = std::fs::read_to_string(dir.path().join(OWNER_MARKER)).unwrap();
        assert_eq!(marker.trim(), current_principal_marker());
    }

    #[test]
    fn matching_marker_is_idempotent() {
        let dir = unique_dir("local-mcp-match");
        assert_single_user_data_dir(dir.path()).unwrap();
        assert_single_user_data_dir(dir.path()).expect("idempotent re-init");
    }

    #[test]
    fn mismatched_marker_refuses_startup() {
        let dir = unique_dir("local-mcp-conflict");
        write_marker(dir.path(), "intruder@/elsewhere");
        let err =
            assert_single_user_data_dir(dir.path()).expect_err("mismatched marker must error");
        assert!(format!("{err}").contains("DW-528"));
    }
}
