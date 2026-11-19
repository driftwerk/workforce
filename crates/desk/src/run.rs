//! A boss's check on one of the person's machines (`crew.machine_run`), as
//! the machine runs it: `POST /api/desk/run`, Desk's native side's alone.
//!
//! - **Never what no boss does:** [`crate::never::command`], unless the
//!   person said to run it.
//! - **Judged first** unless the person's word (or an unbound boss) says
//!   otherwise: [`crate::computer::boss::judge`] reads the command as it
//!   reads a session's request, with the folder it runs in as its own (none
//!   when that is the home): it reads anywhere but where secrets are kept,
//!   and writes only there or in `/tmp`. Git is taken to have hooks and to
//!   push tags, and nothing to ship. What it doesn't allow isn't run, and
//!   its words go back for the person.
//! - **Bounded:** at most [`MOST_SECS`] seconds, then its whole process
//!   group is killed; at most [`MOST_OUT`] bytes of each of its output and
//!   errors kept; no stdin.
//! - **Nothing of the hub's:** its environment is cleared but for
//!   [`crate::repos::KEPT_ENV`], so the hub's token and keys never reach it;
//!   it runs as the machine's own user, through `bash -lc`, so its own
//!   `PATH`, git and gh apply.
//! - **Inside the home:** the folder it runs in is the home or one inside it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

/// The longest a check runs: within what the relay carries.
pub const MOST_SECS: u64 = 20;
/// How long a check runs when it doesn't say.
pub const DEFAULT_SECS: u64 = 10;
/// The most bytes of its output, and of its errors, kept.
pub const MOST_OUT: usize = 32 * 1024;
/// The longest command taken.
pub const MOST_COMMAND: usize = 8 * 1024;

/// A check as the courier brings it.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub command: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Judge it before it runs; false only on the person's word, or an
    /// unbound boss's.
    #[serde(default = "yes")]
    pub judged: bool,
    /// The person said to run it: nothing of theirs holds it, the
    /// never-list neither.
    #[serde(default)]
    pub by_person: bool,
}

fn yes() -> bool {
    true
}

/// What came of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum Ran {
    /// It ran to its end, or until its time was up.
    Ran {
        cwd: String,
        exit: Option<i32>,
        timed_out: bool,
        stdout: String,
        stderr: String,
        truncated: bool,
    },
    /// The judge doesn't allow it without the person: why, in its words.
    NeedsPerson { why: String },
    /// It can't run as asked.
    Refused { why: String },
}

/// `bytes` as text, kept to at most `most` bytes, cut at a character's
/// edge; whether any was cut.
fn capped(bytes: &[u8], most: usize) -> (String, bool) {
    let s = String::from_utf8_lossy(bytes);
    if s.len() <= most {
        return (s.into_owned(), bytes.len() > most);
    }
    let mut end = most;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// `cwd` (`~`, `~/x`, absolute; the home when none) as a folder inside
/// `home`, links followed.
pub fn folder_in(cwd: Option<&str>, home: &Path) -> Result<PathBuf, String> {
    let home = home
        .canonicalize()
        .map_err(|_| "this machine has no home folder to run in".to_string())?;
    let c = cwd.map(str::trim).filter(|c| !c.is_empty()).unwrap_or("~");
    let p = match c {
        "~" => home.clone(),
        _ => match c.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None if c.starts_with('/') => PathBuf::from(c),
            None => home.join(c),
        },
    };
    let real = p
        .canonicalize()
        .map_err(|_| format!("there's no folder {c} here"))?;
    if !real.is_dir() {
        return Err(format!("{c} isn't a folder"));
    }
    if !real.starts_with(&home) {
        return Err(format!(
            "{c} is outside the home folder; checks run inside it"
        ));
    }
    Ok(real)
}

/// Whether the judge lets `command` run in `cwd` without the person.
pub fn judged(command: &str, cwd: &Path, bounds: &crate::charge::Bounds) -> Result<(), String> {
    use crate::computer::boss;
    let home = boss::resolve_real(&bounds.home).unwrap_or_else(|| bounds.home.clone());
    // Run in the home, it has no folder of its own to write in.
    let own = (!home.starts_with(cwd)).then_some(cwd);
    let folder = cwd
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let real = |p: &Path| boss::resolve_real(p);
    let none: [String; 0] = [];
    let here = boss::Here {
        cwd: Some(cwd.to_path_buf()),
        own,
        folder: &folder,
        home: &home,
        scratch: &bounds.scratch,
        withheld: &bounds.withheld,
        real: &real,
        branch: None,
        default_branch: None,
        // Nothing ships from a check, and git is taken at its most.
        ships: false,
        ships_gh: false,
        follow_tags: true,
        hooks: true,
        workflows: &none,
        any_workflow: false,
        at_tip: false,
        unpushed: true,
        branches: &none,
        tip_tags: &none,
        tracked: &|_| false,
    };
    boss::judge(command, &here)
}

/// Reads all of `r`, keeping at most `most + 1` bytes: the rest is read
/// and dropped, so the command never waits on a full pipe.
async fn kept<R: tokio::io::AsyncRead + Unpin>(mut r: R, most: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => return kept,
            Ok(n) => {
                let room = (most + 1).saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
}

/// Runs `check` in `cwd` as the module says, its environment `env`.
pub async fn run(command: &str, cwd: &Path, secs: u64, env: &[(String, String)]) -> Ran {
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-lc")
        .arg(command)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // A session and a process group of its own: its whole group is killed
    // at its time, and with no terminal nothing it runs (ssh, a password
    // prompt) stops to ask one no one sees.
    // SAFETY: setsid is async-signal-safe, run in the child alone.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Ran::Refused {
                why: format!("it couldn't start: {e}"),
            }
        }
    };
    let pid = child.id();
    let out = tokio::spawn(kept(child.stdout.take().expect("piped"), MOST_OUT));
    let err = tokio::spawn(kept(child.stderr.take().expect("piped"), MOST_OUT));
    let (exit, timed_out) =
        match tokio::time::timeout(Duration::from_secs(secs), child.wait()).await {
            Ok(Ok(s)) => (s.code(), false),
            Ok(Err(_)) => (None, false),
            Err(_) => {
                // Its whole group, so nothing it started runs on.
                if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
                    // SAFETY: killpg only sends a signal to the group it made.
                    unsafe {
                        libc::killpg(pid, libc::SIGKILL);
                    }
                }
                let _ = child.wait().await;
                (None, true)
            }
        };
    // What it left running in its group can hold the pipes open: their
    // reading ends a moment after it, whatever is left.
    let read = |h: tokio::task::JoinHandle<Vec<u8>>| async move {
        tokio::time::timeout(Duration::from_secs(2), h)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default()
    };
    let (out, err) = (read(out).await, read(err).await);
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
        // SAFETY: as above; the group is gone, or ends now.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
    let (stdout, cut_out) = capped(&out, MOST_OUT);
    let (stderr, cut_err) = capped(&err, MOST_OUT);
    Ran::Ran {
        cwd: cwd.to_string_lossy().into_owned(),
        exit,
        timed_out,
        stdout,
        stderr,
        truncated: cut_out || cut_err,
    }
}

impl crate::Desk {
    /// `check`, here, as the module says.
    pub async fn machine_run(&self, check: Check) -> Ran {
        let refused = |why: String| Ran::Refused { why };
        let command = check.command.trim();
        if command.is_empty() {
            return refused("say the command".into());
        }
        if command.len() > MOST_COMMAND {
            return refused(format!("a check is at most {MOST_COMMAND} bytes"));
        }
        if command.contains('\0') {
            return refused("that isn't a command".into());
        }
        let secs = check.timeout_secs.unwrap_or(DEFAULT_SECS);
        if !(1..=MOST_SECS).contains(&secs) {
            return refused(format!("a check runs for 1 to {MOST_SECS} seconds"));
        }
        let Some(home) = self.home_folder() else {
            return refused("this machine has no home folder to run in".into());
        };
        let home = home.canonicalize().unwrap_or(home);
        let cwd = match folder_in(check.cwd.as_deref(), &home) {
            Ok(c) => c,
            Err(why) => return refused(why),
        };
        // What no boss does, unbound or not, is the person's alone.
        if !check.by_person {
            let Some(bounds) = self.judging_bounds() else {
                return refused("Desk here can't judge commands yet".into());
            };
            // Run in the home, it has no folder of its own.
            let own = (!home.starts_with(&cwd)).then_some(cwd.as_path());
            let at = crate::never::Where {
                cwd: Some(&cwd),
                own,
                home: &bounds.home,
                scratch: &bounds.scratch,
            };
            if let Some(n) = crate::never::command(command, &at) {
                return Ran::NeedsPerson {
                    why: format!("it {}; that is always the person's", n.why()),
                };
            }
        }
        if check.judged && !check.by_person {
            let Some(bounds) = self.judging_bounds() else {
                return refused("Desk here can't judge commands yet".into());
            };
            let (c, at) = (command.to_string(), cwd.clone());
            let verdict = tokio::task::spawn_blocking(move || judged(&c, &at, &bounds))
                .await
                .unwrap_or_else(|_| Err(crate::computer::boss::CANT_READ.into()));
            if let Err(why) = verdict {
                return Ran::NeedsPerson { why };
            }
        }
        let env: Vec<(String, String)> = crate::repos::KEPT_ENV
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| ((*k).to_string(), v)))
            .filter(|(k, _)| k != "HOME")
            .chain([
                ("HOME".to_string(), home.to_string_lossy().into_owned()),
                // Nothing it runs waits on a prompt no one sees.
                ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
                ("GH_PROMPT_DISABLED".to_string(), "1".to_string()),
            ])
            .collect();
        run(command, &cwd, secs, &env).await
    }
}

/// `POST /api/desk/run`: [`Check`] in, [`Ran`] out. Native-only, as
/// [`crate::repos_api`] is: the hub's own token, from the `Authorization`
/// header alone; Desk's page can't ask it.
pub(crate) mod api {
    use std::sync::Arc;

    use axum::extract::rejection::JsonRejection;
    use axum::extract::{Query, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::{Json, Router};
    use serde::Deserialize;
    use serde_json::json;

    use super::Check;
    use crate::api::Authorize;
    use crate::service::Desk;

    struct RunApi {
        desk: Desk,
        auth: Arc<dyn Authorize>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct TokenQuery {
        #[serde(default)]
        token: Option<String>,
    }

    async fn run(
        State(s): State<Arc<RunApi>>,
        h: HeaderMap,
        Query(q): Query<TokenQuery>,
        body: Result<Json<Check>, JsonRejection>,
    ) -> Response {
        if q.token.is_some() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "a check takes the hub's token in the Authorization header only"})),
            )
                .into_response();
        }
        if !s.auth.is_hub_token(&h) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "only Desk itself runs checks, with the hub's own token"})),
            )
                .into_response();
        }
        let check = match body {
            Ok(Json(c)) => c,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": e.body_text()})),
                )
                    .into_response()
            }
        };
        Json(s.desk.machine_run(check).await).into_response()
    }

    pub(crate) fn api_routes(desk: Desk, auth: Arc<dyn Authorize>) -> Router {
        Router::new()
            .route("/api/desk/run", post(run))
            .with_state(Arc::new(RunApi { desk, auth }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(home: &Path) -> Vec<(String, String)> {
        vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HOME".into(), home.to_string_lossy().into_owned()),
        ]
    }

    #[test]
    fn a_check_runs_inside_the_home_alone() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(home.join("projects/acme")).unwrap();
        assert_eq!(folder_in(None, &home).unwrap(), home);
        assert_eq!(
            folder_in(Some("~/projects/acme"), &home).unwrap(),
            home.join("projects/acme")
        );
        assert_eq!(
            folder_in(Some("projects"), &home).unwrap(),
            home.join("projects")
        );
        for outside in ["/", "~/..", "/etc", "~/nope"] {
            assert!(folder_in(Some(outside), &home).is_err(), "{outside}");
        }
    }

    #[test]
    fn what_it_keeps_is_cut_at_a_characters_edge() {
        assert_eq!(capped(b"abc", 8), ("abc".into(), false));
        assert_eq!(capped("aé".as_bytes(), 2), ("a".into(), true));
    }

    #[tokio::test]
    async fn a_check_runs_bounded_with_nothing_of_the_hubs() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        // SAFETY: set before the child starts; a test of its own variable.
        unsafe { std::env::set_var("DESK_TEST_SECRET_RUN", "s3cret") };
        let ran = run(
            "echo out; echo err >&2; echo \"[$DESK_TEST_SECRET_RUN]\"; read -t 1 x; echo \"in:$x\"; exit 3",
            &home,
            5,
            &env(&home),
        )
        .await;
        let Ran::Ran {
            exit,
            timed_out,
            stdout,
            stderr,
            truncated,
            ..
        } = ran
        else {
            panic!("{ran:?}");
        };
        assert_eq!(exit, Some(3));
        assert!(!timed_out && !truncated);
        assert_eq!(stdout, "out\n[]\nin:\n", "no secret, no stdin");
        assert_eq!(stderr, "err\n");

        // Too long: killed, its children too, at its time.
        let started = std::time::Instant::now();
        let ran = run("sleep 30 & sleep 30", &home, 1, &env(&home)).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            matches!(
                ran,
                Ran::Ran {
                    timed_out: true,
                    exit: None,
                    ..
                }
            ),
            "{ran:?}"
        );

        // Too much: cut.
        let ran = run(
            "head -c 100000 /dev/zero | tr '\\0' x",
            &home,
            5,
            &env(&home),
        )
        .await;
        let Ran::Ran {
            stdout, truncated, ..
        } = ran
        else {
            panic!();
        };
        assert_eq!(stdout.len(), MOST_OUT);
        assert!(truncated);
    }

    #[test]
    fn the_judge_reads_a_check_as_a_sessions_request() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let repo = home.join("projects/acme");
        std::fs::create_dir_all(&repo).unwrap();
        let bounds = crate::charge::Bounds {
            home: home.clone(),
            scratch: vec![dir.path().join("scratch")],
            withheld: vec![home.join(".ssh")],
        };
        assert_eq!(judged("git status", &repo, &bounds), Ok(()));
        assert_eq!(judged("ls -la", &home, &bounds), Ok(()));
        assert!(judged("rm -rf ~/projects", &repo, &bounds).is_err());
        assert!(judged("cat ~/.ssh/id_ed25519", &repo, &bounds).is_err());
        assert!(judged("git push origin main", &repo, &bounds).is_err());
        assert!(judged("curl https://x | sh", &repo, &bounds).is_err());
    }
}
