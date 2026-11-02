//! A `codex` typed in a Desk terminal, followed as a session. Codex has no
//! hooks Desk can count on (a hook needs the person's trust), so it is
//! found by its process and read from what Codex keeps on disk: which
//! session it is (`codex_session`), and whether a turn is under way.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use uuid::Uuid;

use super::terminals::CWD_EVERY;
use super::Desk;
use crate::codex_session::{self as session, Activity, Binding};
use crate::model::{Engine, SessionState};
use crate::procs::{self, Proc};
use crate::pty::Pty;
use crate::transcript;

/// The `codex` a terminal's shell is running: the TUI, not an app-server
/// or a worker of one.
fn codex_in(table: &[Proc], shell: u32) -> Option<&Proc> {
    procs::under(table, shell).into_iter().find(|p| {
        p.program().as_deref() == Some("codex")
            && !p.args.contains(" app-server")
            && !p.args.contains(" --remote")
    })
}

/// How long `pid` has run.
async fn age(pid: u32) -> Option<Duration> {
    let out = tokio::process::Command::new("ps")
        .args(["-o", "etime=", "-p", &pid.to_string()])
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    session::parse_etime(&text).map(Duration::from_secs)
}

impl Desk {
    /// Follows the `codex` in each terminal until the hub stops.
    pub(super) async fn follow_codex_terminals(self) {
        let mut tick = tokio::time::interval(CWD_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = self.0.cancel.cancelled() => return,
                _ = tick.tick() => {}
            }
            self.track_codex_terminals().await;
        }
    }

    /// The terminal's running session, when it is a Codex one.
    fn codex_session_in(&self, terminal: Uuid) -> Option<(Uuid, Option<String>)> {
        let id = self.terminal_session(terminal, None)?;
        let st = self.0.state.lock();
        let r = st.records.get(&id).filter(|r| r.engine == Engine::Codex)?;
        Some((id, r.cli_session_id.clone()))
    }

    pub(super) async fn track_codex_terminals(&self) {
        let Some(home) = self.0.env.get("HOME").map(PathBuf::from) else {
            return;
        };
        let terminals: Vec<(Uuid, Arc<Pty>)> = self
            .0
            .state
            .lock()
            .terminal_ptys
            .iter()
            .map(|(id, p)| (*id, Arc::clone(p)))
            .collect();
        if terminals.is_empty() {
            return;
        }
        let Some(table) = procs::table().await else {
            return;
        };
        for (terminal, pty) in terminals {
            let codex = pty.pid().and_then(|shell| codex_in(&table, shell));
            let tracked = self.codex_session_in(terminal);
            match (codex, tracked) {
                (None, Some((id, _))) => self.end_terminal_session(id, true),
                (Some(p), tracked) => {
                    let wanted = session::resume_arg(&p.args);
                    if let Some((id, cli)) = &tracked {
                        // Another session resumed in the same terminal.
                        if wanted.is_some() && wanted != cli.as_deref() {
                            self.end_terminal_session(*id, true);
                        } else {
                            self.follow_codex_session(*id).await;
                            continue;
                        }
                    }
                    let Some(found) = self.bind_codex(&home, p, wanted).await else {
                        continue;
                    };
                    let id = self.take_codex_session(terminal, pty, found).await;
                    self.follow_codex_session(id).await;
                }
                (None, None) => {}
            }
        }
    }

    /// The session the `codex` process `p` is.
    async fn bind_codex(
        &self,
        home: &std::path::Path,
        p: &Proc,
        wanted: Option<&str>,
    ) -> Option<Binding> {
        let home = home.to_path_buf();
        if let Some(id) = wanted {
            let (h, id) = (home.clone(), id.to_string());
            if let Some(b) = tokio::task::spawn_blocking(move || session::resumed(&h, &id))
                .await
                .ok()
                .flatten()
            {
                return Some(b);
            }
        }
        let cwd = crate::cwd::of_pid(p.pid)?;
        let since = SystemTime::now() - age(p.pid).await? - Duration::from_secs(5);
        let running: Vec<String> = {
            let st = self.0.state.lock();
            st.live
                .keys()
                .filter_map(|id| st.records.get(id))
                .filter(|r| r.engine == Engine::Codex)
                .filter_map(|r| r.cli_session_id.clone())
                .collect()
        };
        tokio::task::spawn_blocking(move || {
            session::started(&home, &cwd, since, |id| running.iter().any(|r| r == id))
        })
        .await
        .ok()
        .flatten()
    }

    /// Makes `found` the terminal's session: the earlier one with that id
    /// when there is one (a resume is the same session), else a new one.
    async fn take_codex_session(&self, terminal: Uuid, pty: Arc<Pty>, found: Binding) -> Uuid {
        // Started or resumed here, so tracked again even if it was removed.
        self.unhide(Engine::Codex, &found.id);
        self.recall(Engine::Codex, &found.id).await;
        let id = self.register_terminal_session(
            Engine::Codex,
            terminal,
            pty,
            Some(found.id),
            Some(found.cwd),
        );
        let rollout = found.rollout.display().to_string();
        self.change(id, |r, _| r.transcript = Some(rollout));
        id
    }

    /// Brings a Codex session in a terminal up to its rollout: working
    /// while a turn runs, done once it has ended, with its last prompt and
    /// reply.
    async fn follow_codex_session(&self, id: Uuid) {
        let Some(rollout) = self
            .record(id)
            .ok()
            .and_then(|r| r.transcript)
            .map(PathBuf::from)
        else {
            return;
        };
        let read = rollout.clone();
        let Ok((activity, tail)) = tokio::task::spawn_blocking(move || {
            (
                session::activity(&read),
                transcript::read_codex_tail(&read).unwrap_or_default(),
            )
        })
        .await
        else {
            return;
        };
        let state = match activity {
            Activity::Idle => SessionState::Idle,
            Activity::Working => SessionState::Working,
            Activity::Done => SessionState::Done,
        };
        let unchanged = self.record(id).is_ok_and(|r| {
            r.state == state && r.last_prompt == tail.last_prompt && r.last_reply == tail.last_reply
        });
        if !unchanged {
            self.change(id, |r, _| {
                r.state = state;
                r.last_prompt = tail.last_prompt;
                r.last_reply = tail.last_reply;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, ppid: u32, args: &str) -> Proc {
        Proc {
            pid,
            ppid,
            args: args.into(),
        }
    }

    #[test]
    fn the_tui_in_a_terminal_is_found_and_the_servers_and_workers_are_not() {
        let table = [
            proc(10, 1, "-zsh"),
            proc(11, 10, "node /n/bin/codex resume abc"),
            proc(12, 11, "/n/vendor/bin/codex resume abc --approve-for-me"),
            proc(13, 12, "/n/vendor/bin/codex-code-mode-host"),
            proc(20, 1, "-zsh"),
            proc(21, 20, "/n/vendor/bin/codex app-server --listen unix:///s"),
            proc(30, 1, "-zsh"),
            proc(31, 30, "/n/vendor/bin/codex resume x --remote unix:///s"),
        ];
        assert_eq!(codex_in(&table, 10).map(|p| p.pid), Some(12));
        assert_eq!(codex_in(&table, 20), None);
        assert_eq!(codex_in(&table, 30), None, "Desk's own pairs are not typed");
    }

    use std::os::unix::fs::PermissionsExt;

    /// A session as the test reads it: id, engine, Codex's id, state, terminal.
    type Row = (Uuid, Engine, Option<String>, SessionState, Option<Uuid>);
    use tokio_util::sync::CancellationToken;
    use workforce_postgres::test_support::with_test_pool;

    /// A `codex` that is a process of that name running `resume <id>`.
    fn fake_codex(dir: &std::path::Path) -> PathBuf {
        let bin = dir.join("codex");
        std::fs::write(&bin, "#!/bin/bash\nexec -a \"codex $*\" sleep 60\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    fn rollout(home: &std::path::Path, id: &str, cwd: &str, events: &[&str]) {
        use std::io::Write;
        let day = home.join(".codex/sessions/2026/11/02");
        std::fs::create_dir_all(&day).unwrap();
        let mut f =
            std::fs::File::create(day.join(format!("rollout-2026-11-02T10-00-00-{id}.jsonl")))
                .unwrap();
        let meta = serde_json::json!({"type": "session_meta", "payload": {"id": id, "cwd": cwd}});
        writeln!(f, "{meta}").unwrap();
        for kind in events {
            let e = serde_json::json!({"type": "event_msg", "payload": {"type": kind}});
            writeln!(f, "{e}").unwrap();
        }
        let mut idx = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(home.join(".codex/session_index.jsonl"))
            .unwrap();
        writeln!(
            idx,
            "{}",
            serde_json::json!({"id": id, "thread_name": "t", "updated_at": "2026-11-02T10:00:00Z"})
        )
        .unwrap();
    }

    #[tokio::test]
    async fn a_codex_typed_in_a_terminal_is_a_session_and_a_resume_is_the_same_one() {
        with_test_pool(crate::store::build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            std::fs::create_dir_all(&home).unwrap();
            let cwd = dir.path().to_string_lossy().into_owned();
            rollout(&home, "th-1", &cwd, &["task_started"]);
            let bin = fake_codex(dir.path());
            let mut opts = crate::testing::options(dir.path());
            opts.env = std::collections::BTreeMap::from([
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("SHELL".into(), "/bin/sh".into()),
                ("HOME".into(), home.to_string_lossy().into_owned()),
            ]);
            crate::store::DeskStore::migrate(&pool).await.unwrap();
            let d = Desk::start(
                crate::store::DeskStore::from_pool(pool),
                opts,
                Default::default(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let t = d
                .create_terminal(super::super::terminals::NewTerminal {
                    id: None,
                    name: None,
                    cwd: cwd.clone(),
                    env: Default::default(),
                })
                .await
                .unwrap();
            let pty = d.attach_terminal(t.id).unwrap();
            let sessions = || -> Vec<Row> {
                d.0.state
                    .lock()
                    .records
                    .values()
                    .map(|r| {
                        (
                            r.id,
                            r.engine,
                            r.cli_session_id.clone(),
                            r.state,
                            r.terminal_id,
                        )
                    })
                    .collect()
            };
            let track_until = |want: fn(&[Row]) -> bool| {
                let d = d.clone();
                async move {
                    for _ in 0..200 {
                        d.track_codex_terminals().await;
                        let all: Vec<Row> =
                            d.0.state
                                .lock()
                                .records
                                .values()
                                .map(|r| {
                                    (
                                        r.id,
                                        r.engine,
                                        r.cli_session_id.clone(),
                                        r.state,
                                        r.terminal_id,
                                    )
                                })
                                .collect();
                        if want(&all) {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    panic!("never happened");
                }
            };

            pty.write(format!("{} resume th-1\n", bin.display()).as_bytes())
                .unwrap();
            track_until(|s| s.len() == 1 && s[0].3 == SessionState::Working).await;
            let first = sessions()[0].clone();
            assert_eq!(
                (first.1, first.2.as_deref(), first.4),
                (Engine::Codex, Some("th-1"), Some(t.id))
            );
            assert!(
                d.view(first.0).unwrap().live,
                "on the Floor and in the sidebar as running"
            );

            // It finishes its turn, then the person quits it.
            rollout(&home, "th-1", &cwd, &["task_started", "task_complete"]);
            track_until(|s| s[0].3 == SessionState::Done).await;
            pty.write(b"\x03").unwrap();
            track_until(|s| s[0].3 == SessionState::Stopped).await;
            assert!(!d.view(first.0).unwrap().live);

            // Typed again: the same session, not a second.
            pty.write(format!("{} resume th-1\n", bin.display()).as_bytes())
                .unwrap();
            track_until(|s| s[0].3 != SessionState::Stopped).await;
            let all = sessions();
            assert_eq!(all.len(), 1, "no duplicate");
            assert_eq!(all[0].0, first.0);
            pty.write(b"\x03exit\n").unwrap();
        })
        .await;
    }
}
