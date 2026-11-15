//! At Act fully the boss answers every prompt its sessions raise, whether
//! their approvals are asked in Desk or in their terminal (a session
//! started by hand in a Desk terminal asks there until the person says
//! otherwise): what happened to Cufflink Migration on 11/14.
//!
//! - A request from a session whose terminal asks its approvals is held in
//!   Desk for the boss, decided at once, and its answer reaches Claude Code.
//! - While the person is in control of the session, it is theirs: its
//!   terminal asks it, and the boss neither decides nor says anything.
//! - A prompt only its terminal's own dialog answers is cleared by its
//!   boss (Esc), and the session is told to ask again, where the boss
//!   answers it; nothing is put to the person.
//! - A request answered already, raced by the boss's own turn, is said to
//!   it as done, never as a wait to pass on.

use super::decides_at_once::{calls_about, decides, said, THEIRS};
use super::*;
use workforce_desk::charge::How;
use workforce_desk::crew::Party;
use workforce_desk::model::{Approvals, SessionPatch};

fn asked(url: &str, command: &str) -> tokio::task::JoinHandle<Value> {
    let url = url.to_string();
    let body = json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash",
        "tool_input": {"command": command}});
    tokio::spawn(async move { hook(&url, body).await })
}

fn behavior(v: &Value) -> Value {
    v["hookSpecificOutput"]["decision"]["behavior"].clone()
}

/// What the boss said to the person in its DM that mentions `part` (its
/// own turns' prompts aside).
fn told(overseer: &AgentService, part: &str) -> Vec<String> {
    overseer
        .sessions
        .find(DESK_CHANNEL, PERSON)
        .ok()
        .flatten()
        .map(|dm| {
            dm.messages
                .iter()
                .filter(|m| m.role == workforce_common::session::MessageRole::Assistant)
                .filter(|m| m.content.contains(part))
                .map(|m| m.content.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// A session whose terminal asks its approvals, as one started by hand in
/// a Desk terminal does.
async fn asking_in_terminal(
    d: &Desk,
    base: &Path,
    name: &str,
) -> (workforce_desk::model::SessionView, String) {
    let s = new_session(d, base, name).await;
    let url = hook_url(base, s.id).await;
    d.update(
        s.id,
        SessionPatch {
            approvals: Some(Approvals::Terminal),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(d.view(s.id).unwrap().approvals, Approvals::Terminal);
    (s, url)
}

#[tokio::test(flavor = "multi_thread")]
async fn act_fully_decides_what_a_session_asks_in_its_terminal_and_the_person_in_control_keeps_it()
{
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, _crew, d, llm) = overseeing(&base, pool, "ok", THEIRS, &[]).await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        decides(&llm, "gh run view 4361", "ALLOW: it reads a CI run").await;
        decides(&llm, "git push origin main", "ALLOW: it ships as asked").await;
        eventually("Desk to know it is unbound", async || {
            d.boss_unbound("desk").then_some(())
        })
        .await;
        let (s, url) = asking_in_terminal(&d, &base, "Cufflink Migration").await;

        // Held for the boss, and allowed by it, with no person involved.
        let answer = tokio::time::timeout(Duration::from_secs(10), asked(&url, "gh run view 4361"))
            .await
            .expect("decided within seconds")
            .unwrap();
        assert_eq!(behavior(&answer), "allow", "{answer}");
        assert!(d.view(s.id).unwrap().pending.is_none());
        assert_eq!(calls_about(&llm, "gh run view 4361").await.len(), 1);
        assert_eq!(
            said(&overseer, &["terminal"]),
            0,
            "nothing said of a terminal"
        );

        // The person takes the wheel: what it asks is theirs, in its
        // terminal, at once, and the boss stays out of it.
        let boss = Party::manager("desk", &super::super::name_of(&overseer));
        d.take_on(s.id, &boss, How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        d.drive(s.id).await.unwrap();
        let answer =
            tokio::time::timeout(Duration::from_secs(10), asked(&url, "git push origin main"))
                .await
                .expect("its terminal asks it at once")
                .unwrap();
        assert_eq!(
            answer,
            json!({}),
            "Claude Code's own dialog asks the person"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            calls_about(&llm, "git push origin main").await.is_empty(),
            "the boss doesn't decide it"
        );
        let interrupted = d
            .activity(workforce_desk::crew::Filter::All, None, 100)
            .await
            .unwrap()
            .events
            .into_iter()
            .any(|e| e.kind == workforce_desk::crew::Kind::Interrupted);
        assert!(!interrupted, "no key is pressed in the person's terminal");
        assert_eq!(
            told(&overseer, "git push origin main"),
            Vec::<String>::new(),
            "nothing is said of it"
        );
    })
    .await;
}

/// The person takes control while a request is held for the boss: it goes
/// to their terminal's dialog at once, never left held where they can't see it.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_held_for_the_boss_goes_to_the_terminal_once_the_person_takes_control() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, _crew, d, llm) = overseeing(&base, pool, "ok", THEIRS, &[]).await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        // The boss is slow to decide it.
        {
            use wiremock::matchers::{body_string_contains, method, path};
            use wiremock::{Mock, ResponseTemplate};
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .and(body_string_contains(super::super::decide::MARK))
                .and(body_string_contains("make release"))
                .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(20)))
                .with_priority(1)
                .mount(&llm)
                .await;
        }
        eventually("Desk to know it is unbound", async || {
            d.boss_unbound("desk").then_some(())
        })
        .await;
        let (s, url) = asking_in_terminal(&d, &base, "Cufflink Migration").await;
        let waiting = asked(&url, "make release");
        eventually("it to be held for the boss", async || {
            d.view(s.id).unwrap().pending.filter(|p| p.answerable)
        })
        .await;
        let boss = Party::manager("desk", &super::super::name_of(&overseer));
        d.take_on(s.id, &boss, How::Handed, &Default::default(), &By::you())
            .await
            .unwrap();
        d.drive(s.id).await.unwrap();
        let answer = tokio::time::timeout(Duration::from_secs(10), waiting)
            .await
            .expect("let go to the terminal within a sweep")
            .unwrap();
        assert_eq!(answer, json!({}), "its terminal's dialog asks the person");
        assert!(!d.view(s.id).unwrap().pending.unwrap().answerable);
    })
    .await;
}

/// A prompt only its terminal's dialog answers (here, Claude Code says it
/// shows one, and no request reached Desk), while nobody is at the
/// session: its boss clears it and has the session ask again; nothing is
/// put to the person.
#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_only_the_terminal_answers_is_cleared_and_asked_again_in_desk() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, _crew, d, _llm) = overseeing(&base, pool, "ok", THEIRS, &[]).await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        eventually("Desk to know it is unbound", async || {
            d.boss_unbound("desk").then_some(())
        })
        .await;
        let s = new_session(&d, &base, "Cufflink Migration").await;
        let url = hook_url(&base, s.id).await;
        hook(
            &url,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "ship it"}),
        )
        .await;
        hook(
            &url,
            json!({"hook_event_name": "Notification", "notification_type": "permission_prompt",
                "message": "Claude needs your permission to use Bash"}),
        )
        .await;
        let p = d.view(s.id).unwrap().pending.expect("its dialog waits");
        assert!(!p.answerable);
        let message = eventually("the session to be told to ask again", async || {
            d.store()
                .line_inbox(&s.id.to_string())
                .await
                .unwrap()
                .into_iter()
                .find(|m| m.text.contains("pressed Esc"))
        })
        .await;
        assert!(
            message.text.contains("Run the call it was for again"),
            "{}",
            message.text
        );
        let interrupted = d
            .activity(workforce_desk::crew::Filter::All, None, 100)
            .await
            .unwrap()
            .events
            .into_iter()
            .any(|e| e.kind == workforce_desk::crew::Kind::Interrupted);
        assert!(interrupted, "Esc was pressed, as a decision of the boss");
        assert_eq!(said(&overseer, &["answer it there"]), 0);
        assert_eq!(
            said(&overseer, &["waiting on you"]),
            0,
            "nothing put to the person"
        );
    })
    .await;
}

/// The boss's own turn answers a request its decision call answered a
/// moment before: it is told that is done, not that something waits.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_raced_by_the_bosss_turn_is_said_as_done() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, crew, d, llm) = overseeing(&base, pool, "ok", THEIRS, &[]).await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        decides(&llm, "cargo test -p leads", "ALLOW: it runs tests").await;
        eventually("Desk to know it is unbound", async || {
            d.boss_unbound("desk").then_some(())
        })
        .await;
        let (s, url) = asking_in_terminal(&d, &base, "Cufflink Migration").await;
        // Decided at once, it shows only for a moment: seen as it comes.
        let (_, mut events) = d.subscribe();
        let waiting = asked(&url, "cargo test -p leads");
        let p = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(workforce_desk::model::DeskEvent::Session { session }) =
                    events.recv().await
                {
                    if let Some(p) = session
                        .pending
                        .filter(|p| session.id == s.id && p.answerable)
                    {
                        break p;
                    }
                }
            }
        })
        .await
        .expect("it to be held");
        let answer = tokio::time::timeout(Duration::from_secs(10), waiting)
            .await
            .expect("decided")
            .unwrap();
        assert_eq!(behavior(&answer), "allow", "{answer} {p:?}");
        let late = crew
            .perform(
                &super::super::actions::Action::Approve {
                    session: s.id,
                    approval: p.id,
                },
                By::manager("desk", &super::super::name_of(&overseer), "allowing it"),
            )
            .await
            .expect_err("nothing is answered twice");
        assert!(
            late.starts_with("Already answered") && late.contains("nothing waits"),
            "{late}"
        );
        assert!(!late.contains("terminal is asking"), "{late}");
    })
    .await;
}
