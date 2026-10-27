//! Claude Code sessions started outside Desk (iTerm, Ghostty, an IDE).
//! `claude agents --json` lists every live session on the machine —
//! verified on CLI 2.1.310 — so Desk can show them and, once they are
//! gone, resume them.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use eyre::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct External {
    pub session_id: String,
    pub cwd: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub pid: Option<i32>,
    pub kind: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

impl External {
    /// Interactive sessions report `status` busy|idle; background ones
    /// report `state` working|….
    pub fn busy(&self) -> bool {
        matches!(
            self.status.as_deref().or(self.state.as_deref()),
            Some("busy" | "working")
        )
    }
}

pub fn parse(json: &[u8]) -> Result<Vec<External>> {
    serde_json::from_slice(json).context("reading `claude agents --json`")
}

pub async fn list(program: &Path, env: &BTreeMap<String, String>) -> Result<Vec<External>> {
    let out = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(program)
            .args(["agents", "--json"])
            .env_clear()
            .envs(env)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("`claude agents --json` timed out")?
    .context("running `claude agents --json`")?;
    if !out.status.success() {
        eyre::bail!(
            "`claude agents --json` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse(&out.stdout)
}

/// Whether a process id is still alive (signal 0 probes without signalling).
pub fn alive(pid: i32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_interactive_and_background_sessions() {
        let json = br#"[
          {"pid":15989,"cwd":"/w","kind":"interactive","startedAt":1,"sessionId":"a","name":"workforce-e7","status":"idle"},
          {"id":"5735dd7e","cwd":"/w","kind":"background","startedAt":2,"sessionId":"b","name":"Open sourcing","state":"working"},
          {"pid":7,"cwd":"/v","kind":"interactive","startedAt":3,"sessionId":"c","status":"busy"}
        ]"#;
        let all = parse(json).unwrap();
        assert_eq!(all.len(), 3);
        assert!(!all[0].busy());
        assert!(all[1].busy());
        assert_eq!(all[1].pid, None);
        assert!(all[2].busy());
        assert_eq!(all[2].name, None);
    }

    #[test]
    fn this_process_is_alive() {
        assert!(alive(std::process::id() as i32));
        assert!(!alive(i32::MAX - 1));
    }
}
