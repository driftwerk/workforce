//! A plan's ticket session's folder made ready as it starts: its
//! dependencies installed once, in the background, and the session told.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_postgres::test_support::with_test_pool;

use super::super::tests::options;
use super::*;
use crate::model::{Engine, NewSession, SessionView};
use crate::store::{build_pool, DeskStore};

const COMMAND: &str = "npx --yes --package=pnpm@9.15.9 -- pnpm install --frozen-lockfile";
const LOCK: &str = "lockfileVersion: '9.0'\n";

/// The home of a computer whose `npx` is a fake: each run is a line of
/// `runs`, it waits for `go` beside it, and ends with the exit in `exit`.
struct Home {
    _dir: tempfile::TempDir,
    home: PathBuf,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let bin = home.join("tools");
        std::fs::create_dir_all(&bin).unwrap();
        let npx = format!(
            "#!/bin/sh\necho \"$@\" >> {h}/runs\n\
             i=0; while [ ! -e {h}/go ] && [ $i -lt 500 ]; do sleep 0.02; i=$((i+1)); done\n\
             /bin/mkdir -p node_modules && : > node_modules/.modules.yaml\n\
             code=$(cat {h}/exit 2>/dev/null || echo 0)\n\
             [ \"$code\" = 0 ] || echo ERR_PNPM_FETCH_401 Unauthorized >&2\nexit $code\n",
            h = home.display()
        );
        workforce_common::executable::write(&bin.join("npx"), &npx).unwrap();
        Self { _dir: dir, home }
    }

    fn project(&self, name: &str) -> PathBuf {
        let at = self.home.join(name);
        std::fs::create_dir_all(&at).unwrap();
        std::fs::write(
            at.join("package.json"),
            r#"{"name":"mantis","packageManager":"pnpm@9.15.9"}"#,
        )
        .unwrap();
        std::fs::write(at.join("pnpm-lock.yaml"), LOCK).unwrap();
        at
    }

    fn runs(&self) -> usize {
        std::fs::read_to_string(self.home.join("runs"))
            .map(|r| r.lines().count())
            .unwrap_or(0)
    }

    fn go(&self) {
        std::fs::write(self.home.join("go"), "").unwrap();
    }

    async fn desk(&self, pool: workforce_postgres::PostgresPool) -> Desk {
        DeskStore::migrate(&pool).await.unwrap();
        let mut o = options(&self.home);
        o.env.insert(
            "PATH".into(),
            format!("{}:/usr/bin:/bin", self.home.join("tools").display()),
        );
        o.env
            .insert("HOME".into(), self.home.to_string_lossy().into_owned());
        Desk::start(
            DeskStore::from_pool(pool),
            o,
            Default::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap()
    }
}

fn start(name: &str, cwd: &Path) -> NewSession {
    NewSession {
        fresh_worktree: false,
        engine: Engine::Claude,
        view: None,
        name: name.into(),
        cwd: cwd.to_string_lossy().into_owned(),
        goal: String::new(),
        worktree: None,
        prompt: Some(format!("Work on {name}.")),
        approvals: None,
        approve_for_me: false,
        fast: false,
        model: None,
        account: None,
        env: Default::default(),
        group: None,
    }
}

fn deps_of(d: &Desk, id: Uuid) -> Option<Deps> {
    d.view(id).ok().and_then(|v| v.deps)
}

async fn until(mut ok: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("not in time");
}

/// Waits until `id` is told on its line what `says`.
async fn told(d: &Desk, id: Uuid, says: impl Fn(&str) -> bool) {
    for _ in 0..500 {
        let said: Vec<String> = d
            .line_thread(id, None, None)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.text)
            .collect();
        if said.iter().any(|t| says(t)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never told");
}

fn installed(t: &str) -> bool {
    t.starts_with("Desk installed this folder's dependencies")
}

fn installing(v: &SessionView) -> bool {
    matches!(&v.deps, Some(Deps::Installing { command, .. }) if command == COMMAND)
}

#[tokio::test]
async fn a_tickets_folder_is_installed_once_and_every_session_in_it_is_told() {
    with_test_pool(build_pool, |pool| async move {
        let h = Home::new();
        let d = h.desk(pool).await;
        let work = h.project("mantis");
        let a = d.create_chosen(start("MAN-1", &work), true).await.unwrap();
        assert!(installing(&a), "{:?}", a.deps);
        let prompt = a.last_prompt.clone().unwrap();
        assert!(prompt.starts_with("Work on MAN-1.\n\nDesk is installing"), "{prompt}");
        assert!(prompt.contains(&format!("`{COMMAND}`")));
        // A second start into the folder while it installs joins it.
        let b = d.create_chosen(start("MAN-2", &work), true).await.unwrap();
        assert!(installing(&b), "{:?}", b.deps);
        until(|| h.runs() == 1).await;
        h.go();
        until(|| {
            [a.id, b.id]
                .iter()
                .all(|id| matches!(deps_of(&d, *id), Some(Deps::Installed { .. })))
        })
        .await;
        assert_eq!(h.runs(), 1, "installed once");
        for id in [a.id, b.id] {
            told(&d, id, installed).await;
        }
        // Current now: a third start is told so, and nothing runs.
        let c = d.create_chosen(start("MAN-3", &work), true).await.unwrap();
        assert_eq!(
            c.deps,
            Some(Deps::Installed {
                command: COMMAND.into()
            })
        );
        assert!(c
            .last_prompt
            .unwrap()
            .ends_with(&format!("This folder's dependencies are installed (`{COMMAND}`): run its tests and type checks as you go.")));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(h.runs(), 1);
        // A start that doesn't ask installs nothing and says nothing.
        let other = h.project("other");
        let plain = d.create_chosen(start("MAN-4", &other), false).await.unwrap();
        assert_eq!(plain.deps, None);
        assert_eq!(plain.last_prompt.as_deref(), Some("Work on MAN-4."));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(h.runs(), 1);
    })
    .await;
}

#[tokio::test]
async fn a_failed_install_says_why_and_how_to_try_again_and_the_session_runs_on() {
    with_test_pool(build_pool, |pool| async move {
        let h = Home::new();
        std::fs::write(h.home.join("exit"), "1").unwrap();
        h.go();
        let d = h.desk(pool).await;
        let work = h.project("mantis");
        let a = d.create_chosen(start("MAN-1", &work), true).await.unwrap();
        let why = "the package registry refused this machine's sign-in".to_string();
        until(|| deps_of(&d, a.id).is_some_and(|s| !matches!(s, Deps::Installing { .. }))).await;
        assert_eq!(
            deps_of(&d, a.id),
            Some(Deps::Failed {
                command: COMMAND.into(),
                why: why.clone()
            })
        );
        told(&d, a.id, |t| {
            t.contains(&why) && t.contains(&format!("Run `{COMMAND}`"))
        })
        .await;
        assert_eq!(
            std::fs::read_to_string(work.join("node_modules/.desk-deps")).unwrap(),
            "failed",
            "pnpm's own state isn't trusted after it"
        );
        assert!(d.view(a.id).unwrap().live, "it runs on");
        // Started again in the folder, it is installed again.
        std::fs::write(h.home.join("exit"), "0").unwrap();
        let b = d.create_chosen(start("MAN-2", &work), true).await.unwrap();
        assert!(installing(&b), "{:?}", b.deps);
        until(|| matches!(deps_of(&d, b.id), Some(Deps::Installed { .. }))).await;
        assert_eq!(h.runs(), 2);
    })
    .await;
}

#[tokio::test]
async fn a_folder_desk_wont_install_says_why_in_its_brief() {
    with_test_pool(build_pool, |pool| async move {
        let h = Home::new();
        let d = h.desk(pool).await;
        let work = h.project("mantis");
        std::fs::write(work.join("package-lock.json"), "{}").unwrap();
        std::fs::write(work.join("package.json"), r#"{"name":"mantis"}"#).unwrap();
        let a = d.create_chosen(start("MAN-1", &work), true).await.unwrap();
        let why = "it has lockfiles of several package managers and package.json names none";
        assert_eq!(a.deps, Some(Deps::Left { why: why.into() }));
        assert!(a.last_prompt.unwrap().contains(&format!(
            "Desk didn't install this folder's dependencies: {why}."
        )));
        // Not a JS folder: nothing is said.
        let plain = h.home.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let b = d.create_chosen(start("MAN-2", &plain), true).await.unwrap();
        assert_eq!(b.deps, None);
        assert_eq!(b.last_prompt.as_deref(), Some("Work on MAN-2."));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(h.runs(), 0);
    })
    .await;
}

#[tokio::test]
async fn a_session_forgotten_while_its_folder_installs_is_never_told() {
    with_test_pool(build_pool, |pool| async move {
        let h = Home::new();
        let d = h.desk(pool).await;
        let work = h.project("mantis");
        let a = d.create_chosen(start("MAN-1", &work), true).await.unwrap();
        // Another path to the same folder joins the same install.
        let link = h.home.join("link");
        std::os::unix::fs::symlink(&work, &link).unwrap();
        let b = d.create_chosen(start("MAN-2", &link), true).await.unwrap();
        until(|| h.runs() == 1).await;
        d.forget(a.id).await.unwrap();
        h.go();
        until(|| matches!(deps_of(&d, b.id), Some(Deps::Installed { .. }))).await;
        assert_eq!(h.runs(), 1, "one install for both paths");
        assert_eq!(deps_of(&d, a.id), None);
        assert!(d.view(a.id).is_err());
        told(&d, b.id, installed).await;
    })
    .await;
}

#[test]
fn what_a_session_was_told_of_its_folder_leaves_its_card_once_a_later_turn_ends() {
    let command = "npm ci".to_string();
    let mut st = State::default();
    let id = Uuid::new_v4();
    let (said, prompt) = ("done", "go on");
    st.deps.insert(
        id,
        Kept::new(Deps::Failed {
            command,
            why: "x".into(),
        }),
    );
    // The turn that ran as it was said ends: it stays.
    assert!(!Desk::deps_read(
        &mut st,
        id,
        &[Transition::Finished { reply: said }],
        true
    ));
    assert!(st.deps.contains_key(&id));
    // A turn after it begins, then ends: it goes.
    assert!(!Desk::deps_read(
        &mut st,
        id,
        &[Transition::Prompted { prompt }],
        false
    ));
    assert!(Desk::deps_read(
        &mut st,
        id,
        &[Transition::Finished { reply: said }],
        true
    ));
    assert!(!st.deps.contains_key(&id));
    // Failed mid-turn: that turn's end leaves it, and so does a prompt that
    // reached it otherwise (typed while it was busy); a turn prompted and
    // ended in one change clears it.
    let failed = |st: &mut State| {
        st.deps.insert(
            id,
            Kept::new(Deps::Failed {
                command: "npm ci".into(),
                why: "x".into(),
            }),
        )
    };
    failed(&mut st);
    for t in [
        Transition::Finished { reply: said },
        Transition::Spoke { prompt },
    ] {
        assert!(!Desk::deps_read(&mut st, id, &[t], true));
    }
    assert!(st.deps.contains_key(&id));
    assert!(Desk::deps_read(
        &mut st,
        id,
        &[
            Transition::Prompted { prompt },
            Transition::Finished { reply: said }
        ],
        true
    ));
    assert!(!st.deps.contains_key(&id));
    // One left at a start is read once its first turn ends.
    st.deps
        .insert(id, Kept::new(Deps::Left { why: "x".into() }));
    assert!(!Desk::deps_read(&mut st, id, &[Transition::Started], false));
    assert!(Desk::deps_read(
        &mut st,
        id,
        &[Transition::Finished { reply: said }],
        true
    ));
    // One installing stays whatever its turns.
    st.deps.insert(
        id,
        Kept::new(Deps::Installing {
            command: COMMAND.into(),
        }),
    );
    for _ in 0..2 {
        assert!(!Desk::deps_read(
            &mut st,
            id,
            &[Transition::Prompted { prompt }],
            true
        ));
    }
    assert!(st.deps.contains_key(&id));
}

#[tokio::test]
async fn a_desk_started_again_looks_again_at_the_folders_it_was_installing() {
    with_test_pool(build_pool, |pool| async move {
        let h = Home::new();
        let d = h.desk(pool.clone()).await;
        let work = h.project("mantis");
        let a = d.create_chosen(start("MAN-1", &work), true).await.unwrap();
        until(|| h.runs() == 1).await;
        let id = a.id.to_string();
        let kept = |d: Desk| {
            let id = id.clone();
            async move {
                d.0.store
                    .state(INSTALLING)
                    .await
                    .unwrap()
                    .is_some_and(|e| e.value.to_string().contains(&id))
            }
        };
        for _ in 0..500 {
            if kept(d.clone()).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(kept(d.clone()).await, "the folder installing is kept");
        let again = h.desk(pool).await;
        until(|| h.runs() == 2).await;
        assert!(
            matches!(deps_of(&again, a.id), Some(Deps::Installing { .. })),
            "{:?}",
            deps_of(&again, a.id)
        );
        h.go();
        until(|| matches!(deps_of(&again, a.id), Some(Deps::Installed { .. }))).await;
        told(&again, a.id, installed).await;
    })
    .await;
}

#[tokio::test]
async fn an_install_left_running_is_ended_though_its_folder_is_current_by_the_next_start() {
    with_test_pool(build_pool, |pool| async move {
        let h = Home::new();
        let d = h.desk(pool.clone()).await;
        let work = h.project("mantis");
        let a = d.create_chosen(start("MAN-1", &work), true).await.unwrap();
        until(|| h.runs() == 1).await;
        let running = crate::deps::running_in(&h.home.join("run"), &work);
        let mut pid = None;
        for _ in 0..500 {
            pid = std::fs::read_to_string(&running)
                .ok()
                .and_then(|k| k.split_whitespace().next()?.parse::<u32>().ok());
            if pid.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let pid = pid.expect("the install's process is kept");
        // Installed by hand meanwhile: pnpm's own state, newer than the lockfile.
        std::fs::create_dir_all(work.join("node_modules")).unwrap();
        std::fs::write(work.join("node_modules/.modules.yaml"), "").unwrap();
        let again = h.desk(pool).await;
        until(|| matches!(deps_of(&again, a.id), Some(Deps::Installed { .. }))).await;
        told(&again, a.id, |t| {
            t.starts_with("This folder's dependencies are installed")
        })
        .await;
        let mut ended = false;
        for _ in 0..500 {
            if crate::procs::started_at(pid).await.is_none() {
                ended = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ended, "the install left running was ended");
        assert_eq!(h.runs(), 1, "nothing installed again");
        drop(d);
    })
    .await;
}
