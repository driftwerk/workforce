//! A hold on the person's word carries its question (DW-755, 24 Nov: the
//! boss held MAN-544 "until the owner decides", never asked the person,
//! its own turns paused by the plan, and refused the session's read-only
//! `gh pr checks` for the hold).
//!
//! - The boss can't hold a session on the person's word until it has
//!   asked them: asked, the question is a needs-you post at once, on the
//!   session's tile until it is answered.
//! - With the boss's own turns held, a hold on the person's word already
//!   sent still reaches them at once, with no turn of the boss's.
//! - A hold stops new work, never reading: a read is allowed with no
//!   decision call, and the boss can't deny one; a write is still denied.

use workforce_desk::charge::How;
use workforce_desk::crew::Party;

use super::super::actions::{Action, Target, READ_NOT_DENIED};
use super::super::items::{ASK, HOLD_ASKS};
use super::decides_at_once::{asked, behavior, calls_about, decides, THEIRS};
use super::silent_waits::stopped;
use super::*;

/// The boss's words in the incident.
const HOLD: &str = "Stop polling CI and wait for Desk to send the scope once the owner decides.";

/// What MAN-544 ran while held.
const CHECKS: &str = "gh pr checks 878 --repo mantis-crm/monorepo";

const ACT: &str = "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true }\n\
                   updates = { enabled = false }\n";

async fn call(crew: &Arc<CrewHub>, name: &str, input: Value) -> workforce_agent::tools::ToolResult {
    let tool = super::super::tools::CrewTool::for_level(crew, "desk", &Default::default(), false)
        .into_iter()
        .find(|t| workforce_agent::tools::Tool::name(t) == name)
        .unwrap();
    workforce_agent::tools::Tool::execute(&tool, input)
        .await
        .unwrap()
}

/// Whether the person's DM has a needs-you post saying `has`.
fn needs_you(overseer: &AgentService, has: &str) -> bool {
    overseer
        .sessions
        .find(DESK_CHANNEL, PERSON)
        .unwrap()
        .is_some_and(|dm| {
            dm.messages
                .iter()
                .any(|m| m.content.starts_with("**Needs you.**") && m.content.contains(has))
        })
}

/// What the session's tile says waits on the person, once it does.
async fn on_its_tile(d: &Desk, s: uuid::Uuid) -> String {
    eventually("the question on its tile", async || {
        d.view(s).ok().and_then(|v| v.waits_on_you)
    })
    .await
}

async fn taken_on(d: &Desk, overseer: &AgentService, s: uuid::Uuid) {
    let boss = Party::manager("desk", &super::super::name_of(overseer));
    d.take_on(s, &boss, How::Handed, &Default::default(), &By::you())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hold_on_the_persons_word_is_sent_only_with_its_question_shown_until_answered() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let s = new_session(&d, dir.path(), "MAN-544").await;
        taken_on(&d, &overseer, s.id).await;
        let hold = json!({ "session_id": s.id.to_string(), "message": HOLD,
            "reason": "The scope is the owner's to set." });

        let refused = call(&crew, "crew.nudge", hold.clone()).await;
        assert!(!refused.success, "{:?}", refused.output);
        assert_eq!(refused.error.as_deref(), Some(HOLD_ASKS));
        let thread = d.line_thread(s.id, None, None).await.unwrap();
        assert!(!thread.iter().any(|m| m.text == HOLD), "never sent unasked");

        let ask = call(
            &crew,
            ASK,
            json!({
                "text": "MAN-544's fix is ready; ship the small scope (the fix) or the whole ticket?",
                "question": "Ship MAN-544's small scope, or the whole ticket?",
                "session_id": s.id.to_string(),
                "options": ["Small scope", "Whole ticket"],
                "holding_up": "MAN-544 holds until you decide",
            }),
        )
        .await;
        assert!(
            ask.success && ask.output.to_string().contains("Asked"),
            "{:?} {}",
            ask.error,
            ask.output
        );
        eventually("its needs-you post", async || {
            needs_you(&overseer, "Ship MAN-544's small scope").then_some(())
        })
        .await;
        let tile = on_its_tile(&d, s.id).await;
        assert!(
            tile.ends_with("asks you: Ship MAN-544's small scope, or the whole ticket?"),
            "{tile}"
        );
        assert!(d.view(s.id).unwrap().boss_has.is_none(), "it is the person's");

        let sent = call(&crew, "crew.nudge", hold).await;
        assert!(sent.success, "{:?}", sent.error);

        let item = d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.session == Some(s.id))
            .unwrap();
        crew.answer_item(item.id, "Small scope").await.unwrap();
        crew.look_at_items().await;
        eventually("its tile cleared", async || {
            d.view(s.id).unwrap().waits_on_you.is_none().then_some(())
        })
        .await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hold_on_the_persons_word_reaches_them_while_the_bosss_own_turns_are_held() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let dir = tempfile::tempdir().unwrap();
        let config = format!("{ACT}wake = {{ waiting_after_minutes = 60 }}\n");
        let (overseer, crew, d, llm) = overseeing(dir.path(), pool, "ok", &config, &[]).await;
        // No turn of the boss's runs, as while the plan's pause held them.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429).set_body_string(
                    "Background work is waiting: your weekly Claude plan is at 85%",
                ),
            )
            .with_priority(1)
            .mount(&llm)
            .await;
        let (s, _) = stopped(&d, dir.path(), "MAN-544").await;
        taken_on(&d, &overseer, s.id).await;
        let by = crew
            .by_overseer("The scope is the owner's to set.")
            .unwrap();
        crew.perform(
            &Action::Nudge {
                to: Target::Session(s.id),
                message: HOLD.into(),
                reply_to: None,
            },
            by,
        )
        .await
        .unwrap();
        let hold = d
            .line_thread(s.id, None, None)
            .await
            .unwrap()
            .into_iter()
            .rfind(|m| m.text == HOLD)
            .unwrap();
        let _ = d.store().line_went_in(hold.id).await;

        // At once, well within the hour's bound, and with no turn run.
        let item = tokio::time::timeout(
            Duration::from_secs(3),
            eventually("the hold given to the person", async || {
                d.store()
                    .items_not_closed(Some("desk"))
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|i| i.dedupe_key.ends_with(&hold.id.to_string()))
            }),
        )
        .await
        .expect("at once, not after the bound");
        assert_eq!(item.session, Some(s.id));
        eventually("its needs-you post", async || {
            needs_you(&overseer, "MAN-544").then_some(())
        })
        .await;
        let tile = on_its_tile(&d, s.id).await;
        assert!(tile.contains("asks you: MAN-544"), "{tile}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_during_a_hold_is_allowed_and_a_write_is_still_denied() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, crew, d, llm) = overseeing(&base, pool, "ok", THEIRS, &[]).await;
        d.boss_bounds_with(workforce_desk::charge::Bounds {
            home: base.clone(),
            ..Default::default()
        });
        // A boss that would deny whatever the held session asks.
        decides(
            &llm,
            "gh pr checks",
            "DENY: it is on hold until the owner decides\nINSTEAD: wait",
        )
        .await;
        decides(
            &llm,
            "npm install",
            "DENY: it is on hold until the owner decides\nINSTEAD: wait for the scope",
        )
        .await;
        eventually("Desk to know it is unbound", async || {
            d.boss_unbound("desk").then_some(())
        })
        .await;
        let s = new_session(&d, &base, "MAN-544").await;
        taken_on(&d, &overseer, s.id).await;
        let url = hook_url(&base, s.id).await;
        let by = crew
            .by_overseer("The scope is the owner's to set.")
            .unwrap();
        crew.perform(
            &Action::Nudge {
                to: Target::Session(s.id),
                message: HOLD.into(),
                reply_to: None,
            },
            by,
        )
        .await
        .unwrap();

        let read = tokio::time::timeout(Duration::from_secs(10), asked(&url, CHECKS))
            .await
            .expect("decided within seconds")
            .unwrap();
        assert_eq!(behavior(&read), "allow", "{read}");
        assert!(
            calls_about(&llm, "gh pr checks").await.is_empty(),
            "a read is allowed with no decision call"
        );

        let write =
            tokio::time::timeout(Duration::from_secs(10), asked(&url, "npm install left-pad"))
                .await
                .expect("decided within seconds")
                .unwrap();
        assert_eq!(behavior(&write), "deny", "{write}");
        let message = write["hookSpecificOutput"]["decision"]["message"].to_string();
        assert!(message.contains("on hold"), "{message}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_boss_cant_deny_a_read_of_a_session_it_holds() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, crew, d, _llm) = overseeing(&base, pool, "ok", ACT, &[]).await;
        let s = new_session(&d, &base, "MAN-544").await;
        taken_on(&d, &overseer, s.id).await;
        let url = hook_url(&base, s.id).await;
        let _read = asked(&url, CHECKS);
        let pending = eventually("the read waiting", async || d.view(s.id).unwrap().pending).await;
        let denied = call(
            &crew,
            "crew.deny",
            json!({ "session_id": s.id.to_string(), "approval_id": pending.id.to_string(),
                "reason": "It is on hold." }),
        )
        .await;
        assert!(!denied.success, "{:?}", denied.output);
        assert_eq!(denied.error.as_deref(), Some(READ_NOT_DENIED));
        assert_eq!(
            d.view(s.id).unwrap().pending.map(|p| p.id),
            Some(pending.id),
            "still the session's to run"
        );
    })
    .await;
}

/// At Act fully a hold is a directive's: one with no directive is refused;
/// one naming the person's directive and a condition holds the session
/// (it isn't closed) until the boss records the condition met, which
/// closes its question too.
#[tokio::test(flavor = "multi_thread")]
async fn at_act_fully_a_hold_names_its_directive_and_ends_when_its_condition_is_met() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        use super::super::hold::NAMES_DIRECTIVE;
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) =
            overseeing(dir.path(), pool, "ok", super::unbound::UNBOUND, &[]).await;
        crew.read_spent_today(&crew.overseers()).await;
        let s = new_session(&d, dir.path(), "MAN-835").await;
        taken_on(&d, &overseer, s.id).await;
        let asked = json!({
                "text": "MAN-835 is green; the release needs your OK to merge.",
                "question": "Merge MAN-835 now?",
                "session_id": s.id.to_string(),
                "options": ["Merge", "Wait"],
                "holding_up": "MAN-835's merge",
                "cant_decide": "Their directive says releases need their OK.",
            });
        let ask = call(&crew, ASK, asked.clone()).await;
        // At Act fully an ask is pushed back once to be checked against
        // the person's instructions; asked again, the same, it goes.
        assert!(!ask.success);
        let ask = call(&crew, ASK, asked.clone()).await;
        assert!(ask.success, "{:?}", ask.error);
        let item = d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.session == Some(s.id))
            .unwrap();
        let directive = d
            .store()
            .directive_own(&item.owner, "main", "Releases need my OK")
            .await
            .unwrap()
            .unwrap();
        let hold = |extra: Value| {
            let mut h = json!({ "session_id": s.id.to_string(), "message": HOLD,
                "reason": "A release waits on the person." });
            h.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            h
        };

        let unnamed = call(&crew, "crew.nudge", hold(json!({}))).await;
        assert_eq!(unnamed.error.as_deref(), Some(NAMES_DIRECTIVE));
        // 25 Nov 19:08: a hold in the boss's own words, with no directive.
        let own_words = call(
            &crew,
            "crew.nudge",
            json!({ "session_id": s.id.to_string(), "reason": "Waiting on the release.",
                "message": "Keep the hold in place on MAN-835: do not push, rerun or merge." }),
        )
        .await;
        assert_eq!(own_words.error.as_deref(), Some(NAMES_DIRECTIVE));
        let not_one = call(
            &crew,
            "crew.nudge",
            hold(json!({ "directive_id": uuid::Uuid::new_v4().to_string() })),
        )
        .await;
        assert!(!not_one.success);
        assert!(crew.hold_on(s.id).await.unwrap().is_none());

        let held = call(
            &crew,
            "crew.nudge",
            hold(json!({ "directive_id": directive.id.to_string(), "until": "CI green at the head" })),
        )
        .await;
        assert!(held.success, "{:?}", held.error);
        let h = crew.hold_on(s.id).await.unwrap().expect("held");
        assert_eq!(h.directive_id, directive.id);
        assert_eq!(
            h.until,
            super::super::hold::Until::Condition("CI green at the head".into())
        );
        let v = d.view(s.id).unwrap();
        assert!(crew.close_held(&v, "desk").await.is_some(), "held stays open");
        // Kept through a restart: a crew started afresh on the same Desk,
        // with nothing of this one's, finds it.
        let stop = CancellationToken::new();
        let restarted = CrewHub::start(
            hub(std::slice::from_ref(&overseer)),
            d.clone(),
            FAST,
            stop.clone(),
        );
        assert_eq!(restarted.hold_on(s.id).await.unwrap(), Some(h.clone()));
        stop.cancel();

        let met = call(
            &crew,
            "crew.nudge",
            json!({ "session_id": s.id.to_string(), "message": "CI is green: merge it.",
                "hold_met": "CI went green at the head", "reason": "Its condition was met." }),
        )
        .await;
        assert!(met.success, "{:?}", met.error);
        assert!(crew.hold_on(s.id).await.unwrap().is_none());
        assert!(!d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .iter()
            .any(|i| i.id == h.question));
        let again = call(
            &crew,
            "crew.nudge",
            json!({ "session_id": s.id.to_string(), "message": "go", "hold_met": "x", "reason": "r" }),
        )
        .await;
        assert!(!again.success, "nothing left to end");
    })
    .await;
}
