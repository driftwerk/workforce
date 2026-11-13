//! Act fully (`level = "full"`) is unbound: in any repository, named or
//! not, Desk allows what merges, tags, promotes and deploys the moment a
//! session asks; only its budget, Stop and the activity log hold it.

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
/// merging, tagging, publishing and deploying are allowed the moment a
/// session asks, in a session it runs and in one it doesn't.
#[tokio::test(flavor = "multi_thread")]
async fn act_fully_ships_in_any_repository_at_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let acme = base.join("acme-web");
        let globex = base.join("globex-api");
        repo(&acme, "git@github.com:acme/acme-web.git");
        repo(&globex, "git@github.com:globex/globex-api.git");
        let (overseer, crew, d, _llm) = overseeing(&base, pool, "ok", FULL, &[]).await;
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
                "gh pr merge 42 --squash",
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
        let (overseer, crew, d, _llm) = overseeing(
            &base,
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"full\"\nfull = { budget_usd_per_day = 25 }\n",
            &[],
        )
        .await;
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
