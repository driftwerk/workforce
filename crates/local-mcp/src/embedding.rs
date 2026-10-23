//! In-process embedding backend using `fastembed-rs` + `BGE-small-en-v1.5`.
//!
//! The model is ~130 MB and auto-downloads to `~/.cache/fastembed` on first
//! use. Initialisation is synchronous (ONNX model load) so we run it on a
//! blocking task. Subsequent embed calls are also CPU-bound and run on the
//! blocking pool to avoid starving tokio's async worker threads.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use tokio::sync::Mutex;
use workforce_memory::embedding::EmbeddingBackend;

/// Single-model fastembed-rs backend. Embedding dimension is 384 (BGE-small).
pub struct FastembedBackend {
    /// `TextEmbedding` is not `Sync` in all versions; wrap in a Mutex so the
    /// backend is safe to share via `Arc<dyn EmbeddingBackend>`.
    model: Arc<Mutex<TextEmbedding>>,
}

impl FastembedBackend {
    /// Load BGE-small-en-v1.5, caching the ~130 MB ONNX model under
    /// `cache_dir`. Blocks on model load/download — call this from startup,
    /// not from a request handler.
    pub async fn bge_small_en_v15(cache_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&cache_dir)
            .wrap_err_with(|| format!("creating embedding cache dir {}", cache_dir.display()))?;
        let model = tokio::task::spawn_blocking(move || {
            TextEmbedding::try_new(
                InitOptions::new(EmbeddingModel::BGESmallENV15).with_cache_dir(cache_dir),
            )
        })
        .await
        .wrap_err("fastembed model init task panicked")?
        .map_err(|e| eyre::eyre!("failed to load BGE-small-en-v1.5 embedding model: {e}"))?;
        Ok(Self {
            model: Arc::new(Mutex::new(model)),
        })
    }
}

#[async_trait]
impl EmbeddingBackend for FastembedBackend {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let text = text.to_string();
        let model = Arc::clone(&self.model);
        tokio::task::spawn_blocking(move || {
            let mut guard = model.blocking_lock();
            let mut embeddings = guard
                .embed(vec![text], None)
                .map_err(|e| eyre::eyre!("fastembed embed failed: {e}"))?;
            embeddings
                .pop()
                .ok_or_else(|| eyre::eyre!("fastembed returned empty batch"))
        })
        .await
        .wrap_err("fastembed embed task panicked")?
    }
}
