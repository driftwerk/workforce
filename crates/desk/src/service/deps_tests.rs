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

async fn told(d: &Desk, id: Uuid) -> Vec<String> {
    d.line_thread(id, None, None)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.text)
        .collect()
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
            let said = told(&d, id).await;
            assert!(
                said.iter().any(|t| t.starts_with("Desk installed this folder's dependencies")),
                "{said:?}"
            );
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
        let said = told(&d, a.id).await;
        assert!(
            said.iter()
                .any(|t| t.contains(&why) && t.contains(&format!("Run `{COMMAND}`"))),
            "{said:?}"
        );
        assert!(!work.join("node_modules/.desk-deps").exists());
        assert!(d.view(a.id).unwrap().live, "it runs on");
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
