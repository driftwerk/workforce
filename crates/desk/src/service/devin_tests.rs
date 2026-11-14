//! A Devin session from start to resume, run on a fake `devin` that posts
//! its hooks the way Desk's hooks post them: `curl -K` of the session's
//! hook file, the event in its header, payloads as Devin's docs show them.

use std::os::unix::fs::PermissionsExt;

use super::tests::options;
use super::*;
use crate::store::build_pool;
use workforce_postgres::test_support::with_test_pool;
use workforce_postgres::PostgresPool;

/// A `devin` that logs its arguments, prints `list.json` for `list`, and
/// otherwise posts `SessionStart` (a resume when given `-r`) and its
/// prompt, saves a setting to the config it runs on, posts its `Stop`, then
/// waits.
fn fake_devin(dir: &Path) -> PathBuf {
    let path = dir.join("devin");
    let script = format!(
        r#"#!/bin/sh
d='{dir}'
if [ "$1" = list ]; then cat "$d/list.json"; exit 0; fi
{{ echo ---; printf '%s\n' "$@"; }} >> "$d/args.log"
config="$2"
hook="${{config%.devin.json}}.hook"
post() {{ curl -sf --max-time 5 -K "$hook" -H "X-Desk-Devin-Event: $1" --data-binary "$2" > /dev/null; }}
source=startup
mode=normal
prev=
for a in "$@"; do
  [ "$a" = -r ] && source=resume
  [ "$prev" = --permission-mode ] && mode="$a"
  prev="$a"
done
for a in "$@"; do last="$a"; done
post SessionStart '{{"session_id":"ses_fake1","source":"'$source'"}}'
post UserPromptSubmit '{{"session_id":"ses_fake1","prompt_id":"p1","prompt":"'"$last"'"}}'
sed 's/^{{$/{{"notify":"always",/' "$config" > "$d/saved" && cat "$d/saved" > "$config"
post Stop '{{"session_id":"ses_fake1","prompt_id":"p1","stop_hook_active":false,"permission_mode":"'$mode'","last_assistant_message":"Done: the tests pass."}}'
exec sleep 60
"#,
        dir = dir.display()
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// What the last start of the fake was given.
fn last_args(dir: &Path) -> Vec<String> {
    let log = std::fs::read_to_string(dir.join("args.log")).unwrap_or_default();
    log.rsplit("---\n")
        .next()
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

async fn until(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..400 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("never: {what}");
}

#[tokio::test]
async fn a_devin_session_runs_on_its_hooks_and_resumes_its_conversation() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let mine = home.join(".config/devin/config.json");
        std::fs::create_dir_all(mine.parent().unwrap()).unwrap();
        std::fs::write(
            &mine,
            "{\n  // mine\n  \"agent\": { \"model\": \"swe-1-6-fast\" }\n}\n",
        )
        .unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(dir.path().join("list.json"), "[]").unwrap();
        let mut o = options(dir.path()).with_program(Engine::Devin, fake_devin(dir.path()));
        o.env.insert("HOME".into(), home.display().to_string());
        o.env
            .insert("WINDSURF_API_KEY".into(), "not-for-devin".into());
        DeskStore::migrate(&pool).await.unwrap();
        let d = Desk::start(
            DeskStore::from_pool(pool),
            o,
            Default::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let s = d
            .create(
                NewSession {
                    fresh_worktree: false,
                    engine: Engine::Devin,
                    view: None,
                    name: "tests".into(),
                    cwd: work.to_string_lossy().into_owned(),
                    goal: "make the tests pass".into(),
                    worktree: None,
                    prompt: Some("run the tests".into()),
                    approvals: None,
                    approve_for_me: false,
                    fast: false,
                    account: None,
                    env: Default::default(),
                    group: None,
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(s.cli_session_id, None, "Devin chooses its id");
        until("its turn ended", || {
            d.view(s.id)
                .is_ok_and(|v| v.state == SessionState::Done && v.last_reply.is_some())
        })
        .await;
        let v = d.view(s.id).unwrap();
        assert_eq!(
            v.cli_session_id.as_deref(),
            Some("ses_fake1"),
            "its SessionStart said it"
        );
        assert_eq!(v.last_prompt.as_deref(), Some("run the tests"));
        assert_eq!(v.last_reply.as_deref(), Some("Done: the tests pass."));
        let args = last_args(dir.path());
        assert_eq!(args[0], "--config");
        assert_eq!(&args[args.len() - 2..], ["--", "run the tests"]);
        assert!(!args.iter().any(|a| a == "-r"));
        let token = d.0.state.lock().live[&s.id].token.clone();
        assert!(
            !args.iter().any(|a| a.contains(&token)),
            "no token on its command line"
        );
        let files = devin::SessionFiles::of(&dir.path().join("run"), s.id);
        for f in [&files.config, &files.base, &files.hook] {
            let mode = std::fs::metadata(f).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", f.display());
        }
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&files.config).unwrap()).unwrap();
        assert_eq!(
            config["agent"]["model"], "swe-1-6-fast",
            "theirs, copied whole"
        );

        // Stopped: what Devin saved is in the person's own config, their
        // comment kept, and the session's files are gone.
        d.stop(s.id, None).unwrap();
        until("its files went", || {
            !files.hook.exists() && !files.config.exists()
        })
        .await;
        let theirs = std::fs::read_to_string(&mine).unwrap();
        let v = devin_config::parse(&theirs).unwrap();
        assert_eq!(v["notify"], "always", "{theirs}");
        assert_eq!(v["agent"]["model"], "swe-1-6-fast");
        assert!(v.get("hooks").is_none(), "Desk's hooks never reach it");
        assert!(theirs.contains("// mine"));

        // Resumed while Devin still lists it: by its id.
        std::fs::write(dir.path().join("list.json"), r#"[{"id":"ses_fake1"}]"#).unwrap();
        until("it stopped", || !d.0.state.lock().live.contains_key(&s.id)).await;
        d.resume(s.id, Some("go on".into()), None).await.unwrap();
        until("it resumed", || {
            last_args(dir.path()).last().map(String::as_str) == Some("go on")
        })
        .await;
        let args = last_args(dir.path());
        let at = args.iter().position(|a| a == "-r").expect("resumed by id");
        assert_eq!(args[at + 1], "ses_fake1");
        until("its turn ended again", || {
            d.view(s.id).is_ok_and(|v| {
                v.state == SessionState::Done && v.last_prompt.as_deref() == Some("go on")
            })
        })
        .await;

        // Devin no longer has it: a new conversation, not a resume.
        d.stop(s.id, None).unwrap();
        until("it stopped again", || {
            !d.0.state.lock().live.contains_key(&s.id)
        })
        .await;
        std::fs::write(dir.path().join("list.json"), "[]").unwrap();
        d.resume(s.id, Some("once more".into()), None)
            .await
            .unwrap();
        until("it started afresh", || {
            last_args(dir.path()).last().map(String::as_str) == Some("once more")
        })
        .await;
        assert!(!last_args(dir.path()).iter().any(|a| a == "-r"));
        d.stop(s.id, None).unwrap();
    })
    .await;
}

#[tokio::test]
async fn devin_files_left_by_a_session_that_ended_while_the_hub_was_away_are_ended_at_its_start() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let mine = home.join(".config/devin/config.json");
        std::fs::create_dir_all(mine.parent().unwrap()).unwrap();
        std::fs::write(&mine, "{}\n").unwrap();
        let run = dir.path().join("run");
        let gone = Uuid::new_v4();
        let files = devin::SessionFiles::of(&run, gone);
        files
            .write(
                &run,
                "http://127.0.0.1:9/hook/x/y",
                60,
                false,
                &devin_config::Person::Read {
                    config: serde_json::json!({}),
                    real: None,
                },
            )
            .unwrap();
        // What Devin saved in it before it ended.
        let mut ended: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&files.config).unwrap()).unwrap();
        ended["agent"] = serde_json::json!({"pinned_models": ["swe-1-6"]});
        std::fs::write(&files.config, ended.to_string()).unwrap();
        let mut o = options(dir.path());
        o.env.insert("HOME".into(), home.display().to_string());
        DeskStore::migrate(&pool).await.unwrap();
        let _d = Desk::start(
            DeskStore::from_pool(pool),
            o,
            Default::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        for f in [&files.config, &files.base, &files.hook] {
            assert!(!f.exists(), "{}", f.display());
        }
        let v = devin_config::parse(&std::fs::read_to_string(&mine).unwrap()).unwrap();
        assert_eq!(v["agent"]["pinned_models"], serde_json::json!(["swe-1-6"]));
    })
    .await;
}

#[tokio::test]
async fn desks_run_folder_holding_devins_hook_tokens_is_withheld_from_every_boss() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let o = options(dir.path());
        let run = o.run_dir.clone();
        std::fs::create_dir_all(&run).unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let d = Desk::start(
            DeskStore::from_pool(pool),
            o,
            Default::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        // The hub's bounds call the temp folder scratch, where it is.
        d.boss_bounds_with(crate::charge::Bounds {
            withheld: Vec::new(),
            scratch: vec![dir.path().to_path_buf()],
            home: dir.path().join("home"),
        });
        let bounds = d.0.boss_bounds.lock().clone().unwrap();
        assert!(bounds
            .withheld
            .contains(&std::fs::canonicalize(&run).unwrap()));
    })
    .await;
}

/// The Devin folders Devin's sessions find (config and data) are withheld
/// from a boss where their environment puts them, even where what a
/// session's start added sets them apart from the hub's, after the hub
/// said its bounds, and for as long as a run that found others runs.
#[tokio::test]
async fn the_devin_folders_devins_sessions_find_are_withheld_from_every_boss() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let o = options(dir.path());
        std::fs::create_dir_all(&o.run_dir).unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let d = Desk::start(
            DeskStore::from_pool(pool),
            o,
            Default::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        d.boss_bounds_with(crate::charge::Bounds {
            withheld: Vec::new(),
            scratch: Vec::new(),
            home: dir.path().join("home"),
        });
        let real = std::fs::canonicalize(dir.path()).unwrap();
        let elsewhere = real.join("elsewhere");
        std::fs::create_dir_all(elsewhere.join("devin")).unwrap();
        d.0.state.lock().session_env = BTreeMap::from([
            (
                "XDG_CONFIG_HOME".to_string(),
                elsewhere.display().to_string(),
            ),
            (
                "XDG_DATA_HOME".to_string(),
                real.join("data").display().to_string(),
            ),
        ]);
        let config = elsewhere.join("devin");
        let data = real.join("data/devin");
        let withheld = d.judging_bounds().unwrap().withheld;
        assert!(withheld.contains(&config), "{withheld:?}");
        assert!(withheld.contains(&data), "{withheld:?}");
        // A run launched now keeps them withheld while it runs, and writes
        // back to its own config, whatever a later start sets.
        let id = Uuid::new_v4();
        d.devin_launched(id, Utc::now());
        let home = real.join("other-home");
        d.0.state.lock().session_env =
            BTreeMap::from([("HOME".to_string(), home.display().to_string())]);
        let withheld = d.judging_bounds().unwrap().withheld;
        for w in [
            config.clone(),
            data.clone(),
            home.join(".config/devin"),
            home.join(".local/share/devin"),
        ] {
            assert!(withheld.contains(&w), "{w:?} in {withheld:?}");
        }
        assert_eq!(
            d.0.state.lock().devin_runs[&id].config,
            Some(config.join("config.json"))
        );
        // Once it has ended, only where sessions find them now.
        d.0.state.lock().devin_runs.get_mut(&id).unwrap().ended = Some(Utc::now());
        let withheld = d.judging_bounds().unwrap().withheld;
        assert!(!withheld.contains(&config), "{withheld:?}");
        assert!(withheld.contains(&home.join(".config/devin")));
    })
    .await;
}

/// A Desk on the fake `devin`, the person's config at `home`, and a
/// Devin session it started in `work`, its first turn over.
async fn devin_desk(
    pool: PostgresPool,
    dir: &Path,
    tune: impl FnOnce(&mut Options),
) -> (Desk, CancellationToken) {
    let home = dir.join("home");
    let mine = home.join(".config/devin/config.json");
    std::fs::create_dir_all(mine.parent().unwrap()).unwrap();
    if !mine.exists() {
        std::fs::write(&mine, "{}\n").unwrap();
    }
    std::fs::create_dir_all(dir.join("work")).unwrap();
    if !dir.join("list.json").exists() {
        std::fs::write(dir.join("list.json"), "[]").unwrap();
    }
    let mut o = options(dir).with_program(Engine::Devin, fake_devin(dir));
    o.env.insert("HOME".into(), home.display().to_string());
    tune(&mut o);
    DeskStore::migrate(&pool).await.unwrap();
    let cancel = CancellationToken::new();
    let d = Desk::start(
        DeskStore::from_pool(pool),
        o,
        Default::default(),
        cancel.clone(),
    )
    .await
    .unwrap();
    (d, cancel)
}

async fn devin_session(d: &Desk, dir: &Path) -> SessionView {
    let s = d
        .create(
            NewSession {
                fresh_worktree: false,
                engine: Engine::Devin,
                view: None,
                name: "tests".into(),
                cwd: dir.join("work").to_string_lossy().into_owned(),
                goal: "make the tests pass".into(),
                worktree: None,
                prompt: Some("run the tests".into()),
                approvals: None,
                approve_for_me: false,
                fast: false,
                account: None,
                env: Default::default(),
                group: None,
            },
            None,
        )
        .await
        .unwrap();
    until("its turn ended", || {
        d.view(s.id)
            .is_ok_and(|v| v.state == SessionState::Done && v.last_reply.is_some())
    })
    .await;
    s
}

/// The new run's `Stop` reached Desk: on its new hook file and token.
async fn its_next_hook_lands(d: &Desk, id: Uuid, what: &str) {
    until(what, || {
        d.view(id)
            .is_ok_and(|v| v.state == SessionState::Done && v.last_reply.is_some())
    })
    .await;
}

#[tokio::test]
async fn a_devin_session_started_again_at_once_keeps_its_new_runs_files() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("list.json"), r#"[{"id":"ses_fake1"}]"#).unwrap();
        let (d, _cancel) = devin_desk(pool, dir.path(), |_| {}).await;
        let s = devin_session(&d, dir.path()).await;
        let files = devin::SessionFiles::of(&dir.path().join("run"), s.id);

        // Ending a run's files takes a while: a resume the moment its
        // process is gone would otherwise start before they are ended.
        d.0.state.lock().devin_end_delay = Some(Duration::from_millis(600));
        // A new mode while it runs: stopped and resumed in place, the
        // moment its process is gone.
        d.change(s.id, |r, _| r.last_reply = None);
        let set = d
            .set_mode(s.id, crate::mode::Mode::AcceptEdits, By::you())
            .await
            .unwrap();
        assert!(set.not_started.is_none(), "{:?}", set.not_started);
        assert_eq!(
            set.confirmed, None,
            "Devin's mode is never waited on until a real capture says it is sent"
        );
        its_next_hook_lands(&d, s.id, "the new run's Stop landed").await;
        let args = last_args(dir.path());
        assert!(args.iter().any(|a| a == "-r"), "{args:?}");
        // The run before's end never takes the new run's files, nor says
        // its state over the new one's.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(files.hook.exists() && files.config.exists());
        assert!(d.0.state.lock().live.contains_key(&s.id));
        assert_eq!(d.view(s.id).unwrap().state, SessionState::Done);

        // Restarted with a prompt: the same.
        d.change(s.id, |r, _| r.last_reply = None);
        d.restart(s.id, Some("again".into()), By::you())
            .await
            .unwrap();
        its_next_hook_lands(&d, s.id, "the restarted run's Stop landed").await;
        assert_eq!(
            last_args(dir.path()).last().map(String::as_str),
            Some("again")
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(files.hook.exists() && files.config.exists());
        assert!(d.0.state.lock().live.contains_key(&s.id));
        d.stop(s.id, None).unwrap();
        until("its files went", || !files.hook.exists()).await;
    })
    .await;
}

#[tokio::test]
async fn a_resume_devin_refuses_leaves_none_of_its_files() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, _cancel) = devin_desk(pool, dir.path(), |_| {}).await;
        let s = devin_session(&d, dir.path()).await;
        d.stop(s.id, None).unwrap();
        until("it stopped", || !d.0.state.lock().live.contains_key(&s.id)).await;
        let files = devin::SessionFiles::of(&dir.path().join("run"), s.id);
        until("its files went", || !files.hook.exists()).await;
        // An id that would read as an option, and Devin still lists it.
        d.change(s.id, |r, _| r.cli_session_id = Some("-x".into()));
        std::fs::write(dir.path().join("list.json"), r#"[{"id":"-x"}]"#).unwrap();
        let starts = std::fs::read_to_string(dir.path().join("args.log")).unwrap();
        let refused = d.resume(s.id, None, None).await.unwrap_err();
        assert!(
            refused.to_string().contains("wouldn't pass on"),
            "{refused}"
        );
        for f in [&files.config, &files.base, &files.hook] {
            assert!(!f.exists(), "{} left", f.display());
        }
        assert!(!d.0.state.lock().live.contains_key(&s.id));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("args.log")).unwrap(),
            starts,
            "Devin never ran"
        );
    })
    .await;
}

#[tokio::test]
async fn a_devin_resume_that_fails_while_its_run_ends_keeps_what_the_end_said() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, _cancel) = devin_desk(pool, dir.path(), |_| {}).await;
        let s = devin_session(&d, dir.path()).await;
        assert_eq!(d.view(s.id).unwrap().state, SessionState::Done);
        // A resume Devin refuses, made while the run before is still ending.
        d.change(s.id, |r, _| r.cli_session_id = Some("-x".into()));
        std::fs::write(dir.path().join("list.json"), r#"[{"id":"-x"}]"#).unwrap();
        d.0.state.lock().devin_end_delay = Some(Duration::from_millis(600));
        d.stop(s.id, None).unwrap();
        until("its process went", || {
            !d.0.state.lock().live.contains_key(&s.id)
        })
        .await;
        let refused = d.resume(s.id, None, None).await.unwrap_err();
        assert!(
            refused.to_string().contains("wouldn't pass on"),
            "{refused}"
        );
        // Read after the end, never before it: its `Stopped` stays.
        assert_eq!(d.view(s.id).unwrap().state, SessionState::Stopped);
    })
    .await;
}

#[tokio::test]
async fn a_devin_session_removed_while_it_runs_leaves_no_files() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (d, _cancel) = devin_desk(pool, dir.path(), |_| {}).await;
        let s = devin_session(&d, dir.path()).await;
        let files = devin::SessionFiles::of(&dir.path().join("run"), s.id);
        assert!(files.hook.exists());
        d.forget(s.id).await.unwrap();
        until("its files went", || {
            !files.hook.exists() && !files.config.exists() && !files.base.exists()
        })
        .await;
        // What Devin saved is still written back.
        let mine = dir.path().join("home/.config/devin/config.json");
        let v = devin_config::parse(&std::fs::read_to_string(&mine).unwrap()).unwrap();
        assert_eq!(v["notify"], "always");
    })
    .await;
}

#[tokio::test]
async fn devin_files_stay_when_the_holder_couldnt_say_what_it_runs() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        let running = Uuid::new_v4();
        let files = devin::SessionFiles::of(&run, running);
        files
            .write(
                &run,
                "http://127.0.0.1:9/hook/x/y",
                60,
                false,
                &devin_config::Person::Read {
                    config: serde_json::json!({}),
                    real: None,
                },
            )
            .unwrap();
        let (_d, _cancel) = devin_desk(pool, dir.path(), |o| {
            o.holder = Some(HolderOptions {
                socket: dir.path().join("no-holder.sock"),
                token: crate::holder::tests::TOKEN.into(),
            });
        })
        .await;
        for f in [&files.config, &files.base, &files.hook] {
            assert!(f.exists(), "{} ended", f.display());
        }
    })
    .await;
}

#[tokio::test]
async fn a_devin_session_the_holder_kept_keeps_its_files_when_the_hub_starts_again() {
    with_test_pool(build_pool, |pool| async move {
        let (_hdir, socket, holder_cancel) = crate::holder::tests::holder().await;
        let dir = tempfile::tempdir().unwrap();
        let holder = HolderOptions {
            socket: socket.clone(),
            token: crate::holder::tests::TOKEN.into(),
        };
        let kept = holder.clone();
        let (first, first_cancel) =
            devin_desk(pool.clone(), dir.path(), move |o| o.holder = Some(kept)).await;
        let s = devin_session(&first, dir.path()).await;
        let files = devin::SessionFiles::of(&dir.path().join("run"), s.id);
        first_cancel.cancel();
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(first);

        let (second, _cancel) =
            devin_desk(pool, dir.path(), move |o| o.holder = Some(holder)).await;
        assert!(second.view(s.id).unwrap().live, "taken back");
        // Its shell may have moved before the restart: where a relative
        // path lands is unknown until Devin says.
        assert!(
            second.0.state.lock().live[&s.id].moved,
            "taken back unmoved"
        );
        for f in [&files.config, &files.base, &files.hook] {
            assert!(f.exists(), "{} ended under it", f.display());
        }
        // It ends: then they go.
        second.stop(s.id, None).unwrap();
        until("its files went", || !files.hook.exists()).await;
        holder_cancel.cancel();
    })
    .await;
}
