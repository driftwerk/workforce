//! The bosses' one live picture (designs/desk-boss-one-picture.md),
//! replayed from two real exchanges, anonymised: names, the client, PR
//! and run numbers changed; order, timing and what was said kept.
//!
//! - **16 Nov, 13:33–13:35.** A line turn started on a session's report;
//!   while it ran, the chat told the session not to write its own fix (a
//!   colleague owned it). The line turn, deciding on what it knew at its
//!   start, then told it "prepare your PR" and "carry on with the
//!   permanent PR", and the session built a duplicate fix.
//! - **15 Nov, 21:11–21:13.** The person's pause went out; twelve
//!   seconds later a focused call begun before it sent "the go-ahead for
//!   #639 is given"; then the session was told the same step, in new
//!   words, seven times in two minutes.

use std::future::Future;
use std::time::Duration;

use workforce_desk::picture::{Note, What};

use super::super::actions::{Action, Target};
use super::super::picture::stale;
use super::*;

const ACT: &str = "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true }\n";

fn nudge(to: uuid::Uuid, message: &str) -> Action {
    Action::Nudge {
        to: Target::Session(to),
        message: message.into(),
        reply_to: None,
    }
}

/// `f` run inside the boss's agent turn on runner session `sid`, as a
/// tool call of that turn runs.
async fn in_turn<F: Future>(sid: &str, f: F) -> F::Output {
    workforce_agent::runner::CURRENT_SESSION_ID
        .scope(sid.to_string(), f)
        .await
}

/// What the boss's line to `s` carries, oldest first.
async fn told(d: &Desk, s: uuid::Uuid) -> Vec<String> {
    d.line_thread(s, None, None)
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.from.kind == PartyKind::Manager)
        .map(|m| m.text)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_line_turn_on_an_old_picture_never_contradicts_the_chat() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        d.picture_load().await;
        let s = new_session(&d, dir.path(), "ledger-migration").await;
        let by = || crew.by_overseer("Reports are down.").unwrap();

        // 13:33:00 a line turn starts on the session's diagnosis.
        crew.picture_turn_began("line-ledger");
        // 13:33:1x the person, in the chat: the colleague owns the fix.
        let theirs = "Sam has found it and his fix is going up now. The session must not write its own.";
        crew.picture_person_told("desk", theirs);
        crew.picture_turn_began("dm");
        // 13:33:19 the chat's turn tells the session.
        let dont = "Sam has the fix for the reports (PR #648). Do NOT write your own fix: review his PR read-only when it appears.";
        let sent = in_turn("dm", crew.perform(&nudge(s.id, dont), by())).await.unwrap();
        assert!(sent.starts_with("told ledger-migration"), "{sent}");

        // 13:33:53 the line turn, on what it knew at 13:33:00.
        let prepare = "Good work. Yes, prepare option 2's PR now; don't merge it.";
        let refused = in_turn("line-ledger", crew.perform(&nudge(s.id, prepare), by()))
            .await
            .unwrap_err();
        assert!(stale(&refused), "{refused}");
        assert!(refused.contains("Do NOT write your own fix"), "it is told what changed: {refused}");
        assert!(refused.contains(theirs), "and the person's words: {refused}");

        // 13:33:56 the person asks in the chat, and its turn asks the session.
        crew.picture_person_told("desk", "Who made the dashboard embed change?");
        crew.picture_turn_began("dm");
        let status = "Who made the dashboard embed change? Answer in three lines from git, read-only, then carry on as you were.";
        in_turn("dm", crew.perform(&nudge(s.id, status), by())).await.unwrap();

        // 13:35:22 the line turn again, on a picture moved once more.
        let carry_on = "Until the person answers, don't queue or merge anything. Carry on with the permanent PR: accept both id forms, add tests, get an independent review.";
        let refused = in_turn("line-ledger", crew.perform(&nudge(s.id, carry_on), by()))
            .await
            .unwrap_err();
        assert!(stale(&refused), "{refused}");

        // One consistent instruction: the chat's, and nothing that crossed it.
        assert_eq!(told(&d, s.id).await, vec![dont.to_string(), status.to_string()]);
        // Told what changed, the line turn has nothing new to say: the
        // same step again isn't sent either.
        let review = "Understood: Sam owns the fix. Review #648 read-only and report.";
        let said = in_turn("line-ledger", crew.perform(&nudge(s.id, review), by()))
            .await
            .unwrap();
        assert!(said.starts_with("not sent: ledger-migration already has"), "{said}");
        assert_eq!(told(&d, s.id).await.len(), 2);
    })
    .await;
}

const WATCH: &str = "Keep watching production promotion 81234 until it succeeds, then enqueue #639 pinned to its head and confirm it merges. After that, stop and start nothing new.";

fn action_session(a: &Action) -> uuid::Uuid {
    match a {
        Action::Nudge {
            to: Target::Session(s),
            ..
        } => *s,
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relayed_pause_is_never_contradicted_and_one_step_is_told_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        d.picture_load().await;
        let s = new_session(&d, dir.path(), "acme-622").await;
        let by = || crew.by_overseer("Its work isn't done.").unwrap();

        // 21:11:19 the person's pause, relayed to the session by a turn.
        let (go, decided) = tokio::sync::oneshot::channel::<()>();
        let early = {
            let crew = Arc::clone(&crew);
            let action = nudge(
                s.id,
                "Desk's go-ahead for #639 is given. Check the release is live in production; if it is, enqueue #639 and watch it merge.",
            );
            tokio::spawn(async move {
                // A focused call begun before the pause, answering after it.
                crew.on_picture(async {
                    let _ = decided.await;
                    let by = || crew.by_overseer("Its work isn't done.").unwrap();
                    let refused = crew.perform(&action, by()).await;
                    // 21:12:00 decided again, on the pause it was told of.
                    let watch = nudge(action_session(&action), WATCH);
                    (refused, crew.perform(&watch, by()).await)
                })
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        crew.picture_person_told(
            "desk",
            "All work on the migration needs to pause, except what is in progress and almost finished.",
        );
        crew.picture_turn_began("dm");
        let pause = "The person has paused all migration work as of 21:10. Don't start anything new. Finish what is already running, report it, then stop.";
        in_turn("dm", crew.perform(&nudge(s.id, pause), by())).await.unwrap();
        go.send(()).unwrap();
        let (refused, sent) = early.await.unwrap();
        let refused = refused.unwrap_err();
        assert!(stale(&refused), "the go-ahead is never sent: {refused}");
        assert!(refused.contains("paused all migration work"), "{refused}");
        // Decided again on what it was told, the step it was about to run
        // anyway: the pause already says to finish it, so it isn't sent.
        let said = sent.unwrap();
        assert!(said.starts_with("not sent: acme-622 already has your instruction from"), "{said}");

        // 21:12:07–21:13:31: the session ends a turn each time it looks at
        // its watcher, and each time the step comes back in new words.
        for again in [
            "Check promotion 81234 now. If it has succeeded, enqueue #639 pinned to its head and confirm it merges. If it is still running, keep the watcher going.",
            "Check promotion 81234 now. If it has succeeded, enqueue #639 and confirm it merges; if it is still running, keep the watcher going and do it as soon as it finishes.",
            "Check promotion 81234 now; if it failed, report the failing job and don't enqueue #639.",
            "Check promotion 81234 now. When it succeeds, enqueue #639 pinned to its head and confirm it merges; if it fails, report the failing job.",
            "Check promotion 81234 now. If it has succeeded, enqueue #639. If it is still running, leave the watcher going and do that as soon as it finishes.",
            "Check promotion 81234 now. If it has succeeded, enqueue #639 and confirm it merges. If it is still running, wait on the watcher.",
            "Check promotion 81234 now. Once it succeeds, enqueue #639 and confirm it merges. If it fails, report it and don't enqueue.",
        ] {
            let said = crew
                .on_picture(crew.perform(&nudge(s.id, again), by()))
                .await
                .unwrap();
            assert!(said.starts_with("not sent: acme-622 already has your instruction from"), "{said}");
        }
        assert_eq!(
            told(&d, s.id).await,
            vec![pause.to_string()],
            "the pause, carried out: never the go-ahead, never seven steps"
        );
        let rows = d
            .store()
            .decisions_since("desk", chrono::DateTime::UNIX_EPOCH, 50)
            .await
            .unwrap();
        assert_eq!(rows.iter().filter(|r| r.session == Some(s.id)).count(), 1, "{rows:?}");

        // Once the session says something new, the next step goes.
        d.picture_note(
            Note::new(What::SessionReported, "Promotion 81234 succeeded; #639 is queued.").session(s.id),
        );
        let next = "Good. Confirm #639 merged and staging is healthy, then stop.";
        let sent = crew
            .on_picture(crew.perform(&nudge(s.id, next), by()))
            .await
            .unwrap();
        assert!(sent.starts_with("told acme-622"), "{sent}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_change_to_one_session_holds_no_other_back() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        d.picture_load().await;
        let mut sessions = Vec::new();
        for i in 0..12 {
            sessions.push(new_session(&d, dir.path(), &format!("s{i}")).await);
        }
        // Every call begins on the same picture; then the person types into
        // the first session only.
        let basis_taken = Arc::new(tokio::sync::Barrier::new(sessions.len() + 1));
        let (go_tx, _) = tokio::sync::broadcast::channel::<()>(1);
        let mut calls = Vec::new();
        for s in &sessions {
            let crew = Arc::clone(&crew);
            let id = s.id;
            let taken = Arc::clone(&basis_taken);
            let mut go = go_tx.subscribe();
            calls.push(tokio::spawn(async move {
                crew.on_picture(async {
                    taken.wait().await;
                    let _ = go.recv().await;
                    crew.perform(
                        &nudge(id, "Run the tests."),
                        crew.by_overseer("Next step.").unwrap(),
                    )
                    .await
                })
                .await
            }));
        }
        basis_taken.wait().await;
        d.picture_note(Note::new(What::PersonSaid, "leave this one to me").session(sessions[0].id));
        // The first session's lane is busy; the others' sends never wait on it.
        let held = crew.lane(sessions[0].id).await;
        go_tx.send(()).unwrap();
        let started = std::time::Instant::now();
        let mut results = Vec::new();
        for c in calls.drain(1..) {
            results.push(
                tokio::time::timeout(Duration::from_secs(5), c)
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        assert!(started.elapsed() < Duration::from_secs(5));
        for r in &results {
            assert!(r.as_ref().unwrap().starts_with("told s"), "{r:?}");
        }
        drop(held);
        let first = calls.pop().unwrap().await.unwrap().unwrap_err();
        assert!(
            stale(&first),
            "only the session the person touched decides again: {first}"
        );
    })
    .await;
}

/// The model's answer to the first request carrying `marker`, slow: the
/// call decides on what it was given as it began, and answers after
/// `delay`. `body` is the whole response: a tool call (streamed) or a
/// focused call's text.
async fn slow_once(llm: &wiremock::MockServer, marker: &str, delay: Duration, answer: Answer) {
    use wiremock::matchers::{body_partial_json, body_string_contains, method, path};
    use wiremock::{Mock, ResponseTemplate};
    let chunk = |body: Value| format!("data: {body}\n\n");
    let m = Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains(marker));
    let m = match answer {
        Answer::Tool(tool, arguments) => {
            let call = [
                chunk(json!({"id": "c0", "object": "chat.completion.chunk", "model": "mock",
                    "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{"index": 0,
                        "id": "call_1", "type": "function", "function": {"name": tool, "arguments": arguments.to_string()}}]},
                        "finish_reason": null}]})),
                chunk(json!({"id": "c0", "object": "chat.completion.chunk", "model": "mock",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}})),
                "data: [DONE]\n\n".to_string(),
            ]
            .concat();
            m.and(body_partial_json(json!({"stream": true})))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(call)
                        .set_delay(delay),
                )
        }
        Answer::Text(text) => m
            .and(body_string_contains(super::super::decide::MARK))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({
                        "id": "d1", "object": "chat.completion", "model": "mock",
                        "choices": [{"index": 0, "message": {"role": "assistant", "content": text},
                            "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
                    }))
                    .set_delay(delay),
            ),
    };
    m.up_to_n_times(1).with_priority(1).mount(llm).await;
}

enum Answer {
    Tool(&'static str, Value),
    Text(&'static str),
}

/// The bodies of every model request carrying `marker`.
async fn asked_with(llm: &wiremock::MockServer, marker: &str) -> Vec<String> {
    llm.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body).to_string())
        .filter(|b| b.contains(marker))
        .collect()
}

const DIAGNOSIS: &str =
    "Diagnosed the reports outage: the embed sends the old id form. Option 2 is the permanent fix.";
const THEIR_WORDS: &str =
    "Sam has found it and his fix is going up now. The session must not write its own.";

/// The 16 Nov crossing through the real path: the session writes on its
/// line, the boss's line turn runs (`run_turn`, its model slow), the
/// person speaks in the chat meanwhile, and the turn's own `crew_nudge`
/// is refused with what moved.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_line_turn_never_sends_what_it_decided_before_the_persons_words() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        d.picture_load().await;
        let s = new_session(&d, dir.path(), "ledger-migration").await;
        let prepare = "Good work. Yes, prepare option 2's PR now; don't merge it.";
        slow_once(
            &llm,
            "Option 2 is the permanent fix",
            Duration::from_millis(1500),
            Answer::Tool(
                "crew_nudge",
                json!({"session_id": s.id.to_string(), "message": prepare, "reason": "Its diagnosis is right."}),
            ),
        )
        .await;
        d.line_call(s.id, "tell", &json!({"to": "desk", "text": DIAGNOSIS}), None)
            .await
            .unwrap();
        eventually("the line turn deciding", async || {
            (!asked_with(&llm, "Option 2 is the permanent fix").await.is_empty()).then_some(())
        })
        .await;
        // 13:33:19, while it decides: the person, in the chat.
        crew.picture_person_told("desk", THEIR_WORDS);

        // Its tool call comes back refused, with the person's words.
        let told_it = eventually("the turn told what moved", async || {
            asked_with(&llm, "Not sent: since you decided this").await.into_iter().next()
        })
        .await;
        assert!(told_it.contains("must not write its own"), "{told_it}");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !told(&d, s.id).await.iter().any(|t| t.contains("option 2")),
            "{:?}",
            told(&d, s.id).await
        );
    })
    .await;
}

/// The Act fully idle sweep, the real path: the session's turn ends, the
/// focused call is slow, the person speaks meanwhile. Its step decided
/// before their words is never sent; the next sweep decides again, on a
/// brief that has their words, and that step goes.
#[tokio::test(flavor = "multi_thread")]
async fn the_idle_sweep_decides_again_on_the_persons_words_and_sends_only_that() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let full = super::decides_at_once::THEIRS.replace(
            "quiet_minutes = 30\n",
            "quiet_minutes = 30\nanswer_after_minutes = 30\n",
        );
        let (_overseer, crew, d, llm) = overseeing(&base, pool, "ok", &full, &[]).await;
        d.picture_load().await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        eventually("Desk to know it is unbound", async || {
            d.boss_unbound("desk").then_some(())
        })
        .await;
        slow_once(
            &llm,
            "Option 2 is the permanent fix",
            Duration::from_millis(1500),
            Answer::Text("CONTINUE: Prepare option 2's PR now.\nWHY: it is the permanent fix"),
        )
        .await;
        super::decides_at_once::decides(
            &llm,
            "Option 2 is the permanent fix",
            "CONTINUE: Review Sam's PR 648 read-only and report what it changes.\nWHY: Sam owns the fix",
        )
        .await;
        let s = new_session(&d, &base, "ledger-migration").await;
        let url = hook_url(&base, s.id).await;
        hook(
            &url,
            json!({"hook_event_name": "SessionStart", "session_id": "cc-ledger"}),
        )
        .await;
        hook(
            &url,
            json!({"hook_event_name": "Stop", "last_assistant_message": DIAGNOSIS}),
        )
        .await;
        eventually("the focused call deciding", async || {
            (!super::decides_at_once::calls_about(&llm, "Option 2 is the permanent fix")
                .await
                .is_empty())
            .then_some(())
        })
        .await;
        crew.picture_person_told("desk", THEIR_WORDS);

        // (Taking it on, the boss first says so.)
        let steps = async || {
            told(&d, s.id)
                .await
                .into_iter()
                .filter(|t| !t.starts_with("I'm running this session"))
                .collect::<Vec<_>>()
        };
        let sent = eventually("the step decided again", async || {
            steps().await.into_iter().next()
        })
        .await;
        assert!(sent.starts_with("Review Sam's PR 648"), "{sent}");
        let calls = super::decides_at_once::calls_about(&llm, "Option 2 is the permanent fix").await;
        assert!(calls.len() >= 2, "decided again: {}", calls.len());
        assert!(
            calls.last().unwrap().contains("must not write its own"),
            "the second call's brief has the person's words"
        );
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(steps().await, vec![sent], "the first step never went");
    })
    .await;
}
