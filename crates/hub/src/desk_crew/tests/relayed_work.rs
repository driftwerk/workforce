//! A boss relayed from the home on a session the person started on a
//! machine (designs/boss-takes-on-machine-sessions.md): on devbox, Desk (the
//! Mac's main overseer, `home/desk`) takes on VEL-9, in no group there,
//! under devbox's Ungrouped grant, and hears what it says and does by its
//! home's takes.

use super::relayed_decide::post;
use super::served::native;
use super::*;
use workforce_desk::line::Asked;

use super::super::relayed_work::{Taken, WORK_TAKE};
use super::super::served::{ServedCall, ServedStatus};

const HOME_DESK: &str = "home/desk";

fn ungrouped(revision: i64, members: &[uuid::Uuid], take_on_all: bool) -> Value {
    json!({"grantVersion": 1, "revision": revision, "name": "Ungrouped", "boss": "local:desk",
           "level": "act", "sees": "replies", "members": members,
           "act": {"nudge": true, "interrupt": true, "take_on_all": take_on_all}})
}

async fn put_ungrouped(d: &Desk, revision: i64, members: &[uuid::Uuid], take_on_all: bool) {
    let body = ungrouped(revision, members, take_on_all);
    let (code, out) = native(d, "PUT", "/api/desk/grants/ungrouped", body).await;
    assert_eq!(code, StatusCode::OK, "{out}");
}

/// The home's call under devbox's Ungrouped grant, as Desk posts it, with
/// the home letting its boss take on every session or not.
fn relayed(tool: &str, input: Value, take_on_all: bool) -> Value {
    json!({"group": "ungrouped", "tool": tool, "input": input, "boss": "local:desk",
           "linkId": "home-relay", "id": uuid::Uuid::new_v4().to_string(), "level": "act",
           "act": {"nudge": true, "interrupt": true, "take_on_all": take_on_all}})
}

fn take_on(id: uuid::Uuid, all: bool) -> Value {
    relayed(
        "crew.take_on",
        json!({"session_id": id, "reason": "The person asked."}),
        all,
    )
}

async fn take(app: &axum::Router, token: &str, input: Value) -> Taken {
    let out = post(app, token, relayed(WORK_TAKE, input, true)).await;
    assert_eq!(out["status"], "ok", "{out}");
    serde_json::from_value(out["body"].clone()).unwrap()
}

/// A session the person started on `d` and stopped: one running without
/// Desk's closed check of each call is never a served boss's.
pub(super) async fn person_started(
    d: &Desk,
    dir: &Path,
    name: &str,
) -> workforce_desk::model::SessionView {
    let s = new_session(d, dir, name).await;
    d.stop(s.id, Some(By::you())).unwrap();
    eventually("stopped", async || d.servable_launch(s.id).then_some(())).await;
    s
}

/// devbox, overseeing nothing of its own, with VEL-9, VEL-15 and VEL-20 in
/// its Ungrouped grant for the Mac's Desk, and notes outside it.
async fn devbox(
    dir: &Path,
    pool: workforce_postgres::PostgresPool,
) -> (Arc<CrewHub>, Desk, [workforce_desk::model::SessionView; 4]) {
    let (_any, crew, d, _llm) =
        overseeing(dir, pool, "ok", "[oversight]\nenabled = false\n", &[]).await;
    let sessions = [
        person_started(&d, dir, "VEL-9").await,
        person_started(&d, dir, "VEL-15").await,
        person_started(&d, dir, "VEL-20").await,
        person_started(&d, dir, "notes").await,
    ];
    let members: Vec<uuid::Uuid> = sessions[..3].iter().map(|s| s.id).collect();
    put_ungrouped(&d, 1, &members, true).await;
    (crew, d, sessions)
}

/// On devbox: the home's boss takes on a session of its grant only as both
/// the grant and its home let it, once however often it is sent, on the
/// relay alone; the person hands one to it from devbox's own Desk.
#[tokio::test(flavor = "multi_thread")]
async fn the_homes_boss_takes_on_a_machines_ungrouped_session_only_as_both_ends_let_it() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (crew, d, [vel9, vel15, vel20, notes]) = devbox(dir.path(), pool).await;
        let app = super::super::relay::routes(Arc::clone(&crew.state), Arc::clone(&crew));
        let token = crew.state.auth_token.clone();
        let from = chrono::Utc::now() - chrono::Duration::hours(1);

        let once = take_on(vel9.id, true);
        let out = post(&app, &token, once.clone()).await;
        assert_eq!(out["status"], "ok", "{out}");
        let c = d.charge_of(vel9.id).expect("taken on");
        assert_eq!(c.boss, HOME_DESK);
        assert_eq!(c.how, workforce_desk::charge::How::Taken);
        let again = post(&app, &token, once).await;
        assert_eq!(again["body"]["again"], json!(true), "done once: {again}");
        let noted = d
            .store()
            .events_since(from, &[vel9.id.to_string()], 20, None)
            .await
            .unwrap();
        assert!(
            noted
                .iter()
                .any(|e| e.kind == workforce_desk::crew::Kind::TakenOn),
            "{noted:?}"
        );

        // Its home doesn't let it take on every session: the person is
        // offered it instead.
        let out = post(&app, &token, take_on(vel15.id, false)).await;
        assert_eq!(out["status"], "suggested", "{out}");
        assert!(d.charge_of(vel15.id).is_none());
        // Nor does its grant there: the same, whatever its home says.
        put_ungrouped(&d, 2, &[vel9.id, vel15.id, vel20.id], false).await;
        let out = post(&app, &token, take_on(vel15.id, true)).await;
        assert_eq!(out["status"], "suggested", "{out}");
        assert!(d.charge_of(vel15.id).is_none());
        // Not one of the grant's: refused.
        let out = post(&app, &token, take_on(notes.id, true)).await;
        assert_eq!(out["status"], "refused", "{out}");
        assert!(d.charge_of(notes.id).is_none());
        // Never on a link of its own: the relay's alone.
        let r = crew
            .serve_crew(
                ServedCall {
                    tool: "crew.take_on".into(),
                    input: json!({"session_id": vel15.id}),
                    boss: workforce_desk::grant::BossRef::Local {
                        agent: "desk".into(),
                    },
                    link_id: Some("link-of-mine".into()),
                    call_id: Some("c-1".into()),
                    ceiling: None,
                },
                "ungrouped",
            )
            .await;
        assert_eq!(r.status, ServedStatus::Refused, "{:?}", r.body);
        assert!(d.charge_of(vel15.id).is_none());

        // The person hands one in no group to it from devbox's own Desk.
        let (code, out) = native(
            &d,
            "POST",
            &format!("/api/desk/sessions/{}/charge", vel20.id),
            json!({"boss": HOME_DESK}),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{out}");
        assert_eq!(d.charge_of(vel20.id).unwrap().boss, HOME_DESK);
    })
    .await;
}

/// On devbox: a take hands its home what the sessions its boss runs did
/// since the home's cursor (or devbox's own), what they ask it and report
/// to it on Desk's line, each report once acknowledged; while its home
/// takes, the sessions reach that boss as "desk", and once it doesn't,
/// what they asked is the person's.
#[tokio::test(flavor = "multi_thread")]
async fn a_take_hands_the_home_its_sessions_work_and_their_line_waits_for_it_while_it_takes() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (crew, d, [vel9, vel15, _, _]) = devbox(dir.path(), pool).await;
        let app = super::super::relay::routes(Arc::clone(&crew.state), Arc::clone(&crew));
        let token = crew.state.auth_token.clone();
        let out = post(&app, &token, take_on(vel9.id, true)).await;
        assert_eq!(out["status"], "ok", "{out}");

        let first = take(&app, &token, json!({})).await;
        assert_eq!(
            first.sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![vel9.id],
            "the sessions it runs alone"
        );
        assert_eq!(first.sessions[0].name, "VEL-9");
        assert!(first.events.is_empty(), "nothing before its first take");

        // VEL-9 asks Desk, and tells it: its home's boss is "desk".
        let asked = d
            .line_call(
                vel9.id,
                "ask",
                &json!({"to": "desk", "text": "Ship VEL-9 now?", "wait_seconds": 60}),
                Some(Duration::from_millis(10)),
            )
            .await
            .unwrap();
        let ask: uuid::Uuid = serde_json::from_value(asked["id"].clone()).unwrap();
        assert_eq!(
            d.store().line_message(ask).await.unwrap().unwrap().to.id,
            HOME_DESK
        );
        let told = d
            .line_call(
                vel9.id,
                "tell",
                &json!({"to": "desk", "text": "Tests pass."}),
                None,
            )
            .await
            .unwrap();
        let report: uuid::Uuid = serde_json::from_value(told["id"].clone()).unwrap();
        // VEL-15, which it doesn't run, reaches the person as "desk".
        let other = d
            .line_call(
                vel15.id,
                "tell",
                &json!({"to": "desk", "text": "Not yours."}),
                None,
            )
            .await
            .unwrap();
        let other: uuid::Uuid = serde_json::from_value(other["id"].clone()).unwrap();
        assert_ne!(
            d.store().line_message(other).await.unwrap().unwrap().to.id,
            HOME_DESK
        );

        // Its report waits for the home; its question stays open for it.
        let next = eventually("its report kept for the home", async || {
            let t = take(&app, &token, json!({})).await;
            t.reports.iter().any(|r| r.said.id == report).then_some(t)
        })
        .await;
        assert_eq!(next.asks.len(), 1, "{next:?}");
        assert_eq!(next.asks[0].said.id, ask);
        assert_eq!(next.asks[0].session, vel9.id);
        assert!(next.asks[0].said.text.contains("Ship VEL-9 now?"));
        assert!(next.reports.iter().all(|r| r.said.id != other));
        // Acknowledged, it is taken once.
        let after = take(&app, &token, json!({"taken": [report, other]})).await;
        assert!(after.reports.is_empty(), "{after:?}");
        assert_eq!(
            d.store()
                .line_message(other)
                .await
                .unwrap()
                .unwrap()
                .delivery,
            workforce_desk::line::Delivery::Delivered,
            "another's report isn't touched"
        );
        // A home that restarted (no cursor) reads on from devbox's.
        let resumed = take(&app, &token, json!({})).await;
        assert!(resumed.now >= after.now);
        // A peek leaves the cursor where it was.
        let peeked = take(&app, &token, json!({"peek": true})).await;
        assert_eq!(peeked.asks.len(), 1);

        // Its home stops taking: what VEL-9 asked is the person's.
        crew.homes_gone();
        let q = eventually("its question given to the person", async || {
            d.store()
                .line_message(ask)
                .await
                .unwrap()
                .filter(|q| q.asked == Some(Asked::Escalated))
        })
        .await;
        assert!(q.escalation.is_some(), "{q:?}");
        // And what it asks from now goes straight there.
        let later = d
            .line_call(
                vel9.id,
                "tell",
                &json!({"to": "desk", "text": "Done."}),
                None,
            )
            .await
            .unwrap();
        let later: uuid::Uuid = serde_json::from_value(later["id"].clone()).unwrap();
        assert_ne!(
            d.store().line_message(later).await.unwrap().unwrap().to.id,
            HOME_DESK
        );
    })
    .await;
}

/// On devbox: an event stamped before the last a take handed back, but
/// saved after that take, is handed back by the next, and never again.
#[tokio::test(flavor = "multi_thread")]
async fn an_event_saved_late_is_still_handed_back_and_only_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (crew, d, [vel9, _, _, _]) = devbox(dir.path(), pool).await;
        let app = super::super::relay::routes(Arc::clone(&crew.state), Arc::clone(&crew));
        let token = crew.state.auth_token.clone();
        let out = post(&app, &token, take_on(vel9.id, true)).await;
        assert_eq!(out["status"], "ok", "{out}");
        take(&app, &token, json!({})).await;
        let handed = |text: &str| {
            workforce_desk::crew::Event::new(
                workforce_desk::crew::Kind::HandedBack,
                workforce_desk::crew::Party::you(),
                Some(workforce_desk::crew::Party::session(&vel9)),
                text.to_string(),
            )
        };
        let first = handed("back once");
        d.store().add_event(&first).await.unwrap();
        let read = take(&app, &token, json!({})).await;
        assert!(read.events.iter().any(|e| e.id == first.id), "{read:?}");
        let cursor = read.after.clone().expect("read on past it");
        // Stamped before what that take read, saved only now.
        let mut late = handed("back late");
        late.at = first.at - chrono::Duration::seconds(1);
        d.store().add_event(&late).await.unwrap();
        let next = take(&app, &token, json!({"after": cursor})).await;
        assert_eq!(
            next.events
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            [late.id.as_str()],
            "the late one alone"
        );
        assert_eq!(next.after, read.after, "never moved back");
        let again = take(&app, &token, json!({"after": cursor})).await;
        assert!(again.events.is_empty(), "{again:?}");
    })
    .await;
}

/// On devbox: a question VEL-9 asked the home's boss is the person's once
/// Desk lets that boss go, its grant stopped, though its home still takes.
#[tokio::test(flavor = "multi_thread")]
async fn a_question_to_the_homes_boss_is_the_persons_once_desk_lets_the_boss_go() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (crew, d, [vel9, _, _, _]) = devbox(dir.path(), pool).await;
        let app = super::super::relay::routes(Arc::clone(&crew.state), Arc::clone(&crew));
        let token = crew.state.auth_token.clone();
        let out = post(&app, &token, take_on(vel9.id, true)).await;
        assert_eq!(out["status"], "ok", "{out}");
        take(&app, &token, json!({})).await;
        let asked = d
            .line_call(
                vel9.id,
                "ask",
                &json!({"to": "desk", "text": "Ship VEL-9 now?", "wait_seconds": 60}),
                Some(Duration::from_millis(10)),
            )
            .await
            .unwrap();
        let ask: uuid::Uuid = serde_json::from_value(asked["id"].clone()).unwrap();

        let (code, _) = native(
            &d,
            "POST",
            "/api/desk/bosses/stop",
            json!({"group": "ungrouped", "from": "desk"}),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        // Its home took just now: it isn't for not reading that it is given.
        assert!(crew.home_reads(HOME_DESK, Some(vel9.id)));
        let q = eventually("its question given to the person", async || {
            d.store()
                .line_message(ask)
                .await
                .unwrap()
                .filter(|q| q.asked == Some(Asked::Escalated))
        })
        .await;
        assert!(d.charge_of(vel9.id).is_none_or(|c| !c.live()), "let go");
        assert!(
            q.escalation
                .as_ref()
                .is_some_and(|e| e.why.contains("no longer runs this session")),
            "{q:?}"
        );
    })
    .await;
}

/// On devbox: what a take handed in an answer that never reached the home
/// is handed again by the home's next take, made from where it last heard.
#[tokio::test(flavor = "multi_thread")]
async fn an_event_whose_answer_was_lost_is_handed_again() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (crew, d, [vel9, _, _, _]) = devbox(dir.path(), pool).await;
        let app = super::super::relay::routes(Arc::clone(&crew.state), Arc::clone(&crew));
        let token = crew.state.auth_token.clone();
        let out = post(&app, &token, take_on(vel9.id, true)).await;
        assert_eq!(out["status"], "ok", "{out}");
        take(&app, &token, json!({})).await;
        let handed = |text: &str| {
            workforce_desk::crew::Event::new(
                workforce_desk::crew::Kind::HandedBack,
                workforce_desk::crew::Party::you(),
                Some(workforce_desk::crew::Party::session(&vel9)),
                text.to_string(),
            )
        };
        let first = handed("back once");
        d.store().add_event(&first).await.unwrap();
        let heard = take(&app, &token, json!({})).await;
        assert!(heard.events.iter().any(|e| e.id == first.id), "{heard:?}");
        let cursor = heard.after.clone().expect("read on past it");
        let next = handed("back again");
        d.store().add_event(&next).await.unwrap();
        // Handed, but its answer never reaches the home.
        let lost = take(&app, &token, json!({"after": cursor})).await;
        assert!(lost.events.iter().any(|e| e.id == next.id), "{lost:?}");
        let again = take(&app, &token, json!({"after": cursor})).await;
        assert!(again.events.iter().any(|e| e.id == next.id), "{again:?}");
    })
    .await;
}

/// On devbox: a question VEL-9 asked the home's boss is the person's once
/// that boss lets VEL-9 go itself.
#[tokio::test(flavor = "multi_thread")]
async fn a_question_to_the_homes_boss_is_the_persons_once_the_boss_lets_its_session_go() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (crew, d, [vel9, _, _, _]) = devbox(dir.path(), pool).await;
        let app = super::super::relay::routes(Arc::clone(&crew.state), Arc::clone(&crew));
        let token = crew.state.auth_token.clone();
        let out = post(&app, &token, take_on(vel9.id, true)).await;
        assert_eq!(out["status"], "ok", "{out}");
        take(&app, &token, json!({})).await;
        let asked = d
            .line_call(
                vel9.id,
                "ask",
                &json!({"to": "desk", "text": "Ship VEL-9 now?", "wait_seconds": 60}),
                Some(Duration::from_millis(10)),
            )
            .await
            .unwrap();
        let ask: uuid::Uuid = serde_json::from_value(asked["id"].clone()).unwrap();

        let release = relayed(
            "crew.release",
            json!({"session_id": vel9.id, "reason": "The person runs it now."}),
            true,
        );
        let out = post(&app, &token, release).await;
        assert_eq!(out["status"], "ok", "{out}");
        let q = eventually("its question given to the person", async || {
            d.store()
                .line_message(ask)
                .await
                .unwrap()
                .filter(|q| q.asked == Some(Asked::Escalated))
        })
        .await;
        assert!(d.charge_of(vel9.id).is_none_or(|c| !c.live()), "let go");
        assert!(
            q.escalation
                .as_ref()
                .is_some_and(|e| e.why.contains("Its boss let this session go")),
            "{q:?}"
        );
    })
    .await;
}

/// On devbox: the home goes on taking under its Acme grant there but no
/// longer under Ungrouped, as a Mac that can't read devbox's sessions does:
/// what VEL-9, in Ungrouped, asked its boss is the person's.
#[tokio::test(flavor = "multi_thread")]
async fn a_question_is_the_persons_once_its_home_takes_only_under_another_grant() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (crew, d, [vel9, _, _, notes]) = devbox(dir.path(), pool).await;
        let app = super::super::relay::routes(Arc::clone(&crew.state), Arc::clone(&crew));
        let token = crew.state.auth_token.clone();
        let mut acme = ungrouped(1, &[notes.id], true);
        acme["name"] = json!("Acme");
        let (code, out) = native(&d, "PUT", "/api/desk/grants/acme", acme).await;
        assert_eq!(code, StatusCode::OK, "{out}");
        let out = post(&app, &token, take_on(vel9.id, true)).await;
        assert_eq!(out["status"], "ok", "{out}");
        take(&app, &token, json!({})).await;
        let asked = d
            .line_call(
                vel9.id,
                "ask",
                &json!({"to": "desk", "text": "Ship VEL-9 now?", "wait_seconds": 60}),
                Some(Duration::from_millis(10)),
            )
            .await
            .unwrap();
        let ask: uuid::Uuid = serde_json::from_value(asked["id"].clone()).unwrap();

        crew.homes_gone();
        let mut under_acme = relayed(WORK_TAKE, json!({}), true);
        under_acme["group"] = json!("acme");
        let q = eventually("its question given to the person", async || {
            let out = post(&app, &token, under_acme.clone()).await;
            assert_eq!(out["status"], "ok", "{out}");
            d.store()
                .line_message(ask)
                .await
                .unwrap()
                .filter(|q| q.asked == Some(Asked::Escalated))
        })
        .await;
        assert!(crew.home_reads(HOME_DESK, Some(notes.id)), "Acme's taken");
        assert!(
            q.escalation
                .as_ref()
                .is_some_and(|e| e.why.contains("doesn't reach this session")),
            "{q:?}"
        );
    })
    .await;
}

/// On devbox: its Ungrouped grant Desk just wrote again, its home not yet
/// taking under it and the hub long started: what its first session asks its boss
/// waits for the home's take, and isn't the person's.
#[tokio::test(flavor = "multi_thread")]
async fn a_question_under_a_grant_just_written_waits_for_its_homes_first_take() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (crew, d, [vel9, vel15, vel20, _]) = devbox(dir.path(), pool).await;
        let app = super::super::relay::routes(Arc::clone(&crew.state), Arc::clone(&crew));
        let token = crew.state.auth_token.clone();
        crew.homes_gone();
        put_ungrouped(&d, 2, &[vel9.id, vel15.id, vel20.id], true).await;
        let out = post(&app, &token, take_on(vel9.id, true)).await;
        assert_eq!(out["status"], "ok", "{out}");
        let asked = d
            .line_call(
                vel9.id,
                "ask",
                &json!({"to": "desk", "text": "Ship VEL-9 now?", "wait_seconds": 60}),
                Some(Duration::from_millis(10)),
            )
            .await
            .unwrap();
        let ask: uuid::Uuid = serde_json::from_value(asked["id"].clone()).unwrap();
        // Past a sweep or more.
        tokio::time::sleep(Duration::from_secs(7)).await;
        let q = d.store().line_message(ask).await.unwrap().unwrap();
        assert_eq!(q.to.id, HOME_DESK, "{q:?}");
        assert_eq!(q.asked, Some(Asked::Open), "{q:?}");
    })
    .await;
}

/// On devbox: its Ungrouped grant, taken under once, is deleted and written
/// again: the take under the one gone counts for nothing, and what VEL-9
/// asks under the new one waits for the home's first take.
#[tokio::test(flavor = "multi_thread")]
async fn a_grant_written_again_is_new_whatever_was_taken_under_the_one_gone() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (crew, d, [vel9, vel15, vel20, _]) = devbox(dir.path(), pool).await;
        let app = super::super::relay::routes(Arc::clone(&crew.state), Arc::clone(&crew));
        let token = crew.state.auth_token.clone();
        take(&app, &token, json!({})).await;
        crew.homes_gone();
        let (code, out) = native(&d, "DELETE", "/api/desk/grants/ungrouped", json!({})).await;
        assert!(code.is_success(), "{out}");
        put_ungrouped(&d, 2, &[vel9.id, vel15.id, vel20.id], true).await;
        let out = post(&app, &token, take_on(vel9.id, true)).await;
        assert_eq!(out["status"], "ok", "{out}");
        let asked = d
            .line_call(
                vel9.id,
                "ask",
                &json!({"to": "desk", "text": "Ship VEL-9 now?", "wait_seconds": 60}),
                Some(Duration::from_millis(10)),
            )
            .await
            .unwrap();
        let ask: uuid::Uuid = serde_json::from_value(asked["id"].clone()).unwrap();
        // Past a sweep or more.
        tokio::time::sleep(Duration::from_secs(7)).await;
        let q = d.store().line_message(ask).await.unwrap().unwrap();
        assert_eq!(q.to.id, HOME_DESK, "{q:?}");
        assert_eq!(q.asked, Some(Asked::Open), "{q:?}");
    })
    .await;
}

/// On devbox: a take is for the home's own boss of a live grant alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_take_is_refused_to_any_boss_but_the_grants_own_from_its_home() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (crew, _d, _) = devbox(dir.path(), pool).await;
        let app = super::super::relay::routes(Arc::clone(&crew.state), Arc::clone(&crew));
        let token = crew.state.auth_token.clone();
        let mut other = relayed(WORK_TAKE, json!({}), true);
        other["boss"] = json!("local:mia");
        assert_eq!(post(&app, &token, other).await["status"], "refused");
        let mut elsewhere = relayed(WORK_TAKE, json!({}), true);
        elsewhere["group"] = json!("acme");
        assert_eq!(post(&app, &token, elsewhere).await["status"], "refused");
        let r = crew
            .serve_crew(
                ServedCall {
                    tool: WORK_TAKE.into(),
                    input: json!({}),
                    boss: workforce_desk::grant::BossRef::Local {
                        agent: "desk".into(),
                    },
                    link_id: Some("home-relay".into()),
                    call_id: Some("c-1".into()),
                    ceiling: None,
                },
                "ungrouped",
            )
            .await;
        assert_eq!(r.status, ServedStatus::Refused, "never a crew tool");
    })
    .await;
}

/// On the Mac: a session the boss runs on devbox that has gone quiet is
/// said in its check-in as one here is, once, in the same words and after
/// the same wait; one whose wait devbox raises elsewhere is not.
#[tokio::test(flavor = "multi_thread")]
async fn a_machine_session_gone_quiet_is_said_in_the_check_in_as_one_here_is() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_main, crew, _d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"act\"\n",
            &[],
        )
        .await;
        // VEL-9, on devbox, was never placed in the sidebar: it is in no
        // group, the main overseer's.
        let id = uuid::Uuid::new_v4();
        let (_, o) = eventually("the main overseer", async || crew.overseer_named("desk")).await;
        eventually("the grants", async || crew.machine_grants(true, true).map(drop)).await;
        let quiet_since = chrono::Utc::now() - chrono::Duration::hours(1);
        let item: workforce_desk::crew::Item = serde_json::from_value(json!({
            "kind": "session", "id": id.to_string(), "name": "VEL-9", "engine": "claude",
            "origin": "desk", "state": "working", "task": "Ship the fix", "detail": "● Bash",
            "since": quiet_since, "lastActivity": quiet_since, "approval": null, "question": null,
        }))
        .unwrap();
        // devbox, standing in: its take says how VEL-9 is.
        let shown = Arc::new(parking_lot::Mutex::new(Some(item)));
        let poller = {
            let (crew, shown) = (Arc::clone(&crew), Arc::clone(&shown));
            tokio::spawn(async move {
                loop {
                    for c in crew.relay_take(Duration::from_millis(200)).await {
                        let body = match c.tool.as_str() {
                            WORK_TAKE => json!(Taken {
                                now: chrono::Utc::now().timestamp_millis(),
                                after: None,
                                sessions: vec![super::super::relayed_work::Run {
                                    id,
                                    name: "VEL-9".into(),
                                    engine: "Claude Code".into(),
                                    state: "working".into(),
                                    wheel: workforce_desk::charge::Wheel::Boss,
                                    goal: "Ship the fix".into(),
                                    folder: "/src".into(),
                                    last_reply: None,
                                    answered: false,
                                    item: shown.lock().clone(),
                                }],
                                ..Taken::default()
                            }),
                            "desk.machines" => {
                                json!({"machines": [{"name": "devbox", "reached": true}]})
                            }
                            // As Desk lists them: each with the grant that holds it.
                            "desk.sessions" => {
                                json!([{"id": id, "state": "working", "grant": null}])
                            }
                            _ => json!({}),
                        };
                        crew.relay_answer(
                            c.id,
                            super::super::relay::RelayAnswer {
                                status: ServedStatus::Ok,
                                body,
                            },
                        );
                    }
                }
            })
        };
        // What the check-in says of VEL-9, and what it noted.
        let quiet = |mut noted: std::collections::HashSet<String>| {
            let (crew, o) = (Arc::clone(&crew), o.clone());
            async move {
                let said: Vec<String> = crew
                    .thresholds("desk", &o, &mut noted)
                    .await
                    .into_iter()
                    .map(|t| t.line)
                    .filter(|l| l.contains("VEL-9"))
                    .collect();
                (said, noted)
            }
        };

        let (said, noted) = eventually("VEL-9 said quiet", async || {
            let (said, noted) = quiet(std::collections::HashSet::new()).await;
            (!said.is_empty()).then_some((said, noted))
        })
        .await;
        assert_eq!(
            said,
            [format!(
                "VEL-9 (Claude Code session devbox/{id}) has been quiet for 60 min while working on \
                 Ship the fix (last: ● Bash)"
            )]
        );
        let (said, _) = quiet(noted).await;
        assert!(said.is_empty(), "said once: {said:?}");
        assert!(crew.machine_items("sarah").is_empty(), "the boss's own");

        // devbox raises what it waits on elsewhere: nothing said here.
        *shown.lock() = None;
        eventually("devbox's word heard", async || {
            crew.machine_items("desk").is_empty().then_some(())
        })
        .await;
        let (said, _) = quiet(std::collections::HashSet::new()).await;
        assert!(said.is_empty(), "{said:?}");
        poller.abort();
    })
    .await;
}
