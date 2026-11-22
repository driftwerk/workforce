//! crew.inside: the boss reads inside its own sessions itself, refused in
//! one sentence for any id it can't see, every facet fenced as data and no
//! secret in any of it; ask_session is its last resort in every text.

use super::*;
use std::borrow::Cow;
use workforce_desk::memory::{Redacted, Redactor};

const SECRET: &str = "SECRET-abc";

/// Hides [`SECRET`], as the hub's patterns would.
struct Hides;

impl Redactor for Hides {
    fn stored<'a>(&self, text: &'a str) -> Redacted<'a> {
        match text.contains(SECRET) {
            true => Redacted {
                text: Cow::Owned(text.replace(SECRET, "[secret]")),
                count: 1,
            },
            false => Redacted {
                text: Cow::Borrowed(text),
                count: 0,
            },
        }
    }
}

/// An overseer at Act over a Desk that hides [`SECRET`].
async fn overseeing_hidden(
    dir: &Path,
    pool: workforce_postgres::PostgresPool,
) -> (Arc<AgentService>, Desk, wiremock::MockServer) {
    let (overseer, llm) = agent(dir, "desk", "ok").await;
    write_config(&overseer, "[oversight]\nenabled = true\nlevel = \"act\"\n");
    let floor = workforce_desk::Floor {
        redactor: Some(Arc::new(Hides)),
        ..Default::default()
    };
    let d = desk_with(pool, dir, floor).await;
    CrewHub::start(
        hub(&[Arc::clone(&overseer)]),
        d.clone(),
        FAST,
        CancellationToken::new(),
    );
    eventually("the overseer's tools", async || {
        overseer.tools.has(super::super::inside::NAME).then_some(())
    })
    .await;
    (overseer, d, llm)
}

/// A repository in `dir` with a commit and an uncommitted file that both
/// name [`SECRET`].
fn repo_with_secrets(dir: &Path) {
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    };
    git(&["init", "-q", "-b", "dw-1"]);
    std::fs::write(dir.join("a.txt"), "a").unwrap();
    git(&["add", "a.txt"]);
    git(&[
        "commit",
        "-q",
        "-m",
        &format!("Keep the token {SECRET} out"),
    ]);
    std::fs::write(dir.join(format!("{SECRET}.txt")), "b").unwrap();
}

async fn inside(overseer: &AgentService, input: Value) -> workforce_agent::tools::ToolResult {
    overseer
        .tools
        .get(super::super::inside::NAME)
        .unwrap()
        .execute(input)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_id_it_cant_see_is_refused_as_none_and_an_agents_id_points_at_its_history() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, _d, _llm) = overseeing_hidden(dir.path(), pool).await;
        let made_up = inside(
            &overseer,
            json!({"session_id": uuid::Uuid::new_v4().to_string()}),
        )
        .await;
        assert!(!made_up.success);
        assert_eq!(made_up.error.as_deref(), Some("That isn't one of yours."));
        let agent = inside(&overseer, json!({"session_id": "scout"})).await;
        assert!(agent.error.unwrap().contains("crew.history (agent_id)"));
        let every = inside(&overseer, json!({"session_id": "*", "facet": "logs"})).await;
        assert!(every.error.unwrap().contains("facet glance only"));
        let facet = inside(
            &overseer,
            json!({"session_id": uuid::Uuid::new_v4().to_string(), "facet": "screen"}),
        )
        .await;
        assert!(facet.error.unwrap().starts_with("facet is one of glance,"));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn every_facet_is_fenced_and_names_no_secret() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, d, _llm) = overseeing_hidden(dir.path(), pool).await;
        let work = dir.path().join("work");
        std::fs::create_dir(&work).unwrap();
        repo_with_secrets(&work);
        let s = new_session(&d, &work, "acme-web").await;
        let id = s.id.to_string();
        for facet in ["glance", "processes", "workspace"] {
            let r = inside(&overseer, json!({"session_id": id, "facet": facet})).await;
            assert!(r.success, "{facet}: {r:?}");
            let out = r.output.to_string();
            assert!(!out.contains(SECRET), "{facet}: {out}");
            let said = r.output[facet].as_str().unwrap();
            assert!(
                said.starts_with("<<<EXTERNAL_UNTRUSTED_CONTENT_")
                    || facet == "processes" && !said.contains('<'),
                "{facet}: {said}"
            );
        }
        let ws = inside(&overseer, json!({"session_id": id, "facet": "workspace"})).await;
        let said = ws.output["workspace"].as_str().unwrap();
        for seen in [
            "\"branch\":\"dw-1\"",
            "Keep the token [secret] out",
            "[secret].txt",
            "Not a GitHub repository.",
        ] {
            assert!(said.contains(seen), "{seen}: {said}");
        }
        let glance = inside(&overseer, json!({"session_id": id})).await;
        let said = glance.output["glance"].as_str().unwrap();
        assert!(
            said.contains("Workspace: branch dw-1, 1 not pushed, 1 uncommitted"),
            "{said}"
        );
        let logs = inside(&overseer, json!({"session_id": id, "facet": "logs"})).await;
        assert!(logs.error.unwrap().contains("no Desk logs"));
        let account = overseer
            .tools
            .get("crew.account")
            .unwrap()
            .execute(json!({"session_id": id}))
            .await
            .unwrap()
            .output;
        let seen = account["inside"].as_str().unwrap_or_default();
        assert!(!account.to_string().contains(SECRET), "{account}");
        assert!(
            !d.view(s.id).unwrap().live || seen.contains("Workspace: branch dw-1"),
            "{account}"
        );
    })
    .await;
}

#[test]
fn ask_session_is_the_last_resort_and_every_text_names_crew_inside_first() {
    use super::super::{context, items, picture, tools, watcher};
    let ask = tools::describe("crew.ask_session");
    assert!(
        ask.starts_with("Last resort") && ask.contains("crew.inside"),
        "{ask}"
    );
    let named_first = |text: &str| match (text.find("crew.inside"), text.find("crew.ask_session")) {
        (_, None) => true,
        (Some(i), Some(a)) => i < a,
        (None, Some(_)) => false,
    };
    for text in [
        watcher::CHECK_FIRST,
        context::CHECK_FIRST,
        picture::NOTES_SPENT,
        items::PROMISE_DESCRIPTION,
        tools::NOT_IN_RECORD,
        ask,
    ] {
        assert!(text.contains("crew.inside"), "{text}");
        assert!(named_first(text), "{text}");
    }
    for name in tools::READ.iter().chain(tools::ACT.iter()) {
        assert!(named_first(tools::describe(name)), "{name}");
    }
}
