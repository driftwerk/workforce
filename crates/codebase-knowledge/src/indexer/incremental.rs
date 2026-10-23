use eyre::Result;
use std::path::Path;
use tracing::{debug, info};

/// Delta between the last indexed state and the current state.
#[derive(Debug, Clone, Default)]
pub struct IndexDelta {
    pub added_files: Vec<String>,
    pub modified_files: Vec<String>,
    pub deleted_files: Vec<String>,
    pub renamed_files: Vec<(String, String)>,
}

impl IndexDelta {
    pub fn is_empty(&self) -> bool {
        self.added_files.is_empty()
            && self.modified_files.is_empty()
            && self.deleted_files.is_empty()
            && self.renamed_files.is_empty()
    }

    pub fn total_changes(&self) -> usize {
        self.added_files.len()
            + self.modified_files.len()
            + self.deleted_files.len()
            + self.renamed_files.len()
    }
}

/// Computes incremental deltas using git diff.
pub struct IncrementalDetector;

impl IncrementalDetector {
    /// Compute what changed since the given commit.
    pub async fn compute_delta(root: &Path, since_commit: &str) -> Result<IndexDelta> {
        let output = tokio::process::Command::new("git")
            .args([
                "-c",
                "safe.directory=*",
                "diff",
                "--name-status",
                &format!("{since_commit}..HEAD"),
            ])
            .current_dir(root)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(eyre::eyre!("git diff failed: {}", stderr));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut delta = IndexDelta::default();

        for line in stdout.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 2 {
                continue;
            }

            let status = parts[0];
            let path = parts[1].to_string();

            match status.chars().next() {
                Some('A') => delta.added_files.push(path),
                Some('M') => delta.modified_files.push(path),
                Some('D') => delta.deleted_files.push(path),
                Some('R') => {
                    if parts.len() >= 3 {
                        delta.renamed_files.push((path, parts[2].to_string()));
                    }
                }
                _ => {
                    debug!("Unknown git status: {}", line);
                }
            }
        }

        info!(
            "Delta since {}: +{} ~{} -{} R{}",
            &since_commit[..8.min(since_commit.len())],
            delta.added_files.len(),
            delta.modified_files.len(),
            delta.deleted_files.len(),
            delta.renamed_files.len(),
        );

        Ok(delta)
    }

    /// Get the current HEAD commit SHA.
    pub async fn current_commit(root: &Path) -> Result<String> {
        let output = tokio::process::Command::new("git")
            .args(["-c", "safe.directory=*", "rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .await?;

        if !output.status.success() {
            return Err(eyre::eyre!("Not a git repository or git not available"));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}
