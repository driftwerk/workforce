//! Claude Code sessions started outside Desk (iTerm, Ghostty, an IDE).
//! `claude agents --json` lists every live session on the machine —
//! verified on CLI 2.1.310 — so Desk can show them and, once they are
//! gone, resume them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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

/// What the hub itself runs `claude` in and as: its own folders (its data
/// directory, its agents' hot work trees, its worktrees) and its own
/// process. A session in one of them, or one the hub started itself, is
/// the hub's (a cheap completion, an agent's turn, a coding round), never
/// one of the person's to adopt.
#[derive(Debug, Clone, Default)]
pub struct HubInternal {
    /// Each folder as configured and with its links resolved, as
    /// `claude agents` reports a cwd.
    dirs: Vec<PathBuf>,
    /// The hub's own process: a `claude` it started is its child.
    parent: Option<u32>,
}

impl HubInternal {
    /// The hub's folders `dirs` (absolute). One that holds `home` — the
    /// filesystem root, or the home folder itself — is left out: it would
    /// hold every session of the person's.
    pub fn new(dirs: impl IntoIterator<Item = PathBuf>, home: Option<&Path>) -> Self {
        let homes: Vec<PathBuf> = home
            .into_iter()
            .flat_map(|h| [h.to_path_buf(), resolved(h)])
            .collect();
        let mut held: Vec<PathBuf> = Vec::new();
        for dir in dirs {
            for form in [resolved(&dir), dir] {
                if form.parent().is_none() || homes.iter().any(|h| h.starts_with(&form)) {
                    continue;
                }
                if !held.contains(&form) {
                    held.push(form);
                }
            }
        }
        Self {
            dirs: held,
            parent: None,
        }
    }

    /// The same, with the processes `pid` starts the hub's too.
    pub fn started_by(self, pid: u32) -> Self {
        Self {
            parent: Some(pid),
            ..self
        }
    }

    /// Whether `cwd` is in one of the hub's folders: as reported, or with
    /// the links of as much of it as still exists resolved (a completion's
    /// folder is gone once it answers).
    pub fn holds(&self, cwd: &Path) -> bool {
        if self.dirs.is_empty() || !cwd.is_absolute() {
            return false;
        }
        let real = resolved(cwd);
        self.dirs
            .iter()
            .any(|dir| cwd.starts_with(dir) || real.starts_with(dir))
    }

    /// Whether the process `pid` is one the hub started itself, by `table`.
    pub fn started(&self, table: &[crate::procs::Proc], pid: i32) -> bool {
        let (Some(parent), Ok(pid)) = (self.parent, u32::try_from(pid)) else {
            return false;
        };
        table.iter().any(|p| p.pid == pid && p.ppid == parent)
    }

    /// Whether processes are told apart by who started them.
    pub fn knows_its_own(&self) -> bool {
        self.parent.is_some()
    }
}

/// `path` with the links of its longest existing part resolved, the rest
/// as given; `path` itself when none of it resolves.
fn resolved(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut at = path;
    loop {
        if let Ok(real) = std::fs::canonicalize(at) {
            return rest.iter().rev().fold(real, |p, part| p.join(part));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                at = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
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
    fn what_runs_in_the_hubs_folders_is_the_hubs_and_nothing_else_is() {
        let hub = HubInternal::new(
            [PathBuf::from("/data"), PathBuf::from("/hot/work")],
            Some(Path::new("/nohome/k")),
        );
        let holds = |p: &str| hub.holds(Path::new(p));
        assert!(holds(
            "/data/cli-completions/b369e00e-c276-4ae0-8394-6d83707380a0"
        ));
        assert!(holds(
            "/data/agents/scout/projects/_agent-turn/work/session/at-dm-d4774cbb6c89-2e"
        ));
        assert!(holds("/hot/work/scout/_agent-turn/session/at-dm-1/sub"));
        assert!(holds("/data/worktrees/fix-1"));
        assert!(!holds("/nohome/k/dev/app"), "the person's folder");
        assert!(!holds("/"), "an ancestor of the hub's");
        assert!(!holds("/data-old/x"), "a sibling sharing its prefix");
        assert!(!holds("/hot/elsewhere"));
        assert!(!holds("data/cli-completions/x"), "not a reported cwd");
        assert!(
            !HubInternal::default().holds(Path::new("/data/cli-completions/x")),
            "no folders, nothing held"
        );
    }

    #[test]
    fn a_folder_holding_the_home_folder_is_never_the_hubs() {
        let hub = HubInternal::new(
            [PathBuf::from("/"), PathBuf::from("/nohome/k")],
            Some(Path::new("/nohome/k")),
        );
        assert!(!hub.holds(Path::new("/nohome/k/dev/app")));
        assert!(!hub.holds(Path::new("/srv/x")));
    }

    #[test]
    fn the_hubs_folder_is_known_through_its_links() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir_all(real.join("data/cli-completions/live")).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let real = std::fs::canonicalize(&real).unwrap();

        let through_link = HubInternal::new([link.join("data")], None);
        assert!(through_link.holds(&real.join("data/cli-completions/live")));
        assert!(
            through_link.holds(&real.join("data/cli-completions/gone")),
            "a completion's folder, gone once it answered"
        );
        assert!(!through_link.holds(&real.join("other")));

        let real_dir = HubInternal::new([real.join("data")], None);
        assert!(real_dir.holds(&link.join("data/cli-completions/live")));
        assert!(real_dir.holds(&link.join("data/cli-completions/gone")));
        assert!(!real_dir.holds(&link.join("other")));
    }

    /// macOS's `/tmp` is a link to `/private/tmp`, which is what a session
    /// started in it reports.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_hub_in_tmp_holds_what_runs_in_private_tmp() {
        let dir = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let name = dir.path().file_name().unwrap().to_owned();
        let tmp = Path::new("/tmp").join(&name);
        let private = Path::new("/private/tmp").join(&name);

        let hub = HubInternal::new([tmp.join("data")], None);
        assert!(hub.holds(&private.join("data/cli-completions/u")));
        let hub = HubInternal::new([private.join("data")], None);
        assert!(hub.holds(&tmp.join("data/cli-completions/u")));
        assert!(!hub.holds(&private.join("elsewhere")));
    }

    #[test]
    fn a_process_the_hub_started_is_its_own() {
        let table =
            crate::procs::parse("  1 0 launchd\n 50 1 hub\n 51 50 claude -p\n 61 60 claude\n");
        let hub = HubInternal::default().started_by(50);
        assert!(hub.started(&table, 51));
        assert!(!hub.started(&table, 61), "a terminal's");
        assert!(!hub.started(&table, 99), "not listed");
        assert!(
            !HubInternal::default().started(&table, 51),
            "no parent known"
        );
    }

    #[test]
    fn this_process_is_alive() {
        assert!(alive(std::process::id() as i32));
        assert!(!alive(i32::MAX - 1));
    }
}
