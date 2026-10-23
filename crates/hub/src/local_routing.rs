//! Local agent execution routing.
//!
//! Determines whether a tool call should execute on the gateway or be
//! forwarded to a connected local agent. Identity files (SOUL.md, AGENTS.md,
//! etc.) always stay on the gateway for safety (DW-327).

/// Where a tool should execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionTarget {
    /// Always execute on the gateway server.
    Gateway,
    /// Route to local agent if one is selected for the session.
    LocalPreferred,
}

/// Determine execution target for a tool by name.
pub fn execution_target(tool_name: &str) -> ExecutionTarget {
    match tool_name {
        "exec.run" => ExecutionTarget::LocalPreferred,
        // Future extensions: file read/write tools can be added here
        _ => ExecutionTarget::Gateway,
    }
}

/// Paths that must always be accessed on the gateway (DW-327).
///
/// These define agent identity and session state. Local agents must never
/// modify them directly — all access goes through gateway filesystem.
const PROTECTED_PATTERNS: &[&str] = &[
    "SOUL.md",
    "AGENTS.md",
    "TOOLS.md",
    "IDENTITY.md",
    "HEARTBEAT.md",
    "WELCOME.md",
    "/memory/",
    "/notes/",
    "/.local/",
];

/// Check if a shell command touches protected identity files.
///
/// If true, the command should be routed to the gateway even if a
/// local agent is selected for the session.
pub fn is_protected_path(command: &str) -> bool {
    PROTECTED_PATTERNS.iter().any(|p| command.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_run_is_local_preferred() {
        assert_eq!(
            execution_target("exec.run"),
            ExecutionTarget::LocalPreferred
        );
    }

    #[test]
    fn other_tools_are_gateway() {
        assert_eq!(execution_target("slack.send"), ExecutionTarget::Gateway);
        assert_eq!(execution_target("github.pr"), ExecutionTarget::Gateway);
    }

    #[test]
    fn identity_files_are_protected() {
        assert!(is_protected_path("cat SOUL.md"));
        assert!(!is_protected_path("echo 'test' >> MEMORY.md"));
        assert!(is_protected_path("ls /memory/users/"));
        assert!(is_protected_path("cat /.local/config"));
    }

    #[test]
    fn normal_commands_not_protected() {
        assert!(!is_protected_path("cargo build"));
        assert!(!is_protected_path("ls -la"));
        assert!(!is_protected_path("git status"));
    }
}
