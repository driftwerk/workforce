//! Disk persistence for job manifests, output, and chain state.
//!
//! Jobs are stored in `.local/jobs/{job_id}/` with:
//! - `manifest.json` — the job manifest (atomic writes via temp + rename)
//! - `output.json` — the full job output (written on completion)
//! - `output_attempt_N.json` — per-attempt output for retried jobs
//!
//! Chains are stored in `.local/jobs/chains/{chain_id}/`:
//! - `chain.json` — the chain manifest (atomic writes via temp + rename)

use eyre::{Context, Result};
use std::path::PathBuf;
use tokio::fs;
use tracing::{debug, error, warn};

use super::types::{JobChain, JobManifest, JobOutput};

/// Manages job persistence to disk.
#[derive(Debug, Clone)]
pub struct JobPersistence {
    /// Base directory for job storage (e.g. `.local/jobs/`).
    base_dir: PathBuf,
}

impl JobPersistence {
    /// Create a new persistence layer with the given base directory.
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Ensure the base directory exists.
    pub async fn ensure_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.base_dir)
            .await
            .wrap_err_with(|| format!("Failed to create jobs dir: {:?}", self.base_dir))?;
        Ok(())
    }

    // ── Job Manifest Persistence ─────────────────────────────────────

    /// Directory for a specific job.
    fn job_dir(&self, job_id: &str) -> PathBuf {
        self.base_dir.join(job_id)
    }

    /// Path to a job's manifest file.
    fn manifest_path(&self, job_id: &str) -> PathBuf {
        self.job_dir(job_id).join("manifest.json")
    }

    /// Path to a job's output file.
    fn output_path(&self, job_id: &str) -> PathBuf {
        self.job_dir(job_id).join("output.json")
    }

    /// Path to a specific attempt's output file.
    fn attempt_output_path(&self, job_id: &str, attempt: u32) -> PathBuf {
        self.job_dir(job_id)
            .join(format!("output_attempt_{}.json", attempt))
    }

    /// Save a job manifest atomically (write to temp file, then rename).
    pub async fn save_manifest(&self, manifest: &JobManifest) -> Result<()> {
        let dir = self.job_dir(&manifest.job_id);
        fs::create_dir_all(&dir)
            .await
            .wrap_err_with(|| format!("Failed to create job dir: {:?}", dir))?;

        let json =
            serde_json::to_string_pretty(manifest).wrap_err("Failed to serialise job manifest")?;

        let manifest_path = self.manifest_path(&manifest.job_id);
        let tmp_path = dir.join("manifest.json.tmp");

        fs::write(&tmp_path, json.as_bytes())
            .await
            .wrap_err_with(|| format!("Failed to write temp manifest: {:?}", tmp_path))?;

        fs::rename(&tmp_path, &manifest_path)
            .await
            .wrap_err_with(|| format!("Failed to rename manifest: {:?}", manifest_path))?;

        debug!(job_id = %manifest.job_id, status = %manifest.status, "Manifest saved");
        Ok(())
    }

    /// Save job output to disk.
    pub async fn save_output(&self, output: &JobOutput) -> Result<()> {
        let dir = self.job_dir(&output.job_id);
        fs::create_dir_all(&dir).await?;

        let json =
            serde_json::to_string_pretty(output).wrap_err("Failed to serialise job output")?;

        let path = self.output_path(&output.job_id);
        fs::write(&path, json.as_bytes())
            .await
            .wrap_err_with(|| format!("Failed to write job output: {:?}", path))?;

        debug!(job_id = %output.job_id, "Output saved");
        Ok(())
    }

    /// Save output for a specific retry attempt.
    pub async fn save_attempt_output(&self, output: &JobOutput, attempt: u32) -> Result<()> {
        let dir = self.job_dir(&output.job_id);
        fs::create_dir_all(&dir).await?;

        let json =
            serde_json::to_string_pretty(output).wrap_err("Failed to serialise attempt output")?;

        let path = self.attempt_output_path(&output.job_id, attempt);
        fs::write(&path, json.as_bytes())
            .await
            .wrap_err_with(|| format!("Failed to write attempt output: {:?}", path))?;

        debug!(job_id = %output.job_id, attempt, "Attempt output saved");
        Ok(())
    }

    /// Load a job manifest from disk.
    pub async fn load_manifest(&self, job_id: &str) -> Result<Option<JobManifest>> {
        let path = self.manifest_path(job_id);
        if !path.exists() {
            return Ok(None);
        }

        let data = fs::read_to_string(&path)
            .await
            .wrap_err_with(|| format!("Failed to read manifest: {:?}", path))?;

        let manifest: JobManifest = serde_json::from_str(&data)
            .wrap_err_with(|| format!("Failed to parse manifest: {:?}", path))?;

        Ok(Some(manifest))
    }

    /// Load job output from disk.
    pub async fn load_output(&self, job_id: &str) -> Result<Option<JobOutput>> {
        let path = self.output_path(job_id);
        if !path.exists() {
            return Ok(None);
        }

        let data = fs::read_to_string(&path)
            .await
            .wrap_err_with(|| format!("Failed to read output: {:?}", path))?;

        let output: JobOutput = serde_json::from_str(&data)
            .wrap_err_with(|| format!("Failed to parse output: {:?}", path))?;

        Ok(Some(output))
    }

    /// Load output for a specific retry attempt.
    pub async fn load_attempt_output(
        &self,
        job_id: &str,
        attempt: u32,
    ) -> Result<Option<JobOutput>> {
        let path = self.attempt_output_path(job_id, attempt);
        if !path.exists() {
            return Ok(None);
        }

        let data = fs::read_to_string(&path)
            .await
            .wrap_err_with(|| format!("Failed to read attempt output: {:?}", path))?;

        let output: JobOutput = serde_json::from_str(&data)
            .wrap_err_with(|| format!("Failed to parse attempt output: {:?}", path))?;

        Ok(Some(output))
    }

    /// Scan all job directories and load their manifests.
    /// Returns manifests sorted by submission time (newest first).
    pub async fn scan_all_manifests(&self) -> Result<Vec<JobManifest>> {
        let mut manifests = Vec::new();

        if !self.base_dir.exists() {
            return Ok(manifests);
        }

        let mut entries = fs::read_dir(&self.base_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let dir_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(name) => name.to_string(),
                None => continue,
            };

            // Only process directories that look like job IDs (j-xxxxxxxx).
            if !dir_name.starts_with("j-") {
                continue;
            }

            match self.load_manifest(&dir_name).await {
                Ok(Some(manifest)) => manifests.push(manifest),
                Ok(None) => {
                    warn!(job_id = %dir_name, "Job directory has no manifest");
                }
                Err(e) => {
                    error!(job_id = %dir_name, error = %e, "Failed to load manifest");
                }
            }
        }

        // Sort by submission time, newest first.
        manifests.sort_by(|a, b| b.submitted_at.cmp(&a.submitted_at));
        Ok(manifests)
    }

    /// Remove a job's files from disk (used for cleanup of old jobs).
    pub async fn remove_job(&self, job_id: &str) -> Result<()> {
        let dir = self.job_dir(job_id);
        if dir.exists() {
            fs::remove_dir_all(&dir)
                .await
                .wrap_err_with(|| format!("Failed to remove job dir: {:?}", dir))?;
        }
        Ok(())
    }

    /// Clean up old completed jobs (keep only the last N).
    pub async fn cleanup_old_jobs(&self, keep_count: usize) -> Result<usize> {
        let manifests = self.scan_all_manifests().await?;
        let terminal: Vec<_> = manifests
            .iter()
            .filter(|m| m.status.is_terminal())
            .collect();

        if terminal.len() <= keep_count {
            return Ok(0);
        }

        let mut removed = 0;
        for manifest in terminal.iter().skip(keep_count) {
            if let Err(e) = self.remove_job(&manifest.job_id).await {
                warn!(job_id = %manifest.job_id, error = %e, "Failed to clean up job");
            } else {
                removed += 1;
            }
        }

        debug!(removed, kept = keep_count, "Cleaned up old jobs");
        Ok(removed)
    }

    // ── Chain Persistence ────────────────────────────────────────────

    /// Directory for chain storage.
    fn chains_dir(&self) -> PathBuf {
        self.base_dir.join("chains")
    }

    /// Directory for a specific chain.
    fn chain_dir(&self, chain_id: &str) -> PathBuf {
        self.chains_dir().join(chain_id)
    }

    /// Path to a chain's manifest file.
    fn chain_manifest_path(&self, chain_id: &str) -> PathBuf {
        self.chain_dir(chain_id).join("chain.json")
    }

    /// Save a chain manifest atomically.
    pub async fn save_chain(&self, chain: &JobChain) -> Result<()> {
        let dir = self.chain_dir(&chain.id);
        fs::create_dir_all(&dir)
            .await
            .wrap_err_with(|| format!("Failed to create chain dir: {:?}", dir))?;

        let json =
            serde_json::to_string_pretty(chain).wrap_err("Failed to serialise chain manifest")?;

        let chain_path = self.chain_manifest_path(&chain.id);
        let tmp_path = dir.join("chain.json.tmp");

        fs::write(&tmp_path, json.as_bytes())
            .await
            .wrap_err_with(|| format!("Failed to write temp chain: {:?}", tmp_path))?;

        fs::rename(&tmp_path, &chain_path)
            .await
            .wrap_err_with(|| format!("Failed to rename chain: {:?}", chain_path))?;

        debug!(chain_id = %chain.id, status = %chain.status, "Chain saved");
        Ok(())
    }

    /// Load a chain manifest from disk.
    pub async fn load_chain(&self, chain_id: &str) -> Result<Option<JobChain>> {
        let path = self.chain_manifest_path(chain_id);
        if !path.exists() {
            return Ok(None);
        }

        let data = fs::read_to_string(&path)
            .await
            .wrap_err_with(|| format!("Failed to read chain: {:?}", path))?;

        let chain: JobChain = serde_json::from_str(&data)
            .wrap_err_with(|| format!("Failed to parse chain: {:?}", path))?;

        Ok(Some(chain))
    }

    /// Scan all chain directories and load their manifests.
    pub async fn scan_all_chains(&self) -> Result<Vec<JobChain>> {
        let mut chains = Vec::new();
        let chains_dir = self.chains_dir();

        if !chains_dir.exists() {
            return Ok(chains);
        }

        let mut entries = fs::read_dir(&chains_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let dir_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(name) => name.to_string(),
                None => continue,
            };

            if !dir_name.starts_with("ch-") {
                continue;
            }

            match self.load_chain(&dir_name).await {
                Ok(Some(chain)) => chains.push(chain),
                Ok(None) => {
                    warn!(chain_id = %dir_name, "Chain directory has no manifest");
                }
                Err(e) => {
                    error!(chain_id = %dir_name, error = %e, "Failed to load chain");
                }
            }
        }

        chains.sort_by(|a, b| b.submitted_at.cmp(&a.submitted_at));
        Ok(chains)
    }

    /// Remove a chain's files from disk.
    pub async fn remove_chain(&self, chain_id: &str) -> Result<()> {
        let dir = self.chain_dir(chain_id);
        if dir.exists() {
            fs::remove_dir_all(&dir)
                .await
                .wrap_err_with(|| format!("Failed to remove chain dir: {:?}", dir))?;
        }
        Ok(())
    }
}
