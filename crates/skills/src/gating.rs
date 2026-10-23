//! Gating checks — verify that a skill's requirements are met before loading.
//!
//! Checks include: required binaries on PATH, required environment variables,
//! and required config keys.

use std::collections::HashMap;

use crate::skill_file::SkillRequires;

/// Result of a gating check — reports which requirements passed and which failed.
#[derive(Debug, Clone)]
pub struct GatingResult {
    /// Whether all requirements are met.
    pub passed: bool,
    /// Individual check results.
    pub checks: Vec<GatingCheck>,
}

/// A single gating check result.
#[derive(Debug, Clone)]
pub struct GatingCheck {
    /// What was checked (e.g., "bin:git", "env:GITHUB_TOKEN").
    pub requirement: String,
    /// Whether this check passed.
    pub passed: bool,
    /// Human-readable reason if failed.
    pub reason: Option<String>,
}

/// Check all requirements for a skill.
///
/// `config` is an optional map of config keys that the caller provides,
/// used to verify `requires.config` entries.
pub fn check_requirements(
    requires: &SkillRequires,
    config: Option<&HashMap<String, String>>,
) -> GatingResult {
    let mut checks = Vec::new();

    // Check required binaries
    for bin in &requires.bins {
        let found = which_exists(bin);
        checks.push(GatingCheck {
            requirement: format!("bin:{}", bin),
            passed: found,
            reason: if found {
                None
            } else {
                Some(format!("Binary '{}' not found on PATH", bin))
            },
        });
    }

    // Check required environment variables
    for var in &requires.env {
        let set = std::env::var(var).is_ok();
        checks.push(GatingCheck {
            requirement: format!("env:{}", var),
            passed: set,
            reason: if set {
                None
            } else {
                Some(format!("Environment variable '{}' is not set", var))
            },
        });
    }

    // Check required config keys
    for key in &requires.config {
        let present = config.is_some_and(|c| c.contains_key(key));
        checks.push(GatingCheck {
            requirement: format!("config:{}", key),
            passed: present,
            reason: if present {
                None
            } else {
                Some(format!("Config key '{}' is not set", key))
            },
        });
    }

    let passed = checks.iter().all(|c| c.passed);
    GatingResult { passed, checks }
}

/// Format the gating result as a human-readable summary.
pub fn format_gating_result(result: &GatingResult) -> String {
    if result.passed {
        return "All requirements met.".to_string();
    }

    let failures: Vec<&GatingCheck> = result.checks.iter().filter(|c| !c.passed).collect();
    let mut lines = vec![format!("{} requirement(s) not met:", failures.len())];
    for check in failures {
        let reason = check.reason.as_deref().unwrap_or("unknown reason");
        lines.push(format!("  ✗ {} — {}", check.requirement, reason));
    }
    lines.join("\n")
}

/// Check if a binary exists on PATH using which-like lookup.
fn which_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let candidate = dir.join(name);
                candidate.is_file() || candidate.with_extension("exe").is_file()
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill_file::SkillRequires;

    #[test]
    fn empty_requirements_pass() {
        let requires = SkillRequires::default();
        let result = check_requirements(&requires, None);
        assert!(result.passed);
        assert!(result.checks.is_empty());
    }

    #[test]
    fn binary_check_finds_common_binary() {
        // `sh` should exist on any Unix system
        let requires = SkillRequires {
            bins: vec!["sh".to_string()],
            env: Vec::new(),
            config: Vec::new(),
            ..Default::default()
        };
        let result = check_requirements(&requires, None);
        assert!(result.passed);
        assert_eq!(result.checks.len(), 1);
        assert!(result.checks[0].passed);
    }

    #[test]
    fn binary_check_fails_for_nonexistent() {
        let requires = SkillRequires {
            bins: vec!["definitely_not_a_real_binary_xyz_42".to_string()],
            env: Vec::new(),
            config: Vec::new(),
            ..Default::default()
        };
        let result = check_requirements(&requires, None);
        assert!(!result.passed);
        assert!(!result.checks[0].passed);
        assert!(result.checks[0].reason.is_some());
    }

    #[test]
    fn env_check_passes_for_set_var() {
        std::env::set_var("WORKFORCE_TEST_GATING_VAR", "hello");
        let requires = SkillRequires {
            bins: Vec::new(),
            env: vec!["WORKFORCE_TEST_GATING_VAR".to_string()],
            config: Vec::new(),
            ..Default::default()
        };
        let result = check_requirements(&requires, None);
        assert!(result.passed);
        std::env::remove_var("WORKFORCE_TEST_GATING_VAR");
    }

    #[test]
    fn env_check_fails_for_unset_var() {
        std::env::remove_var("WORKFORCE_TEST_DEFINITELY_UNSET_VAR");
        let requires = SkillRequires {
            bins: Vec::new(),
            env: vec!["WORKFORCE_TEST_DEFINITELY_UNSET_VAR".to_string()],
            config: Vec::new(),
            ..Default::default()
        };
        let result = check_requirements(&requires, None);
        assert!(!result.passed);
    }

    #[test]
    fn config_check_passes_when_key_present() {
        let mut config = HashMap::new();
        config.insert("database_url".to_string(), "postgres://...".to_string());
        let requires = SkillRequires {
            bins: Vec::new(),
            env: Vec::new(),
            config: vec!["database_url".to_string()],
            ..Default::default()
        };
        let result = check_requirements(&requires, Some(&config));
        assert!(result.passed);
    }

    #[test]
    fn config_check_fails_when_key_missing() {
        let config = HashMap::new();
        let requires = SkillRequires {
            bins: Vec::new(),
            env: Vec::new(),
            config: vec!["database_url".to_string()],
            ..Default::default()
        };
        let result = check_requirements(&requires, Some(&config));
        assert!(!result.passed);
    }

    #[test]
    fn config_check_fails_when_no_config_provided() {
        let requires = SkillRequires {
            bins: Vec::new(),
            env: Vec::new(),
            config: vec!["something".to_string()],
            ..Default::default()
        };
        let result = check_requirements(&requires, None);
        assert!(!result.passed);
    }

    #[test]
    fn mixed_requirements_partial_failure() {
        let requires = SkillRequires {
            bins: vec!["sh".to_string()],
            env: vec!["WORKFORCE_TEST_DEFINITELY_UNSET_MIXED".to_string()],
            config: Vec::new(),
            ..Default::default()
        };
        std::env::remove_var("WORKFORCE_TEST_DEFINITELY_UNSET_MIXED");
        let result = check_requirements(&requires, None);
        assert!(!result.passed);
        assert_eq!(result.checks.len(), 2);
        assert!(result.checks[0].passed); // sh exists
        assert!(!result.checks[1].passed); // env not set
    }

    #[test]
    fn format_gating_result_all_pass() {
        let result = GatingResult {
            passed: true,
            checks: vec![GatingCheck {
                requirement: "bin:git".to_string(),
                passed: true,
                reason: None,
            }],
        };
        assert_eq!(format_gating_result(&result), "All requirements met.");
    }

    #[test]
    fn format_gating_result_with_failures() {
        let result = GatingResult {
            passed: false,
            checks: vec![
                GatingCheck {
                    requirement: "bin:git".to_string(),
                    passed: true,
                    reason: None,
                },
                GatingCheck {
                    requirement: "env:TOKEN".to_string(),
                    passed: false,
                    reason: Some("Environment variable 'TOKEN' is not set".to_string()),
                },
            ],
        };
        let formatted = format_gating_result(&result);
        assert!(formatted.contains("1 requirement(s) not met:"));
        assert!(formatted.contains("env:TOKEN"));
    }
}
