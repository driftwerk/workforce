//! Where the hub's private state sits, closed to all but the hub uid (see
//! [`workforce_common::private_fs`]).
//!
//! Each store closes its own tree when it opens it. This covers what nothing
//! opens any more — the file stores an install moved to Postgres away from, a
//! traces tree with capture switched off — and is the repair for a live
//! install: applied at every boot, by position, as the owner, and never by
//! walking a tree. That last point is deliberate. An agent workspace holds the
//! repos and scratch files the agent uid must keep writing beside its vault and
//! transcripts, so only the named subtrees are touched, never the workspace.

use std::path::Path;

use workforce_common::config_files;
use workforce_common::private_fs::{
    close_dated_notes, close_private_dir, close_private_sqlite_files, ensure_private_dir,
    withhold_dated_notes, withhold_from_tools, withhold_per_person,
};
use workforce_common::settings::WorkforceConfig;

/// Per-user files inside an agent workspace, closed whether or not they exist
/// yet: their writers are many, and a directory created here first is one none
/// of them can create wider.
const AGENT_PER_USER_DIRS: [&str; 2] = ["memory/users", "users"];

/// Stores a backend owns inside an agent workspace; closed when present.
const AGENT_STORE_DIRS: [&str; 2] = ["sessions", "artifacts-v2"];

/// Beside the per-user and store directories, the agent-level state that
/// holds each person's own records: their reminders and scheduled jobs, and
/// their background jobs' parameters and output.
const AGENT_PER_PERSON_RECORDS: [&str; 2] = ["scheduler", ".local/jobs"];

/// Stores directly under the data dir; closed when present.
const HUB_STORE_DIRS: [&str; 5] = [
    "artifacts",
    "cc-briefs",
    "archived_sessions",
    "memory-backups",
    "scheduler",
];

/// Definition stores directly under the data dir, which nothing but the hub
/// reads: created if absent, so the agent uid cannot create them first.
const HUB_CONFIG_DIRS: [&str; 5] = ["roles", "workflows", "playbooks", "monitors", "wasm-skills"];

/// Close the hub-wide private state and configuration under `config`'s data
/// dir. The data dir and the agents dir become sticky, so the agent uid can no
/// longer rename or remove what the hub keeps in them; the skills directory
/// stays readable, since a skill's own scripts run as the agent.
pub fn close_hub_state(config: &WorkforceConfig) {
    let data_dir = Path::new(&config.hub.data_dir);
    close_private_sqlite_files(data_dir);
    close_private_dir(&crate::trace_writer::traces_dir(config));
    HUB_STORE_DIRS
        .iter()
        .for_each(|dir| close_private_dir(&data_dir.join(dir)));
    for dir in HUB_CONFIG_DIRS.iter().map(|dir| data_dir.join(dir)) {
        if let Err(e) = ensure_private_dir(&dir) {
            tracing::warn!(path = %dir.display(), error = %e, "Could not close hub configuration");
        }
    }
    config_files::close_tree(&config.skills.resolved_skills_dir(&config.hub.data_dir));
    config_files::close(&data_dir.join(workforce_common::config_overlay::OVERLAY_FILE));
    [data_dir.to_path_buf(), data_dir.join("agents")]
        .iter()
        .for_each(|dir| config_files::make_sticky(dir));
    report_runner(config);
}

/// Say whose configuration files are ignored, or that none can be told apart
/// though commands run as another user.
fn report_runner(config: &WorkforceConfig) {
    match (config.exec.runner_socket.is_some(), config_files::runner_uid()) {
        (_, Some(uid)) => tracing::info!(
            uid,
            "Configuration files owned by the agent runner's uid are ignored"
        ),
        (true, None) => tracing::error!(
            variable = config_files::RUNNER_USER_ENV,
            "Commands run apart from the hub, but it was not told as whom: configuration an agent's commands create cannot be told from the hub's"
        ),
        (false, None) => {}
    }
}

/// Close an agent workspace's private subtrees, creating the per-user ones
/// and the directories of configuration the hub acts on unattended (so the
/// agent uid cannot create them first), and close its configuration files.
/// What it keeps for each person is withheld from the agent's tools, which
/// run as the hub, save each person's own notes and profile from their own
/// turn; the shared dated notes are withheld from a person's turn. Never
/// fails the agent's load: a position that cannot be closed is logged, and
/// `privsep-verify.sh` reports it.
pub fn close_agent_state(workspace: &Path) {
    for dir in AGENT_PER_USER_DIRS
        .iter()
        .chain(&config_files::AGENT_CONFIG_DIRS)
        .map(|dir| workspace.join(dir))
    {
        if let Err(e) = ensure_private_dir(&dir) {
            tracing::warn!(path = %dir.display(), error = %e, "Could not close agent state");
        }
    }
    AGENT_STORE_DIRS
        .iter()
        .for_each(|dir| close_private_dir(&workspace.join(dir)));
    config_files::close_agent_workspace(workspace);
    AGENT_PER_USER_DIRS
        .iter()
        .for_each(|dir| withhold_per_person(&workspace.join(dir)));
    AGENT_STORE_DIRS
        .iter()
        .chain(&AGENT_PER_PERSON_RECORDS)
        .for_each(|dir| withhold_from_tools(&workspace.join(dir)));
    let shared_notes = workspace.join("memory");
    close_dated_notes(&shared_notes);
    withhold_dated_notes(&shared_notes);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn shared_dir(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o2775)).unwrap();
    }

    #[test]
    fn an_agent_workspace_keeps_its_shared_tree_and_loses_only_its_private_ones() {
        let workspace = tempfile::tempdir().unwrap();
        let ws = workspace.path();
        shared_dir(ws);
        for dir in [
            "sessions",
            "artifacts-v2",
            "scheduler",
            "memory",
            "repos/app",
        ] {
            shared_dir(&ws.join(dir));
        }
        let script = ws.join("report.py");
        std::fs::write(&script, "print(1)").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o664)).unwrap();

        close_agent_state(ws);

        for private in [
            "sessions",
            "artifacts-v2",
            "scheduler",
            "memory/users",
            "users",
        ] {
            assert_eq!(mode(&ws.join(private)), 0o700, "{private}");
        }
        assert_eq!(mode(ws), 0o775);
        assert_eq!(mode(&ws.join("memory")), 0o775);
        assert_eq!(mode(&ws.join("repos/app")), 0o775);
        assert_eq!(mode(&script), 0o664);
    }

    /// A dated note in the shared notes directory mixes whoever spoke that
    /// day, so it is closed at every load; the shared notes beside it are
    /// what the agent's users and commands share, and stay open.
    #[test]
    fn an_agent_workspace_closes_its_shared_dated_notes_only() {
        let workspace = tempfile::tempdir().unwrap();
        let ws = workspace.path();
        shared_dir(&ws.join("memory"));
        for note in ["2026-03-14.md", "active-task.md"] {
            std::fs::write(ws.join("memory").join(note), "notes").unwrap();
            std::fs::set_permissions(
                ws.join("memory").join(note),
                std::fs::Permissions::from_mode(0o664),
            )
            .unwrap();
        }

        close_agent_state(ws);

        assert_eq!(mode(&ws.join("memory/2026-03-14.md")), 0o600);
        assert_eq!(mode(&ws.join("memory/active-task.md")), 0o664);
        assert_eq!(mode(&ws.join("memory")), 0o775);
    }

    /// What the hub runs unattended, and the files that configure the agent,
    /// are closed at every load: the directories exist before the agent uid
    /// could make them, and the workspace is sticky so nothing the hub made
    /// in it can be renamed away.
    #[test]
    fn an_agent_workspace_closes_its_configuration() {
        let workspace = tempfile::tempdir().unwrap();
        let ws = workspace.path();
        shared_dir(ws);
        shared_dir(&ws.join("heartbeats"));
        std::fs::write(ws.join("config.toml"), "[scope]").unwrap();
        std::fs::set_permissions(
            ws.join("config.toml"),
            std::fs::Permissions::from_mode(0o664),
        )
        .unwrap();

        close_agent_state(ws);

        for dir in config_files::AGENT_CONFIG_DIRS {
            assert_eq!(mode(&ws.join(dir)), 0o700, "{dir}");
        }
        assert_eq!(mode(&ws.join("config.toml")), 0o644);
        let sticky = std::fs::metadata(ws).unwrap().permissions().mode() & 0o1000;
        assert_eq!(sticky, 0o1000);
    }

    #[test]
    fn a_position_that_cannot_be_closed_does_not_stop_the_rest_or_the_load() {
        let workspace = tempfile::tempdir().unwrap();
        let ws = workspace.path();
        std::fs::write(ws.join("memory"), "not a directory").unwrap();
        shared_dir(&ws.join("sessions"));

        close_agent_state(ws);

        assert!(ws.join("memory").is_file());
        assert_eq!(mode(&ws.join("users")), 0o700);
        assert_eq!(mode(&ws.join("sessions")), 0o700);
    }

    #[test]
    fn an_absent_store_is_not_created_in_an_agent_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        close_agent_state(workspace.path());
        assert!(!workspace.path().join("sessions").exists());
        assert!(!workspace.path().join("artifacts-v2").exists());
    }

    #[test]
    fn the_hub_wide_stores_close_and_the_runner_trees_stay_shared() {
        let data = tempfile::tempdir().unwrap();
        let root = data.path();
        shared_dir(root);
        for dir in [
            "traces/agent",
            "artifacts",
            "cc-briefs",
            "memory-backups",
            "scheduler",
            "worktrees",
            "agents",
        ] {
            shared_dir(&root.join(dir));
        }
        let db = root.join("telemetry.db");
        std::fs::write(&db, b"").unwrap();
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut config = WorkforceConfig::default();
        config.hub.data_dir = root.to_string_lossy().into_owned();

        let skill = root.join("skills/deploy");
        shared_dir(&skill);
        std::fs::write(skill.join("SKILL.md"), "x").unwrap();
        std::fs::set_permissions(
            skill.join("SKILL.md"),
            std::fs::Permissions::from_mode(0o664),
        )
        .unwrap();
        let overlay = root.join(workforce_common::config_overlay::OVERLAY_FILE);
        std::fs::write(&overlay, "").unwrap();
        std::fs::set_permissions(&overlay, std::fs::Permissions::from_mode(0o664)).unwrap();

        close_hub_state(&config);

        for private in [
            "traces",
            "artifacts",
            "cc-briefs",
            "memory-backups",
            "scheduler",
        ]
        .into_iter()
        .chain(HUB_CONFIG_DIRS)
        {
            assert_eq!(mode(&root.join(private)), 0o700, "{private}");
        }
        assert_eq!(mode(&skill), 0o755);
        assert_eq!(mode(&skill.join("SKILL.md")), 0o644);
        assert_eq!(mode(&overlay), 0o644);
        for sticky in [root.to_path_buf(), root.join("agents")] {
            let bits = std::fs::metadata(&sticky).unwrap().permissions().mode() & 0o1000;
            assert_eq!(bits, 0o1000, "{}", sticky.display());
        }
        assert_eq!(mode(&db), 0o600);
        assert_eq!(mode(root), 0o775);
        assert_eq!(mode(&root.join("worktrees")), 0o775);
        assert_eq!(mode(&root.join("agents")), 0o775);
    }
}
