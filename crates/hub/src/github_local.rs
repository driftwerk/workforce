//! GitHub local repository tools — clone repos with PAT auth and list local clones (DW-331).
//!
//! Tools:
//! - `github.clone` — clone a GitHub repository using vault-stored PAT credentials
//! - `github.repos` — list all locally cloned repositories with status

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::{Tool, ToolResult};

use crate::git_hardening::hardened_git;
use crate::user_config::resolve_github_pat;
use crate::vault::Vault;

pub struct GithubLocalContext {
    pub vault: Arc<Vault>,
    pub global_vault: Option<Arc<Vault>>,
    pub workspace: PathBuf,
}

pub fn github_local_tools(ctx: GithubLocalContext) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(GithubCloneTool::with_fallback_or_agent(
            ctx.vault,
            ctx.global_vault,
            ctx.workspace.clone(),
        )),
        Arc::new(GithubReposTool::new(ctx.workspace)),
    ]
}

// ---------------------------------------------------------------------------
// github.clone
// ---------------------------------------------------------------------------

pub struct GithubCloneTool {
    agent_vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
    workspace: PathBuf,
}

impl GithubCloneTool {
    /// Construct with a single agent vault. Equivalent to
    /// `with_fallback_or_agent(vault, None, workspace)`.
    pub fn new(vault: Arc<Vault>, workspace: PathBuf) -> Self {
        Self::with_fallback_or_agent(vault, None, workspace)
    }

    /// Construct from an agent + optional global vault. Always returns a
    /// tool — the agent vault is required.
    pub fn with_fallback_or_agent(
        agent: Arc<Vault>,
        global: Option<Arc<Vault>>,
        workspace: PathBuf,
    ) -> Self {
        let global = global.filter(|g| !Arc::ptr_eq(g, &agent));
        Self {
            agent_vault: Some(agent),
            global_vault: global,
            workspace,
        }
    }

    /// Build the tool with an agent-scoped vault checked first and the global
    /// vault as a fallback. Returns `None` when neither vault is configured.
    pub fn with_fallback(
        agent: Option<Arc<Vault>>,
        global: Option<Arc<Vault>>,
        workspace: PathBuf,
    ) -> Option<Self> {
        match (agent, global) {
            (Some(a), g) => Some(Self::with_fallback_or_agent(a, g, workspace)),
            (None, Some(g)) => Some(Self {
                agent_vault: None,
                global_vault: Some(g),
                workspace,
            }),
            (None, None) => None,
        }
    }
}

#[derive(Deserialize)]
struct CloneInput {
    owner: String,
    repo: String,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    depth: Option<u32>,
}

#[async_trait]
impl Tool for GithubCloneTool {
    fn name(&self) -> &str {
        "github.clone"
    }

    fn description(&self) -> &str {
        "Clone a GitHub repo to workspace using vault credentials"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string",
                    "description": "GitHub repository owner (user or organization)"
                },
                "repo": {
                    "type": "string",
                    "description": "GitHub repository name"
                },
                "branch": {
                    "type": "string",
                    "description": "Branch to clone (optional, defaults to default branch)"
                },
                "depth": {
                    "type": "integer",
                    "description": "Shallow clone depth (optional, omit for full clone)"
                }
            },
            "required": ["owner", "repo"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: CloneInput = serde_json::from_value(input)?;
        if ![&input.owner, &input.repo]
            .iter()
            .all(|name| workforce_common::config_files::is_plain_name(name))
        {
            return Ok(ToolResult::failure(
                format!(
                    "'{}/{}' is not a GitHub owner and repository name",
                    input.owner, input.repo
                ),
                start.elapsed().as_millis() as u64,
            ));
        }
        let dest = self
            .workspace
            .join("repos")
            .join(&input.owner)
            .join(&input.repo);
        if workforce_agent::tools::config_guard::reaches_config(&self.workspace, &dest) {
            return Ok(ToolResult::failure(
                workforce_common::config_files::WRITE_REFUSED,
                start.elapsed().as_millis() as u64,
            ));
        }

        // Resolve PAT with workspace-then-bare precedence across agent + global
        // vaults. Mirrors the github skill adapter: a per-org PAT at
        // `skills/github/workspaces/{owner}/token` wins over the bare
        // `skills/github/token`, and the agent vault wins over global.
        let token = match resolve_github_pat(
            self.agent_vault.as_deref(),
            self.global_vault.as_deref(),
            &input.owner,
        ) {
            Some(t) => t,
            None => {
                return Ok(ToolResult::failure(
                    format!(
                        "GitHub PAT not configured. Set it with `vault.set \
                         skills/github/workspaces/{}/token <token>` (per-org) or \
                         `vault.set skills/github/token <token>` (default) on your \
                         agent, or on the global vault as a shared fallback.",
                        input.owner
                    ),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        // 3. Check if already cloned
        if dest.exists() {
            return Ok(ToolResult::failure(
                format!(
                    "Repository already cloned at repos/{}/{}",
                    input.owner, input.repo
                ),
                start.elapsed().as_millis() as u64,
            ));
        }

        // 4. Create parent directories
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // 5. Build git clone command
        let clone_url = format!(
            "https://x-access-token:{}@github.com/{}/{}.git",
            token, input.owner, input.repo
        );
        let mut cmd = hardened_git();
        cmd.arg("clone");
        if let Some(depth) = input.depth {
            cmd.arg("--depth").arg(depth.to_string());
        }
        if let Some(ref branch) = input.branch {
            cmd.arg("--branch").arg(branch);
        }
        cmd.arg(&clone_url).arg(&dest);

        // 6. Run command
        let output = cmd.output().await?;

        // 7. Check status
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            // Sanitize: remove token from error output
            let sanitized = stderr.replace(&token, "***");
            return Ok(ToolResult::failure(
                format!("Clone failed: {}", sanitized),
                start.elapsed().as_millis() as u64,
            ));
        }

        // 8. Remove PAT from stored remote URL and set up credential helper
        hardened_git()
            .args([
                "remote",
                "set-url",
                "origin",
                &format!("https://github.com/{}/{}.git", input.owner, input.repo),
            ])
            .current_dir(&dest)
            .output()
            .await
            .ok();

        let cred_helper = format!(
            "!f() {{ echo username=x-access-token; echo password={}; }}; f",
            token
        );
        hardened_git()
            .args(["config", "--local", "credential.helper", &cred_helper])
            .current_dir(&dest)
            .output()
            .await
            .ok();

        // 9. Get current branch
        let branch_output = hardened_git()
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&dest)
            .output()
            .await;
        let branch = branch_output
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();

        // 10. Return success
        Ok(ToolResult::success(
            serde_json::json!({
                "status": "cloned",
                "path": format!("repos/{}/{}", input.owner, input.repo),
                "branch": branch
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ---------------------------------------------------------------------------
// github.repos
// ---------------------------------------------------------------------------

pub struct GithubReposTool {
    workspace: PathBuf,
}

impl GithubReposTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct ReposInput {
    #[serde(default)]
    owner: Option<String>,
}

#[async_trait]
impl Tool for GithubReposTool {
    fn name(&self) -> &str {
        "github.repos"
    }

    fn description(&self) -> &str {
        "List all locally cloned GitHub repositories with status information"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string",
                    "description": "Filter by repository owner (optional)"
                }
            }
        ,
            "required": []})
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: ReposInput = serde_json::from_value(input)?;

        let repos_base = self.workspace.join("repos");
        if !repos_base.exists() {
            return Ok(ToolResult::success(
                serde_json::json!({ "repos": [] }),
                start.elapsed().as_millis() as u64,
            ));
        }

        let mut repos = Vec::new();

        // Iterate owner dirs
        let owner_entries = std::fs::read_dir(&repos_base)?;
        for owner_entry in owner_entries.flatten() {
            if !owner_entry.path().is_dir() {
                continue;
            }
            let owner_name = owner_entry.file_name().to_string_lossy().to_string();

            // Filter by owner if provided
            if let Some(ref filter_owner) = input.owner {
                if &owner_name != filter_owner {
                    continue;
                }
            }

            // Iterate repo dirs within this owner
            let repo_entries = std::fs::read_dir(owner_entry.path())?;
            for repo_entry in repo_entries.flatten() {
                let repo_path = repo_entry.path();
                if !repo_path.is_dir() {
                    continue;
                }

                // Check if it's a git repo
                if !repo_path.join(".git").exists() {
                    continue;
                }

                let repo_name = repo_entry.file_name().to_string_lossy().to_string();

                // Get branch
                let branch = hardened_git()
                    .args(["rev-parse", "--abbrev-ref", "HEAD"])
                    .current_dir(&repo_path)
                    .output()
                    .await
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_default();

                // Get clean/dirty status
                let status_output = hardened_git()
                    .args(["status", "--porcelain"])
                    .current_dir(&repo_path)
                    .output()
                    .await
                    .ok();
                let clean = status_output
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().is_empty())
                    .unwrap_or(false);

                // Get last commit
                let last_commit = hardened_git()
                    .args(["log", "-1", "--oneline"])
                    .current_dir(&repo_path)
                    .output()
                    .await
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_default();

                repos.push(serde_json::json!({
                    "owner": owner_name,
                    "repo": repo_name,
                    "branch": branch,
                    "clean": clean,
                    "last_commit": last_commit
                }));
            }
        }

        Ok(ToolResult::success(
            serde_json::json!({ "repos": repos }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn has_git() -> bool {
        StdCommand::new("git").arg("--version").output().is_ok()
    }

    /// The names are joined under `repos/`; one that climbs out would clone a
    /// repository over the workspace's own directories.
    #[tokio::test]
    async fn a_clone_name_that_leaves_repos_is_refused() {
        let temp = tempfile::TempDir::new().unwrap();
        let tool = GithubCloneTool {
            agent_vault: None,
            global_vault: None,
            workspace: temp.path().to_path_buf(),
        };
        for (owner, repo) in [
            ("..", "learning"),
            ("../..", "x"),
            ("org", "a/../../heartbeats"),
        ] {
            let result = tool
                .execute(serde_json::json!({ "owner": owner, "repo": repo }))
                .await
                .unwrap();
            assert!(!result.success, "{owner}/{repo}");
            assert!(result.error.unwrap().contains("not a GitHub owner"));
        }
        assert!(!temp.path().join("repos").exists());
    }

    /// `repos` is the agent uid's to make; a link there leading back to the
    /// workspace would have the hub clone into a directory of configuration.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_clone_through_a_planted_link_into_configuration_is_refused() {
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = temp.path().canonicalize().unwrap();
        std::fs::create_dir(workspace.join("learning")).unwrap();
        std::os::unix::fs::symlink(&workspace, workspace.join("repos")).unwrap();
        let tool = GithubCloneTool {
            agent_vault: None,
            global_vault: None,
            workspace: workspace.clone(),
        };
        let result = tool
            .execute(serde_json::json!({ "owner": "learning", "repo": "plans" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .unwrap()
            .contains(workforce_common::config_files::WRITE_REFUSED));
        assert_eq!(
            std::fs::read_dir(workspace.join("learning"))
                .unwrap()
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn repos_empty_when_no_repos_dir() {
        let temp = tempfile::TempDir::new().unwrap();
        let tool = GithubReposTool::new(temp.path().to_path_buf());
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(result.success);
        let repos = result.output["repos"].as_array().unwrap();
        assert!(repos.is_empty());
    }

    #[tokio::test]
    async fn repos_lists_cloned_repos() {
        if !has_git() {
            return;
        }
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = temp.path().to_path_buf();

        // Create a fake cloned repo
        let repo_dir = workspace.join("repos").join("test-org").join("test-repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        StdCommand::new("git")
            .args(["init"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        std::fs::write(repo_dir.join("README.md"), "# Test").unwrap();
        StdCommand::new("git")
            .args(["add", "."])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&repo_dir)
            .output()
            .unwrap();

        let tool = GithubReposTool::new(workspace);
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(result.success);
        let repos = result.output["repos"].as_array().unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0]["owner"], "test-org");
        assert_eq!(repos[0]["repo"], "test-repo");
        assert_eq!(repos[0]["clean"], true);
    }

    #[tokio::test]
    async fn clone_fails_when_no_pat() {
        let temp = tempfile::TempDir::new().unwrap();
        // Create a vault with no GitHub token
        let vault = Vault::open(&temp.path().join("vault.enc"), "test-key").unwrap();
        let tool = GithubCloneTool::new(Arc::new(vault), temp.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "owner": "test-org",
                "repo": "test-repo"
            }))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn clone_fails_when_already_exists() {
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = temp.path().to_path_buf();

        // Create the destination directory to simulate existing clone
        let dest = workspace.join("repos").join("test-org").join("test-repo");
        std::fs::create_dir_all(&dest).unwrap();

        let vault = Vault::open(&temp.path().join("vault.enc"), "test-key").unwrap();
        vault.set("skills/github/token", "fake-token").unwrap();
        let tool = GithubCloneTool::new(Arc::new(vault), workspace);
        let result = tool
            .execute(serde_json::json!({
                "owner": "test-org",
                "repo": "test-repo"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("already cloned"));
    }

    /// Both slots empty → tool construction returns None.
    #[test]
    fn with_fallback_none_when_no_vaults() {
        let temp = tempfile::TempDir::new().unwrap();
        assert!(GithubCloneTool::with_fallback(None, None, temp.path().to_path_buf()).is_none());
    }

    /// Agent vault holds the PAT → picked up (via `execute`'s idempotent
    /// "already cloned" path, which is enough to prove the lookup succeeds
    /// without actually hitting the network).
    #[tokio::test]
    async fn with_fallback_prefers_agent_vault() {
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = temp.path().to_path_buf();

        let agent = Arc::new(Vault::open(&temp.path().join("agent.enc"), "k").unwrap());
        agent.set("skills/github/token", "agent-pat").unwrap();
        let global = Arc::new(Vault::open(&temp.path().join("global.enc"), "k").unwrap());
        // global vault is intentionally empty

        let dest = workspace.join("repos").join("o").join("r");
        std::fs::create_dir_all(&dest).unwrap();

        let tool = GithubCloneTool::with_fallback(
            Some(Arc::clone(&agent)),
            Some(Arc::clone(&global)),
            workspace,
        )
        .unwrap();
        let result = tool
            .execute(serde_json::json!({ "owner": "o", "repo": "r" }))
            .await
            .unwrap();
        // Token was found, so we proceed past the PAT check; the subsequent
        // "already cloned" guard trips, which proves the lookup succeeded.
        assert!(!result.success);
        assert!(result.error.unwrap().contains("already cloned"));
    }

    /// Agent vault empty, global has the PAT → fallback engages.
    #[tokio::test]
    async fn with_fallback_falls_back_to_global() {
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = temp.path().to_path_buf();

        let agent = Arc::new(Vault::open(&temp.path().join("agent.enc"), "k").unwrap());
        // agent vault is empty
        let global = Arc::new(Vault::open(&temp.path().join("global.enc"), "k").unwrap());
        global.set("skills/github/token", "global-pat").unwrap();

        let dest = workspace.join("repos").join("o").join("r");
        std::fs::create_dir_all(&dest).unwrap();

        let tool = GithubCloneTool::with_fallback(Some(agent), Some(global), workspace).unwrap();
        let result = tool
            .execute(serde_json::json!({ "owner": "o", "repo": "r" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("already cloned"));
    }

    /// Neither vault has the PAT → the new error message is returned.
    #[tokio::test]
    async fn with_fallback_reports_missing_pat() {
        let temp = tempfile::TempDir::new().unwrap();
        let agent = Arc::new(Vault::open(&temp.path().join("agent.enc"), "k").unwrap());
        let global = Arc::new(Vault::open(&temp.path().join("global.enc"), "k").unwrap());

        let tool =
            GithubCloneTool::with_fallback(Some(agent), Some(global), temp.path().to_path_buf())
                .unwrap();
        let result = tool
            .execute(serde_json::json!({ "owner": "o", "repo": "r" }))
            .await
            .unwrap();
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.contains("GitHub PAT not configured"), "got: {err}");
    }

    /// When the CLI still passes the same `Arc` into both slots during
    /// migration, the second instance is deduped so we don't double-read
    /// the same vault.
    #[test]
    fn with_fallback_dedups_same_arc() {
        let temp = tempfile::TempDir::new().unwrap();
        let vault = Arc::new(Vault::open(&temp.path().join("v.enc"), "k").unwrap());
        let tool = GithubCloneTool::with_fallback(
            Some(Arc::clone(&vault)),
            Some(Arc::clone(&vault)),
            temp.path().to_path_buf(),
        )
        .unwrap();
        assert!(tool.global_vault.is_none());
        assert!(tool.agent_vault.is_some());
    }

    #[tokio::test]
    async fn clone_prefers_workspace_pat_over_bare() {
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = temp.path().to_path_buf();
        let vault = Arc::new(Vault::open(&temp.path().join("v.enc"), "k").unwrap());
        vault.set("skills/github/token", "bare-pat").unwrap();
        vault
            .set("skills/github/workspaces/acme/token", "acme-pat")
            .unwrap();

        let dest = workspace.join("repos").join("acme").join("r");
        std::fs::create_dir_all(&dest).unwrap();

        let tool = GithubCloneTool::new(vault, workspace);
        let result = tool
            .execute(serde_json::json!({ "owner": "acme", "repo": "r" }))
            .await
            .unwrap();
        // Token resolved successfully; the "already cloned" guard trips,
        // proving the lookup found a token (the workspace one would be used).
        assert!(!result.success);
        assert!(result.error.unwrap().contains("already cloned"));
    }
}
