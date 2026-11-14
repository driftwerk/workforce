//! A Devin session from start to resume, run on a fake `devin` that posts
//! its hooks the way Desk's hooks post them: `curl -K` of the session's
//! hook file, the event in its header, payloads as Devin's docs show them.

use std::os::unix::fs::PermissionsExt;

use super::tests::options;
use super::*;
use crate::store::build_pool;
use workforce_postgres::test_support::with_test_pool;

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
for a in "$@"; do [ "$a" = -r ] && source=resume; done
for a in "$@"; do last="$a"; done
post SessionStart '{{"session_id":"ses_fake1","source":"'$source'"}}'
post UserPromptSubmit '{{"session_id":"ses_fake1","prompt_id":"p1","prompt":"'"$last"'"}}'
sed 's/^{{$/{{"notify":"always",/' "$config" > "$d/saved" && cat "$d/saved" > "$config"
post Stop '{{"session_id":"ses_fake1","prompt_id":"p1","stop_hook_active":false,"last_assistant_message":"Done: the tests pass."}}'
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
                &devin_config::Person::Read(serde_json::json!({})),
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
