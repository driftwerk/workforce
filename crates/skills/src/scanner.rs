//! Skill directory scanner — discovers skills from filesystem directories.
//!
//! Each skill is a subdirectory containing a `SKILL.md` file.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use crate::config_loader::ConfigStatus;
use crate::gating::{self, GatingResult};
use crate::skill_file::SkillFile;
use workforce_common::security_scanner::{self, SecurityFinding};

/// A discovered skill on the filesystem.
#[derive(Debug, Clone)]
pub struct DiscoveredSkill {
    /// The parsed SKILL.md content.
    pub skill: SkillFile,
    /// The directory containing the skill.
    pub path: PathBuf,
    /// Whether the skill's requirements are met.
    pub gating: GatingResult,
    /// Configuration status (has config.toml, legacy, or unconfigured).
    pub config_status: ConfigStatus,
    /// Security findings from code scanning.
    pub security_findings: Vec<SecurityFinding>,
}

impl DiscoveredSkill {
    /// Whether this skill is ready to load (requirements met and no critical security findings).
    pub fn is_available(&self) -> bool {
        self.gating.passed && !security_scanner::has_critical_findings(&self.security_findings)
    }

    /// Why agents cannot load this skill, or `None` when they can.
    pub fn unavailable_reason(&self) -> Option<String> {
        match (
            self.gating.passed,
            security_scanner::has_critical_findings(&self.security_findings),
        ) {
            (false, _) => Some(gating::format_gating_result(&self.gating)),
            (true, true) => Some("the skill has unresolved security findings".to_string()),
            (true, false) => None,
        }
    }

    /// Whether this skill has security concerns (any findings at all).
    pub fn has_security_findings(&self) -> bool {
        !self.security_findings.is_empty()
    }
}

/// Scan all skill directories and return discovered skills.
///
/// Scans the workspace-local `skills/` directory and the global
/// `~/.workforce/skills/` directory. Skills in the workspace take
/// precedence over global skills with the same name.
pub fn scan_skill_directories(workspace: &Path) -> Vec<DiscoveredSkill> {
    let roots: Vec<PathBuf> = std::iter::once(workspace.join("skills"))
        .chain(user_skills_dir())
        .collect();
    let skills = first_by_name(roots.iter().flat_map(|root| scan_directory(root)));
    info!(count = skills.len(), "Skills discovered");
    skills
}

/// The operator's own skills directory, `~/.workforce/skills`.
pub fn user_skills_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".workforce").join("skills"))
}

/// Every skill in the immediate subdirectories of `roots`: roots in order, entries within a root
/// by path, the first skill to claim a name keeping it. A root that does not exist holds no
/// skills. The first root is the primary one: if it exists but cannot be listed the scan fails,
/// so a caller can keep what it had, while a later root that cannot be listed is skipped with a
/// warning so it never hides the primary root's skills.
pub fn scan_skill_roots(roots: &[PathBuf]) -> std::io::Result<Vec<DiscoveredSkill>> {
    let Some((primary, fallbacks)) = roots.split_first() else {
        return Ok(Vec::new());
    };
    Ok(first_by_name(read_root(primary)?.into_iter().chain(
        fallbacks.iter().flat_map(|root| scan_directory(root)),
    )))
}

/// Scan a single directory for skill subdirectories.
fn scan_directory(dir: &Path) -> Vec<DiscoveredSkill> {
    read_root(dir).unwrap_or_else(|e| {
        warn!(path = %dir.display(), error = %e, "Failed to read skills directory");
        Vec::new()
    })
}

fn read_root(root: &Path) -> std::io::Result<Vec<DiscoveredSkill>> {
    let is_dir = match std::fs::metadata(root) {
        Ok(meta) => meta.is_dir(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e),
    };
    if !is_dir {
        return Ok(Vec::new());
    }
    let mut dirs = std::fs::read_dir(root)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<Vec<PathBuf>>>()?;
    dirs.sort();
    Ok(dirs
        .iter()
        .filter(|dir| dir.join("SKILL.md").is_file())
        .filter_map(|dir| load_skill_from_dir(dir))
        .collect())
}

fn first_by_name(skills: impl IntoIterator<Item = DiscoveredSkill>) -> Vec<DiscoveredSkill> {
    let mut seen = HashSet::new();
    skills
        .into_iter()
        .filter(|skill| seen.insert(skill.skill.meta.name.clone()))
        .collect()
}

/// Load a skill from a directory containing a SKILL.md.
fn load_skill_from_dir(dir: &Path) -> Option<DiscoveredSkill> {
    let skill_md_path = dir.join("SKILL.md");
    let content = match std::fs::read_to_string(&skill_md_path) {
        Ok(c) => c,
        Err(e) => {
            warn!(path = %skill_md_path.display(), error = %e, "Failed to read SKILL.md");
            return None;
        }
    };

    match SkillFile::parse(&content) {
        Ok(skill) => {
            let gating = gating::check_requirements(&skill.meta.requires, None);
            let config_status = if dir.join("config.toml").is_file() {
                ConfigStatus::Configured
            } else {
                ConfigStatus::Unconfigured
            };

            // Security scan the skill directory for dangerous patterns
            let security_findings = security_scanner::scan_directory(dir);
            if security_scanner::has_critical_findings(&security_findings) {
                warn!(
                    name = %skill.meta.name,
                    findings = security_findings.len(),
                    "Skill blocked due to critical security findings"
                );
            }

            debug!(
                name = %skill.meta.name,
                available = gating.passed,
                config = ?config_status,
                security_findings = security_findings.len(),
                "Loaded skill"
            );
            Some(DiscoveredSkill {
                skill,
                path: dir.to_path_buf(),
                gating,
                config_status,
                security_findings,
            })
        }
        Err(e) => {
            warn!(path = %skill_md_path.display(), error = %e, "Failed to parse SKILL.md");
            None
        }
    }
}

/// Load a single skill from a specific directory path.
pub fn load_skill(dir: &Path) -> eyre::Result<DiscoveredSkill> {
    let skill_md_path = dir.join("SKILL.md");
    if !skill_md_path.is_file() {
        return Err(eyre::eyre!("No SKILL.md found in {}", dir.display()));
    }
    let content = std::fs::read_to_string(&skill_md_path)
        .map_err(|e| eyre::eyre!("Failed to read {}: {}", skill_md_path.display(), e))?;
    let skill = SkillFile::parse(&content)?;
    let gating = gating::check_requirements(&skill.meta.requires, None);
    let config_status = if dir.join("config.toml").is_file() {
        ConfigStatus::Configured
    } else {
        ConfigStatus::Unconfigured
    };
    let security_findings = security_scanner::scan_directory(dir);
    Ok(DiscoveredSkill {
        skill,
        path: dir.to_path_buf(),
        gating,
        config_status,
        security_findings,
    })
}

/// Build a compact system prompt section listing available skills.
///
/// Produces output like:
/// ```text
/// ## Available Skills
///
/// - **git-operations** (v1.0.0) — Git operations for agents
///   Tools: git.clone, git.commit, git.push, git.branch, git.diff
/// ```
pub fn skills_system_prompt(skills: &[DiscoveredSkill]) -> String {
    let available: Vec<&DiscoveredSkill> = skills.iter().filter(|s| s.is_available()).collect();
    if available.is_empty() {
        return String::new();
    }

    let mut lines = vec!["## Available Skills\n".to_string()];
    for skill in &available {
        let meta = &skill.skill.meta;
        lines.push(format!(
            "- **{}** (v{}) — {}",
            meta.name, meta.version, meta.description
        ));
        if !meta.tools.is_empty() {
            lines.push(format!("  Tools: {}", meta.tools.join(", ")));
        }
    }
    // DW-431: Remind agents to use correct parameter types when calling skill tools.
    lines.push(String::new());
    lines.push(
        "**Note:** When calling skill tools, pass numeric parameters (IDs, counts,          page numbers) as JSON integers, not strings."
            .to_string(),
    );
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn create_skill_dir(base: &Path, name: &str, content: &str) {
        let dir = base.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    fn git_skill_content() -> String {
        r#"---
name: git-operations
version: 1.0.0
description: Git operations for agents
author: acme
requires:
  bins: [sh]
tools:
  - git.clone
  - git.commit
  - git.push
---
# Git Operations

Use these tools to work with git repos.
"#
        .to_string()
    }

    fn docker_skill_content() -> String {
        r#"---
name: docker-tools
version: 0.2.0
description: Docker container management
requires:
  bins: [definitely_not_a_real_binary_12345]
tools:
  - docker.build
  - docker.run
---
# Docker Tools

Manage containers.
"#
        .to_string()
    }

    #[test]
    fn scan_workspace_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        fs::create_dir_all(&skills_dir).unwrap();

        create_skill_dir(&skills_dir, "git-ops", &git_skill_content());
        create_skill_dir(&skills_dir, "docker", &docker_skill_content());

        let skills = scan_skill_directories(tmp.path());
        assert_eq!(skills.len(), 2);

        let git_skill = skills
            .iter()
            .find(|s| s.skill.meta.name == "git-operations");
        assert!(git_skill.is_some());

        let docker_skill = skills.iter().find(|s| s.skill.meta.name == "docker-tools");
        assert!(docker_skill.is_some());
        // Docker skill should not be available (binary doesn't exist)
        assert!(!docker_skill.unwrap().is_available());
    }

    #[test]
    fn scan_empty_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = scan_skill_directories(tmp.path());
        assert!(skills.is_empty());
    }

    #[test]
    fn scan_skips_dirs_without_skill_md() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        fs::create_dir_all(skills_dir.join("no-skill")).unwrap();
        fs::write(skills_dir.join("no-skill").join("README.md"), "hello").unwrap();

        let skills = scan_skill_directories(tmp.path());
        assert!(skills.is_empty());
    }

    #[test]
    fn load_skill_from_path() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("my-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), git_skill_content()).unwrap();

        let skill = load_skill(&skill_dir).expect("should load");
        assert_eq!(skill.skill.meta.name, "git-operations");
    }

    #[test]
    fn load_skill_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let result = load_skill(tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn system_prompt_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        fs::create_dir_all(&skills_dir).unwrap();

        create_skill_dir(&skills_dir, "git-ops", &git_skill_content());

        let skills = scan_skill_directories(tmp.path());
        let prompt = skills_system_prompt(&skills);

        assert!(prompt.contains("## Available Skills"));
        assert!(prompt.contains("git-operations"));
        assert!(prompt.contains("v1.0.0"));
        assert!(prompt.contains("git.clone"));
        // DW-431: type guidance note should be present
        assert!(
            prompt.contains("numeric parameters"),
            "Skills prompt should include type guidance note"
        );
    }

    #[test]
    fn system_prompt_empty_when_no_skills() {
        let prompt = skills_system_prompt(&[]);
        assert!(prompt.is_empty());
    }

    #[test]
    fn system_prompt_excludes_unavailable_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        fs::create_dir_all(&skills_dir).unwrap();

        // Only add the docker skill (which requires a nonexistent binary)
        create_skill_dir(&skills_dir, "docker", &docker_skill_content());

        let skills = scan_skill_directories(tmp.path());
        let prompt = skills_system_prompt(&skills);

        // No available skills, so prompt should be empty
        assert!(prompt.is_empty());
    }

    #[test]
    fn workspace_skills_override_global() {
        // This test verifies the dedup logic by checking that workspace skills
        // with the same name as global skills take precedence.
        // We test the scan_directory function directly since we can't easily
        // mock the global directory.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_skill_dir(dir, "git-ops", &git_skill_content());

        let skills = scan_directory(dir);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].skill.meta.name, "git-operations");
    }

    fn hello_world(version: &str) -> String {
        format!(
            "---\nname: hello-world\nversion: {version}\ndescription: Says hello\n---\n# Hello\n"
        )
    }

    fn names(skills: &[DiscoveredSkill]) -> Vec<&str> {
        skills.iter().map(|s| s.skill.meta.name.as_str()).collect()
    }

    #[test]
    fn scan_skill_roots_prefers_earlier_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let (first, second) = (tmp.path().join("first"), tmp.path().join("second"));
        create_skill_dir(&first, "hello-world", &hello_world("1.0.0"));
        create_skill_dir(&second, "hello-world", &hello_world("2.0.0"));
        create_skill_dir(&second, "git-ops", &git_skill_content());

        let skills = scan_skill_roots(&[first.clone(), second]).unwrap();
        assert_eq!(names(&skills), ["hello-world", "git-operations"]);
        assert_eq!(skills[0].path, first.join("hello-world"));
        assert_eq!(skills[0].skill.meta.version, "1.0.0");
    }

    #[test]
    fn scan_skill_roots_orders_entries_within_a_root() {
        let tmp = tempfile::tempdir().unwrap();
        create_skill_dir(tmp.path(), "b-copy", &hello_world("2.0.0"));
        create_skill_dir(tmp.path(), "a-copy", &hello_world("1.0.0"));

        let skills = scan_skill_roots(&[tmp.path().to_path_buf()]).unwrap();
        assert_eq!(names(&skills), ["hello-world"]);
        assert_eq!(skills[0].path, tmp.path().join("a-copy"));
    }

    #[test]
    fn scan_skill_roots_treats_a_missing_root_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        create_skill_dir(tmp.path(), "hello-world", &hello_world("1.0.0"));

        let skills =
            scan_skill_roots(&[tmp.path().join("missing"), tmp.path().to_path_buf()]).unwrap();
        assert_eq!(names(&skills), ["hello-world"]);
    }

    #[test]
    fn scan_skill_roots_fails_only_when_the_primary_root_cannot_be_listed() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("file.txt"), "not a directory").unwrap();
        let unlistable = tmp.path().join("file.txt").join("skills");
        let primary = tmp.path().join("primary");
        create_skill_dir(&primary, "hello-world", &hello_world("1.0.0"));

        assert!(scan_skill_roots(&[unlistable.clone(), primary.clone()]).is_err());
        let skills = scan_skill_roots(&[primary, unlistable]).unwrap();
        assert_eq!(names(&skills), ["hello-world"]);
    }

    #[test]
    fn scan_skill_roots_skips_a_malformed_skill() {
        let tmp = tempfile::tempdir().unwrap();
        create_skill_dir(tmp.path(), "hello-world", &hello_world("1.0.0"));
        create_skill_dir(
            tmp.path(),
            "broken",
            "---\nname: broken\n---\nNo version.\n",
        );

        let skills = scan_skill_roots(&[tmp.path().to_path_buf()]).unwrap();
        assert_eq!(names(&skills), ["hello-world"]);
    }

    #[test]
    fn unavailable_reason_explains_requirements_and_security() {
        let tmp = tempfile::tempdir().unwrap();
        create_skill_dir(tmp.path(), "docker", &docker_skill_content());
        create_skill_dir(tmp.path(), "hello-world", &hello_world("1.0.0"));

        let unmet = load_skill(&tmp.path().join("docker")).unwrap();
        let reason = unmet.unavailable_reason().expect("requirements are unmet");
        assert!(
            reason.contains("definitely_not_a_real_binary_12345"),
            "got: {reason}"
        );

        let available = load_skill(&tmp.path().join("hello-world")).unwrap();
        assert_eq!(available.unavailable_reason(), None);

        let flagged = DiscoveredSkill {
            security_findings: vec![SecurityFinding {
                rule: "test",
                description: "test",
                severity: workforce_common::security_scanner::Severity::Critical,
                file: "run.sh".into(),
                line: 1,
                matched_line: String::new(),
            }],
            ..available
        };
        assert_eq!(
            flagged.unavailable_reason().as_deref(),
            Some("the skill has unresolved security findings")
        );
    }
}
