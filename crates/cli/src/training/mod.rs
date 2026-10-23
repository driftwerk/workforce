//! `workforce training {load,list,delete}` — CLI for per-agent training
//! corpora (DW-539).
//!
//! Two operating modes share one set of subcommands:
//!
//! - **Direct** (default): opens the configured `SqliteTrainingStore` +
//!   memory store from `workforce.toml`. No hub process required. Used
//!   for migrations, ops, single-machine installs.
//! - **Remote** (`--gateway <url>` or `[gateway]` from config): hits
//!   the hub's `/api/agents/{id}/training/corpora` endpoints. The hub
//!   owns the disk + Weaviate writes, the CLI just streams bytes.
//!
//! When `--gateway` is supplied or `[gateway].auth_token` is in
//! `workforce.toml`, the CLI runs remote-mode end-to-end. Direct mode
//! never crosses the wire.

pub mod format;
mod remote;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Args, Subcommand};
use eyre::{eyre, Context, Result};
use tracing::info;
use uuid::Uuid;

use workforce_common::settings::WorkforceConfig;
use workforce_hub::training::{
    delete_corpus_cascade, delete_document_cascade, doc_short_id, load_corpus, LoadRequest,
};
use workforce_memory::store::MemoryStore;
use workforce_training::{sanitize_filename, validate_slug, SourceKind, TrainingStore};

#[derive(Subcommand, Debug)]
pub enum TrainingCommand {
    /// Ingest a file into an agent's training corpus.
    Load(LoadArgs),
    /// List corpora (and their document counts) for an agent.
    List(ListArgs),
    /// List documents in one corpus with full metadata.
    Docs(DocsArgs),
    /// Delete a corpus and all of its chunks for an agent.
    Delete(DeleteArgs),
    /// Delete a single document (by short id) and its chunks/files.
    DocDelete(DocDeleteArgs),
}

/// Flags shared by every subcommand for picking direct vs remote mode.
///
/// Resolution order for the gateway URL/token:
/// 1. Explicit `--gateway` / `--token` flag on the subcommand.
/// 2. `[gateway]` block in the loaded `workforce.toml`.
/// 3. Direct mode (the local SQLite + Weaviate path).
#[derive(Args, Debug, Default)]
pub struct RemoteOpts {
    /// Hub URL (e.g. `https://hub.example.com`). When set, the
    /// CLI talks to the hub over HTTP and never opens a local store.
    #[arg(long, global = true)]
    pub gateway: Option<String>,
    /// Owner auth token for the remote hub. Falls back to
    /// `[gateway].auth_token` from `workforce.toml`.
    #[arg(long, global = true)]
    pub token: Option<String>,
}

#[derive(Args, Debug)]
pub struct LoadArgs {
    /// Path to a file in any supported format (JSON, PDF, XLSX/ODS,
    /// TXT, MD, DOCX). Format is picked from the extension.
    pub file: PathBuf,
    /// Agent whose corpus is being trained.
    #[arg(long)]
    pub agent: String,
    /// Corpus name (slug). Reused on subsequent loads with the same name.
    #[arg(long)]
    pub corpus: String,
    /// Workforce config path.
    #[arg(long, default_value = "workforce.toml")]
    pub config: PathBuf,
    #[command(flatten)]
    pub remote: RemoteOpts,
}

#[derive(Args, Debug)]
pub struct ListArgs {
    #[arg(long)]
    pub agent: String,
    #[arg(long, default_value = "workforce.toml")]
    pub config: PathBuf,
    #[command(flatten)]
    pub remote: RemoteOpts,
}

#[derive(Args, Debug)]
pub struct DeleteArgs {
    #[arg(long)]
    pub agent: String,
    #[arg(long)]
    pub corpus: String,
    #[arg(long, default_value = "workforce.toml")]
    pub config: PathBuf,
    #[command(flatten)]
    pub remote: RemoteOpts,
}

#[derive(Args, Debug)]
pub struct DocsArgs {
    /// Agent whose corpus is being listed.
    #[arg(long)]
    pub agent: String,
    /// Corpus whose documents are listed.
    #[arg(long)]
    pub corpus: String,
    /// Emit JSON matching the `GET /api/agents/{id}/training/corpora/{name}`
    /// envelope instead of the column-aligned table.
    #[arg(long)]
    pub json: bool,
    #[arg(long, default_value = "workforce.toml")]
    pub config: PathBuf,
    #[command(flatten)]
    pub remote: RemoteOpts,
}

#[derive(Args, Debug)]
pub struct DocDeleteArgs {
    /// Agent that owns the document.
    #[arg(long)]
    pub agent: String,
    /// Corpus the document belongs to (used to disambiguate when the
    /// 8-char short id repeats across an agent's corpora).
    #[arg(long)]
    pub corpus: String,
    /// 8-character short id from `workforce training list` (or the
    /// full UUID).
    #[arg(long)]
    pub doc: String,
    #[arg(long, default_value = "workforce.toml")]
    pub config: PathBuf,
    #[command(flatten)]
    pub remote: RemoteOpts,
}

pub async fn run(command: TrainingCommand) -> Result<()> {
    match command {
        TrainingCommand::Load(args) => {
            if let Some(client) = resolve_remote(&args.config, &args.remote)? {
                remote::load(&client, &args).await
            } else {
                load(args).await
            }
        }
        TrainingCommand::List(args) => {
            if let Some(client) = resolve_remote(&args.config, &args.remote)? {
                remote::list(&client, &args).await
            } else {
                list(args).await
            }
        }
        TrainingCommand::Docs(args) => {
            if let Some(client) = resolve_remote(&args.config, &args.remote)? {
                remote::docs(&client, &args).await
            } else {
                docs(args).await
            }
        }
        TrainingCommand::Delete(args) => {
            if let Some(client) = resolve_remote(&args.config, &args.remote)? {
                remote::delete(&client, &args).await
            } else {
                delete(args).await
            }
        }
        TrainingCommand::DocDelete(args) => {
            if let Some(client) = resolve_remote(&args.config, &args.remote)? {
                remote::doc_delete(&client, &args).await
            } else {
                doc_delete(args).await
            }
        }
    }
}

/// Build a remote client when either the user supplied `--gateway` or
/// the loaded config has a `[gateway].auth_token`. Returns `None` to
/// signal "stay local". Errors only when the user explicitly asked for
/// remote but we can't satisfy the token requirement.
fn resolve_remote(config_path: &Path, opts: &RemoteOpts) -> Result<Option<remote::RemoteClient>> {
    let wf = WorkforceConfig::load(config_path).ok();
    let url = opts.gateway.clone().or_else(|| {
        wf.as_ref().and_then(|c| {
            let listen = c.gateway.listen.trim();
            if listen.starts_with("http://") || listen.starts_with("https://") {
                Some(listen.to_string())
            } else {
                None
            }
        })
    });
    let token = opts
        .token
        .clone()
        .or_else(|| wf.as_ref().and_then(|c| c.gateway.auth_token.clone()));

    match (url, token) {
        (Some(u), Some(t)) => Ok(Some(remote::RemoteClient::new(u, t))),
        (Some(_), None) if opts.gateway.is_some() => Err(eyre!(
            "--gateway supplied but no token available (use --token or set [gateway].auth_token)",
        )),
        _ => Ok(None),
    }
}

async fn load(args: LoadArgs) -> Result<()> {
    let wf = WorkforceConfig::load(&args.config)
        .wrap_err_with(|| format!("load {}", args.config.display()))?;
    if !wf.training.enabled {
        return Err(eyre!("`[training] enabled = false` in workforce.toml"));
    }
    if !args.file.is_file() {
        return Err(eyre!("not a file: {}", args.file.display()));
    }
    validate_slug(&args.agent).map_err(|e| eyre!("--agent {e}"))?;
    validate_slug(&args.corpus).map_err(|e| eyre!("--corpus {e}"))?;

    let training = open_training_store(&wf).await?;
    let memory = open_memory_store(&wf).await?;

    let bytes =
        std::fs::read(&args.file).wrap_err_with(|| format!("read {}", args.file.display()))?;
    let raw_filename = args
        .file
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| args.file.display().to_string());
    let filename = sanitize_filename(&raw_filename);

    let storage_uri = stash_original(&wf, &args.agent, &args.corpus, &filename, &bytes)?;

    // CLI direct mode has no vision LLM wired (no AgentService here).
    // Users who need OCR for image-only PDFs pass `--gateway`, which
    // routes the upload through the hub where vision is available.
    let req = LoadRequest {
        agent_id: &args.agent,
        corpus_name: &args.corpus,
        original_filename: &filename,
        source_kind: SourceKind::Cli,
        source_ref: None,
        uploaded_by: &whoami(),
        bytes: &bytes,
        storage_uri: &storage_uri,
        format: None,
        vision: None,
    };

    let outcome = load_corpus(training, memory, req).await?;
    println!(
        "loaded {} into `{}`: {} written, {} unchanged, {} deleted",
        filename, args.corpus, outcome.written, outcome.unchanged, outcome.deleted
    );
    if let Some(doc_id) = outcome.document_id {
        println!("  document id: {doc_id}");
    }
    Ok(())
}

async fn list(args: ListArgs) -> Result<()> {
    let wf = WorkforceConfig::load(&args.config)
        .wrap_err_with(|| format!("load {}", args.config.display()))?;
    validate_slug(&args.agent).map_err(|e| eyre!("--agent {e}"))?;
    let training = open_training_store(&wf).await?;
    let corpora = training.list_corpora(&args.agent)?;
    if corpora.is_empty() {
        println!("no training corpora for `{}`", args.agent);
        return Ok(());
    }
    for corpus in corpora {
        let docs = training.list_documents(corpus.id)?;
        let mut total_chunks: usize = 0;
        let mut chunk_count_known = true;
        for doc in &docs {
            match training.list_chunks_for_document(doc.id) {
                Ok(cs) => total_chunks += cs.len(),
                Err(e) => {
                    tracing::warn!(
                        document_id = %doc.id,
                        error = %e,
                        "training: failed to count chunks for document",
                    );
                    chunk_count_known = false;
                }
            }
        }
        let chunk_display = if chunk_count_known {
            total_chunks.to_string()
        } else {
            "?".to_string()
        };
        println!(
            "{:<32}  {} doc(s)  {} chunk(s)  created {}",
            corpus.name,
            docs.len(),
            chunk_display,
            corpus.created_at.to_rfc3339(),
        );
        for doc in docs {
            println!(
                "    {:<8}  {:<48}  status={}",
                doc.id
                    .simple()
                    .to_string()
                    .chars()
                    .take(8)
                    .collect::<String>(),
                doc.original_filename,
                doc.processing_status.as_str(),
            );
        }
    }
    Ok(())
}

async fn docs(args: DocsArgs) -> Result<()> {
    let wf = WorkforceConfig::load(&args.config)
        .wrap_err_with(|| format!("load {}", args.config.display()))?;
    validate_slug(&args.agent).map_err(|e| eyre!("--agent {e}"))?;
    validate_slug(&args.corpus).map_err(|e| eyre!("--corpus {e}"))?;
    let training = open_training_store(&wf).await?;

    let Some(corpus) = training.get_corpus_by_name(&args.agent, &args.corpus)? else {
        return Err(eyre!(
            "no corpus `{}` for agent `{}`",
            args.corpus,
            args.agent
        ));
    };
    let documents = training.list_documents(corpus.id)?;
    let rows: Vec<(workforce_training::Document, Option<usize>)> = documents
        .into_iter()
        .map(|d| {
            let count = match training.list_chunks_for_document(d.id) {
                Ok(chunks) => Some(chunks.len()),
                Err(e) => {
                    tracing::warn!(
                        document_id = %d.id,
                        error = %e,
                        "training: failed to count chunks for document",
                    );
                    None
                }
            };
            (d, count)
        })
        .collect();

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&corpus_envelope_json(&corpus, &rows))?,
        );
    } else {
        let doc_rows: Vec<format::DocRow> = rows
            .iter()
            .map(|(d, n)| doc_row_from_document(d, *n))
            .collect();
        println!(
            "{}",
            format::render_doc_table(&args.agent, &args.corpus, &doc_rows),
        );
    }
    Ok(())
}

fn doc_row_from_document(
    d: &workforce_training::Document,
    chunk_count: Option<usize>,
) -> format::DocRow {
    format::DocRow {
        id: d.id,
        short_id: doc_short_id(d.id),
        filename: d.original_filename.clone(),
        size_bytes: d.size_bytes,
        chunk_count,
        status: d.processing_status.as_str().to_string(),
        uploaded_at: d.uploaded_at,
    }
}

fn corpus_envelope_json(
    corpus: &workforce_training::Corpus,
    rows: &[(workforce_training::Document, Option<usize>)],
) -> serde_json::Value {
    let documents: Vec<serde_json::Value> = rows
        .iter()
        .map(|(d, count)| {
            serde_json::json!({
                "id": d.id.to_string(),
                "short_id": doc_short_id(d.id),
                "filename": d.original_filename,
                "mime_type": d.mime_type,
                "size_bytes": d.size_bytes,
                "source_kind": d.source_kind.as_str(),
                "processing_status": d.processing_status.as_str(),
                "processing_error": d.processing_error,
                "uploaded_at": d.uploaded_at.to_rfc3339(),
                "uploaded_by": d.uploaded_by,
                "chunk_count": count,
            })
        })
        .collect();
    serde_json::json!({
        "id": corpus.id.to_string(),
        "name": corpus.name,
        "created_at": corpus.created_at.to_rfc3339(),
        "created_by": corpus.created_by,
        "documents": documents,
    })
}

async fn delete(args: DeleteArgs) -> Result<()> {
    let wf = WorkforceConfig::load(&args.config)
        .wrap_err_with(|| format!("load {}", args.config.display()))?;
    validate_slug(&args.agent).map_err(|e| eyre!("--agent {e}"))?;
    validate_slug(&args.corpus).map_err(|e| eyre!("--corpus {e}"))?;
    let training = open_training_store(&wf).await?;
    let memory = open_memory_store(&wf).await?;
    let Some(corpus) = training.get_corpus_by_name(&args.agent, &args.corpus)? else {
        return Err(eyre!(
            "no corpus `{}` for agent `{}`",
            args.corpus,
            args.agent
        ));
    };
    let outcome = delete_corpus_cascade(training, memory, corpus.id).await?;
    println!(
        "deleted corpus `{}` for `{}` ({} chunk(s) removed, {} memory row(s) forgotten, {} file(s) unlinked)",
        args.corpus,
        args.agent,
        outcome.chunks_removed,
        outcome.memories_forgotten,
        outcome.files_unlinked,
    );
    info!(
        agent = %args.agent,
        corpus = %args.corpus,
        chunks = outcome.chunks_removed,
        memories = outcome.memories_forgotten,
        files = outcome.files_unlinked,
        "training: corpus deleted"
    );
    Ok(())
}

async fn doc_delete(args: DocDeleteArgs) -> Result<()> {
    let wf = WorkforceConfig::load(&args.config)
        .wrap_err_with(|| format!("load {}", args.config.display()))?;
    validate_slug(&args.agent).map_err(|e| eyre!("--agent {e}"))?;
    validate_slug(&args.corpus).map_err(|e| eyre!("--corpus {e}"))?;
    let training = open_training_store(&wf).await?;
    let memory = open_memory_store(&wf).await?;

    let Some(corpus) = training.get_corpus_by_name(&args.agent, &args.corpus)? else {
        return Err(eyre!(
            "no corpus `{}` for agent `{}`",
            args.corpus,
            args.agent
        ));
    };
    let doc_id = resolve_doc_id(training.as_ref(), corpus.id, &args.doc)?;

    let outcome = delete_document_cascade(training, memory, doc_id).await?;
    println!(
        "deleted document `{}` from `{}/{}` ({} chunk(s) removed, {} memory row(s) forgotten, {} file(s) unlinked)",
        doc_short_id(doc_id),
        args.agent,
        args.corpus,
        outcome.chunks_removed,
        outcome.memories_forgotten,
        outcome.files_unlinked,
    );
    info!(
        agent = %args.agent,
        corpus = %args.corpus,
        doc = %doc_id,
        chunks = outcome.chunks_removed,
        memories = outcome.memories_forgotten,
        files = outcome.files_unlinked,
        "training: document deleted"
    );
    Ok(())
}

/// Resolve a user-supplied document reference (full UUID or 8-char
/// short id) to the full `Uuid` of a document within `corpus_id`.
/// Short-id collisions across the *same* corpus error out with the
/// candidate list so the operator can disambiguate.
fn resolve_doc_id(training: &dyn TrainingStore, corpus_id: Uuid, reference: &str) -> Result<Uuid> {
    if let Ok(full) = Uuid::parse_str(reference) {
        return Ok(full);
    }
    let lower = reference.trim().to_lowercase();
    if lower.is_empty() {
        return Err(eyre!("--doc is empty"));
    }
    let docs = training.list_documents(corpus_id)?;
    let matches: Vec<&workforce_training::Document> = docs
        .iter()
        .filter(|d| doc_short_id(d.id) == lower)
        .collect();
    match matches.as_slice() {
        [d] => Ok(d.id),
        [] => Err(eyre!("no document with short id `{lower}` in this corpus")),
        many => Err(eyre!(
            "short id `{lower}` matches {} documents — supply the full UUID instead ({})",
            many.len(),
            many.iter()
                .map(|d| d.id.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        )),
    }
}

async fn open_training_store(wf: &WorkforceConfig) -> Result<Arc<dyn TrainingStore>> {
    workforce_hub::training::open_training_store(wf)
        .await?
        .ok_or_else(|| eyre!("training store unavailable (see logs for backend init details)"))
}

async fn open_memory_store(wf: &WorkforceConfig) -> Result<Arc<dyn MemoryStore>> {
    wf.check_memory_backend()?;
    if wf.weaviate.enabled || wf.memory.is_vellev() {
        let store = workforce_hub::embeddings::switched_memory_store(
            wf,
            workforce_hub::embeddings::base_store(wf)?,
        )
        .await?;
        let store: Arc<dyn workforce_memory::store::MemoryStore> = store;
        Ok(store)
    } else {
        Err(eyre!(
            "no persistent memory backend configured ([weaviate] enabled = false); \
             training writes need a real store, not in-memory"
        ))
    }
}

/// Stash the original file under the configured training data dir.
///
/// Preconditions enforced by callers:
/// - `agent` and `corpus` are [`validate_slug`]-clean
/// - `filename` is [`sanitize_filename`]-clean
fn stash_original(
    wf: &WorkforceConfig,
    agent: &str,
    corpus: &str,
    filename: &str,
    bytes: &[u8],
) -> Result<String> {
    let raw = Path::new(&wf.training.data_dir);
    let root = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        Path::new(&wf.hub.data_dir).join(raw)
    };
    let dest_dir = root.join(agent).join(corpus);
    std::fs::create_dir_all(&dest_dir).wrap_err_with(|| format!("mkdir {}", dest_dir.display()))?;
    let stamped = format!("{}__{}", Uuid::new_v4().simple(), filename);
    let dest = dest_dir.join(stamped);
    std::fs::write(&dest, bytes).wrap_err_with(|| format!("write {}", dest.display()))?;
    Ok(format!("file://{}", dest.display()))
}

fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}
