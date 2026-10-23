use eyre::Result;
use std::path::Path;

/// Monitors git state changes for triggering re-indexing.
pub struct GitMonitor;

impl GitMonitor {
    /// Get the current HEAD commit SHA.
    pub async fn current_head(repo_root: &Path) -> Result<String> {
        let output = tokio::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo_root)
            .output()
            .await?;

        if !output.status.success() {
            return Err(eyre::eyre!("Not a git repository"));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Check if the repository has changed since a known commit.
    pub async fn has_changed_since(repo_root: &Path, since_commit: &str) -> Result<bool> {
        let current = Self::current_head(repo_root).await?;
        Ok(current != since_commit)
    }

    /// Get the list of changed files between two commits.
    pub async fn changed_files(
        repo_root: &Path,
        from_commit: &str,
        to_commit: &str,
    ) -> Result<Vec<String>> {
        let output = tokio::process::Command::new("git")
            .args([
                "diff",
                "--name-only",
                &format!("{from_commit}..{to_commit}"),
            ])
            .current_dir(repo_root)
            .output()
            .await?;

        if !output.status.success() {
            return Err(eyre::eyre!("git diff failed"));
        }

        let files = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|l| l.to_string())
            .collect();

        Ok(files)
    }
}
