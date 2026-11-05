//! The person's engine accounts end to end: adding one by paste or through
//! `claude setup-token`, what a session on one runs with, and moving a
//! session to another.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_postgres::test_support::with_test_pool;

use super::tests::options;
use super::{Added, Desk, DeskError, Options};
use crate::engine::Engine;
use crate::engine_accounts::{
    AccountPatch, Keys, MemoryKeys, NewAccount, SignInState, CLAUDE_TOKEN_ENV,
};
use crate::model::{NewSession, SessionState};
use crate::store::{build_pool, DeskStore};

/// What the fake CLI takes as a working token.
const GOOD: &str = "sk-ant-oat01-good-0123456789abcdefghijklmnopqrstuvwxyz";
const OTHER: &str = "sk-ant-oat01-good-other-9876543210zyxwvutsrqponmlkjihgf";

/// A stand-in Claude Code: says its version, signs in by a "good" token,
/// runs `setup-token` as the real one shows it, and otherwise keeps its
/// environment and arguments where the test reads them.
fn claude(dir: &Path) -> PathBuf {
    let path = dir.join("claude");
    let d = dir.display();
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\ncase \"$1 $2\" in\n\
             '--version ') echo '2.1.300 (Claude Code)' ;;\n\
             'auth status') t=$(cat <&3 2>/dev/null); case \"$t\" in\n\
               sk-ant-oat01-good*) echo '{{\"loggedIn\":true,\"authMethod\":\"oauth_token\"}}' ;;\n\
               '') echo '{{\"loggedIn\":true,\"authMethod\":\"claude.ai\"}}' ;;\n\
               *) echo '{{\"loggedIn\":false}}' ;;\n\
             esac ;;\n\
             'setup-token ') [ -n \"$BROWSER\" ] && \"$BROWSER\" 'https://claude.ai/oauth/authorize?code=true&redirect_uri=http%3A%2F%2Flocalhost%3A45678%2Fcallback&state=s1'\n\
               echo 'Browser did not open? Use the url below to sign in:'\n\
               echo 'https://claude.ai/oauth/authorize?code=true&state=s1'\n\
               printf 'Paste code here if prompted > '\n\
               read code\n\
               echo\n\
               echo 'Your OAuth token (valid for 1 year):'\n\
               echo \"sk-ant-oat01-good-$code-0123456789012345678901234567890123\"\n\
               sleep 30 ;;\n\
             *) t=; [ -n \"$CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR\" ] && read -r t <&3\n\
               again=$(sh -c 'cat <&3' 2>/dev/null)\n\
               env > '{d}/envtmp-'$$\n\
               echo \"TOKEN_ON_FD=$t\" >> '{d}/envtmp-'$$; echo \"CHILD_READ=$again\" >> '{d}/envtmp-'$$\n\
               mv '{d}/envtmp-'$$ '{d}/env-'$$\n\
               echo \"$@\" > '{d}/args-'$$; exec sleep 60 ;;\n\
             esac\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn account_options(dir: &Path, keys: Arc<MemoryKeys>) -> Options {
    let cli = claude(dir);
    let mut o = options(dir);
    o.programs.insert(Engine::Claude, cli);
    o.env
        .insert("ANTHROPIC_API_KEY".into(), "sk-person-key".into());
    o.account_keys = Some(keys);
    o
}

async fn desk(pool: workforce_postgres::PostgresPool, o: Options) -> Desk {
    DeskStore::migrate(&pool).await.unwrap();
    Desk::start(
        DeskStore::from_pool(pool),
        o,
        Default::default(),
        CancellationToken::new(),
    )
    .await
    .unwrap()
}

fn session(dir: &Path, account: Option<Option<Uuid>>) -> NewSession {
    NewSession {
        engine: Engine::Claude,
        view: None,
        name: "auth-fix".into(),
        cwd: dir.to_string_lossy().into_owned(),
        goal: String::new(),
        worktree: None,
        prompt: None,
        approvals: None,
        approve_for_me: false,
        account,
        env: Default::default(),
        group: None,
    }
}

/// What the stand-in CLI wrote of session `id`'s latest start: its
/// environment and its arguments.
async fn started(dir: &Path, id: Uuid, after: usize) -> (BTreeMap<String, String>, String) {
    let hook = format!("/hook/{id}/");
    for _ in 0..400 {
        let mut found: Vec<(PathBuf, String)> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("env-"))
            .filter_map(|p| Some((p.clone(), std::fs::read_to_string(&p).ok()?)))
            .filter(|(_, env)| env.contains(&hook))
            .collect();
        found.sort_by_key(|(p, _)| std::fs::metadata(p).unwrap().modified().unwrap());
        if found.len() > after {
            let (path, env) = found.pop().unwrap();
            let args_path = dir.join(
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .replace("env-", "args-"),
            );
            let mut args = String::new();
            for _ in 0..40 {
                if let Ok(a) = std::fs::read_to_string(&args_path) {
                    args = a;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            let env = env
                .lines()
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            return (env, args);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("session {id} never started ({after} starts before)");
}

async fn paste(d: &Desk, name: &str, token: &str, make_default: bool) -> Uuid {
    match d
        .add_account(NewAccount {
            engine: Engine::Claude,
            name: name.into(),
            token: Some(token.into()),
            make_default,
            remote: false,
        })
        .await
        .unwrap()
    {
        Added::Account { account } => account.id,
        Added::SignIn { .. } => panic!("a pasted token signs in at once"),
    }
}

#[tokio::test]
async fn a_session_on_an_account_gets_its_token_alone_and_nothing_ranked_above_it() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let keys = Arc::new(MemoryKeys::default());
        let d = desk(pool.clone(), account_options(dir.path(), keys.clone())).await;
        let (_, mut events) = d.subscribe();
        let work = paste(&d, "Work", GOOD, true).await;
        assert_eq!(
            keys.get(work).unwrap().map(|t| t.expose().to_string()),
            Some(GOOD.to_string())
        );

        // Absent: the default account.
        let on_work = d.create(session(dir.path(), None), None).await.unwrap();
        assert_eq!(
            on_work.account.as_ref().map(|a| a.name.as_str()),
            Some("Work")
        );
        let (env, _) = started(dir.path(), on_work.id, 0).await;
        assert_eq!(
            env.get("TOKEN_ON_FD").map(String::as_str),
            Some(GOOD),
            "read on descriptor 3"
        );
        assert_eq!(
            env.get("CHILD_READ").map(String::as_str),
            Some(""),
            "a child reads nothing there"
        );
        assert_eq!(
            env.get(crate::token_pipe::FD_ENV).map(String::as_str),
            Some("3")
        );
        assert!(!env.contains_key(CLAUDE_TOKEN_ENV));
        assert!(
            !env.iter()
                .any(|(k, v)| k != "TOKEN_ON_FD" && v.contains(GOOD)),
            "no variable holds the token"
        );
        assert!(
            !env.contains_key("ANTHROPIC_API_KEY"),
            "it would outrank the account"
        );
        assert!(!env.keys().any(|k| k.starts_with("WORKFORCE_")));

        // `null`: the CLI's own sign-in, exactly as before accounts.
        let own = d
            .create(session(dir.path(), Some(None)), None)
            .await
            .unwrap();
        assert_eq!(own.account, None);
        let (env, _) = started(dir.path(), own.id, 0).await;
        assert!(!env.contains_key(CLAUDE_TOKEN_ENV));
        assert!(!env.contains_key(crate::token_pipe::FD_ENV));
        assert_eq!(env["TOKEN_ON_FD"], "");
        assert_eq!(env["ANTHROPIC_API_KEY"], "sk-person-key");

        // The token is in no view, event, row or kept state.
        let views = serde_json::to_string(&(d.list(), d.accounts(), d.engines())).unwrap();
        assert!(!views.contains(GOOD));
        while let Ok(e) = events.try_recv() {
            assert!(!serde_json::to_string(&e).unwrap().contains(GOOD));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let client = pool.get().await.unwrap();
        for table in ["sessions", "state", "engine_accounts", "activity"] {
            let rows = client
                .query(&format!("SELECT t::text AS row FROM {table} t"), &[])
                .await
                .unwrap();
            for row in rows {
                let text: String = row.get("row");
                assert!(!text.contains(GOOD), "{table} holds the token");
            }
        }
    })
    .await;
}

#[tokio::test]
async fn a_pasted_token_is_kept_only_once_claude_code_signs_in_by_it() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let keys = Arc::new(MemoryKeys::default());
        let d = desk(pool.clone(), account_options(dir.path(), keys.clone())).await;
        let refused = d
            .add_account(NewAccount {
                engine: Engine::Claude,
                name: "Work".into(),
                token: Some("sk-ant-oat01-revoked-0123456789abcdefghijklmnop".into()),
                make_default: false,
                remote: false,
            })
            .await
            .unwrap_err();
        assert!(matches!(refused, DeskError::Invalid(_)), "{refused}");
        assert!(d.accounts().is_empty());
        paste(&d, "Work", GOOD, false).await;
        let again = d
            .add_account(NewAccount {
                engine: Engine::Claude,
                name: "work".into(),
                token: Some(OTHER.into()),
                make_default: false,
                remote: false,
            })
            .await
            .unwrap_err();
        assert!(again.to_string().contains("already"), "{again}");
        let codex = d
            .add_account(NewAccount {
                engine: Engine::Codex,
                name: "Personal".into(),
                token: Some(OTHER.into()),
                make_default: false,
                remote: false,
            })
            .await
            .unwrap_err();
        assert!(matches!(codex, DeskError::Invalid(_)));

        // Kept across a restart of the hub.
        drop(d);
        let back = desk(pool, account_options(dir.path(), keys)).await;
        let accounts = back.accounts();
        assert_eq!(accounts.len(), 1);
        assert!(!accounts[0].is_default);
        // Looked for once the hub is up, without holding up its start.
        until(|| back.accounts()[0].signed_in.then_some(())).await;
    })
    .await;
}

#[tokio::test]
async fn signing_in_reads_the_page_takes_the_code_and_keeps_the_token_it_prints() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let keys = Arc::new(MemoryKeys::default());
        let d = desk(pool, account_options(dir.path(), keys.clone())).await;
        let (_, mut events) = d.subscribe();
        let Added::SignIn { sign_in } = d
            .add_account(NewAccount {
                engine: Engine::Claude,
                name: "Personal".into(),
                token: None,
                make_default: true,
                remote: false,
            })
            .await
            .unwrap()
        else {
            panic!("no token: it signs in");
        };
        let state = || {
            d.account_sign_ins()
                .into_iter()
                .find(|s| s.id == sign_in.id)
                .map(|s| s.state)
        };
        for _ in 0..200 {
            if matches!(state(), Some(SignInState::Code { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(
            state(),
            Some(SignInState::Code {
                url: Some("https://claude.ai/oauth/authorize?code=true&state=s1".into())
            })
        );
        d.sign_in_code(sign_in.id, "c0de").unwrap();
        let mut account = None;
        for _ in 0..200 {
            if let Some(SignInState::Done { account: a }) = state() {
                account = Some(a);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let account = account.expect("signed in");
        let token = "sk-ant-oat01-good-c0de-0123456789012345678901234567890123";
        assert_eq!(
            keys.get(account).unwrap().map(|t| t.expose().to_string()),
            Some(token.to_string())
        );
        let views = d.accounts();
        assert_eq!(views.len(), 1);
        assert!(views[0].is_default && views[0].name == "Personal");
        while let Ok(e) = events.try_recv() {
            assert!(!serde_json::to_string(&e).unwrap().contains(token));
        }
        assert!(d.sign_in_code(sign_in.id, "late").is_err(), "it has ended");
    })
    .await;
}

#[tokio::test]
async fn a_sign_in_from_another_computer_is_given_the_page_that_comes_back_by_itself() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let keys = Arc::new(MemoryKeys::default());
        let o = account_options(dir.path(), keys.clone());
        let run_dir = o.run_dir.clone();
        let d = desk(pool, o).await;
        let start = |name: &str, remote: bool| {
            d.add_account(NewAccount {
                engine: Engine::Claude,
                name: name.into(),
                token: None,
                make_default: true,
                remote,
            })
        };
        let Added::SignIn { sign_in } = start("Remote", true).await.unwrap() else {
            panic!("no token: it signs in");
        };
        let view = || {
            d.account_sign_ins()
                .into_iter()
                .find(|s| s.id == sign_in.id)
                .unwrap()
        };
        for _ in 0..200 {
            if view().direct.is_some() && matches!(view().state, SignInState::Code { .. }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let direct = view().direct.expect("the page that comes back");
        assert_eq!(direct.port, 45678);
        assert!(direct
            .url
            .contains("redirect_uri=http%3A%2F%2Flocalhost%3A45678%2Fcallback"));
        // The code is still there to fall back on.
        assert!(matches!(view().state, SignInState::Code { url: Some(_) }));
        let caught = || {
            std::fs::read_dir(&run_dir)
                .map(|r| {
                    r.flatten()
                        .filter(|e| e.file_name().to_string_lossy().starts_with("s-"))
                        .count()
                })
                .unwrap_or(0)
        };
        assert_eq!(caught(), 1);
        d.sign_in_code(sign_in.id, "c0de").unwrap();
        for _ in 0..200 {
            if matches!(view().state, SignInState::Done { .. }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(matches!(view().state, SignInState::Done { .. }));
        assert_eq!(view().direct, None, "over: nothing to come back to");
        assert_eq!(caught(), 0, "its folder goes with it");

        // From this computer, the CLI opens its own browser as it always has.
        let Added::SignIn { sign_in: here } = start("Here", false).await.unwrap() else {
            panic!("no token: it signs in");
        };
        for _ in 0..200 {
            if d.account_sign_ins()
                .iter()
                .any(|s| s.id == here.id && matches!(s.state, SignInState::Code { .. }))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let here = d
            .account_sign_ins()
            .into_iter()
            .find(|s| s.id == here.id)
            .unwrap();
        assert!(matches!(here.state, SignInState::Code { .. }));
        assert_eq!(here.direct, None);
        assert_eq!(caught(), 0);
    })
    .await;
}

/// A stand-in Codex: says its version, signs in by its device page once
/// the test says the person approved it, and says whether it is signed in.
fn codex(dir: &Path) -> PathBuf {
    let path = dir.join("codex");
    let d = dir.display();
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\ncase \"$1 $2\" in\n\
             '--version ') echo 'codex-cli 0.166.0' ;;\n\
             'login status') if [ -e '{d}/approved' ]; then echo 'Logged in using ChatGPT' >&2; else echo 'Not logged in' >&2; fi ;;\n\
             'login --device-auth') echo 'Follow these steps to sign in with ChatGPT using device code authorization:'\n\
               echo '1. Open this link in your browser and sign in to your account'\n\
               echo '   https://auth.openai.com/codex/device'\n\
               echo '2. Enter this one-time code (expires in 15 minutes)'\n\
               echo '   WXYZ-98765'\n\
               while [ ! -e '{d}/approved' ] && [ ! -e '{d}/refused' ]; do sleep 0.05; done\n\
               [ -e '{d}/approved' ] || exit 1 ;;\n\
             esac\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn codex_signs_in_by_its_device_page_and_its_sign_in_is_read_again_at_once() {
    use super::engine_sign_ins::EngineSignInState;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut o = account_options(dir.path(), Arc::new(MemoryKeys::default()));
        o.programs.insert(Engine::Codex, codex(dir.path()));
        let d = desk(pool, o).await;
        assert!(
            d.start_engine_sign_in(Engine::Claude).is_err(),
            "Claude signs in another way"
        );
        let started = d.start_engine_sign_in(Engine::Codex).unwrap();
        assert_eq!(started.state, EngineSignInState::Starting);
        let state = || d.engine_sign_in(started.id).map(|v| v.state);
        for _ in 0..200 {
            if matches!(state(), Ok(EngineSignInState::Device { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(
            state().unwrap(),
            EngineSignInState::Device {
                url: "https://auth.openai.com/codex/device".into(),
                code: "WXYZ-98765".into()
            }
        );
        std::fs::write(dir.path().join("approved"), "").unwrap();
        for _ in 0..200 {
            if state().unwrap() == EngineSignInState::Done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(state().unwrap(), EngineSignInState::Done);
        let mut signed_in = false;
        for _ in 0..200 {
            signed_in = d.engines().iter().any(|e| {
                e.engine == Engine::Codex && matches!(e.sign_in, crate::engine::SignIn::In { .. })
            });
            if signed_in {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(signed_in, "read again at once, not at the next hourly look");

        // A newer one ends the last; one the page never approved fails in plain words.
        std::fs::remove_file(dir.path().join("approved")).unwrap();
        let first = d.start_engine_sign_in(Engine::Codex).unwrap();
        let second = d.start_engine_sign_in(Engine::Codex).unwrap();
        assert!(d.engine_sign_in(first.id).is_err());
        std::fs::write(dir.path().join("refused"), "").unwrap();
        for _ in 0..200 {
            if matches!(
                d.engine_sign_in(second.id).unwrap().state,
                EngineSignInState::Failed { .. }
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(
            d.engine_sign_in(second.id).unwrap().state,
            EngineSignInState::Failed {
                why: super::engine_sign_ins::DEVICE_FAILED.into()
            }
        );
        assert!(d.cancel_engine_sign_in(second.id).is_ok());
        assert!(d.engine_sign_in(second.id).is_err());
    })
    .await;
}

#[test]
fn a_sign_ins_page_is_never_caught_under_a_folder_others_may_write_to() {
    let dir = tempfile::tempdir().unwrap();
    // One of the person's own that others could write to is closed to them first.
    let open = dir.path().join("open-to-all");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
    drop(super::accounts::Relay::new(&open).unwrap());
    assert_eq!(
        std::fs::metadata(&open).unwrap().permissions().mode() & 0o777,
        0o755
    );
    // Someone else's (the system's own `/`) is never used.
    assert!(super::accounts::Relay::new(Path::new("/")).is_err());
    let mine = dir.path().join("mine");
    let relay = super::accounts::Relay::new(&mine).unwrap();
    let browser = std::fs::metadata(relay.browser()).unwrap();
    assert_eq!(browser.permissions().mode() & 0o777, 0o700);
    let folder = std::fs::metadata(relay.browser().parent().unwrap()).unwrap();
    assert_eq!(folder.permissions().mode() & 0o777, 0o700);
}

#[tokio::test]
async fn a_working_session_switches_after_its_turn_and_one_on_its_limit_carries_on() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let keys = Arc::new(MemoryKeys::default());
        let d = desk(pool, account_options(dir.path(), keys)).await;
        let work = paste(&d, "Work", GOOD, true).await;
        let personal = paste(&d, "Personal", OTHER, false).await;

        // Working: it moves once the turn ends.
        let s = d.create(session(dir.path(), None), None).await.unwrap();
        started(dir.path(), s.id, 0).await;
        d.change(s.id, |r, _| r.state = SessionState::Working);
        let v = d.switch_account(s.id, Some(personal), None).await.unwrap();
        assert_eq!(v.account.as_ref().map(|a| a.id), Some(work));
        assert_eq!(v.switching_to.as_ref().map(|a| a.id), Some(personal));
        d.change(s.id, |r, _| {
            r.state = SessionState::Done;
            r.last_reply = Some("Fixed the race.".into());
        });
        let (env, args) = started(dir.path(), s.id, 1).await;
        assert_eq!(env["TOKEN_ON_FD"], OTHER);
        assert!(args.starts_with("--resume"), "{args}");
        assert_eq!(d.view(s.id).unwrap().switching_to, None);

        // Stopped on its limit: moved now, and told to carry on.
        for _ in 0..200 {
            if d.view(s.id).unwrap().live {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        d.change(s.id, |r, _| {
            r.state = SessionState::Done;
            r.last_reply = Some("You've hit your usage limit · resets 3pm".into());
        });
        d.switch_account(s.id, Some(work), None).await.unwrap();
        let (env, args) = started(dir.path(), s.id, 2).await;
        assert_eq!(env["TOKEN_ON_FD"], GOOD);
        assert!(args.trim_end().ends_with("Continue"), "{args}");

        // Not running: its next start.
        d.stop(s.id, None).unwrap();
        for _ in 0..200 {
            if !d.view(s.id).unwrap().live {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let v = d.switch_account(s.id, None, None).await.unwrap();
        assert_eq!((v.account, v.live), (None, false));
    })
    .await;
}

#[tokio::test]
async fn a_removed_account_moves_its_sessions_to_the_default_at_their_next_start() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let keys = Arc::new(MemoryKeys::default());
        let d = desk(pool.clone(), account_options(dir.path(), keys.clone())).await;
        let work = paste(&d, "Work", GOOD, true).await;
        let personal = paste(&d, "Personal", OTHER, false).await;
        let s = d
            .create(session(dir.path(), Some(Some(personal))), None)
            .await
            .unwrap();
        d.remove_account(personal).await.unwrap();
        assert_eq!(keys.get(personal).unwrap(), None);
        // Still starting: it moves to Work once its turn is over.
        until(|| {
            d.view(s.id)
                .ok()?
                .switching_to
                .filter(|a| a.id == work)
                .map(drop)
        })
        .await;

        // No default left: the CLI's own sign-in.
        d.update_account(
            work,
            AccountPatch {
                name: None,
                is_default: Some(false),
            },
        )
        .await
        .unwrap();
        let t = d.create(session(dir.path(), None), None).await.unwrap();
        assert_eq!(t.account, None);

        // A token gone from the store: the session says so and doesn't start.
        keys.delete(work).unwrap();
        let gone = d
            .create(session(dir.path(), Some(Some(work))), None)
            .await
            .unwrap_err();
        assert!(gone.to_string().contains("add the account again"), "{gone}");
        let work_view = d.accounts().into_iter().find(|a| a.id == work).unwrap();
        assert!(
            !work_view.signed_in,
            "seen gone at launch, without reading the store again"
        );
    })
    .await;
}

#[tokio::test]
async fn a_desk_that_keeps_no_accounts_starts_every_session_on_its_own_sign_in() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut o = account_options(dir.path(), Arc::new(MemoryKeys::default()));
        o.account_keys = None;
        let d = desk(pool, o).await;
        let s = d.create(session(dir.path(), None), None).await.unwrap();
        assert_eq!(s.account, None);
        let (env, _) = started(dir.path(), s.id, 0).await;
        assert!(!env.contains_key(CLAUDE_TOKEN_ENV));
        assert!(!env.contains_key(crate::token_pipe::FD_ENV));
        assert_eq!(env["TOKEN_ON_FD"], "");
        assert_eq!(env["ANTHROPIC_API_KEY"], "sk-person-key");
        assert!(d
            .add_account(NewAccount {
                engine: Engine::Claude,
                name: "Work".into(),
                token: Some(GOOD.into()),
                make_default: false,
                remote: false,
            })
            .await
            .is_err());
    })
    .await;
}

#[test]
fn accounts_and_sign_ins_reach_desk_in_its_wire_shape() {
    let sign_in = crate::engine_accounts::SignInView {
        id: Uuid::nil(),
        engine: Engine::Claude,
        name: "Work".into(),
        state: SignInState::Code {
            url: Some("https://claude.ai/oauth/authorize".into()),
        },
        direct: None,
    };
    let event = serde_json::to_value(crate::model::DeskEvent::AccountSignIn {
        sign_in: sign_in.clone(),
    })
    .unwrap();
    assert_eq!(event["type"], "accountSignIn");
    assert_eq!(event["signIn"]["state"], "code");
    assert_eq!(event["signIn"]["url"], "https://claude.ai/oauth/authorize");
    assert!(event["signIn"].get("direct").is_none());
    let added = serde_json::to_value(Added::SignIn { sign_in }).unwrap();
    assert_eq!(added["kind"], "signIn");
    assert_eq!(added["signIn"]["name"], "Work");
    let accounts = serde_json::to_value(crate::model::DeskEvent::Accounts {
        accounts: vec![],
        needs: vec![],
        auto_switch: true,
        problems: vec![],
    })
    .unwrap();
    assert_eq!(accounts["type"], "accounts");
    assert_eq!(accounts["autoSwitch"], true);
}

/// Two sessions on Work, the default, both running.
async fn two_on_work(d: &Desk, dir: &Path) -> (Uuid, Uuid, Uuid, Uuid) {
    let work = paste(d, "Work", GOOD, true).await;
    let personal = paste(d, "Personal", OTHER, false).await;
    let a = d.create(session(dir, None), None).await.unwrap();
    let b = d.create(session(dir, None), None).await.unwrap();
    started(dir, a.id, 0).await;
    started(dir, b.id, 0).await;
    for id in [a.id, b.id] {
        d.change(id, |r, _| r.state = SessionState::Idle);
    }
    (work, personal, a.id, b.id)
}

/// Session `id`'s turn ends on its plan's limit.
fn stops_on_its_limit(d: &Desk, id: Uuid) {
    d.change(id, |r, _| r.state = SessionState::Working);
    d.change(id, |r, _| {
        r.state = SessionState::Done;
        r.last_reply = Some("You've hit your usage limit · resets Fri 9am".into());
    });
}

async fn until<T>(mut f: impl FnMut() -> Option<T>) -> T {
    for _ in 0..400 {
        if let Some(v) = f() {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("never came about");
}

#[tokio::test]
async fn an_account_at_its_limit_asks_to_switch_all_and_asks_back_once_it_resets() {
    use crate::engine_accounts::AccountNeed;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(
            pool,
            account_options(dir.path(), Arc::new(MemoryKeys::default())),
        )
        .await;
        let (work, personal, a, b) = two_on_work(&d, dir.path()).await;
        assert!(d.account_needs().is_empty());
        stops_on_its_limit(&d, a);
        let need = until(|| d.account_needs().into_iter().next()).await;
        let AccountNeed::Limit {
            id,
            from,
            to,
            sessions,
            ..
        } = need.clone()
        else {
            panic!("{need:?}");
        };
        assert_eq!(
            (from.id, to.id, sessions.len()),
            (Some(work), Some(personal), 2)
        );
        // Not now: put off while it stands.
        d.put_off(&id);
        assert!(d.account_needs().is_empty());
        d.0.state.lock().put_off.clear();

        // Switch all: the one stopped on its limit carries on, the idle one
        // restarts, both on Personal.
        let done = d
            .switch_all(Engine::Claude, Some(work), Some(personal))
            .await
            .unwrap();
        assert_eq!(done.now.len(), 2, "{done:?}");
        let (env, args) = started(dir.path(), a, 1).await;
        assert_eq!(env["TOKEN_ON_FD"], OTHER);
        assert!(args.trim_end().ends_with("Continue"), "{args}");
        let (env, args) = started(dir.path(), b, 1).await;
        assert_eq!(env["TOKEN_ON_FD"], OTHER);
        assert!(!args.contains("Continue"), "{args}");
        until(|| d.view(a).ok().filter(|v| v.live).map(drop)).await;
        until(|| d.view(b).ok().filter(|v| v.live).map(drop)).await;
        assert!(d.account_needs().is_empty(), "nothing left on Work");
        for id in [a, b] {
            d.change(id, |r, _| r.state = SessionState::Idle);
        }

        // Work reads with room again: asked back, never switched back alone.
        d.0.state
            .lock()
            .standing
            .get_mut(&Some(work))
            .unwrap()
            .hit_at = Some(chrono::Utc::now() - chrono::Duration::hours(6));
        let need = until(|| d.account_needs().into_iter().next()).await;
        assert!(
            matches!(&need, AccountNeed::Reset { account, from, sessions, .. }
                if account.id == Some(work) && from.id == Some(personal) && sessions.len() == 2),
            "{need:?}"
        );
        assert_eq!(d.view(a).unwrap().account.map(|x| x.id), Some(personal));
        d.switch_all(Engine::Claude, Some(personal), Some(work))
            .await
            .unwrap();
        assert!(d.account_needs().is_empty());
    })
    .await;
}

#[tokio::test]
async fn with_switch_automatically_on_desk_moves_them_itself_and_never_when_all_are_full() {
    use crate::engine_accounts::AccountNeed;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(
            pool.clone(),
            account_options(dir.path(), Arc::new(MemoryKeys::default())),
        )
        .await;
        let (work, personal, a, b) = two_on_work(&d, dir.path()).await;
        // Both full: when each resets, and nothing moves.
        d.0.state
            .lock()
            .standing
            .entry(Some(personal))
            .or_default()
            .hit_at = Some(chrono::Utc::now());
        d.set_auto_switch(true).await.unwrap();
        stops_on_its_limit(&d, a);
        let need = until(|| d.account_needs().into_iter().next()).await;
        assert!(matches!(need, AccountNeed::AllFull { .. }), "{need:?}");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(d.view(b).unwrap().account.map(|x| x.id), Some(work));

        // Personal has room: Desk switches by itself.
        d.0.state.lock().standing.remove(&Some(personal));
        d.accounts_moved().await;
        let (env, _) = started(dir.path(), b, 1).await;
        assert_eq!(env["TOKEN_ON_FD"], OTHER);
        assert_eq!(d.view(b).unwrap().account.map(|x| x.id), Some(personal));

        // The choice is kept.
        drop(d);
        let back = desk(
            pool,
            account_options(dir.path(), Arc::new(MemoryKeys::default())),
        )
        .await;
        assert!(back.auto_switch());
    })
    .await;
}

/// The hub's plan readings, as a test sees them: what each report was
/// kept as, and a window parser that takes `[{used, resets}]`.
#[derive(Default)]
struct Plans(parking_lot::Mutex<Vec<crate::usage::LocalAccount>>);

impl crate::usage::PlanSink for Plans {
    fn local(
        &self,
        _: crate::usage::LocalPlan,
        _: serde_json::Value,
        account: crate::usage::LocalAccount,
    ) {
        self.0.lock().push(account);
    }
    fn windows(
        &self,
        _: crate::usage::LocalPlan,
        payload: &serde_json::Value,
    ) -> Vec<crate::usage::LimitWindow> {
        payload
            .as_array()
            .into_iter()
            .flatten()
            .map(|w| crate::usage::LimitWindow {
                used_percent: w["used"].as_f64().unwrap_or_default(),
                resets_at: w["resets"]
                    .as_str()
                    .and_then(|r| chrono::DateTime::parse_from_rfc3339(r).ok())
                    .map(|r| r.to_utc()),
                minutes: 10_080,
            })
            .collect()
    }
    fn account_key(&self, _: crate::usage::LocalPlan, ids: &[String]) -> Option<String> {
        Some(format!("key:{}", ids.join(",")))
    }
}

#[tokio::test]
async fn a_full_weekly_window_reported_by_a_session_on_an_account_raises_its_need_and_is_that_accounts(
) {
    use crate::engine_accounts::AccountNeed;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let plans = Arc::new(Plans::default());
        let d = Desk::start(
            DeskStore::from_pool(pool),
            account_options(dir.path(), Arc::new(MemoryKeys::default())),
            super::Floor {
                plans: Some(plans.clone()),
                ..Default::default()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let (work, personal, a, _) = two_on_work(&d, dir.path()).await;
        let resets = (chrono::Utc::now() + chrono::Duration::days(2)).to_rfc3339();
        d.report_usage(
            serde_json::json!([{ "used": 100.0, "resets": resets }]),
            Some(a),
        );
        let need = until(|| d.account_needs().into_iter().next()).await;
        assert!(
            matches!(&need, AccountNeed::Limit { from, to, resets_at: Some(_), .. }
                if from.id == Some(work) && to.id == Some(personal)),
            "{need:?}"
        );
        // Kept as Work's reading, under the key its plan card is named by.
        let kept = plans.0.lock().clone();
        assert_eq!(
            kept.last().unwrap().ids,
            vec![format!("desk-account:{work}")]
        );
        let view = d.accounts().into_iter().find(|v| v.id == work).unwrap();
        assert_eq!(view.plan_key, Some(format!("key:desk-account:{work}")));
        // A report with room: Work isn't full any more.
        d.report_usage(
            serde_json::json!([{ "used": 20.0, "resets": resets }]),
            Some(a),
        );
        until(|| d.account_needs().is_empty().then_some(())).await;
    })
    .await;
}

#[tokio::test]
async fn a_session_never_moves_to_an_account_whose_sign_in_cant_be_read() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let keys = Arc::new(MemoryKeys::default());
        let d = desk(pool, account_options(dir.path(), keys.clone())).await;
        let work = paste(&d, "Work", GOOD, true).await;
        let personal = paste(&d, "Personal", OTHER, false).await;
        let s = d.create(session(dir.path(), None), None).await.unwrap();
        started(dir.path(), s.id, 0).await;
        d.change(s.id, |r, _| r.state = SessionState::Idle);
        keys.delete(personal).unwrap();
        let refused = d
            .switch_account(s.id, Some(personal), None)
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("no longer in"), "{refused}");
        let v = d.view(s.id).unwrap();
        assert_eq!(
            (v.account.map(|a| a.id), v.switching_to, v.live),
            (Some(work), None, true)
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("env-"))
                .count(),
            1,
            "it was never stopped"
        );
        let problems = d.sign_in_problems();
        assert_eq!(problems.len(), 1);
        assert_eq!(
            (problems[0].account, problems[0].problem),
            (personal, crate::engine_accounts::SignInProblem::Gone)
        );
        // Added again: the problem clears once it reads.
        keys.set(
            personal,
            &crate::engine_accounts::AccountToken::new(OTHER).unwrap(),
        )
        .unwrap();
        d.switch_account(s.id, Some(personal), None).await.unwrap();
        assert!(d.sign_in_problems().is_empty());
    })
    .await;
}

#[tokio::test]
async fn switches_of_one_session_are_carried_out_one_at_a_time_the_latest_winning() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(
            pool,
            account_options(dir.path(), Arc::new(MemoryKeys::default())),
        )
        .await;
        let work = paste(&d, "Work", GOOD, true).await;
        let personal = paste(&d, "Personal", OTHER, false).await;
        let s = d.create(session(dir.path(), None), None).await.unwrap();
        started(dir.path(), s.id, 0).await;
        d.change(s.id, |r, _| r.state = SessionState::Working);
        // Asked three times while it works: one restart, on the last asked.
        d.switch_account(s.id, Some(personal), None).await.unwrap();
        d.switch_account(s.id, None, None).await.unwrap();
        d.switch_account(s.id, Some(personal), None).await.unwrap();
        let v = d.view(s.id).unwrap();
        assert_eq!(v.switching_to.map(|a| a.id), Some(personal));
        // The turn ends, and the after-turn and a racing switch meet.
        d.change(s.id, |r, _| {
            r.state = SessionState::Done;
            r.last_reply = Some("Done.".into());
        });
        let racing = d.drive_switch(s.id, None);
        let (env, _) = started(dir.path(), s.id, 1).await;
        racing.await.unwrap();
        assert_eq!(env["TOKEN_ON_FD"], OTHER);
        tokio::time::sleep(Duration::from_millis(400)).await;
        let starts = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter_map(|e| {
                std::fs::read_to_string(e.path())
                    .ok()
                    .filter(|_| e.file_name().to_string_lossy().starts_with("env-"))
            })
            .filter(|env| env.contains(&format!("/hook/{}/", s.id)))
            .count();
        assert_eq!(starts, 2, "started, then restarted once");
        assert_ne!(d.view(s.id).unwrap().account.map(|a| a.id), Some(work));
    })
    .await;
}

#[tokio::test]
async fn a_removed_accounts_running_sessions_say_so_and_move_to_the_default() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(
            pool,
            account_options(dir.path(), Arc::new(MemoryKeys::default())),
        )
        .await;
        let work = paste(&d, "Work", GOOD, true).await;
        let personal = paste(&d, "Personal", OTHER, false).await;
        let s = d
            .create(session(dir.path(), Some(Some(personal))), None)
            .await
            .unwrap();
        started(dir.path(), s.id, 0).await;
        d.change(s.id, |r, _| r.state = SessionState::Working);
        d.remove_account(personal).await.unwrap();
        let v = d.view(s.id).unwrap();
        assert_eq!(
            v.account.map(|a| a.name).as_deref(),
            Some("Personal (removed)")
        );
        assert_eq!(v.switching_to.map(|a| a.id), Some(work));
        d.change(s.id, |r, _| {
            r.state = SessionState::Done;
            r.last_reply = Some("Done.".into());
        });
        let (env, _) = started(dir.path(), s.id, 1).await;
        assert_eq!(env["TOKEN_ON_FD"], GOOD);
        until(|| (d.view(s.id).ok()?.account?.id == work).then_some(())).await;
    })
    .await;
}

#[tokio::test]
async fn switch_all_and_auto_switch_never_pick_an_account_whose_sign_in_cant_be_read() {
    use crate::engine_accounts::AccountNeed;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let keys = Arc::new(MemoryKeys::default());
        let d = desk(pool, account_options(dir.path(), keys.clone())).await;
        let (work, personal, a, b) = two_on_work(&d, dir.path()).await;
        keys.delete(personal).unwrap();
        // Switch all to it: refused before any session is stopped.
        let refused = d
            .switch_all(Engine::Claude, Some(work), Some(personal))
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("no longer in"), "{refused}");
        for s in [a, b] {
            let v = d.view(s).unwrap();
            assert_eq!((v.account.map(|x| x.id), v.live), (Some(work), true));
        }
        // The Floor says why, and the limit need offers nothing to move to.
        d.set_auto_switch(true).await.unwrap();
        stops_on_its_limit(&d, a);
        let needs = until(|| {
            let n = d.account_needs();
            n.iter()
                .any(|n| matches!(n, AccountNeed::AllFull { .. }))
                .then_some(n)
        })
        .await;
        assert!(
            needs.iter().any(
                |n| matches!(n, AccountNeed::SignIn { account, .. } if account.id == Some(personal))
            ),
            "{needs:?}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            d.view(b).unwrap().account.map(|x| x.id),
            Some(work),
            "auto-switch moved nothing"
        );
    })
    .await;
}
