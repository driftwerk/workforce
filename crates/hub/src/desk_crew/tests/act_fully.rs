//! Act fully (`level = "full"`) is unbound: in any repository, named or
//! not, Desk allows what merges, tags, promotes and deploys the moment a
//! session asks; only its budget, Stop and the activity log hold it.

use super::super::CrewHub;
use super::unbound::UNBOUND;
use super::*;
use workforce_common::oversight::{Level, Oversight};

const FULL: &str = "[oversight]\nenabled = true\nlevel = \"full\"\n\
                    [oversight.full]\nrepos = [\"acme/acme-web\"]\nworkflows = [\"deploy.yml\", \"promote.yml\"]\nbudget_usd_per_day = 25\n";

pub(super) fn git(dir: &Path, args: &[&str]) {
    // Hub-side git is hardened, a test's too.
    let mut git = crate::git_hardening::hardened_git();
    let out = git
        .as_std_mut()
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Acme")
        .env("GIT_AUTHOR_EMAIL", "dev@example.com")
        .env("GIT_COMMITTER_NAME", "Acme")
        .env("GIT_COMMITTER_EMAIL", "dev@example.com")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A checkout at `dir` on `main`, at the commit `origin`'s `main` is at
/// (tagged v1.2.0 there), its `origin` at `origin`, holding a deploy script.
pub(super) fn repo(dir: &Path, origin: &str) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(
        dir.join("package.json"),
        r#"{"scripts": {"deploy": "tsc -p . && wrangler deploy", "release": "npm publish"}}"#,
    )
    .unwrap();
    git(dir, &["add", "package.json"]);
    git(dir, &["commit", "-q", "-m", "Scripts"]);
    git(dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    git(dir, &["tag", "v1.2.0"]);
    git(dir, &["remote", "add", "origin", origin]);
    git(
        dir,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
}

/// Act fully is unbound: in a repository it names and in one it doesn't,
/// tagging, publishing and deploying are allowed the moment a session
/// asks, in a session it runs and in one it doesn't; a session's own merge
/// is refused, for its boss merges it (DW-765).
#[tokio::test(flavor = "multi_thread")]
async fn act_fully_ships_in_any_repository_at_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let acme = base.join("acme-web");
        let globex = base.join("globex-api");
        repo(&acme, "git@github.com:acme/acme-web.git");
        repo(&globex, "git@github.com:globex/globex-api.git");
        let (overseer, crew, d, llm) = overseeing(&base, pool, "ok", FULL, &[]).await;
        super::boss_allows(&llm).await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        let id = overseer.agent_id.as_str().to_string();
        crew.read_spent_today(&crew.overseers()).await;
        let (_, o) = crew.overseer_service().unwrap();
        assert_eq!(crew.level_now(&id, &o), (Level::Full, None));
        assert!(crew.unbound(&o));
        eventually("Desk to know it is unbound", async || {
            d.boss_unbound("desk").then_some(())
        })
        .await;
        let boss = workforce_desk::crew::Party::manager("desk", &super::super::name_of(&overseer));
        let ask = |url: &str, command: &str| {
            let url = url.to_string();
            let body = json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash",
                "tool_input": {"command": command}});
            tokio::spawn(async move { hook(&url, body).await })
        };
        let named = new_session(&d, &acme, "acme-web").await;
        let other = new_session(&d, &globex, "globex-api").await;
        d.take_on(
            named.id,
            &boss,
            workforce_desk::charge::How::Handed,
            &Default::default(),
            &By::you(),
        )
        .await
        .unwrap();
        for s in [named.id, other.id] {
            let url = hook_url(&base, s).await;
            for command in [
                "git tag v1.3.0 && git push origin v1.3.0",
                "npm publish",
                "gh workflow run cleanup.yml",
            ] {
                let answer = ask(&url, command).await.unwrap();
                assert_eq!(
                    answer["hookSpecificOutput"]["decision"]["behavior"], "allow",
                    "{command}: {answer}"
                );
            }
            let merge = ask(&url, "gh pr merge 42 --squash").await.unwrap();
            assert_eq!(
                merge["hookSpecificOutput"]["decision"]["behavior"], "deny",
                "its boss merges it: {merge}"
            );
        }
    })
    .await;
}

/// Act fully needs no named repositories (it is unbound); with its budget
/// spent or its spend unread it acts as Act and says why. With no budget
/// set, nothing it spends holds it.
#[tokio::test(flavor = "multi_thread")]
async fn act_fully_needs_no_repositories_and_falls_back_to_act_only_over_its_budget() {
    let parse = |body: &str| {
        let doc: toml::Table = toml::from_str(body).unwrap();
        Oversight::of(doc.get("oversight")).unwrap()
    };
    let no_repos = parse("[oversight]\nlevel = \"full\"\nfull = { budget_usd_per_day = 25 }\n");
    assert!(super::super::fully_given(&no_repos, Some(0.0)).is_ok());
    let no_budget =
        parse("[oversight]\nlevel = \"full\"\nfull = { repos = [\"acme/acme-web\"] }\n");
    // A budget is optional: with none, only the check-in ceilings and
    // Stop bound it (a served boss still needs one, see `served`).
    assert!(super::super::fully_given(&no_budget, Some(0.0)).is_ok());
    let full = parse(FULL);
    assert!(super::super::fully_given(&full, Some(24.99)).is_ok());
    assert!(super::super::fully_given(&full, Some(25.0))
        .unwrap_err()
        .contains("Act fully's daily budget is spent: $25.00 of $25.00 in real money today"));
    assert!(super::super::fully_given(&full, None)
        .unwrap_err()
        .contains("today's spend is read"));

    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, crew, d, llm) = overseeing(
            &base,
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"full\"\nfull = { budget_usd_per_day = 25 }\n",
            &[],
        )
        .await;
        super::boss_allows(&llm).await;
        crew.read_spent_today(&crew.overseers()).await;
        let id = overseer.agent_id.as_str().to_string();
        let (_, o) = crew.overseer_service().unwrap();
        assert_eq!(crew.level_now(&id, &o), (Level::Full, None));
        assert!(crew.unbound(&o), "Act fully with no list is unbound");
        assert!(
            d.bosses().iter().all(|b| b.level == "full"),
            "Desk is told it acts fully"
        );
    })
    .await;
}

/// Act fully's every prompt carries the end-state rule; Act's doesn't.
#[tokio::test(flavor = "multi_thread")]
async fn act_fully_aims_at_the_end_state_in_parallel_without_polish_first() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", UNBOUND, &[]).await;
        let s = new_session(&d, dir.path(), "Legacy removal").await;
        let (_, o) = crew.overseer_service().unwrap();
        assert!(crew.unbound(&o));
        let ended = ["Turn ended.".to_string()];
        let said = [
            CrewHub::boss_hint(&o),
            CrewHub::dm_hint(Level::Full, &o, true),
            crew.line_prompt(s.id, &[], &ended, &o),
            // Bound at Act fully (shipping in a named repository, or its
            // DM while not unbound): the same rules.
            CrewHub::dm_hint(Level::Full, &o, false),
            CrewHub::level_hint(Level::Full, &o),
            CrewHub::bound_level_line(Level::Full),
        ];
        let end_state = [
            "Aim straight at the end state an order names",
            "Never hold the deliverable behind polish their instructions don't ask for",
        ];
        for hint in &said {
            for rule in end_state
                .iter()
                .chain(&["Run steps that don't depend on each other at once"])
            {
                assert!(hint.contains(rule), "{rule}: {hint}");
            }
        }
        // What it decides at Act fully by itself (a finished turn's next
        // step, a session's question, a plan) aims at the end state, with
        // no tools to run steps of its own.
        for decides in [
            super::super::idle::SYSTEM,
            super::super::answer::SYSTEM,
            super::super::answer::PLAN_SYSTEM,
        ] {
            for rule in end_state {
                assert!(decides.contains(rule), "{rule}: {decides}");
            }
            assert!(!decides.contains("Run steps"), "{decides}");
        }
        for act in [
            CrewHub::level_hint(Level::Act, &o),
            CrewHub::dm_hint(Level::Act, &o, false),
            CrewHub::bound_level_line(Level::Act),
        ] {
            assert!(!act.contains("end state"), "{act}");
        }
    })
    .await;
}

/// One text says what Act fully allows, everywhere the boss decides by it:
/// ordinary development (its own branch, PRs, merging a reviewed green
/// one), a gate only where a directive states one, asked at once.
#[test]
fn every_act_fully_text_carries_the_one_rule() {
    use super::super::watcher::{ACT_FULLY, STANDING_RULE};
    for says in [
        super::super::decide::SYSTEM,
        STANDING_RULE,
        CrewHub::bound_level_line(Level::Full).as_str(),
        super::super::tools::describe("crew.escalate"),
    ] {
        assert!(says.contains(ACT_FULLY), "{says}");
    }
    for words in [
        "pushing the session's own branch",
        "merging one that is green with a clean review at its head yourself (crew.github_merge), never by telling its session to",
        "A gate exists only if a directive states it: never invent one",
        "ask them at once with crew.ask_person, naming the directive",
    ] {
        assert!(ACT_FULLY.contains(words), "{words}");
    }
    assert!(!super::super::decide::SYSTEM.contains("pushing the default branch"));
    assert!(super::super::orchestrator::LOOP.contains("crew.github_merge"));
    assert!(super::super::orchestrator::LOOP.contains("Never hold a session without a directive"));
    assert!(super::super::orchestrator::LOOP
        .contains("Never tell a session to merge its own pull request"));
    assert!(CrewHub::bound_level_line(Level::Full).contains("never by the session"));
    assert!(super::super::decide::SYSTEM.contains("A session's own merge of a pull request"));
}

/// Desk's judgements run at no less than Medium, the person's higher
/// choice kept.
#[test]
fn decisions_run_at_the_effort_floor_or_the_persons_higher_choice() {
    use super::super::decide::{judging_effort, EFFORT_FLOOR};
    use workforce_agent::llm::anthropic::Effort;
    assert_eq!(EFFORT_FLOOR, Effort::Medium);
    assert_eq!(judging_effort(None), Effort::Medium);
    assert_eq!(judging_effort(Some(Effort::Low)), Effort::Medium);
    assert_eq!(judging_effort(Some(Effort::High)), Effort::High);
    assert_eq!(judging_effort(Some(Effort::Max)), Effort::Max);
}
