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
    AccountPatch, Keys, MemoryKeys, NewAccount, SignInAgain, SignInState, CLAUDE_TOKEN_ENV,
};
use crate::model::{NewSession, SessionState};
use crate::store::{build_pool, DeskStore};

/// What the fake CLI takes as a working token.
const GOOD: &str = "sk-ant-oat01-good-0123456789abcdefghijklmnopqrstuvwxyz";
const OTHER: &str = "sk-ant-oat01-good-other-9876543210zyxwvutsrqponmlkjihgf";

/// A stand-in Claude Code: says its version, signs in by a "good" token
/// (and by itself, unless the file `SIGNED_OUT_HERE` names is there: a
/// computer it was never logged in on; one that says `garbled`, it says
/// nothing readable; `key`, an API key), counts each look at its own sign-in in `own-reads`,
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
               '') echo x >> '{d}/own-reads'; said=; [ -n \"$SIGNED_OUT_HERE\" ] && [ -e \"$SIGNED_OUT_HERE\" ] && {{ read -r said < \"$SIGNED_OUT_HERE\"; said=${{said:-out}}; }}\n\
               case \"$said\" in\n\
                 out) echo '{{\"loggedIn\":false}}' ;;\n\
                 garbled) echo 'Something went wrong' ;;\n\
                 key) echo '{{\"loggedIn\":true,\"authMethod\":\"api_key\"}}' ;;\n\
                 gmail) echo '{{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"orgId\":\"org-gmail\",\"email\":\"kim@example.com\",\"subscriptionType\":\"max\"}}' ;;\n\
                 star) echo '{{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"orgId\":\"org-star\",\"email\":\"kim@star.example\",\"subscriptionType\":\"max\"}}' ;;\n\
                 *) echo '{{\"loggedIn\":true,\"authMethod\":\"claude.ai\"}}' ;;\n\
               esac ;;\n\
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
        fresh_worktree: false,
        engine: Engine::Claude,
        view: None,
        name: "auth-fix".into(),
        cwd: dir.to_string_lossy().into_owned(),
        goal: String::new(),
        worktree: None,
        prompt: None,
        approvals: None,
        approve_for_me: false,
        fast: false,
        model: None,
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
        let (_, mut events) = d.subscribe();
        for _ in 0..200 {
            if view().direct_ready && matches!(view().state, SignInState::Code { .. }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let direct = d
            .sign_in_direct(sign_in.id)
            .expect("the page that comes back");
        while let Ok(e) = events.try_recv() {
            assert!(
                !serde_json::to_string(&e).unwrap().contains("45678"),
                "never announced"
            );
        }
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
        assert!(d.sign_in_direct(sign_in.id).is_err());
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
            d.start_engine_sign_in(Engine::Claude, false).is_err(),
            "Claude signs in another way"
        );
        let started = d.start_engine_sign_in(Engine::Codex, false).unwrap();
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
        let first = d.start_engine_sign_in(Engine::Codex, false).unwrap();
        let second = d.start_engine_sign_in(Engine::Codex, false).unwrap();
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

/// The page Devin 3000.12.57 shows with no browser, as recorded
/// (fixtures/devin/3-manual.bin; its state and challenge replaced).
const DEVIN_PAGE: &str = "https://app.devin.ai/auth/cli/continue?state=Zx8q3V1kQ2&prompt=select_account&code_challenge=Q1w2E3r4T5y6U7i8O9p0A1s2D3f4G5h6J7k8L9z0X1c&code_challenge_method=S256&cli_pkce_marker=1";
/// The code the stand-in takes: it signs in only when given exactly this.
const DEVIN_TOKEN: &str = "devin-pasted-Qm9vbXNoYWthbGFrYQ-7f3a9c";

/// What Devin 3000.12.57 prints, recorded.
const DEVIN_PICKER: &[u8] = include_bytes!("../../fixtures/devin/1-picker.bin");
const DEVIN_BROWSER: &[u8] = include_bytes!("../../fixtures/devin/2-browser.bin");
const DEVIN_MANUAL: &[u8] = include_bytes!("../../fixtures/devin/3-manual.bin");
const DEVIN_STATUS_IN: &[u8] = include_bytes!("../../fixtures/devin/4-status-in.txt");
const DEVIN_STATUS_OUT: &[u8] = include_bytes!("../../fixtures/devin/5-status-out.txt");

/// The terminal's answers to Devin's colour queries (OSC 10, then 11).
const COLOURS: &[u8] = b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:0000/0000/0000\x1b\\";

/// A stand-in Devin, printing the recorded bytes of 3000.12.57 (their
/// files in `dir`, which a test may change) and, like it, waiting for the
/// terminal's answers to its queries before it goes on: each one checked
/// byte for byte. Its sign-in reads as `devin auth status` does (signed in
/// once `devin-in` is there; by a key, if one reached it).
///
/// `auth login` asks the kitty flags and DA1 (not in the recording: real
/// Devin asks only its colours there and pushes kitty flags; asked here
/// for robustness, in case a later Devin does), draws its picker, takes one
/// key (kept in `got-key`; Enter only goes on); with `devin-silent` it then
/// shows nothing more, with `devin-other` another prompt; else it waits in the browser
/// until the test writes `devin-approved` (it signs in), `devin-refused`
/// (it ends) or `devin-no-code` (it says it received no authorization
/// code). With `--force-manual-token-flow` it draws its page and Code
/// prompt, reads the code in raw mode, drawing each key itself, and signs
/// in only by [`DEVIN_TOKEN`], ignoring a hang-up meanwhile, as does the
/// child it starts (both pids in `login-pids`); with `child-traps` there,
/// only the child ignores it. Its environment is kept in `login-env`.
fn devin(dir: &Path) -> PathBuf {
    let files: [(&str, Vec<u8>); 9] = [
        ("picker.bin", DEVIN_PICKER.to_vec()),
        (
            "picker-answers",
            [b"\x1b[?0u\x1b[?1;2c", COLOURS, COLOURS].concat(),
        ),
        ("browser-wait.bin", devin_browser().0.to_vec()),
        ("browser-done.bin", devin_browser().1.to_vec()),
        ("colours", COLOURS.to_vec()),
        ("colours-twice", COLOURS.repeat(2)),
        ("manual.bin", DEVIN_MANUAL.to_vec()),
        ("status-in.txt", DEVIN_STATUS_IN.to_vec()),
        ("status-out.txt", DEVIN_STATUS_OUT.to_vec()),
    ];
    for (name, bytes) in files {
        std::fs::write(dir.join(name), bytes).unwrap();
    }
    let path = dir.join("devin");
    let d = dir.display();
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\n\
             answered() {{ dd bs=1 count=$(wc -c < \"$1\") 2>/dev/null > '{d}/got'; cmp -s '{d}/got' \"$1\" || exit 4; }}\n\
             case \"$1 $2\" in\n\
             '--version ') echo 'devin 3000.12.57 (96542e930752)' ;;\n\
             'auth status') if [ -n \"$WINDSURF_API_KEY\" ]; then echo 'Logged in with provided API key.'\n\
               elif [ -e '{d}/devin-in' ]; then cat '{d}/status-in.txt'\n\
               else cat '{d}/status-out.txt'; fi ;;\n\
             'auth login') env > '{d}/login-env'; stty raw -echo\n\
               if [ \"$3\" = '--force-manual-token-flow' ]; then\n\
                 if [ -e '{d}/child-traps' ]; then (trap '' HUP; exec sleep 30) </dev/null >/dev/null 2>&1 &\n\
                 else trap '' HUP; sleep 30 </dev/null >/dev/null 2>&1 & fi\n\
                 echo \"$$ $!\" > '{d}/login-pids'\n\
                 cat '{d}/manual.bin'; answered '{d}/colours-twice'\n\
                 cr=$(printf '\\r'); token=''\n\
                 while :; do c=$(dd bs=1 count=1 2>/dev/null); [ -n \"$c\" ] || exit 1\n\
                   [ \"$c\" = \"$cr\" ] && break; token=\"$token$c\"; printf '%s' \"$c\"; done\n\
                 printf '\\r\\nChecking code %s\\r\\n' \"$token\"\n\
                 sleep 1\n\
                 [ \"$token\" = '{DEVIN_TOKEN}' ] || exit 1\n\
                 touch '{d}/devin-in'; printf 'Login successful! Credentials stored.\\r\\n'; exit 0\n\
               fi\n\
               printf '\\033[?u\\033[c'; cat '{d}/picker.bin'; answered '{d}/picker-answers'\n\
               dd bs=1 count=1 2>/dev/null | od -An -c | tr -d ' \\n' > '{d}/got-key'\n\
               [ \"$(cat '{d}/got-key')\" = '\\r' ] || exit 3\n\
               [ -e '{d}/devin-silent' ] && exec sleep 30\n\
               [ -e '{d}/devin-other' ] && printf '\\r\\n Enter your Windsurf Enterprise subdomain:\\r\\n \\342\\206\\265 submit \\302\\267 esc cancel\\r\\n' && exec sleep 30\n\
               cat '{d}/browser-wait.bin'; answered '{d}/colours'\n\
               while [ ! -e '{d}/devin-approved' ] && [ ! -e '{d}/devin-refused' ] && [ ! -e '{d}/devin-no-code' ]; do sleep 0.05; done\n\
               if [ -e '{d}/devin-no-code' ]; then printf '\\r\\nError: Failed to receive authorization code\\r\\n'; sleep 30; fi\n\
               [ -e '{d}/devin-approved' ] || exit 1\n\
               touch '{d}/devin-in'; cat '{d}/browser-done.bin'; answered '{d}/colours' ;;\n\
             esac\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The recorded browser login, cut where it waits for the person: its
/// waiting, and its verifying and signing in.
fn devin_browser() -> (&'static [u8], &'static [u8]) {
    let at = DEVIN_BROWSER
        .windows(5)
        .rposition(|w| w == b"\x1b[<1u")
        .unwrap();
    let at = DEVIN_BROWSER[..at]
        .windows(5)
        .rposition(|w| w == b"\x1b[<1u")
        .unwrap();
    DEVIN_BROWSER.split_at(at)
}

/// Why Devin's sign-in `id` failed, once it has.
async fn devin_failed(d: &Desk, id: Uuid) -> String {
    use super::engine_sign_ins::EngineSignInState;
    until(|| match d.engine_sign_in(id).unwrap().state {
        EngineSignInState::Failed { why } => Some(why),
        _ => None,
    })
    .await
}

/// What Desk says of Devin's sign-in on this computer now.
fn devin_signed_in(d: &Desk) -> crate::engine::SignIn {
    d.engines()
        .into_iter()
        .find(|e| e.engine == Engine::Devin)
        .map(|e| e.sign_in)
        .unwrap()
}

/// Everything logged, as one string.
#[derive(Clone, Default)]
struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Captured {
        self.clone()
    }
}

/// Every run of 6 of `secret`'s characters that `text` repeats.
fn leaked(text: &str, secret: &str) -> Vec<String> {
    (0..=secret.len() - 6)
        .map(|at| &secret[at..at + 6])
        .filter(|piece| text.contains(piece))
        .map(str::to_string)
        .collect()
}

// One thread, so everything the desk spawns logs to the subscriber set here.
#[tokio::test(flavor = "current_thread")]
async fn devin_signs_in_by_a_pasted_code_kept_and_said_nowhere() {
    use super::engine_sign_ins::EngineSignInState;
    let log = Captured::default();
    let _logging = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(log.clone())
            .finish(),
    );
    // Under `cargo test` other tests' threads share these callsites. A
    // second live subscriber keeps tracing from caching "off" for them on
    // a thread that listens to nothing; what is caught is still only what
    // this thread logs (its own desk's tasks run here).
    let _second = tracing::Dispatch::new(tracing_subscriber::registry());
    tracing::callsite::rebuild_interest_cache();
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut o = account_options(dir.path(), Arc::new(MemoryKeys::default()));
        o.programs.insert(Engine::Devin, devin(dir.path()));
        // A key in the hub's environment never counts as Devin's sign-in.
        o.env
            .insert("WINDSURF_API_KEY".into(), "wsk-not-a-sign-in".into());
        let d = desk(pool, o).await;
        let (_, mut events) = d.subscribe();
        let app = crate::api::sessions_router(d.clone(), crate::testing::allow_all());
        until(|| (devin_signed_in(&d) == crate::engine::SignIn::Out).then_some(())).await;

        let (code, started) = api(
            &app,
            "POST",
            "/api/desk/engines/sign-ins",
            Some(serde_json::json!({"engine": "devin", "manual": true})),
        )
        .await;
        assert_eq!(code, 202, "{started}");
        assert_eq!(started["state"], "starting");
        let id: Uuid = serde_json::from_value(started["id"].clone()).unwrap();
        let state = || d.engine_sign_in(id).unwrap().state;
        let url = until(|| match state() {
            EngineSignInState::Paste { url } => Some(url),
            _ => None,
        })
        .await;
        assert_eq!(url, DEVIN_PAGE);
        let token_at = format!("/api/desk/engines/sign-ins/{id}/token");
        let (code, _) = api(
            &app,
            "POST",
            &token_at,
            Some(serde_json::json!({"token": "\u{1b}[Aexit\r"})),
        )
        .await;
        assert_eq!(
            code, 400,
            "a token that would type keys of its own is refused"
        );
        let (code, body) = api(
            &app,
            "POST",
            &token_at,
            Some(serde_json::json!({"token": format!("  {DEVIN_TOKEN}\n")})),
        )
        .await;
        assert_eq!(code, 204, "{body}");
        // A second is refused: the first was typed.
        let (code, body) = api(
            &app,
            "POST",
            &token_at,
            Some(serde_json::json!({"code": DEVIN_TOKEN})),
        )
        .await;
        assert_eq!(code, 409, "{body}");
        assert!(
            body.to_string()
                .contains(super::engine_sign_ins::DEVIN_TYPED_ALREADY),
            "{body}"
        );
        // The terminal echoes it at once, and the CLI draws it: neither is
        // ever on the screen, from the moment it was typed.
        for _ in 0..8 {
            let screen = d.engine_sign_in_screen(id).expect("the CLI still runs");
            assert!(leaked(&screen, DEVIN_TOKEN).is_empty(), "{screen}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Nor later.
        tokio::time::sleep(Duration::from_millis(700)).await;
        let screen = d.engine_sign_in_screen(id).expect("the CLI still runs");
        assert!(leaked(&screen, DEVIN_TOKEN).is_empty(), "{screen}");
        assert!(matches!(state(), EngineSignInState::Paste { .. }));

        // Signed in by exactly that token, and read again at once, as who.
        until(|| (state() == EngineSignInState::Done).then_some(())).await;
        assert_eq!(
            d.engine_sign_in_screen(id),
            None,
            "nothing of its screen is kept"
        );
        let login_env = std::fs::read_to_string(dir.path().join("login-env")).unwrap();
        for key in ["WINDSURF_API_KEY", "ANTHROPIC_API_KEY"] {
            assert!(!login_env.contains(key), "{key} reached the login");
        }
        let who = until(|| match devin_signed_in(&d) {
            crate::engine::SignIn::In { who, .. } => Some(who),
            _ => None,
        })
        .await;
        assert_eq!(who.as_deref(), Some("kim@example.com"));
        let (code, _) = api(
            &app,
            "POST",
            &token_at,
            Some(serde_json::json!({"token": DEVIN_TOKEN})),
        )
        .await;
        assert_eq!(code, 409, "it is no longer waiting for one");

        // The token is in no view, event or log.
        let (code, view) = api(
            &app,
            "GET",
            &format!("/api/desk/engines/sign-ins/{id}"),
            None,
        )
        .await;
        assert_eq!((code, view["state"].as_str()), (200, Some("done")));
        assert!(leaked(&view.to_string(), DEVIN_TOKEN).is_empty(), "{view}");
        let (_, engines) = api(&app, "GET", "/api/desk/engines", None).await;
        let engines = engines.to_string();
        assert!(leaked(&engines, DEVIN_TOKEN).is_empty());
        assert!(!engines.contains("credentials.toml"), "{engines}");
        let mut seen = 0;
        while let Ok(event) = events.try_recv() {
            seen += 1;
            let event = serde_json::to_string(&event).unwrap();
            assert!(leaked(&event, DEVIN_TOKEN).is_empty(), "{event}");
        }
        let logged = log.text();
        assert!(
            logged.contains("an engine's own sign-in ended"),
            "the log was caught: {seen} events, {logged}"
        );
        assert!(leaked(&logged, DEVIN_TOKEN).is_empty(), "{logged}");
        // Each step once, its page by scheme, host and path alone; never
        // its query, nor whom it signed in.
        assert!(
            logged.contains("page=\"https://app.devin.ai/auth/cli/continue\""),
            "{logged}"
        );
        assert_eq!(logged.matches("step=\"paste\"").count(), 1, "{logged}");
        for never in ["state=Zx8", "code_challenge", "kim@example.com", "?state"] {
            assert!(!logged.contains(never), "{never}: {logged}");
        }

        // One that is turned away says so in plain words.
        let again = d.start_engine_sign_in(Engine::Devin, true).unwrap();
        until(|| {
            matches!(
                d.engine_sign_in(again.id).unwrap().state,
                EngineSignInState::Paste { .. }
            )
            .then_some(())
        })
        .await;
        d.engine_sign_in_token(again.id, "devin-pasted-wrong")
            .unwrap();
        let failed = until(|| match d.engine_sign_in(again.id).unwrap().state {
            EngineSignInState::Failed { why } => Some(why),
            _ => None,
        })
        .await;
        assert_eq!(failed, super::engine_sign_ins::DEVIN_TOKEN_FAILED);

        // Given up on while it waits for a token, it and what it started
        // end, even ignoring the hang-up.
        std::fs::remove_file(dir.path().join("login-pids")).unwrap();
        let given_up = d.start_engine_sign_in(Engine::Devin, true).unwrap();
        until(|| {
            matches!(
                d.engine_sign_in(given_up.id).unwrap().state,
                EngineSignInState::Paste { .. }
            )
            .then_some(())
        })
        .await;
        let pids: Vec<libc::pid_t> =
            until(|| std::fs::read_to_string(dir.path().join("login-pids")).ok())
                .await
                .split_whitespace()
                .map(|p| p.parse().unwrap())
                .collect();
        assert_eq!(pids.len(), 2);
        d.cancel_engine_sign_in(given_up.id).unwrap();
        // SAFETY: signal 0 only asks whether the process is there.
        let alive = |pid| unsafe { libc::kill(pid, 0) } == 0;
        until(|| pids.iter().all(|&p| !alive(p)).then_some(())).await;

        // One whose CLI goes at the hang-up, but whose child stays: the
        // child is killed with it all the same.
        std::fs::write(dir.path().join("child-traps"), "").unwrap();
        std::fs::remove_file(dir.path().join("login-pids")).unwrap();
        let left = d.start_engine_sign_in(Engine::Devin, true).unwrap();
        until(|| {
            matches!(
                d.engine_sign_in(left.id).unwrap().state,
                EngineSignInState::Paste { .. }
            )
            .then_some(())
        })
        .await;
        let pids: Vec<libc::pid_t> =
            until(|| std::fs::read_to_string(dir.path().join("login-pids")).ok())
                .await
                .split_whitespace()
                .map(|p| p.parse().unwrap())
                .collect();
        d.cancel_engine_sign_in(left.id).unwrap();
        until(|| (!alive(pids[0])).then_some(())).await;
        until(|| (!alive(pids[1])).then_some(())).await;
    })
    .await;
}

#[tokio::test]
async fn devin_signs_in_in_the_browser_it_opens_from_its_own_picker() {
    use super::engine_sign_ins::{self as sign_ins, EngineSignInState};
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut o = account_options(dir.path(), Arc::new(MemoryKeys::default()));
        o.programs.insert(Engine::Devin, devin(dir.path()));
        let d = desk(pool, o).await;
        let app = crate::api::sessions_router(d.clone(), crate::testing::allow_all());
        let state = |id| d.engine_sign_in(id).unwrap().state;
        until(|| (devin_signed_in(&d) == crate::engine::SignIn::Out).then_some(())).await;

        // Its picker answered with Enter, once its queries are: it waits in
        // the browser it opened, with no page to show.
        let started = d.start_engine_sign_in(Engine::Devin, false).unwrap();
        until(|| (state(started.id) == EngineSignInState::browser()).then_some(())).await;
        let (code, view) = api(
            &app,
            "GET",
            &format!("/api/desk/engines/sign-ins/{}", started.id),
            None,
        )
        .await;
        assert_eq!(code, 200);
        assert_eq!(
            view,
            serde_json::json!({
                "id": started.id,
                "engine": "devin",
                "state": "browser",
                "say": sign_ins::DEVIN_IN_BROWSER,
                "instead": sign_ins::DEVIN_PASTE_INSTEAD,
            })
        );
        let key = || std::fs::read_to_string(dir.path().join("got-key")).unwrap_or_default();
        let pressed = until(|| Some(key()).filter(|k| !k.is_empty())).await;
        assert_eq!(pressed, "\\r", "Enter, once");
        // A sign-in in the browser takes no code.
        let (code, _) = api(
            &app,
            "POST",
            &format!("/api/desk/engines/sign-ins/{}/token", started.id),
            Some(serde_json::json!({"code": DEVIN_TOKEN})),
        )
        .await;
        assert_eq!(code, 409);
        // Held in the browser past the wait for it to say so: still waiting.
        tokio::time::sleep(Duration::from_millis(2600)).await;
        assert_eq!(
            state(started.id),
            EngineSignInState::browser(),
            "still waiting"
        );

        // Signed in there: done, and read again at once, as who.
        std::fs::write(dir.path().join("devin-approved"), "").unwrap();
        until(|| (state(started.id) == EngineSignInState::Done).then_some(())).await;
        assert!(matches!(
            devin_signed_in(&d),
            crate::engine::SignIn::In { who: Some(w), .. } if w == "kim@example.com"
        ));

        // The browser's answer never came: said in plain words, the CLI ended.
        std::fs::remove_file(dir.path().join("devin-approved")).unwrap();
        std::fs::remove_file(dir.path().join("devin-in")).unwrap();
        std::fs::write(dir.path().join("devin-no-code"), "").unwrap();
        let lost = d.start_engine_sign_in(Engine::Devin, false).unwrap();
        assert_eq!(devin_failed(&d, lost.id).await, sign_ins::DEVIN_NO_ANSWER);
        std::fs::remove_file(dir.path().join("devin-no-code")).unwrap();

        // Enter taken, then nothing, or something else: never in the
        // browser, and said plainly once it has waited long enough.
        for after in ["devin-silent", "devin-other"] {
            std::fs::write(dir.path().join(after), "").unwrap();
            let _ = std::fs::remove_file(dir.path().join("got-key"));
            let stalled = d.start_engine_sign_in(Engine::Devin, false).unwrap();
            let why = until(|| match state(stalled.id) {
                EngineSignInState::Failed { why } => Some(why),
                EngineSignInState::Browser { .. } => panic!("{after}: in the browser"),
                _ => None,
            })
            .await;
            assert_eq!(why, sign_ins::DEVIN_ASKED_UNKNOWN, "{after}");
            assert_eq!(key(), "\\r", "{after}: Enter was pressed");
            std::fs::remove_file(dir.path().join(after)).unwrap();
        }

        // A picker Desk doesn't know: never answered, and said plainly.
        for changed in [
            String::from_utf8(DEVIN_PICKER.to_vec())
                .unwrap()
                .replace("1 Log in with browser", "1 Log in with Windsurf"),
            String::from_utf8(DEVIN_PICKER.to_vec())
                .unwrap()
                .replace(" \u{276d} 1 Log", "   1 Log")
                .replace("   3 Log", " \u{276d} 3 Log"),
        ] {
            std::fs::write(dir.path().join("picker.bin"), changed).unwrap();
            let _ = std::fs::remove_file(dir.path().join("got-key"));
            let asked = d.start_engine_sign_in(Engine::Devin, false).unwrap();
            assert_eq!(
                devin_failed(&d, asked.id).await,
                sign_ins::DEVIN_ASKED_UNKNOWN
            );
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(key(), "", "no key was pressed for it");
        }

        // With no browser, a page on another host is never shown.
        let lookalike = String::from_utf8(DEVIN_MANUAL.to_vec())
            .unwrap()
            .replace("app.devin.ai", "app.devin.ai.evil.example");
        std::fs::write(dir.path().join("manual.bin"), lookalike).unwrap();
        let manual = d.start_engine_sign_in(Engine::Devin, true).unwrap();
        assert_eq!(
            devin_failed(&d, manual.id).await,
            sign_ins::DEVIN_PAGE_UNKNOWN
        );
        assert!(
            d.start_engine_sign_in(Engine::Gemini, false).is_err(),
            "Gemini signs in another way"
        );
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
        0o700
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
        assert!(gone.to_string().contains("sign it in again"), "{gone}");
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
        direct: Some(crate::engine_accounts::Direct {
            url: "https://claude.ai/oauth/authorize?redirect_uri=x".into(),
            port: 46009,
        }),
        direct_ready: true,
    };
    let event = serde_json::to_value(crate::model::DeskEvent::AccountSignIn {
        sign_in: sign_in.clone(),
    })
    .unwrap();
    assert_eq!(event["type"], "accountSignIn");
    assert_eq!(event["signIn"]["state"], "code");
    assert_eq!(event["signIn"]["url"], "https://claude.ai/oauth/authorize");
    // The page that comes back is never announced, only that it can be asked for.
    assert!(event["signIn"].get("direct").is_none());
    assert!(!event.to_string().contains("46009"));
    assert_eq!(event["signIn"]["directReady"], true);
    let added = serde_json::to_value(Added::SignIn { sign_in }).unwrap();
    assert_eq!(added["kind"], "signIn");
    assert_eq!(added["signIn"]["name"], "Work");
    let accounts = serde_json::to_value(crate::model::DeskEvent::Accounts {
        accounts: vec![],
        needs: vec![],
        auto_switch: true,
        problems: vec![],
        own_plan_key: None,
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

        // Long since, nothing read of Work: not known to have reset, so
        // nobody is asked back.
        d.0.state
            .lock()
            .standing
            .get_mut(&Some(work))
            .unwrap()
            .hit_at = Some(chrono::Utc::now() - chrono::Duration::hours(6));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(d.account_needs().is_empty(), "{:?}", d.account_needs());
        // Work reads with room again: asked back, never switched back alone.
        d.0.state
            .lock()
            .standing
            .get_mut(&Some(work))
            .unwrap()
            .read_at = Some(chrono::Utc::now());
        d.0.state
            .lock()
            .standing
            .get_mut(&Some(work))
            .unwrap()
            .windows = vec![crate::usage::LimitWindow {
            used_percent: 10.0,
            resets_at: Some(chrono::Utc::now() + chrono::Duration::hours(3)),
            minutes: 300,
        }];
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
            key_options(dir.path(), Arc::new(MemoryKeys::default())),
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

/// Claude Code's `StopFailure`, as 2.1.319 posts it: `said` is the error
/// as its terminal shows it.
fn stop_failure(transcript: &Path, said: &str) -> crate::claude::Hook {
    crate::claude::parse_hook(
        serde_json::json!({
            "session_id": "s1",
            "transcript_path": transcript,
            "cwd": "/w",
            "hook_event_name": "StopFailure",
            "error": "rate_limit",
            "last_assistant_message": said,
        })
        .to_string()
        .as_bytes(),
    )
    .unwrap()
}

#[tokio::test]
async fn a_turn_stopped_by_its_plans_limit_moves_at_once_and_carries_on() {
    use crate::floor::Outcome;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(
            pool,
            key_options(dir.path(), Arc::new(MemoryKeys::default())),
        )
        .await;
        let (work, personal, a, b) = two_on_work(&d, dir.path()).await;
        d.set_auto_switch(true).await.unwrap();

        // A 429 that isn't the plan's: the turn failed, nothing is full.
        let transcript = dir.path().join("a.jsonl");
        std::fs::write(&transcript, "").unwrap();
        d.apply_hook(a, crate::claude::Hook::Prompt("fix it".into()), None)
            .await;
        d.apply_hook(
            a,
            stop_failure(
                &transcript,
                "API Error: Request rejected (429) · This request would exceed your account's rate limit.",
            ),
            None,
        )
        .await;
        let v = d.view(a).unwrap();
        assert_eq!(v.state, SessionState::Done);
        assert_eq!(
            super::transition::finished(v.last_reply.as_deref().unwrap_or_default(), &v).0,
            Outcome::Failed
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(d.account_needs().is_empty());
        assert_eq!(d.view(a).unwrap().account.map(|x| x.id), Some(work));

        // The plan's own limit, mid-turn: no Stop comes, yet the turn is over
        // on its limit, and Desk moves both at once, this one told to carry
        // on.
        d.apply_hook(a, crate::claude::Hook::Prompt("fix it".into()), None)
            .await;
        assert_eq!(d.view(a).unwrap().failure, None, "asked again");
        assert_eq!(d.view(a).unwrap().state, SessionState::Working);
        // Claude Code writes the error to the transcript after the hook.
        d.apply_hook(
            a,
            stop_failure(&transcript, "You've hit your 5-hour limit · resets 9am (Europe/London)"),
            None,
        )
        .await;
        std::fs::write(&transcript, error_line(true)).unwrap();
        let (env, args) = started(dir.path(), a, 1).await;
        assert_eq!(env["TOKEN_ON_FD"], OTHER);
        assert!(args.trim_end().ends_with("Continue"), "{args}");
        let (env, args) = started(dir.path(), b, 1).await;
        assert_eq!(env["TOKEN_ON_FD"], OTHER);
        assert!(!args.contains("Continue"), "{args}");
        assert_eq!(d.view(a).unwrap().account.map(|x| x.id), Some(personal));
        // And Work is full until the moment Claude Code said.
        let reset = Some(resets_in_two_days());
        until(|| {
            d.0.state
                .lock()
                .standing
                .get(&Some(work))
                .and_then(|s| s.hit_until)
                .filter(|at| Some(*at) == reset)
        })
        .await;
    })
    .await;
}

/// Claude Code's API error line for a refused turn, as 2.1.319 writes it
/// after its StopFailure: the plan's own limit, or (`plan` false) a 429
/// that isn't.
fn error_line(plan: bool) -> String {
    if plan {
        r#"{"type":"assistant","isApiErrorMessage":true,"apiError":"usage_limit_reached","apiErrorParams":{"rate_limit_info":{"status":"rejected","resetsAt":RESETS,"rateLimitType":"seven_day"}},"quotaLimits":{"status":"rejected","resetsAt":RESETS,"rateLimitType":"seven_day"},"error":"rate_limit","message":{"content":[{"type":"text","text":"Request rejected (429)"}]}}"#.replace("RESETS", &resets_in_two_days().timestamp().to_string()) + "\n"
    } else {
        r#"{"type":"assistant","isApiErrorMessage":true,"apiError":null,"apiErrorParams":null,"quotaLimits":null,"error":"rate_limit","apiErrorStatus":429,"message":{"content":[{"type":"text","text":"API Error: Request rejected (429) · This request would exceed your account's rate limit."}]}}"#.to_string() + "\n"
    }
}

/// Two days from now, to the hour, as Claude Code's resets are.
fn resets_in_two_days() -> chrono::DateTime<chrono::Utc> {
    let at = chrono::Utc::now().timestamp() + 2 * 86_400;
    chrono::DateTime::from_timestamp(at - at % 3600, 0).unwrap()
}

#[tokio::test]
async fn a_limit_hit_while_background_work_runs_still_moves_the_session_at_once() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(
            pool,
            key_options(dir.path(), Arc::new(MemoryKeys::default())),
        )
        .await;
        let (_, personal, a, _) = two_on_work(&d, dir.path()).await;
        // Asked to move while it works: it waits for the turn to end.
        d.apply_hook(
            a,
            crate::claude::Hook::Prompt("start the dev server".into()),
            None,
        )
        .await;
        d.apply_hook(a, crate::claude::Hook::SubagentStart("a471".into()), None)
            .await;
        let v = d.switch_account(a, Some(personal), None).await.unwrap();
        assert!(v.switching_to.is_some());
        let transcript = dir.path().join("a.jsonl");
        std::fs::write(&transcript, "").unwrap();
        // The plan's limit ends the turn with a sub-agent still running:
        // the Floor waits on it, the switch doesn't.
        d.apply_hook(
            a,
            stop_failure(&transcript, "You've hit your weekly limit · resets Fri 9am"),
            None,
        )
        .await;
        assert_eq!(
            d.view(a).unwrap().waiting_on(),
            1,
            "its work is still known"
        );
        let (env, args) = started(dir.path(), a, 1).await;
        assert_eq!(env["TOKEN_ON_FD"], OTHER);
        assert!(args.trim_end().ends_with("Continue"), "{args}");
    })
    .await;
}

#[tokio::test]
async fn the_transcript_decides_what_a_rate_limit_was() {
    use crate::floor::Outcome;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(
            pool,
            key_options(dir.path(), Arc::new(MemoryKeys::default())),
        )
        .await;
        let (work, personal, a, b) = two_on_work(&d, dir.path()).await;
        let transcript = dir.path().join("a.jsonl");
        let full = |d: &Desk| {
            d.0.state
                .lock()
                .standing
                .get(&Some(work))
                .is_some_and(|s| s.full(chrono::Utc::now()).is_some())
        };

        // Words that read as the plan's, a line that says it wasn't: the
        // turn failed, and Work isn't full.
        std::fs::write(&transcript, "").unwrap();
        d.apply_hook(a, crate::claude::Hook::Prompt("go".into()), None)
            .await;
        d.apply_hook(a, stop_failure(&transcript, "You've hit your limit"), None)
            .await;
        assert!(full(&d));
        std::fs::write(&transcript, error_line(false)).unwrap();
        until(|| (!full(&d)).then_some(())).await;
        assert_eq!(
            d.view(a).unwrap().failure,
            Some(crate::limit::TurnFailure::Failed("rate_limit".into()))
        );

        // Words that don't, a line that says it was the plan's weekly
        // limit: full until its reset, and Desk moves them.
        std::fs::write(&transcript, "").unwrap();
        d.set_auto_switch(true).await.unwrap();
        d.apply_hook(a, crate::claude::Hook::Prompt("go".into()), None)
            .await;
        d.apply_hook(
            a,
            stop_failure(
                &transcript,
                "API Error: Request rejected (429) · rate limited",
            ),
            None,
        )
        .await;
        let v = d.view(a).unwrap();
        assert_eq!(
            super::transition::finished(v.last_reply.as_deref().unwrap_or_default(), &v).0,
            Outcome::Failed
        );
        assert!(!full(&d));
        std::fs::write(&transcript, error_line(true)).unwrap();
        until(|| full(&d).then_some(())).await;
        assert!(d
            .view(a)
            .unwrap()
            .failure
            .is_some_and(|f| f.limit().is_some()));
        let (env, args) = started(dir.path(), a, 1).await;
        assert_eq!(env["TOKEN_ON_FD"], OTHER);
        assert!(args.trim_end().ends_with("Continue"), "{args}");
        started(dir.path(), b, 1).await;
        assert_eq!(d.view(b).unwrap().account.map(|x| x.id), Some(personal));
        assert_eq!(
            d.0.state.lock().standing[&Some(work)].hit_until,
            Some(resets_in_two_days())
        );
    })
    .await;
}

#[tokio::test]
async fn a_session_elsewhere_whose_transcript_ends_on_a_429_that_isnt_the_plans_failed_and_fills_nothing(
) {
    use crate::floor::Outcome;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(
            pool,
            account_options(dir.path(), Arc::new(MemoryKeys::default())),
        )
        .await;
        let (work, _, a, _) = two_on_work(&d, dir.path()).await;
        // As the scan of another terminal's transcript keeps it.
        let f = dir.path().join("t.jsonl");
        std::fs::write(&f, error_line(false)).unwrap();
        let failure = crate::transcript::read_tail(&f).unwrap().failure;
        assert_eq!(
            failure,
            Some(crate::limit::TurnFailure::Failed("rate_limit".into()))
        );
        d.change(a, |r, _| r.state = SessionState::Working);
        d.0.state.lock().turn_failures.insert(a, failure.unwrap());
        d.change(a, |r, _| {
            r.state = SessionState::Done;
            r.last_reply = Some(
                "API Error: Request rejected (429) · This request would exceed your account's rate limit."
                    .into(),
            );
        });
        let v = d.view(a).unwrap();
        assert_eq!(
            super::transition::finished(v.last_reply.as_deref().unwrap_or_default(), &v).0,
            Outcome::Failed
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(d.account_needs().is_empty());
        assert!(d
            .0
            .state
            .lock()
            .standing
            .get(&Some(work))
            .is_none_or(|s| s.hit_at.is_none()));
    })
    .await;
}

/// The hub's plan readings, as a test sees them: what each report was
/// kept as, and a window parser that takes `[{used, resets}]`.
#[derive(Default)]
struct Plans(
    parking_lot::Mutex<Vec<crate::usage::LocalAccount>>,
    /// What was forgotten: the account's ids, the reading's time, and when
    /// what was kept instead was taken.
    parking_lot::Mutex<Forgot>,
);

type Forgot = Vec<(
    Vec<String>,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<chrono::DateTime<chrono::Utc>>,
)>;

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
                minutes: w["minutes"].as_u64().map_or(10_080, |m| m as u32),
            })
            .collect()
    }
    fn account_key(&self, _: crate::usage::LocalPlan, ids: &[String]) -> Option<String> {
        Some(format!("key:{}", ids.join(",")))
    }
    fn forget_local(
        &self,
        _: crate::usage::LocalPlan,
        account: crate::usage::LocalAccount,
        taken: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
        instead: Option<(serde_json::Value, chrono::DateTime<chrono::Utc>)>,
    ) {
        self.1.lock().push((
            account.ids,
            taken.map(|(_, last)| last),
            instead.map(|(_, at)| at),
        ));
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
        let d = desk(pool, key_options(dir.path(), keys.clone())).await;
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

#[tokio::test]
async fn an_accounts_sign_in_is_handed_over_whole_and_said_nowhere_else() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let keys = Arc::new(MemoryKeys::default());
        let d = desk(pool.clone(), account_options(dir.path(), keys.clone())).await;
        let work = paste(&d, "Work", GOOD, true).await;
        let personal = paste(&d, "Personal", OTHER, false).await;

        let handed = d.hand_over_account(work).await.unwrap();
        assert_eq!(handed.token.expose(), GOOD);
        assert_eq!((handed.name.as_str(), handed.is_default), ("Work", true));
        // Its wire shape carries the token; its Debug never does.
        let wire = serde_json::to_value(&handed).unwrap();
        assert_eq!(wire["token"], GOOD);
        assert_eq!(wire["engine"], "claude");
        assert_eq!(wire["isDefault"], true);
        assert!(!format!("{handed:?}").contains("sk-ant"), "{handed:?}");
        let other = d.hand_over_account(personal).await.unwrap();
        assert_eq!((other.token.expose(), other.is_default), (OTHER, false));

        // Handing over moves nothing: the account and its token stay here.
        assert_eq!(keys.get(work).unwrap().unwrap().expose(), GOOD);
        assert!(d.accounts().iter().any(|a| a.id == work && a.signed_in));

        // Gone from the store: said, never an empty token.
        keys.delete(personal).unwrap();
        let gone = d.hand_over_account(personal).await.unwrap_err();
        assert!(gone.to_string().contains("sign it in again"), "{gone}");
        // No such account.
        assert!(d.hand_over_account(Uuid::new_v4()).await.is_err());
    })
    .await;
}

/// Keys that keep tokens but refuse to delete them.
#[derive(Default)]
struct StuckKeys(MemoryKeys);

impl Keys for StuckKeys {
    fn get(&self, account: Uuid) -> eyre::Result<Option<crate::engine_accounts::AccountToken>> {
        self.0.get(account)
    }
    fn set(&self, account: Uuid, token: &crate::engine_accounts::AccountToken) -> eyre::Result<()> {
        self.0.set(account, token)
    }
    fn delete(&self, _: Uuid) -> eyre::Result<()> {
        Err(eyre::eyre!("the keychain is locked"))
    }
    fn name(&self) -> &'static str {
        "the keychain"
    }
}

#[tokio::test]
async fn removing_an_account_says_so_when_its_sign_in_could_not_be_deleted() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let mut o = account_options(dir.path(), Arc::new(MemoryKeys::default()));
        o.account_keys = Some(Arc::new(StuckKeys::default()));
        let d = desk(pool.clone(), o).await;
        let work = paste(&d, "Work", GOOD, true).await;
        let err = d.remove_account(work).await.unwrap_err();
        let said = err.to_string();
        assert!(
            said.contains("removed, but its sign-in couldn't be deleted from the keychain")
                && said.contains("the keychain is locked"),
            "{said}"
        );
        // Not a 2xx over the API, and the account itself is gone.
        let app = crate::api::sessions_router(d.clone(), crate::testing::allow_all());
        assert!(d.accounts().iter().all(|a| a.id != work));
        let again = paste(&d, "Again", OTHER, false).await;
        let (code, body) = api(&app, "DELETE", &format!("/api/desk/accounts/{again}"), None).await;
        assert_eq!(code, 409, "{body}");
        assert!(
            body["error"].as_str().unwrap().contains("keychain"),
            "{body}"
        );
    })
    .await;
}

async fn api(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    use tower::ServiceExt;
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", "Bearer hub-token")
                .header("content-type", "application/json")
                .body(body.map_or_else(axum::body::Body::empty, |b| {
                    axum::body::Body::from(b.to_string())
                }))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn only_the_hubs_own_token_is_handed_a_sign_in_and_only_once_a_minute() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let keys = Arc::new(MemoryKeys::default());
        let d = desk(pool.clone(), account_options(dir.path(), keys)).await;
        let work = paste(&d, "Work", GOOD, true).await;
        let uri = format!("/api/desk/accounts/{work}/hand-over");

        // A signed-in administrator (a system-admin JWT) is refused, and
        // being refused uses up nothing.
        let admin =
            crate::api::sessions_router(d.clone(), crate::testing::identified("acme-admin"));
        let (code, body) = api(&admin, "POST", &uri, None).await;
        assert_eq!(code, 403, "{body}");
        assert!(body.get("token").is_none(), "{body}");

        // The hub's own token is handed it, once.
        let gateway = crate::api::sessions_router(d.clone(), crate::testing::allow_all());
        let (code, body) = api(&gateway, "POST", &uri, None).await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(body["token"], GOOD);
        let (code, body) = api(&gateway, "POST", &uri, None).await;
        assert_eq!(code, 429, "{body}");
        assert!(
            body["error"].as_str().unwrap().contains("a minute"),
            "{body}"
        );
        assert!(body.get("token").is_none(), "{body}");

        // Another account has its own minute.
        let personal = paste(&d, "Personal", OTHER, false).await;
        let (code, _) = api(
            &gateway,
            "POST",
            &format!("/api/desk/accounts/{personal}/hand-over"),
            None,
        )
        .await;
        assert_eq!(code, 200);
    })
    .await;
}

/// A computer Claude Code was never logged in on (a machine of the
/// person's, before or after "Use this Mac's accounts").
fn signed_out_options(dir: &Path, keys: Arc<MemoryKeys>) -> Options {
    let mut o = account_options(dir, keys);
    let file = dir.join("signed-out");
    std::fs::write(&file, "").unwrap();
    o.env.insert(
        "SIGNED_OUT_HERE".into(),
        file.to_string_lossy().into_owned(),
    );
    o
}

/// A computer whose Claude Code is signed in with an API key: no plan of
/// its own for sessions to be moved to at a limit.
fn key_options(dir: &Path, keys: Arc<MemoryKeys>) -> Options {
    let o = signed_out_options(dir, keys);
    std::fs::write(dir.join("signed-out"), "key").unwrap();
    o
}

/// How often the CLI was asked about its own sign-in.
fn own_reads(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("own-reads")).map_or(0, |s| s.lines().count())
}

#[tokio::test]
async fn with_no_sign_in_of_its_own_a_session_starts_on_the_first_account_never_at_a_login() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let keys = Arc::new(MemoryKeys::default());
        let d = desk(pool, signed_out_options(dir.path(), keys.clone())).await;
        // As copying this Mac's accounts left them: none the default.
        let work = paste(&d, "Work", GOOD, false).await;
        let personal = paste(&d, "Personal", OTHER, false).await;

        // Absent: no default, and no sign-in here, so the first account.
        let s = d.create(session(dir.path(), None), None).await.unwrap();
        assert_eq!(s.account.as_ref().map(|a| a.id), Some(work));
        let (env, _) = started(dir.path(), s.id, 0).await;
        assert_eq!(env["TOKEN_ON_FD"], GOOD, "signed in by Work's token");
        assert!(!env.contains_key("ANTHROPIC_API_KEY"));
        assert!(matches!(
            d.sign_ins().of(Engine::Claude),
            crate::engine::SignIn::Out
        ));

        // `null` (this computer's sign-in, which it hasn't): Work too, and kept
        // as the session's for its readings and switches.
        let own = d
            .create(session(dir.path(), Some(None)), None)
            .await
            .unwrap();
        assert_eq!(own.account.as_ref().map(|a| a.id), Some(work));
        let (env, _) = started(dir.path(), own.id, 0).await;
        assert_eq!(env["TOKEN_ON_FD"], GOOD);

        // The default comes first, as a switch takes them.
        d.update_account(
            personal,
            AccountPatch {
                name: None,
                is_default: Some(true),
            },
        )
        .await
        .unwrap();
        let p = d
            .create(session(dir.path(), Some(None)), None)
            .await
            .unwrap();
        assert_eq!(p.account.as_ref().map(|a| a.id), Some(personal));
        let (env, _) = started(dir.path(), p.id, 0).await;
        assert_eq!(env["TOKEN_ON_FD"], OTHER);

        // Work's sign-in gone: the next that reads.
        d.update_account(
            personal,
            AccountPatch {
                name: None,
                is_default: Some(false),
            },
        )
        .await
        .unwrap();
        keys.delete(work).unwrap();
        let next = d.create(session(dir.path(), None), None).await.unwrap();
        assert_eq!(next.account.as_ref().map(|a| a.id), Some(personal));

        // None reads: said plainly, and nothing starts.
        keys.delete(personal).unwrap();
        let none = d.create(session(dir.path(), None), None).await.unwrap_err();
        assert!(
            none.to_string()
                .contains("Claude Code isn't signed in on this computer, and none of its accounts"),
            "{none}"
        );
    })
    .await;
}

#[tokio::test]
async fn with_no_sign_in_of_its_own_the_first_account_switches_to_the_next_at_its_limit() {
    use crate::engine_accounts::AccountNeed;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(
            pool,
            signed_out_options(dir.path(), Arc::new(MemoryKeys::default())),
        )
        .await;
        let work = paste(&d, "Work", GOOD, false).await;
        let personal = paste(&d, "Personal", OTHER, false).await;
        d.set_auto_switch(true).await.unwrap();
        let a = d.create(session(dir.path(), None), None).await.unwrap().id;
        let b = d.create(session(dir.path(), None), None).await.unwrap().id;
        started(dir.path(), a, 0).await;
        started(dir.path(), b, 0).await;
        for id in [a, b] {
            assert_eq!(d.view(id).unwrap().account.map(|x| x.id), Some(work));
            d.change(id, |r, _| r.state = SessionState::Idle);
        }
        // This computer's own sign-in is never a side to switch to.
        stops_on_its_limit(&d, a);
        let (env, args) = started(dir.path(), a, 1).await;
        assert_eq!(env["TOKEN_ON_FD"], OTHER);
        assert!(args.trim_end().ends_with("Continue"), "{args}");
        let (env, _) = started(dir.path(), b, 1).await;
        assert_eq!(env["TOKEN_ON_FD"], OTHER);
        for id in [a, b] {
            assert_eq!(d.view(id).unwrap().account.map(|x| x.id), Some(personal));
        }
        assert!(!d
            .account_needs()
            .iter()
            .any(|n| matches!(n, AccountNeed::Limit { to, .. } if to.id.is_none())));
    })
    .await;
}

#[tokio::test]
async fn after_copying_accounts_to_a_computer_never_logged_in_a_session_started_there_is_signed_in()
{
    // The whole path over the hub's routes, as Desk drives a machine's hub:
    // its engines read, the accounts posted as "Use this Mac's accounts"
    // posts them, then the new session dialog's start.
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(
            pool,
            signed_out_options(dir.path(), Arc::new(MemoryKeys::default())),
        )
        .await;
        let hub = crate::api::sessions_router(d.clone(), crate::testing::allow_all());
        // Read as the hub reads its engines (every engine's at start; just
        // Claude Code's here).
        d.read_engine_now(Engine::Claude).await;
        let (code, engines) = api(&hub, "GET", "/api/desk/engines", None).await;
        assert_eq!(code, 200, "{engines}");
        let claude = engines
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["engine"] == "claude")
            .unwrap();
        assert_eq!(claude["signIn"]["state"], "out", "{claude}");
        for (name, token) in [("Work", GOOD), ("Personal", OTHER)] {
            let (code, body) = api(
                &hub,
                "POST",
                "/api/desk/accounts",
                Some(serde_json::json!({
                    "engine": "claude", "name": name, "token": token, "makeDefault": false,
                })),
            )
            .await;
            assert_eq!(code, 201, "{body}");
        }
        let (_, listed) = api(&hub, "GET", "/api/desk/accounts", None).await;
        let work = listed["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == "Work")
            .unwrap()["id"]
            .clone();
        let cwd = dir.path().to_string_lossy().into_owned();
        // The dialog as it was (this computer's sign-in), and as it is now
        // (the first account), both signed in by Work.
        for account in [serde_json::Value::Null, work.clone()] {
            let (code, s) = api(
                &hub,
                "POST",
                "/api/desk/sessions",
                Some(serde_json::json!({
                    "engine": "claude", "name": "auth-fix", "cwd": cwd, "account": account,
                })),
            )
            .await;
            assert!(code == 200 || code == 201, "{code} {s}");
            assert_eq!(s["account"]["id"], work, "{s}");
            let id: Uuid = s["id"].as_str().unwrap().parse().unwrap();
            let (env, _) = started(dir.path(), id, 0).await;
            assert_eq!(env["TOKEN_ON_FD"], GOOD);
        }
    })
    .await;
}

#[tokio::test]
async fn a_login_since_the_last_read_is_honoured_at_once() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(
            pool,
            signed_out_options(dir.path(), Arc::new(MemoryKeys::default())),
        )
        .await;
        let work = paste(&d, "Work", GOOD, false).await;
        paste(&d, "Personal", OTHER, false).await;
        let s = d.create(session(dir.path(), None), None).await.unwrap();
        assert_eq!(s.account.map(|a| a.id), Some(work));
        assert!(matches!(
            d.sign_ins().of(Engine::Claude),
            crate::engine::SignIn::Out
        ));

        // The person logs Claude Code in: the next session is on it, not
        // an hour later.
        std::fs::remove_file(dir.path().join("signed-out")).unwrap();
        let own = d.create(session(dir.path(), None), None).await.unwrap();
        assert_eq!(own.account, None);
        let (env, _) = started(dir.path(), own.id, 0).await;
        assert_eq!(env["TOKEN_ON_FD"], "");
        assert_eq!(env["ANTHROPIC_API_KEY"], "sk-person-key");
    })
    .await;
}

/// Desk restarted on a computer that says `said` of Claude Code's own
/// sign-in, with three sessions that ran on it to restore, and Work and
/// Personal kept from before.
async fn restored(
    pool: workforce_postgres::PostgresPool,
    dir: &Path,
    said: &str,
) -> (Desk, Vec<Uuid>, Uuid) {
    DeskStore::migrate(&pool).await.unwrap();
    let store = DeskStore::from_pool(pool.clone());
    let keys = Arc::new(MemoryKeys::default());
    let mut work = None;
    for (name, token) in [("Work", GOOD), ("Personal", OTHER)] {
        let a = crate::engine_accounts::Account {
            id: Uuid::new_v4(),
            engine: Engine::Claude,
            name: name.into(),
            is_default: false,
            created_at: chrono::Utc::now(),
        };
        store.put_account(&a).await.unwrap();
        keys.set(
            a.id,
            &crate::engine_accounts::AccountToken::new(token).unwrap(),
        )
        .unwrap();
        work.get_or_insert(a.id);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mut ids = Vec::new();
    for n in 0..3 {
        let mut r = crate::store::tests::record_fixture(&format!("ran-{n}"));
        r.cwd = dir.to_string_lossy().into_owned();
        store.put(&r).await.unwrap();
        ids.push(r.id);
    }
    let mut o = signed_out_options(dir, keys);
    std::fs::write(dir.join("signed-out"), said).unwrap();
    o.restore = super::RestorePolicy::Auto;
    let d = Desk::start(store, o, Default::default(), CancellationToken::new())
        .await
        .unwrap();
    (d, ids, work.unwrap())
}

#[tokio::test]
async fn sessions_restored_on_a_computer_never_logged_in_ask_the_cli_once_and_start_on_the_first_account(
) {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, ids, work) = restored(pool, dir.path(), "out").await;
        for &id in &ids {
            let (env, _) = started(dir.path(), id, 0).await;
            assert_eq!(env["TOKEN_ON_FD"], GOOD);
            assert_eq!(d.view(id).unwrap().account.map(|a| a.id), Some(work));
        }
        assert_eq!(
            own_reads(dir.path()),
            1,
            "one look for all three, and the hub's own"
        );
    })
    .await;
}

#[tokio::test]
async fn a_sign_in_the_cli_says_nothing_readable_of_keeps_sessions_on_it_and_isnt_asked_again() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, ids, _) = restored(pool, dir.path(), "garbled").await;
        for &id in &ids {
            let (env, _) = started(dir.path(), id, 0).await;
            assert_eq!(env["TOKEN_ON_FD"], "", "no account's token");
            assert!(!env.contains_key(crate::token_pipe::FD_ENV));
            assert_eq!(d.view(id).unwrap().account, None);
        }
        assert!(matches!(
            d.sign_ins().of(Engine::Claude),
            crate::engine::SignIn::Unknown
        ));
        assert_eq!(own_reads(dir.path()), 1);
        // A new session isn't another look either.
        let s = d.create(session(dir.path(), None), None).await.unwrap();
        assert_eq!(s.account, None);
        let (env, _) = started(dir.path(), s.id, 0).await;
        assert_eq!(env["TOKEN_ON_FD"], "");
        assert_eq!(own_reads(dir.path()), 1);
    })
    .await;
}

/// A computer whose Claude Code is signed in to a claude.ai account Claude
/// Code names (kim@example.com), with the hub's plan readings kept in
/// `plans`.
async fn signed_in_as_gmail(
    pool: workforce_postgres::PostgresPool,
    dir: &Path,
    plans: Arc<Plans>,
) -> Desk {
    DeskStore::migrate(&pool).await.unwrap();
    let o = signed_out_options(dir, Arc::new(MemoryKeys::default()));
    std::fs::write(dir.join("signed-out"), "gmail").unwrap();
    let d = Desk::start(
        DeskStore::from_pool(pool),
        o,
        super::Floor {
            plans: Some(plans),
            ..Default::default()
        },
        CancellationToken::new(),
    )
    .await
    .unwrap();
    d.read_engine_now(Engine::Claude).await;
    d
}

fn gmail() -> crate::identity::Identity {
    crate::identity::Identity::of("org-gmail", "kim@example.com").unwrap()
}

/// Both windows of a plan, as a session reports them: `five` and `week`
/// used, resetting at `resets` (seconds from now, to the second).
fn both(five: f64, week: f64, resets: (i64, i64)) -> serde_json::Value {
    let at = |s: i64| {
        let now = chrono::Utc::now().timestamp();
        chrono::DateTime::from_timestamp(now - now % 60 + s, 0)
            .unwrap()
            .to_rfc3339()
    };
    serde_json::json!([
        { "used": five, "resets": at(resets.0), "minutes": 300 },
        { "used": week, "resets": at(resets.1), "minutes": 10_080 },
    ])
}

#[tokio::test]
async fn a_login_inside_a_session_files_what_it_reports_as_the_sign_in_it_runs_on_now() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let plans = Arc::new(Plans::default());
        let d = signed_in_as_gmail(pool, dir.path(), plans.clone()).await;
        let (work, _, a, _) = two_on_work(&d, dir.path()).await;
        d.report_usage(both(10.0, 20.0, (3600, 86_400)), Some(a));
        assert_eq!(
            plans.0.lock().last().unwrap().ids,
            vec![format!("desk-account:{work}")]
        );
        // Signed in again inside it: what it reports is this computer's
        // sign-in's, never Work's.
        d.apply_hook(a, crate::claude::Hook::SignedIn, None).await;
        d.report_usage(both(50.0, 60.0, (7200, 172_800)), Some(a));
        assert_eq!(plans.0.lock().last().unwrap().ids, vec![gmail().plan_id()]);
        // And its limit is the sign-in's: the record still says Work for
        // its next start.
        stops_on_its_limit(&d, a);
        {
            let st = d.0.state.lock();
            assert!(st
                .own_standing
                .get(&gmail().key)
                .is_some_and(|s| s.hit_at.is_some()));
            assert!(st
                .standing
                .get(&Some(work))
                .is_none_or(|s| s.hit_at.is_none()));
        }
        assert_eq!(d.view(a).unwrap().account.map(|x| x.id), Some(work));
    })
    .await;
}

#[tokio::test]
async fn this_computers_sign_in_and_the_account_it_is_are_one_plan() {
    use crate::engine_accounts::AccountNeed;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let plans = Arc::new(Plans::default());
        let d = signed_in_as_gmail(pool, dir.path(), plans.clone()).await;
        let (work, personal, a, _) = two_on_work(&d, dir.path()).await;
        let own = d
            .create(session(dir.path(), Some(None)), None)
            .await
            .unwrap();
        started(dir.path(), own.id, 0).await;
        d.change(own.id, |r, _| r.state = SessionState::Idle);
        assert_eq!(d.view(own.id).unwrap().account, None);

        // The person says this computer is signed in to Work.
        d.link_own_sign_in(Some(work)).await.unwrap();
        let view = d.accounts().into_iter().find(|v| v.id == work).unwrap();
        let identity = view.identity.clone().unwrap();
        assert!(identity.this_computer);
        assert_eq!(identity.email.as_deref(), Some("kim@example.com"));
        assert_eq!(identity.linked_by, crate::identity::LinkedBy::Manual);
        // One card: the sign-in's readings and Work's are kept by one key.
        d.report_usage(both(10.0, 20.0, (3600, 86_400)), Some(own.id));
        d.report_usage(both(10.0, 20.0, (3600, 86_400)), Some(a));
        let kept = plans.0.lock().clone();
        assert_eq!(kept[kept.len() - 1].ids, kept[kept.len() - 2].ids);
        assert_eq!(view.plan_key, Some(format!("key:{}", gmail().plan_id())));

        // The sign-in's session at its limit is Work's: one standing, one
        // need, no fourth plan to move to.
        stops_on_its_limit(&d, own.id);
        let need = until(|| d.account_needs().into_iter().next()).await;
        let AccountNeed::Limit {
            from, to, sessions, ..
        } = &need
        else {
            panic!("{need:?}");
        };
        assert_eq!((from.id, to.id), (Some(work), Some(personal)));
        assert_eq!(sessions.len(), 3, "both of Work's and the sign-in's");
        assert!(sessions.contains(&own.id));
        assert!(!d.0.state.lock().standing.contains_key(&None));
        assert_eq!(d.account_needs().len(), 1);
    })
    .await;
}

#[tokio::test]
async fn an_account_whose_usage_matches_the_sign_in_is_linked_until_it_doesnt_and_never_over_the_person(
) {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let plans = Arc::new(Plans::default());
        let d = signed_in_as_gmail(pool.clone(), dir.path(), plans.clone()).await;
        let (work, personal, a, b) = two_on_work(&d, dir.path()).await;
        d.switch_account(b, Some(personal), None).await.unwrap();
        let own = d
            .create(session(dir.path(), Some(None)), None)
            .await
            .unwrap();
        started(dir.path(), own.id, 0).await;
        let linked = |d: &Desk, id: Uuid| d.0.state.lock().links.get(&id).map(|l| l.by);

        // Empty plans look alike: never linked on them.
        for _ in 0..3 {
            d.report_usage(both(0.0, 0.0, (3600, 86_400)), Some(own.id));
            d.report_usage(both(0.0, 0.0, (3600, 86_400)), Some(a));
        }
        assert_eq!(linked(&d, work), None);
        // Personal resets at other moments: never it.
        d.report_usage(both(12.0, 30.0, (7200, 90_000)), Some(b));
        // Work's readings agree with the sign-in's twice.
        d.report_usage(both(12.0, 30.0, (3600, 86_400)), Some(a));
        d.report_usage(both(12.0, 30.0, (3600, 86_400)), Some(own.id));
        assert_eq!(linked(&d, work), None, "once isn't enough");
        tokio::time::sleep(Duration::from_millis(20)).await;
        d.report_usage(both(13.0, 30.0, (3600, 86_400)), Some(a));
        d.report_usage(both(13.0, 30.0, (3600, 86_400)), Some(own.id));
        assert_eq!(linked(&d, work), Some(crate::identity::LinkedBy::Usage));
        assert_eq!(linked(&d, personal), None);
        let view = d.accounts().into_iter().find(|v| v.id == work).unwrap();
        assert!(view.identity.is_some_and(|i| i.this_computer));
        // Kept across a restart.
        let mut kept = false;
        for _ in 0..200 {
            if d.0.store.account_links().await.unwrap().contains_key(&work) {
                kept = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(kept, "the link is kept");

        // A later reading that resets at another moment: not the same plan.
        d.report_usage(both(14.0, 30.0, (3600, 99_000)), Some(a));
        d.report_usage(both(14.0, 30.0, (3600, 86_400)), Some(own.id));
        assert_eq!(linked(&d, work), None);

        // What the person says holds, whatever the readings say.
        d.link_own_sign_in(Some(personal)).await.unwrap();
        d.report_usage(both(15.0, 31.0, (7200, 90_000)), Some(b));
        d.report_usage(both(15.0, 31.0, (3600, 86_400)), Some(own.id));
        assert_eq!(
            linked(&d, personal),
            Some(crate::identity::LinkedBy::Manual)
        );
        drop(d);
        let back = signed_in_as_gmail(pool, dir.path(), plans).await;
        assert_eq!(
            back.0.state.lock().links.get(&personal).map(|l| l.by),
            Some(crate::identity::LinkedBy::Manual)
        );
    })
    .await;
}

#[tokio::test]
async fn the_person_taking_a_sign_in_over_wins_whatever_write_of_it_is_still_pending() {
    use crate::identity::{Identity, Link, LinkedBy};
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = signed_in_as_gmail(pool, dir.path(), Arc::new(Plans::default())).await;
        let (work, personal, _, _) = two_on_work(&d, dir.path()).await;
        let identity = Identity {
            key: "the-sign-in".into(),
            email: Some("me@example.com".into()),
        };
        let link = |by| Link {
            identity: identity.clone(),
            by,
            at: chrono::Utc::now(),
        };
        let store = &d.0.store;
        // Work's link by usage is kept, and its removal is still pending.
        store
            .put_account_link(work, Some(&link(LinkedBy::Usage)))
            .await
            .unwrap();
        // The person takes the sign-in over: it is theirs, work's row gone.
        store
            .put_account_link(personal, Some(&link(LinkedBy::Manual)))
            .await
            .unwrap();
        // The pending removal, then a late write by usage, change nothing.
        store.put_account_link(work, None).await.unwrap();
        store
            .put_account_link(work, Some(&link(LinkedBy::Usage)))
            .await
            .unwrap();
        let kept = store.account_links().await.unwrap();
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(kept[&personal].by, LinkedBy::Manual);
        // Usage doesn't turn the person's own link on its account into its own.
        store
            .put_account_link(personal, Some(&link(LinkedBy::Usage)))
            .await
            .unwrap();
        assert_eq!(
            store.account_links().await.unwrap()[&personal].by,
            LinkedBy::Manual
        );
    })
    .await;
}

#[tokio::test]
async fn a_reading_of_another_account_never_ends_one_being_full_and_a_move_off_is_asked_back_once()
{
    use crate::engine_accounts::AccountNeed;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let plans = Arc::new(Plans::default());
        let d = signed_in_as_gmail(pool, dir.path(), plans).await;
        let (work, personal, a, b) = two_on_work(&d, dir.path()).await;
        // Work stops on its weekly limit, resetting in two days.
        let resets = chrono::Utc::now() + chrono::Duration::days(2);
        stops_on_its_limit(&d, a);
        {
            let mut st = d.0.state.lock();
            let s = st.standing.get_mut(&Some(work)).unwrap();
            s.hit_until = Some(resets);
            s.hit_window = Some(10_080);
        }
        // Moved off it, twice over: one move, one need.
        for _ in 0..2 {
            d.switch_all(Engine::Claude, Some(work), Some(personal))
                .await
                .unwrap();
        }
        assert_eq!(d.0.state.lock().moved_off.len(), 1);
        until(|| d.view(a).ok().filter(|v| v.live).map(drop)).await;
        until(|| d.view(b).ok().filter(|v| v.live).map(drop)).await;

        // Room on Personal, and on this computer's sign-in (signed in again
        // inside one of them): neither is Work's, so Work is still full and
        // nobody is asked back.
        d.report_usage(both(5.0, 10.0, (3600, 86_400)), Some(b));
        d.apply_hook(a, crate::claude::Hook::SignedIn, None).await;
        d.report_usage(both(1.0, 2.0, (7200, 90_000)), Some(a));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            d.0.state.lock().standing[&Some(work)].full(chrono::Utc::now()),
            Some(Some(resets))
        );
        assert!(
            !d.account_needs()
                .iter()
                .any(|n| matches!(n, AccountNeed::Reset { .. })),
            "{:?}",
            d.account_needs()
        );
        // Work's own reading, its weekly window begun again: asked back,
        // once.
        {
            let mut st = d.0.state.lock();
            let s = st.standing.get_mut(&Some(work)).unwrap();
            s.read_at = Some(chrono::Utc::now());
            s.windows = vec![crate::usage::LimitWindow {
                used_percent: 3.0,
                resets_at: Some(resets + chrono::Duration::days(7)),
                minutes: 10_080,
            }];
        }
        let needs = until(|| {
            let n = d.account_needs();
            n.iter()
                .any(|n| matches!(n, AccountNeed::Reset { .. }))
                .then_some(n)
        })
        .await;
        let resets_asked = needs
            .iter()
            .filter(|n| matches!(n, AccountNeed::Reset { .. }))
            .count();
        assert_eq!(resets_asked, 1, "{needs:?}");
        let mut ids: Vec<&str> = needs.iter().map(|n| n.id()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), needs.len());
    })
    .await;
}

#[tokio::test]
async fn standing_and_moves_off_outlive_a_restart() {
    use crate::engine_accounts::AccountNeed;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let o = || account_options(dir.path(), Arc::new(MemoryKeys::default()));
        let d = desk(pool.clone(), o()).await;
        let (work, personal, a, _) = two_on_work(&d, dir.path()).await;
        stops_on_its_limit(&d, a);
        until(|| d.account_needs().into_iter().next()).await;
        d.switch_all(Engine::Claude, Some(work), Some(personal))
            .await
            .unwrap();
        let kept = d.0.state.lock().standing[&Some(work)].clone();
        // Written once it changes.
        let mut written = false;
        for _ in 0..200 {
            d.keep_standing();
            let e = d.0.store.state(super::accounts::STANDING).await.unwrap();
            if e.is_some_and(|e| e.value["moves"].as_array().is_some_and(|m| m.len() == 1)) {
                written = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(written);
        drop(d);

        let back = desk(pool, o()).await;
        assert_eq!(back.0.state.lock().standing.get(&Some(work)), Some(&kept));
        assert_eq!(back.0.state.lock().moved_off.len(), 1);
        assert!(kept.full(chrono::Utc::now()).is_some());
        let _ = AccountNeed::AllFull {
            id: String::new(),
            engine: Engine::Claude,
            resets: vec![],
        };
    })
    .await;
}

#[tokio::test]
async fn a_session_in_a_terminal_of_its_own_on_a_full_plan_has_its_own_need() {
    use crate::engine_accounts::AccountNeed;
    use crate::model::{Approvals, Origin, SessionRecord, View};
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(
            pool,
            account_options(dir.path(), Arc::new(MemoryKeys::default())),
        )
        .await;
        paste(&d, "Work", GOOD, true).await;
        let now = chrono::Utc::now();
        let id = Uuid::new_v4();
        d.save(SessionRecord {
            id,
            engine: Engine::Claude,
            view: View::Terminal,
            engine_version: None,
            approve_for_me: false,
            fast: false,
            model: None,
            account: None,
            name: "api-fix".into(),
            goal: String::new(),
            cwd: dir.path().to_string_lossy().into_owned(),
            branch: None,
            origin: Origin::External,
            state: SessionState::Working,
            approvals: Approvals::Desk,
            cli_session_id: Some("ext-1".into()),
            transcript: None,
            last_prompt: Some("fix it".into()),
            last_reply: None,
            external_pid: Some(1),
            restore_offer: false,
            terminal_id: None,
            display_name: None,
            asked: None,
            folder_chosen: Default::default(),
            created_at: now,
            updated_at: now,
        });
        assert!(d.account_needs().is_empty());
        // Its transcript ends on the plan's limit: its turn is over on it.
        d.0.state.lock().turn_failures.insert(
            id,
            crate::limit::TurnFailure::Limit(crate::limit::LimitHit {
                window: Some("five_hour".into()),
                resets_at: Some(now + chrono::Duration::hours(2)),
                at: None,
            }),
        );
        d.change(id, |r, _| {
            r.state = SessionState::Idle;
            r.last_reply = Some("You've hit your session limit · resets 9am".into());
        });
        let need = until(|| {
            d.account_needs()
                .into_iter()
                .find(|n| matches!(n, AccountNeed::InTerminal { .. }))
        })
        .await;
        let AccountNeed::InTerminal {
            session,
            name,
            account,
            resets_at,
            ..
        } = need
        else {
            unreachable!()
        };
        assert_eq!((session, name.as_str(), account.id), (id, "api-fix", None));
        assert!(resets_at.is_some());
        // Closed there: no longer anyone's need.
        d.change(id, |r, _| r.state = SessionState::Closed);
        assert!(!d
            .account_needs()
            .iter()
            .any(|n| matches!(n, AccountNeed::InTerminal { .. })));
    })
    .await;
}

#[tokio::test]
async fn which_account_this_computer_is_signed_in_to_is_asked_before_any_switch_depends_on_it() {
    use crate::engine_accounts::AccountNeed;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = signed_in_as_gmail(pool, dir.path(), Arc::new(Plans::default())).await;
        let (work, personal, _, _) = two_on_work(&d, dir.path()).await;
        d.set_auto_switch(true).await.unwrap();
        let own = d.create(session(dir.path(), Some(None)), None).await.unwrap();
        started(dir.path(), own.id, 0).await;
        d.change(own.id, |r, _| r.state = SessionState::Idle);
        // The sign-in's session at its limit, the sign-in unlinked: asked,
        // and nothing moves on a guess.
        stops_on_its_limit(&d, own.id);
        let need = until(|| d.account_needs().into_iter().next()).await;
        assert!(
            matches!(&need, AccountNeed::WhichSignIn { email: Some(e), .. } if e == "kim@example.com"),
            "{need:?}"
        );
        assert!(!d.account_needs().iter().any(|n| matches!(n, AccountNeed::Limit { from, .. } if from.id.is_none())));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(d.view(own.id).unwrap().account, None);
        // Answered: it is Personal. Its limit is Personal's now, and Desk
        // moves everything on it to Work.
        d.link_own_sign_in(Some(personal)).await.unwrap();
        // What the sign-in reported is Personal's now, with nothing copied.
        assert!(Desk::standing_of(&d.0.state.lock(), Some(personal))
            .full(chrono::Utc::now())
            .is_some());
        d.accounts_moved().await;
        assert!(!d
            .account_needs()
            .iter()
            .any(|n| matches!(n, AccountNeed::WhichSignIn { .. })));
        let (env, _) = started(dir.path(), own.id, 1).await;
        assert_eq!(env["TOKEN_ON_FD"], GOOD);
        assert_eq!(d.view(own.id).unwrap().account.map(|x| x.id), Some(work));
    })
    .await;
}

/// What Desk passes a machine's hub of this Mac's accounts, read from the
/// Mac's view of them as Desk reads it.
fn standing_of(d: &Desk) -> Vec<crate::engine_accounts::StandingElsewhere> {
    let wire = serde_json::to_value(d.accounts()).unwrap();
    wire.as_array()
        .unwrap()
        .iter()
        .filter(|a| !a["standing"].is_null())
        .map(|a| {
            serde_json::from_value(serde_json::json!({
                "engine": a["engine"],
                "name": a["name"],
                "standing": a["standing"],
            }))
            .unwrap()
        })
        .collect()
}

#[tokio::test]
async fn a_machine_told_an_account_is_full_starts_new_sessions_on_the_next_and_says_when_all_are() {
    with_test_pool(build_pool, |mac_pool| async move {
        with_test_pool(build_pool, |machine_pool| async move {
            let mac_dir = tempfile::tempdir().unwrap();
            let machine_dir = tempfile::tempdir().unwrap();
            let mac = desk(
                mac_pool,
                account_options(mac_dir.path(), Arc::new(MemoryKeys::default())),
            )
            .await;
            // A machine never logged in to Claude Code, with this Mac's two
            // accounts copied to it.
            let machine = desk(
                machine_pool,
                signed_out_options(machine_dir.path(), Arc::new(MemoryKeys::default())),
            )
            .await;
            let (_, _, a, _) = two_on_work(&mac, mac_dir.path()).await;
            paste(&machine, "Work", GOOD, true).await;
            let personal = paste(&machine, "Personal", OTHER, false).await;

            // Work is full on the Mac; the machine is told.
            stops_on_its_limit(&mac, a);
            until(|| {
                mac.0
                    .state
                    .lock()
                    .standing
                    .values()
                    .any(|s| s.hit_at.is_some())
                    .then_some(())
            })
            .await;
            machine
                .learn_standing(standing_of(&mac), Some(chrono::Utc::now()))
                .await;
            let s = machine
                .create(session(machine_dir.path(), None), None)
                .await
                .unwrap();
            assert_eq!(s.account.map(|x| x.id), Some(personal));
            let (env, _) = started(machine_dir.path(), s.id, 0).await;
            assert_eq!(env["TOKEN_ON_FD"], OTHER);
            // Told again: nothing new, nothing changes.
            assert_eq!(
                machine
                    .learn_standing(standing_of(&mac), Some(chrono::Utc::now()))
                    .await,
                0
            );

            // Both full: said plainly, and nothing starts.
            {
                let mut st = machine.0.state.lock();
                let work = st
                    .standing
                    .iter()
                    .find(|(_, s)| s.hit_at.is_some())
                    .unwrap()
                    .1
                    .clone();
                st.standing.insert(Some(personal), work);
            }
            let refused = machine
                .create(session(machine_dir.path(), None), None)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                refused.contains("every Claude Code account here is at its plan limit")
                    && refused.contains("Work")
                    && refused.contains("Personal"),
                "{refused}"
            );
        })
        .await;
    })
    .await;
}

/// How many times session `id` was started.
fn starts(dir: &Path, id: Uuid) -> usize {
    let hook = format!("/hook/{id}/");
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("env-"))
        .filter(|e| std::fs::read_to_string(e.path()).is_ok_and(|env| env.contains(&hook)))
        .count()
}

#[tokio::test]
async fn a_switch_waiting_on_a_rate_limit_waits_for_its_transcript_and_carries_on_at_the_limit() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(
            pool,
            key_options(dir.path(), Arc::new(MemoryKeys::default())),
        )
        .await;
        let (_, personal, a, _) = two_on_work(&d, dir.path()).await;
        d.apply_hook(a, crate::claude::Hook::Prompt("go".into()), None)
            .await;
        d.switch_account(a, Some(personal), None).await.unwrap();
        let transcript = dir.path().join("a.jsonl");
        std::fs::write(&transcript, "").unwrap();
        // Words that don't say the plan's: not moved on them alone.
        d.apply_hook(
            a,
            stop_failure(
                &transcript,
                "API Error: Request rejected (429) · rate limited",
            ),
            None,
        )
        .await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            starts(dir.path(), a),
            1,
            "it waits for the transcript's word"
        );
        // The transcript says the plan's limit: moved, and carries on.
        std::fs::write(&transcript, error_line(true)).unwrap();
        let (env, args) = started(dir.path(), a, 1).await;
        assert_eq!(env["TOKEN_ON_FD"], OTHER);
        assert!(args.trim_end().ends_with("Continue"), "{args}");
    })
    .await;
}

#[tokio::test]
async fn a_refusal_read_as_not_the_plans_undoes_only_its_own_sessions_stop() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(
            pool,
            account_options(dir.path(), Arc::new(MemoryKeys::default())),
        )
        .await;
        let (work, _, a, b) = two_on_work(&d, dir.path()).await;
        let full = |d: &Desk| {
            d.0.state
                .lock()
                .standing
                .get(&Some(work))
                .is_some_and(|s| s.full(chrono::Utc::now()).is_some())
        };
        // B truly stops on Work's limit.
        stops_on_its_limit(&d, b);
        until(|| full(&d).then_some(())).await;
        let genuine = d.0.state.lock().standing[&Some(work)].hit_at;
        tokio::time::sleep(Duration::from_millis(5)).await;
        // A's words read as the limit; its transcript then says otherwise.
        let transcript = dir.path().join("a.jsonl");
        std::fs::write(&transcript, "").unwrap();
        d.apply_hook(a, crate::claude::Hook::Prompt("go".into()), None)
            .await;
        d.apply_hook(a, stop_failure(&transcript, "You've hit your limit"), None)
            .await;
        assert_ne!(d.0.state.lock().standing[&Some(work)].hit_at, genuine);
        std::fs::write(&transcript, error_line(false)).unwrap();
        until(|| {
            matches!(
                d.view(a).ok()?.failure,
                Some(crate::limit::TurnFailure::Failed(_))
            )
            .then_some(())
        })
        .await;
        // B's stop stands: Work is still full, as B left it.
        assert!(full(&d));
        assert_eq!(d.0.state.lock().standing[&Some(work)].hit_at, genuine);
    })
    .await;
}

/// This computer's Claude Code signs in as `who` now (`gmail`, `star`).
async fn cli_signs_in_as(d: &Desk, dir: &Path, who: &str) {
    std::fs::write(dir.join("signed-out"), who).unwrap();
    d.read_engine_now(Engine::Claude).await;
}

fn star() -> crate::identity::Identity {
    crate::identity::Identity::of("org-star", "kim@star.example").unwrap()
}

#[tokio::test]
async fn another_sign_ins_reading_never_stands_for_this_ones_and_linking_loses_nothing() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = signed_in_as_gmail(pool, dir.path(), Arc::new(Plans::default())).await;
        let (work, _, _, _) = two_on_work(&d, dir.path()).await;
        let own = d
            .create(session(dir.path(), Some(None)), None)
            .await
            .unwrap();
        started(dir.path(), own.id, 0).await;
        let full = |d: &Desk, side: Option<Uuid>| {
            Desk::standing_of(&d.0.state.lock(), side)
                .full(chrono::Utc::now())
                .is_some()
        };
        // Gmail, the CLI's sign-in, stops on its limit.
        stops_on_its_limit(&d, own.id);
        assert!(full(&d, None));
        // The CLI signs in to another account: Gmail's stop isn't its.
        cli_signs_in_as(&d, dir.path(), "star").await;
        assert_eq!(Desk::own_key(&d.0.state.lock()), star().key);
        assert!(!full(&d, None), "another sign-in's limit");
        // Nothing but which account the new sign-in is.
        assert!(
            d.account_needs()
                .iter()
                .all(|n| matches!(n, crate::engine_accounts::AccountNeed::WhichSignIn { .. })),
            "{:?}",
            d.account_needs()
        );
        // Back on Gmail: full again, as it was.
        cli_signs_in_as(&d, dir.path(), "gmail").await;
        assert!(full(&d, None));
        // Linked to Work: Work reads full, from what the sign-in said; the
        // sign-in is no side of its own.
        d.link_own_sign_in(Some(work)).await.unwrap();
        assert!(full(&d, Some(work)));
        assert!(!Desk::effective_standing(&d.0.state.lock()).contains_key(&None));
        // Unlinked ("none of these"): Work is as it was alone, and the
        // sign-in keeps its own stop.
        d.link_own_sign_in(None).await.unwrap();
        assert!(!full(&d, Some(work)));
        assert!(full(&d, None));
    })
    .await;
}

#[tokio::test]
async fn none_of_these_holds_across_a_restart_and_usage_never_overrules_it() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let plans = Arc::new(Plans::default());
        let d = signed_in_as_gmail(pool.clone(), dir.path(), plans.clone()).await;
        let (work, _, a, _) = two_on_work(&d, dir.path()).await;
        let own = d
            .create(session(dir.path(), Some(None)), None)
            .await
            .unwrap();
        started(dir.path(), own.id, 0).await;
        d.link_own_sign_in(None).await.unwrap();
        let mut kept = false;
        for _ in 0..200 {
            let e = d.0.store.state(super::identities::OWN_NONE).await.unwrap();
            if e.is_some_and(|e| e.value.as_array().is_some_and(|v| !v.is_empty())) {
                kept = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(kept);
        drop(d);
        let d = signed_in_as_gmail(pool, dir.path(), plans).await;
        assert!(d.0.state.lock().own_none.contains(&gmail().key));
        // Readings that agree, again and again: still not linked.
        for (n, used) in [(1, 12.5), (2, 13.5), (3, 14.5)] {
            d.report_usage(both(used, 30.0, (3600, 86_400)), Some(a));
            d.report_usage(both(used, 30.0, (3600, 86_400)), Some(own.id));
            let _ = n;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(!d.0.state.lock().links.contains_key(&work));
    })
    .await;
}

#[tokio::test]
async fn a_reading_before_the_sign_in_is_read_waits_and_is_filed_as_whose_it_is() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let plans = Arc::new(Plans::default());
        let o = signed_out_options(dir.path(), Arc::new(MemoryKeys::default()));
        std::fs::write(dir.path().join("signed-out"), "gmail").unwrap();
        let d = Desk::start(
            DeskStore::from_pool(pool),
            o,
            super::Floor {
                plans: Some(plans.clone()),
                ..Default::default()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        // Read before Desk asked Claude Code who it is: held.
        d.0.state.lock().own_read = false;
        d.report_usage(both(10.0, 20.0, (3600, 86_400)), None);
        assert!(plans.0.lock().is_empty());
        d.read_engine_now(Engine::Claude).await;
        d.own_identity_read(Some(Some(gmail())));
        assert_eq!(plans.0.lock().last().unwrap().ids, vec![gmail().plan_id()]);
    })
    .await;
}

#[tokio::test]
async fn a_sign_in_inside_a_session_holds_across_a_restart_until_it_starts_afresh() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let plans = Arc::new(Plans::default());
        let d = signed_in_as_gmail(pool.clone(), dir.path(), plans.clone()).await;
        let (_, _, a, _) = two_on_work(&d, dir.path()).await;
        d.apply_hook(a, crate::claude::Hook::SignedIn, None).await;
        let mut kept = false;
        for _ in 0..200 {
            let e = d.0.store.state(super::identities::RELOGGED).await.unwrap();
            if e.is_some_and(|e| e.value.as_array().is_some_and(|v| v.len() == 1)) {
                kept = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(kept, "kept for the hub's next start");
        // Started afresh (on its account again): no longer.
        d.started_afresh(a);
        assert!(!d.0.state.lock().relogged.contains(&a));
    })
    .await;
}

#[tokio::test]
async fn only_the_current_keys_name_this_computers_cards() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = signed_in_as_gmail(pool, dir.path(), Arc::new(Plans::default())).await;
        let (work, personal, _, _) = two_on_work(&d, dir.path()).await;
        let keys = d.current_plan_keys().unwrap();
        assert!(keys.contains(&format!("key:desk-account:{work}")));
        assert!(keys.contains(&format!("key:{}", gmail().plan_id())));
        assert_eq!(d.own_plan_key(), Some(format!("key:{}", gmail().plan_id())));
        d.remove_account(personal).await.unwrap();
        assert!(!d
            .current_plan_keys()
            .unwrap()
            .contains(&format!("key:desk-account:{personal}")));
    })
    .await;
}

#[tokio::test]
async fn a_machine_takes_standing_by_identity_else_name_and_only_times_that_can_be_right() {
    use crate::engine_accounts::{Standing, StandingElsewhere};
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let machine = desk(
            pool,
            account_options(dir.path(), Arc::new(MemoryKeys::default())),
        )
        .await;
        let work = paste(&machine, "Work", GOOD, true).await;
        let personal = paste(&machine, "Personal", OTHER, false).await;
        let now = chrono::Utc::now();
        let full = |hit_at| Standing {
            hit_at: Some(hit_at),
            hit_until: Some(hit_at + chrono::Duration::days(30)),
            ..Default::default()
        };
        let told = |name: &str, identity: Option<&str>, standing: Standing| StandingElsewhere {
            engine: Engine::Claude,
            name: name.into(),
            identity: identity.map(str::to_string),
            standing,
        };
        // The Mac knows Work's identity, the machine doesn't: by name, its
        // reset bounded to a week, and the sender's clock (an hour behind)
        // read as this one.
        let sent = now - chrono::Duration::hours(1);
        assert_eq!(
            machine
                .learn_standing(vec![told(" work ", Some("k-work"), full(sent))], Some(sent))
                .await,
            1
        );
        let kept = machine.0.state.lock().standing[&Some(work)].clone();
        let hit = kept.hit_at.unwrap();
        assert!((hit - now).num_seconds().abs() < 5, "read on this clock");
        assert_eq!(kept.hit_until, Some(hit + chrono::Duration::days(7)));
        // Known there as another identity: never onto it.
        machine.0.state.lock().links.insert(
            personal,
            crate::identity::Link {
                identity: crate::identity::Identity {
                    key: "k-other".into(),
                    email: None,
                },
                by: crate::identity::LinkedBy::Manual,
                at: now,
            },
        );
        assert_eq!(
            machine
                .learn_standing(vec![told("Personal", Some("k-work"), full(now))], Some(now))
                .await,
            0
        );
        // From the future: refused.
        assert_eq!(
            machine
                .learn_standing(
                    vec![told(
                        "Personal",
                        Some("k-other"),
                        full(now + chrono::Duration::hours(1))
                    )],
                    Some(now)
                )
                .await,
            0
        );
        assert!(machine
            .0
            .state
            .lock()
            .standing
            .get(&Some(personal))
            .is_none_or(|s| s.hit_at.is_none()));
    })
    .await;
}

#[tokio::test]
async fn none_of_these_is_never_asked_again_and_the_sign_in_is_a_plan_of_its_own() {
    use crate::engine_accounts::AccountNeed;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = signed_in_as_gmail(pool, dir.path(), Arc::new(Plans::default())).await;
        let (work, personal, _, _) = two_on_work(&d, dir.path()).await;
        let own = d.create(session(dir.path(), Some(None)), None).await.unwrap();
        started(dir.path(), own.id, 0).await;
        d.change(own.id, |r, _| r.state = SessionState::Idle);
        d.link_own_sign_in(None).await.unwrap();
        stops_on_its_limit(&d, own.id);
        let need = until(|| d.account_needs().into_iter().next()).await;
        assert!(
            matches!(&need, AccountNeed::Limit { from, to, .. } if from.id.is_none() && to.id == Some(work)),
            "{need:?}"
        );
        assert!(!d
            .account_needs()
            .iter()
            .any(|n| matches!(n, AccountNeed::WhichSignIn { .. })));
        // Both accounts full, the sign-in with room again: a new session
        // naming none starts on it.
        let later = chrono::Utc::now() + chrono::Duration::days(1);
        {
            let mut st = d.0.state.lock();
            for id in [work, personal] {
                let s = st.standing.entry(Some(id)).or_default();
                s.hit_at = Some(chrono::Utc::now());
                s.hit_until = Some(later);
            }
            st.own_standing.clear();
        }
        let s = d.create(session(dir.path(), None), None).await.unwrap();
        assert_eq!(s.account, None, "on this computer's own sign-in, another plan with room");
    })
    .await;
}

#[tokio::test]
async fn a_sign_in_claude_code_says_nothing_of_still_files_its_readings_with_their_time() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let plans = Arc::new(Plans::default());
        let o = signed_out_options(dir.path(), Arc::new(MemoryKeys::default()));
        std::fs::write(dir.path().join("signed-out"), "garbled").unwrap();
        let d = Desk::start(
            DeskStore::from_pool(pool),
            o,
            super::Floor {
                plans: Some(plans.clone()),
                ..Default::default()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        d.0.state.lock().own_read = false;
        for _ in 0..(super::OWN_WAITING_MAX + 10) {
            d.report_usage(both(10.0, 20.0, (3600, 86_400)), None);
        }
        assert_eq!(
            d.0.state.lock().own_waiting.len(),
            super::OWN_WAITING_MAX,
            "held, but never without end"
        );
        let first = d.0.state.lock().own_waiting[0].2;
        tokio::time::sleep(Duration::from_millis(20)).await;
        // Claude Code says nothing readable: filed all the same, under no
        // identity, at the time each came.
        d.read_engine_now(Engine::Claude).await;
        assert!(d.0.state.lock().own_read);
        assert!(d.0.state.lock().own_waiting.is_empty());
        assert_eq!(plans.0.lock().len(), super::OWN_WAITING_MAX);
        assert_eq!(
            d.0.state.lock().own_standing[""]
                .read_at
                .map(|t| t >= first),
            Some(true)
        );
        assert!(
            d.0.state.lock().own_standing[""].read_at
                < Some(chrono::Utc::now() - chrono::Duration::milliseconds(15))
        );
    })
    .await;
}

#[tokio::test]
async fn the_hubs_own_calls_are_offered_the_accounts_default_first_and_tell_desk_of_a_limit() {
    use crate::engine_accounts::AccountNeed;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(
            pool,
            key_options(dir.path(), Arc::new(MemoryKeys::default())),
        )
        .await;
        let (work, personal, _, _) = two_on_work(&d, dir.path()).await;
        let offered = d.pool_accounts();
        assert_eq!(
            offered,
            vec![(work, None, None), (personal, None, None)],
            "the default first, no token"
        );
        assert_eq!(
            d.pool_token(personal).map(|t| t.expose().to_string()),
            Some(OTHER.to_string())
        );
        assert_eq!(d.pool_token(Uuid::new_v4()), None);
        // A call of the hub's own refused on Work: full for Desk too, and
        // offered as such from then on.
        let until = chrono::Utc::now() + chrono::Duration::days(2);
        d.hub_cooling(work, until, chrono::Utc::now());
        let need = until_need(&d).await;
        assert!(
            matches!(&need, AccountNeed::Limit { from, .. } if from.id == Some(work)),
            "{need:?}"
        );
        assert_eq!(d.pool_accounts()[0].1, Some(until));
        // Told again, nothing new.
        d.hub_cooling(work, until - chrono::Duration::hours(1), chrono::Utc::now());
        assert_eq!(
            d.0.state.lock().standing[&Some(work)].hit_until,
            Some(until)
        );
    })
    .await;
}

/// A refusal Claude Code wrote at `at`, on the weekly window resetting at
/// `resets`.
fn weekly_refusal(
    at: chrono::DateTime<chrono::Utc>,
    resets: chrono::DateTime<chrono::Utc>,
) -> crate::limit::LimitHit {
    crate::limit::LimitHit {
        window: Some("seven_day".into()),
        resets_at: Some(resets),
        at: Some(at),
    }
}

#[tokio::test]
async fn signing_in_again_ends_an_accounts_stop_here_and_in_the_hubs_pool() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let keys = Arc::new(MemoryKeys::default());
        let d = desk(pool, key_options(dir.path(), keys.clone())).await;
        let (work, _, a, _) = two_on_work(&d, dir.path()).await;
        let now = chrono::Utc::now();
        let week = now + chrono::Duration::days(3);
        // Refused this morning, by a session and by the hub's own call.
        let morning = now - chrono::Duration::hours(3);
        {
            let mut st = d.0.state.lock();
            let raw = Desk::raw_running(&st, a);
            Desk::mark_session_hit(&mut st, a, &raw, Some(&weekly_refusal(morning, week)));
        }
        d.hub_cooling(work, week, morning + chrono::Duration::minutes(1));
        assert_eq!(work_standing(&d, work).full(now), Some(Some(week)));
        assert_eq!(d.pool_accounts()[0], (work, Some(week), None));
        // Signed in again: the stop is over, and the hub's pool is told
        // since when, so its own cooldown from before ends too.
        let token = crate::engine_accounts::AccountToken::new(GOOD).unwrap();
        d.renew_account(keys.clone(), work, &token).await.unwrap();
        let after = chrono::Utc::now();
        assert_eq!(work_standing(&d, work).full(after), None);
        let (id, full, cleared) = d.pool_accounts()[0];
        assert_eq!((id, full), (work, None));
        assert!(
            cleared.is_some_and(|c| c >= now && c <= after),
            "{cleared:?}"
        );
        // What was heard before it, told again (the hub still cooling, the
        // transcript's line read again), locks nothing.
        d.hub_cooling(work, week, morning + chrono::Duration::minutes(1));
        {
            let mut st = d.0.state.lock();
            let raw = Desk::raw_running(&st, a);
            Desk::mark_session_hit(&mut st, a, &raw, Some(&weekly_refusal(morning, week)));
        }
        assert_eq!(work_standing(&d, work).full(chrono::Utc::now()), None);
        // A refusal since does.
        std::thread::sleep(std::time::Duration::from_millis(5));
        d.hub_cooling(work, week, chrono::Utc::now());
        assert_eq!(
            work_standing(&d, work).full(chrono::Utc::now()),
            Some(Some(week))
        );
    })
    .await;
}

#[tokio::test]
async fn a_call_that_succeeds_on_an_account_ends_its_stop() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(
            pool,
            key_options(dir.path(), Arc::new(MemoryKeys::default())),
        )
        .await;
        let (work, _, a, _) = two_on_work(&d, dir.path()).await;
        let now = chrono::Utc::now();
        let week = now + chrono::Duration::days(3);
        d.hub_cooling(work, week, now - chrono::Duration::minutes(5));
        assert!(work_standing(&d, work).full(now).is_some());
        // The hub's own call on it succeeded.
        d.hub_served(work, chrono::Utc::now());
        assert_eq!(work_standing(&d, work).full(chrono::Utc::now()), None);
        assert!(d.pool_accounts()[0].2.is_some(), "the pool is told");
        // Refused again, then a session's turn on it ends well.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let refused = chrono::Utc::now();
        d.hub_cooling(work, week, refused);
        assert!(work_standing(&d, work).full(chrono::Utc::now()).is_some());
        std::thread::sleep(std::time::Duration::from_millis(5));
        {
            let mut st = d.0.state.lock();
            let raw = Desk::raw_running(&st, a);
            // A reply written before that refusal, read late, ends nothing:
            // the refusal is newer.
            let earlier = refused - chrono::Duration::milliseconds(1);
            assert_eq!(Desk::mark_ok(&mut st, &raw, earlier), Some(false));
            assert_eq!(
                Desk::mark_ok(&mut st, &raw, chrono::Utc::now()),
                Some(true),
                "it ended a stop"
            );
        }
        assert_eq!(work_standing(&d, work).full(chrono::Utc::now()), None);
        // A transcript's refusal from before that success, read late, is over.
        {
            let mut st = d.0.state.lock();
            let raw = Desk::raw_running(&st, a);
            let before = now - chrono::Duration::minutes(1);
            Desk::mark_session_hit(&mut st, a, &raw, Some(&weekly_refusal(before, week)));
        }
        assert_eq!(work_standing(&d, work).full(chrono::Utc::now()), None);
    })
    .await;
}

async fn until_need(d: &Desk) -> crate::engine_accounts::AccountNeed {
    until(|| d.account_needs().into_iter().next()).await
}

/// A weekly window only, as Claude Code reports a plan now: `used`,
/// resetting at `resets`.
fn weekly(used: f64, resets: chrono::DateTime<chrono::Utc>) -> serde_json::Value {
    serde_json::json!([{ "used": used, "resets": resets.to_rfc3339(), "minutes": 10_080 }])
}

/// `secs` from now, to the second.
fn in_secs(secs: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(chrono::Utc::now().timestamp() + secs, 0).unwrap()
}

/// Work's session `a` (the only one on Work: `b` is on Personal), its
/// process started an hour ago and taken back since.
async fn one_on_work_started_an_hour_ago(d: &Desk, dir: &Path) -> (Uuid, Uuid, Uuid, Uuid) {
    let (work, personal, a, b) = two_on_work(d, dir).await;
    d.change(b, |r, _| r.account = Some(personal));
    d.0.state.lock().live.get_mut(&a).unwrap().started_at =
        chrono::Utc::now() - chrono::Duration::hours(1);
    (work, personal, a, b)
}

/// A `/login` in a session's whole transcript at `first`, and the weekly
/// limit it stopped on a minute before, resetting at `resets`.
fn login_at(
    first: chrono::DateTime<chrono::Utc>,
    resets: Option<chrono::DateTime<chrono::Utc>>,
) -> crate::transcript::SignedInAgain {
    crate::transcript::SignedInAgain {
        first_at: first,
        last_id: format!("login-{}", first.timestamp()),
        last_at: first,
        hit_before: resets.map(|r| {
            (
                first - chrono::Duration::minutes(1),
                crate::limit::LimitHit {
                    window: Some("seven_day".into()),
                    resets_at: Some(r),
                    at: None,
                },
            )
        }),
    }
}

fn work_standing(d: &Desk, work: Uuid) -> crate::engine_accounts::Standing {
    Desk::standing_of(&d.0.state.lock(), Some(work))
}

#[tokio::test]
async fn a_login_found_after_the_fact_takes_back_what_was_filed_as_its_account_and_keeps_its_limit()
{
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let plans = Arc::new(Plans::default());
        let d = signed_in_as_gmail(pool, dir.path(), plans.clone()).await;
        let (work, _, a, _) = one_on_work_started_an_hour_ago(&d, dir.path()).await;
        let now = chrono::Utc::now();
        let login = now - chrono::Duration::minutes(10);
        let work_resets = in_secs(4 * 86_400);
        // Signed in again inside ten minutes ago, unheard: what it reported
        // since, the sign-in's plan, was filed as Work's.
        d.report_usage(weekly(26.0, in_secs(6 * 86_400)), Some(a));
        let read_at = work_standing(&d, work).read_at.unwrap();
        assert_eq!(
            plans.0.lock().last().unwrap().ids,
            vec![format!("desk-account:{work}")]
        );
        assert!(d
            .accounts()
            .iter()
            .any(|x| x.id == work && x.sessions == vec![a]));
        // Found as the hub takes it back: Work stopped on its weekly limit
        // just before.
        d.login_scan_done(a, Some(login_at(login, Some(work_resets))))
            .await;
        assert!(
            d.0.state.lock().relogged.contains(&a),
            "it runs on the sign-in"
        );
        let s = work_standing(&d, work);
        assert!(
            s.windows.is_empty(),
            "the sign-in's reading is no longer Work's"
        );
        assert_eq!(
            s.full(now),
            Some(Some(work_resets)),
            "Work is full until it resets"
        );
        assert_eq!(
            plans.1.lock().clone(),
            vec![(
                vec![format!("desk-account:{work}")],
                Some(read_at),
                Some(login - chrono::Duration::minutes(1))
            )],
            "only the reading it filed is forgotten, its limit kept instead"
        );
        assert!(
            d.accounts()
                .iter()
                .any(|x| x.id == work && x.sessions.is_empty()),
            "not on Work any more"
        );
        // Seen once: found again, nothing more is taken back.
        d.login_scan_done(a, Some(login_at(login, None))).await;
        assert_eq!(plans.1.lock().len(), 1);
        // What it reports now is the sign-in's.
        d.report_usage(weekly(27.0, in_secs(6 * 86_400)), Some(a));
        assert_eq!(plans.0.lock().last().unwrap().ids, vec![gmail().plan_id()]);
    })
    .await;
}

#[tokio::test]
async fn a_login_from_before_its_process_started_is_not_one_inside_it() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let plans = Arc::new(Plans::default());
        let d = signed_in_as_gmail(pool, dir.path(), plans.clone()).await;
        let (work, _, a, _) = one_on_work_started_an_hour_ago(&d, dir.path()).await;
        d.report_usage(weekly(40.0, in_secs(86_400)), Some(a));
        d.login_scan_done(
            a,
            Some(login_at(
                chrono::Utc::now() - chrono::Duration::hours(2),
                None,
            )),
        )
        .await;
        assert!(!d.0.state.lock().relogged.contains(&a));
        assert_eq!(
            work_standing(&d, work).windows.len(),
            1,
            "Work's reading stands"
        );
        assert!(plans.1.lock().is_empty());
    })
    .await;
}

#[tokio::test]
async fn only_what_the_session_itself_wrote_of_its_account_is_taken_back() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let plans = Arc::new(Plans::default());
        let d = signed_in_as_gmail(pool, dir.path(), plans.clone()).await;
        let (work, _, a, b) = one_on_work_started_an_hour_ago(&d, dir.path()).await;
        let login = chrono::Utc::now() - chrono::Duration::minutes(10);
        // Another session on Work too, running: it reports Work's own plan
        // last, then the pool finds Work at its limit.
        d.change(b, |r, _| r.account = Some(work));
        d.report_usage(weekly(26.0, in_secs(6 * 86_400)), Some(a));
        d.report_usage(weekly(55.0, in_secs(2 * 86_400)), Some(b));
        let pool_until = in_secs(3600);
        d.hub_cooling(work, pool_until, chrono::Utc::now());
        d.login_scan_done(a, Some(login_at(login, None))).await;
        assert!(d.0.state.lock().relogged.contains(&a));
        let s = work_standing(&d, work);
        assert_eq!(s.windows[0].used_percent, 55.0, "b's reading stands");
        assert_eq!(s.hit_until, Some(pool_until), "the pool's stop stands");
        assert!(plans.1.lock().iter().all(|(_, exact, _)| exact.is_none()));
    })
    .await;
}

#[tokio::test]
async fn what_the_session_wrote_last_is_taken_back_though_another_runs_on_the_account() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let plans = Arc::new(Plans::default());
        let d = signed_in_as_gmail(pool, dir.path(), plans.clone()).await;
        let (work, _, a, b) = one_on_work_started_an_hour_ago(&d, dir.path()).await;
        d.change(b, |r, _| r.account = Some(work));
        d.report_usage(weekly(55.0, in_secs(2 * 86_400)), Some(b));
        d.report_usage(weekly(26.0, in_secs(6 * 86_400)), Some(a));
        let read_at = work_standing(&d, work).read_at;
        d.login_scan_done(
            a,
            Some(login_at(
                chrono::Utc::now() - chrono::Duration::minutes(10),
                None,
            )),
        )
        .await;
        assert!(work_standing(&d, work).windows.is_empty());
        assert_eq!(plans.1.lock()[0].1, read_at);
    })
    .await;
}

#[tokio::test]
async fn a_closed_sessions_reading_since_the_login_is_never_taken_back() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let plans = Arc::new(Plans::default());
        let d = signed_in_as_gmail(pool, dir.path(), plans.clone()).await;
        let (work, _, a, b) = one_on_work_started_an_hour_ago(&d, dir.path()).await;
        d.change(b, |r, _| r.account = Some(work));
        d.report_usage(weekly(55.0, in_secs(2 * 86_400)), Some(b));
        // b is gone since: what it wrote is still its own.
        d.0.state.lock().live.remove(&b);
        d.login_scan_done(
            a,
            Some(login_at(
                chrono::Utc::now() - chrono::Duration::minutes(10),
                None,
            )),
        )
        .await;
        assert_eq!(work_standing(&d, work).windows[0].used_percent, 55.0);
        assert!(plans.1.lock().is_empty());
    })
    .await;
}

#[tokio::test]
async fn a_login_to_the_same_account_takes_nothing_back() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let plans = Arc::new(Plans::default());
        let d = signed_in_as_gmail(pool, dir.path(), plans.clone()).await;
        let (work, _, a, _) = one_on_work_started_an_hour_ago(&d, dir.path()).await;
        // This computer's sign-in is Work: a /login to it again is Work still.
        d.link_own_sign_in(Some(work)).await.unwrap();
        d.report_usage(weekly(26.0, in_secs(6 * 86_400)), Some(a));
        d.login_scan_done(
            a,
            Some(login_at(
                chrono::Utc::now() - chrono::Duration::minutes(10),
                None,
            )),
        )
        .await;
        assert_eq!(work_standing(&d, work).windows.len(), 1);
        assert!(plans.1.lock().is_empty());
        assert!(
            d.accounts()
                .iter()
                .any(|x| x.id == work && x.sessions == vec![a]),
            "it counts on Work, the account its sign-in is"
        );
    })
    .await;
}

#[tokio::test]
async fn a_login_seen_in_the_tail_keeps_the_stop_its_account_had_before_as_its_reading() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let plans = Arc::new(Plans::default());
        let d = signed_in_as_gmail(pool, dir.path(), plans.clone()).await;
        let (work, _, a, _) = one_on_work_started_an_hour_ago(&d, dir.path()).await;
        let resets = in_secs(86_400);
        let hit_at = chrono::Utc::now() - chrono::Duration::minutes(20);
        {
            let mut st = d.0.state.lock();
            let raw = super::identities::Raw::Account(work);
            Desk::mark_session_hit_at(
                &mut st,
                a,
                &raw,
                Some(&crate::limit::LimitHit {
                    window: Some("seven_day".into()),
                    resets_at: Some(resets),
                    at: None,
                }),
                hit_at,
            );
        }
        d.report_usage(weekly(26.0, in_secs(6 * 86_400)), Some(a));
        let login = chrono::Utc::now() - chrono::Duration::minutes(10);
        d.login_in_transcript(a, Some(("tail-login".into(), login)))
            .await;
        assert_eq!(
            work_standing(&d, work).full(chrono::Utc::now()),
            Some(Some(resets))
        );
        assert_eq!(
            plans.1.lock()[0].2,
            Some(hit_at),
            "at its limit, never \"no reading yet\""
        );
    })
    .await;
}

#[tokio::test]
async fn what_a_session_reports_while_its_transcript_is_read_waits_and_is_filed_as_its_sign_ins() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let plans = Arc::new(Plans::default());
        let d = signed_in_as_gmail(pool, dir.path(), plans.clone()).await;
        let (work, _, a, _) = one_on_work_started_an_hour_ago(&d, dir.path()).await;
        d.0.state.lock().scan_pending.insert(a);
        let reported = plans.0.lock().len();
        d.report_usage(weekly(26.0, in_secs(6 * 86_400)), Some(a));
        stops_on_its_limit(&d, a);
        assert_eq!(plans.0.lock().len(), reported, "waits");
        assert!(
            work_standing(&d, work).hit_at.is_none(),
            "its stop waits too"
        );
        d.login_scan_done(
            a,
            Some(login_at(
                chrono::Utc::now() - chrono::Duration::minutes(10),
                None,
            )),
        )
        .await;
        assert!(d.0.state.lock().scan_pending.is_empty());
        assert_eq!(plans.0.lock().last().unwrap().ids, vec![gmail().plan_id()]);
        let st = d.0.state.lock();
        assert!(st
            .standing
            .get(&Some(work))
            .is_none_or(|s| s.read_at.is_none() && s.hit_at.is_none()));
        assert!(st
            .own_standing
            .get(&gmail().key)
            .is_some_and(|s| s.hit_at.is_some() && s.read_at.is_some()));
    })
    .await;
}

#[tokio::test]
async fn what_waited_on_a_scan_that_found_nothing_is_filed_as_it_was() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let plans = Arc::new(Plans::default());
        let d = signed_in_as_gmail(pool, dir.path(), plans.clone()).await;
        let (work, _, a, _) = one_on_work_started_an_hour_ago(&d, dir.path()).await;
        d.0.state.lock().scan_pending.insert(a);
        d.report_usage(weekly(26.0, in_secs(6 * 86_400)), Some(a));
        d.login_scan_done(a, None).await;
        assert_eq!(
            plans.0.lock().last().unwrap().ids,
            vec![format!("desk-account:{work}")]
        );
        assert!(!d.0.state.lock().relogged.contains(&a));
    })
    .await;
}

#[tokio::test]
async fn a_scan_that_ends_however_it_ends_files_what_waited_on_it() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let plans = Arc::new(Plans::default());
        let d = signed_in_as_gmail(pool, dir.path(), plans.clone()).await;
        let (work, _, a, _) = one_on_work_started_an_hour_ago(&d, dir.path()).await;
        d.0.state.lock().scan_pending.insert(a);
        d.report_usage(weekly(26.0, in_secs(6 * 86_400)), Some(a));
        // Its scan's task gone (timed out, or a panic): dropped, released.
        let task = tokio::spawn({
            let d = d.clone();
            async move {
                let _release = super::identities::ScanRelease::new(d, a);
                panic!("the scan failed");
            }
        });
        assert!(task.await.is_err());
        assert!(d.0.state.lock().scan_pending.is_empty());
        assert_eq!(
            plans.0.lock().last().unwrap().ids,
            vec![format!("desk-account:{work}")],
            "filed as it was"
        );
    })
    .await;
}

#[test]
fn a_sessions_waiting_drops_only_its_own_oldest_readings_never_a_stop() {
    use super::identities::{Waiting, WAITING_MAX};
    let mut st = super::State::default();
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let at = |n: i64| chrono::DateTime::from_timestamp(1_794_000_000 + n, 0).unwrap();
    Waiting::Hit(a, None, at(0)).wait(&mut st);
    for n in 0..WAITING_MAX as i64 {
        Waiting::Usage(serde_json::Value::Null, b, at(n)).wait(&mut st);
    }
    for n in 1..=(WAITING_MAX as i64 + 5) {
        Waiting::Usage(serde_json::Value::Null, a, at(n)).wait(&mut st);
    }
    let of = |id: Uuid, hit: bool| {
        st.scan_waiting
            .iter()
            .filter(|w| match w {
                Waiting::Usage(_, s, _) => !hit && *s == id,
                Waiting::Hit(s, _, _) => hit && *s == id,
            })
            .count()
    };
    assert_eq!(of(a, true), 1, "its stop is kept");
    assert_eq!(of(a, false), WAITING_MAX);
    assert_eq!(of(b, false), WAITING_MAX, "another session's untouched");
    let oldest_a = st.scan_waiting.iter().find_map(|w| match w {
        Waiting::Usage(_, s, t) if *s == a => Some(*t),
        _ => None,
    });
    assert_eq!(oldest_a, Some(at(6)), "its oldest dropped first");
}

#[tokio::test]
async fn which_account_this_computers_sign_in_is_is_asked_while_unknown_with_no_switch_waiting() {
    use crate::engine_accounts::AccountNeed;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = signed_in_as_gmail(pool, dir.path(), Arc::new(Plans::default())).await;
        assert!(
            d.account_needs().is_empty(),
            "no account to be: nothing to ask"
        );
        let gmail_account = paste(&d, "Kingsley Gmail", GOOD, true).await;
        let need = until(|| d.account_needs().into_iter().next()).await;
        assert!(
            matches!(&need, AccountNeed::WhichSignIn { email: Some(e), .. } if e == "kim@example.com"),
            "{need:?}"
        );
        assert_eq!(
            need.id(),
            format!("which-sign-in:{}", gmail().key),
            "asked of this sign-in"
        );
        d.link_own_sign_in(Some(gmail_account)).await.unwrap();
        assert!(d.account_needs().is_empty(), "answered");
        assert_eq!(
            d.own_plan_key(),
            d.account_plan_key(gmail_account),
            "this computer's card is the account's"
        );
    })
    .await;
}

#[tokio::test]
async fn not_now_for_which_account_the_sign_in_is_outlives_a_restart_and_holds_for_that_sign_in_alone(
) {
    use crate::engine_accounts::AccountNeed;
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = signed_in_as_gmail(pool.clone(), dir.path(), Arc::new(Plans::default())).await;
        paste(&d, "Kingsley Gmail", GOOD, true).await;
        let need = until(|| d.account_needs().into_iter().next()).await;
        d.put_off(need.id());
        assert!(d.account_needs().is_empty());
        let mut kept = false;
        for _ in 0..200 {
            let e = d.0.store.state(super::accounts::PUT_OFF).await.unwrap();
            if e.is_some_and(|e| e.value.as_array().is_some_and(|v| v.len() == 1)) {
                kept = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(kept, "kept for the hub's next start");
        drop(d);
        let again = signed_in_as_gmail(pool, dir.path(), Arc::new(Plans::default())).await;
        again.accounts_moved().await;
        assert!(
            !again
                .account_needs()
                .iter()
                .any(|n| matches!(n, AccountNeed::WhichSignIn { .. })),
            "not asked again after a restart"
        );
        // Signed in as another: asked of that one, and the last one's "Not
        // now" is gone with it.
        cli_signs_in_as(&again, dir.path(), "star").await;
        again.accounts_moved().await;
        assert!(
            !again
                .0
                .state
                .lock()
                .put_off
                .contains(&format!("which-sign-in:{}", gmail().key)),
            "a sign-in no longer had keeps no answer"
        );
        let need = until(|| {
            again
                .account_needs()
                .into_iter()
                .find(|n| matches!(n, AccountNeed::WhichSignIn { .. }))
        })
        .await;
        assert_eq!(need.id(), format!("which-sign-in:{}", star().key));
    })
    .await;
}

#[tokio::test]
async fn no_key_is_current_until_this_computers_sign_in_is_read() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = signed_in_as_gmail(pool, dir.path(), Arc::new(Plans::default())).await;
        assert!(d.current_plan_keys().is_some());
        d.0.state.lock().own_read = false;
        assert_eq!(
            d.current_plan_keys(),
            None,
            "not yet known: nothing dropped"
        );
    })
    .await;
}

#[tokio::test]
async fn with_no_claude_code_here_its_sign_in_is_read_as_none_and_keys_are_current() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let mut o = signed_out_options(dir.path(), Arc::new(MemoryKeys::default()));
        o.programs.remove(&Engine::Claude);
        let d = Desk::start(
            DeskStore::from_pool(pool),
            o,
            super::Floor {
                plans: Some(Arc::new(Plans::default())),
                ..Default::default()
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        d.read_engine_now(Engine::Claude).await;
        assert!(d.0.state.lock().own_read);
        assert!(d.current_plan_keys().is_some(), "the filter is on");
    })
    .await;
}

#[tokio::test]
async fn an_account_whose_sign_in_is_gone_is_signed_in_again_and_keeps_everything_else() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let keys = Arc::new(MemoryKeys::default());
        let d = desk(pool, account_options(dir.path(), keys.clone())).await;
        let work = paste(&d, "Work", GOOD, true).await;
        let s = d.create(session(dir.path(), None), None).await.unwrap();
        started(dir.path(), s.id, 0).await;
        // Its sign-in goes from the store: Desk finds out when it reads it.
        keys.delete(work).unwrap();
        d.change(s.id, |r, _| r.state = SessionState::Idle);
        assert!(d.switch_account(s.id, Some(work), None).await.is_err());
        assert_eq!(d.sign_in_problems().len(), 1);
        assert!(!d.accounts()[0].signed_in);

        // Through the engine's own sign-in: its token goes to the same account.
        let Added::SignIn { sign_in } = d
            .sign_in_again(
                work,
                SignInAgain {
                    token: None,
                    remote: false,
                },
            )
            .await
            .unwrap()
        else {
            panic!("no token: it signs in");
        };
        assert_eq!(sign_in.name, "Work");
        let state = || {
            d.account_sign_ins()
                .into_iter()
                .find(|x| x.id == sign_in.id)
                .map(|x| x.state)
        };
        for _ in 0..200 {
            if matches!(state(), Some(SignInState::Code { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        d.sign_in_code(sign_in.id, "c0de").unwrap();
        for _ in 0..200 {
            if matches!(state(), Some(SignInState::Done { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(state(), Some(SignInState::Done { account: work }));
        let views = d.accounts();
        assert_eq!(views.len(), 1, "no new account");
        assert!(
            views[0].id == work
                && views[0].name == "Work"
                && views[0].is_default
                && views[0].signed_in
        );
        assert!(d.sign_in_problems().is_empty());
        assert_eq!(
            keys.get(work).unwrap().map(|t| t.expose().to_string()),
            Some("sk-ant-oat01-good-c0de-0123456789012345678901234567890123".to_string())
        );

        // What Desk knew of which Anthropic account it was is forgotten
        // when it signs in again: the new sign-in may be another one.
        let link = crate::identity::Link {
            identity: crate::identity::Identity {
                key: "k-before".into(),
                email: Some("before@example.com".into()),
            },
            by: crate::identity::LinkedBy::Manual,
            at: chrono::Utc::now(),
        };
        d.0.store.put_account_link(work, Some(&link)).await.unwrap();
        {
            let mut st = d.0.state.lock();
            st.links.insert(work, link);
            st.standing.insert(Some(work), Default::default());
            st.prints
                .entry(super::identities::Raw::Account(work))
                .or_default();
        }

        // By a pasted token: checked, then kept for it.
        let refused = d
            .sign_in_again(
                work,
                SignInAgain {
                    token: Some("sk-ant-oat01-bad".into()),
                    remote: false,
                },
            )
            .await;
        assert!(refused.is_err());
        let Added::Account { account } = d
            .sign_in_again(
                work,
                SignInAgain {
                    token: Some(OTHER.into()),
                    remote: false,
                },
            )
            .await
            .unwrap()
        else {
            panic!("a pasted token signs in at once");
        };
        assert_eq!((account.id, account.name.as_str()), (work, "Work"));
        assert_eq!(
            keys.get(work).unwrap().map(|t| t.expose().to_string()),
            Some(OTHER.to_string())
        );
        {
            let st = d.0.state.lock();
            assert!(!st.links.contains_key(&work), "its link is forgotten");
            // What it knew of its plan goes; only when it was signed in
            // again stays, so a stop from before is over.
            let standing = &st.standing[&Some(work)];
            assert!(standing.signed_in_at.is_some());
            assert_eq!(
                standing,
                &crate::engine_accounts::Standing {
                    signed_in_at: standing.signed_in_at,
                    ..Default::default()
                }
            );
            assert!(!st
                .prints
                .contains_key(&super::identities::Raw::Account(work)));
        }
        assert!(
            !d.0.store.account_links().await.unwrap().contains_key(&work),
            "and forgotten in the store, so a restart doesn't bring it back"
        );
        assert!(matches!(
            d.sign_in_again(Uuid::new_v4(), SignInAgain::default())
                .await,
            Err(DeskError::Invalid(_))
        ));
    })
    .await;
}

#[tokio::test]
async fn extra_usage_or_one_models_limit_spent_never_fills_the_account_and_a_start_goes_ahead() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(
            pool,
            account_options(dir.path(), Arc::new(MemoryKeys::default())),
        )
        .await;
        let (work, _, a, _) = two_on_work(&d, dir.path()).await;
        let now = chrono::Utc::now();
        let december = now + chrono::Duration::days(20);
        let week = now + chrono::Duration::days(3);
        // The session at 34% and the week at 41%.
        d.report_usage(both(34.0, 41.0, (3600, 86_400)), Some(a));
        {
            let mut st = d.0.state.lock();
            let raw = Desk::raw_running(&st, a);
            let hit = |window: &str, resets| crate::limit::LimitHit {
                window: Some(window.into()),
                resets_at: Some(resets),
                at: None,
            };
            // Extra usage spent until 1 Dec; Opus's weekly window spent.
            Desk::mark_session_hit(&mut st, a, &raw, Some(&hit("overage", december)));
            Desk::mark_session_hit(&mut st, a, &raw, Some(&hit("seven_day_opus", week)));
        }
        let standing = work_standing(&d, work);
        assert_eq!(standing.full(now), None, "{standing:?}");
        assert_eq!(standing.hit_until, None);
        // A new session starts on the account.
        d.create(session(dir.path(), None), None).await.unwrap();
        // Extra usage spent while a reading shows the weekly window at
        // 100%: the plan is full after all, until that window resets.
        {
            let mut st = d.0.state.lock();
            let s = st.standing.get_mut(&Some(work)).unwrap();
            s.read_at = Some(chrono::Utc::now());
            s.windows = vec![crate::usage::LimitWindow {
                used_percent: 100.0,
                resets_at: Some(week),
                minutes: 10_080,
            }];
            let raw = Desk::raw_running(&st, a);
            let hit = crate::limit::LimitHit {
                window: Some("overage".into()),
                resets_at: Some(december),
                at: None,
            };
            Desk::mark_session_hit(&mut st, a, &raw, Some(&hit));
        }
        let standing = work_standing(&d, work);
        assert_eq!(standing.hit_until, Some(week), "not December: {standing:?}");
        {
            let mut st = d.0.state.lock();
            let s = st.standing.get_mut(&Some(work)).unwrap();
            s.windows.clear();
            s.hit_at = None;
            s.hit_until = None;
            s.hit_window = None;
        }
        // The plan's own weekly window is a stop that fills it, and the
        // refusal says which and when.
        {
            let mut st = d.0.state.lock();
            let raw = Desk::raw_running(&st, a);
            let hit = crate::limit::LimitHit {
                window: Some("seven_day".into()),
                resets_at: Some(week),
                at: None,
            };
            Desk::mark_session_hit(&mut st, a, &raw, Some(&hit));
        }
        assert_eq!(work_standing(&d, work).full(now), Some(Some(week)));
    })
    .await;
}

#[tokio::test]
async fn only_an_engine_whose_accounts_sign_in_by_token_is_signed_in_again() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let keys = Arc::new(MemoryKeys::default());
        let d = desk(pool, account_options(dir.path(), keys.clone())).await;
        let work = paste(&d, "Work", GOOD, true).await;
        // An account of an engine Desk can't sign accounts in for (none
        // can be added, but one may be in the list from elsewhere).
        let other = Uuid::new_v4();
        {
            let mut st = d.0.state.lock();
            let mut a = st.accounts.iter().find(|a| a.id == work).unwrap().clone();
            a.id = other;
            a.engine = Engine::Codex;
            st.accounts.push(a);
        }
        let refused = d
            .sign_in_again(
                other,
                SignInAgain {
                    token: Some(OTHER.into()),
                    remote: false,
                },
            )
            .await;
        assert!(
            matches!(&refused, Err(DeskError::Invalid(m)) if m.contains("can't sign")),
            "{refused:?}"
        );
        assert!(keys.get(other).unwrap().is_none(), "nothing kept for it");
    })
    .await;
}

/// Keys whose writing of a token is when the account is removed: what
/// Desk sees of a removal that lands while a sign-in again keeps its token.
struct RemovedWhileKept {
    keys: Arc<MemoryKeys>,
    desk: parking_lot::Mutex<Option<Desk>>,
}

impl crate::engine_accounts::Keys for RemovedWhileKept {
    fn get(&self, account: Uuid) -> eyre::Result<Option<crate::engine_accounts::AccountToken>> {
        self.keys.get(account)
    }
    fn set(&self, account: Uuid, token: &crate::engine_accounts::AccountToken) -> eyre::Result<()> {
        self.keys.set(account, token)?;
        if let Some(d) = self.desk.lock().take() {
            d.0.state.lock().accounts.retain(|a| a.id != account);
        }
        Ok(())
    }
    fn delete(&self, account: Uuid) -> eyre::Result<()> {
        self.keys.delete(account)
    }
    fn name(&self) -> &'static str {
        "the test store"
    }
}

#[tokio::test]
async fn an_account_removed_while_it_signs_in_again_leaves_no_sign_in_behind() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let keys = Arc::new(MemoryKeys::default());
        let d = desk(pool, account_options(dir.path(), keys.clone())).await;
        let work = paste(&d, "Work", GOOD, true).await;
        keys.delete(work).unwrap();
        let token = crate::engine_accounts::AccountToken::new(OTHER).unwrap();
        // Its removal takes it out of the list (as it does before it
        // deletes its sign-in) just as its sign-in again keeps the token.
        let racing = Arc::new(RemovedWhileKept {
            keys: keys.clone(),
            desk: parking_lot::Mutex::new(Some(d.clone())),
        });
        let renewed = d.renew_account(racing, work, &token).await;
        assert!(
            matches!(&renewed, Err(DeskError::Invalid(m)) if m.contains("removed")),
            "it isn't signed in again: it's gone ({renewed:?})"
        );
        assert!(
            keys.get(work).unwrap().is_none(),
            "the token it kept went with it"
        );
        assert!(d.accounts().is_empty(), "nor is it back in the list");
    })
    .await;
}
