//! Per-user "about this user" profile file.
//!
//! Stores a short narrative summary of what the agent has learned about a
//! specific user in `{workspace}/memory/users/{user_id}/profile.md`. The hub's
//! per-user prompt builder loads it as `## Learned Preferences` (see
//! `crates/common/src/identity.rs:user_context_block`).
//!
//! The agent writes this file at the end of each per-user consolidation run
//! via the `notes write_profile` tool action — sourced from the user's rich
//! episodic memories rather than per-turn heuristics.

use std::path::{Path, PathBuf};

use eyre::Result;

/// Maximum profile file size in bytes. Caps prompt-inflation from a runaway
/// profile, and trims on whole-line boundaries so content stays readable.
pub const MAX_PROFILE_SIZE: usize = 2000;

/// Manages per-user profile storage.
pub struct ProfileManager {
    workspace: PathBuf,
}

impl ProfileManager {
    pub fn new(workspace: &Path) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
        }
    }

    /// Path to a user's profile file.
    pub fn profile_path(&self, user_id: &str) -> PathBuf {
        self.workspace
            .join("memory")
            .join("users")
            .join(user_id)
            .join("profile.md")
    }

    /// Load existing profile content (empty string if none).
    pub fn load_profile(&self, user_id: &str) -> String {
        std::fs::read_to_string(self.profile_path(user_id)).unwrap_or_default()
    }

    /// Save profile content, truncating to `MAX_PROFILE_SIZE` on whole-line
    /// boundaries. Creates the parent directory if needed.
    pub fn save_profile(&self, user_id: &str, content: &str) -> Result<()> {
        let path = self.profile_path(user_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, truncate_to_cap(content, MAX_PROFILE_SIZE))?;
        Ok(())
    }
}

/// Trim `content` so its byte length does not exceed `cap`, dropping whole
/// trailing lines until it fits. Shorter input is returned unchanged.
pub fn truncate_to_cap(content: &str, cap: usize) -> String {
    if content.len() <= cap {
        return content.to_string();
    }
    let mut lines: Vec<&str> = content.lines().collect();
    while !lines.is_empty() && lines.join("\n").len() > cap {
        lines.pop();
    }
    let mut out = lines.join("\n");
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn profile_path_construction() {
        let dir = TempDir::new().unwrap();
        let mgr = ProfileManager::new(dir.path());
        let path = mgr.profile_path("U123");
        assert!(path.ends_with("memory/users/U123/profile.md"));
    }

    #[test]
    fn load_nonexistent_profile() {
        let dir = TempDir::new().unwrap();
        let mgr = ProfileManager::new(dir.path());
        assert_eq!(mgr.load_profile("U123"), "");
    }

    #[test]
    fn save_and_load_profile_round_trip() {
        let dir = TempDir::new().unwrap();
        let mgr = ProfileManager::new(dir.path());
        mgr.save_profile("U123", "Kings prefers concise replies.\n")
            .unwrap();
        assert_eq!(mgr.load_profile("U123"), "Kings prefers concise replies.\n");
    }

    #[test]
    fn truncate_leaves_short_content_untouched() {
        let input = "line one\nline two\n";
        assert_eq!(truncate_to_cap(input, 1000), input);
    }

    #[test]
    fn truncate_drops_trailing_lines_over_cap() {
        let input = "aaaaaaaa\nbbbbbbbb\ncccccccc\ndddddddd";
        let out = truncate_to_cap(input, 20);
        assert!(out.len() <= 20 + 1); // +1 for the trailing newline we append
        assert!(out.starts_with("aaaaaaaa"));
        assert!(!out.contains("dddddddd"));
    }

    #[test]
    fn truncate_handles_empty() {
        assert_eq!(truncate_to_cap("", 100), "");
    }

    #[test]
    fn save_profile_enforces_cap() {
        let dir = TempDir::new().unwrap();
        let mgr = ProfileManager::new(dir.path());
        let content: String = (0..500)
            .map(|_| "x".to_string())
            .collect::<Vec<_>>()
            .join("\n");
        mgr.save_profile("U123", &content).unwrap();
        assert!(mgr.load_profile("U123").len() <= MAX_PROFILE_SIZE + 1);
    }
}
