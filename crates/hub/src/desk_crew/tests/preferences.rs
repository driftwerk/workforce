//! The person's preferences (overseer H9): kept, listed, changed, paused
//! and forgotten with crew.prefer and crew.preferences; a crew action that
//! applies one is recorded as made by it and cites it, a close match and
//! a conflict said as they are; only the citation is checked, so an
//! unknown or forgotten preference is refused and the same action without
//! one runs unbound as always; and every turn is given the preferences
//! that cover what it is about.

use super::act_fully::repo;
use super::unbound::UNBOUND;
use super::*;
use workforce_desk::decision::How;
use workforce_desk::preference::{PrefScope, PrefSource, PrefState, Preference, MAIN};

fn tool(crew: &Arc<CrewHub>, name: &str) -> super::super::tools::CrewTool {
    super::super::tools::CrewTool::for_level(crew, "desk", &Default::default(), false)
        .into_iter()
        .find(|t| workforce_agent::tools::Tool::name(t) == name)
        .unwrap()
}

async fn call(crew: &Arc<CrewHub>, name: &str, input: Value) -> workforce_agent::tools::ToolResult {
    workforce_agent::tools::Tool::execute(&tool(crew, name), input)
        .await
        .unwrap()
}

fn epoch() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::UNIX_EPOCH
}

/// One of the person's preferences, made `days_ago`.
async fn kept(d: &Desk, when: &str, then: &str, scope: PrefScope, days_ago: i64) -> Preference {
    let mut p = Preference::new(MAIN, when, then, scope, PrefSource::Said);
    p.created_at -= chrono::Duration::days(days_ago);
    d.store().preference_put(&p).await.unwrap();
    p
}

/// Kept, listed, changed, paused, resumed and forgotten by the boss's
/// tools; the same case kept again replaces the older one.
#[tokio::test(flavor = "multi_thread")]
async fn the_boss_keeps_lists_changes_and_forgets_the_persons_preferences() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"watch\"\n",
            &[],
        )
        .await;
        let prefer = super::super::preferences::PREFER;
        let list = super::super::preferences::PREFERENCES;
        // Even at Watch: keeping the person's own words is never gated.
        let r = call(
            &crew,
            prefer,
            json!({ "when": "When a session wants to merge with CI red", "then": "Make it wait for green.", "scope": "repo:acme/acme-web" }),
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        assert!(r.output.to_string().contains("for repo acme/acme-web"), "{}", r.output);
        let all = d.store().preferences_of("you", MAIN, false).await.unwrap();
        assert_eq!(all.len(), 1);
        let first = all[0].clone();
        assert_eq!(first.when_text, "a session wants to merge with CI red");
        assert_eq!(first.then_text, "Make it wait for green");
        assert_eq!(first.source, PrefSource::Said);

        // The same case again replaces it.
        let r = call(
            &crew,
            prefer,
            json!({ "when": "a session wants to merge with CI red", "then": "merge if only flaky e2e failed", "scope": "repo:acme/acme-web" }),
        )
        .await;
        assert!(r.output.to_string().contains("It replaces"), "{}", r.output);
        let all = d.store().preferences_of("you", MAIN, false).await.unwrap();
        assert_eq!(all.len(), 1, "{all:?}");
        let second = all[0].clone();
        assert_ne!(second.id, first.id);
        assert_eq!(
            d.store().preference(first.id).await.unwrap().unwrap().state,
            PrefState::Retired
        );

        let r = call(&crew, list, json!({})).await;
        let listed = r.output.to_string();
        assert!(listed.contains(&second.id.to_string()), "{listed}");
        assert!(listed.contains("not used yet"), "{listed}");
        assert!(!listed.contains(&first.id.to_string()), "a forgotten one isn't listed: {listed}");

        let id = second.id.to_string();
        let r = call(&crew, list, json!({ "action": "change", "preference_id": id, "then": "wait for green." })).await;
        assert!(r.success, "{:?}", r.error);
        let now = d.store().preference(second.id).await.unwrap().unwrap();
        assert_eq!(now.then_text, "wait for green");
        assert_eq!(now.scope, PrefScope::Repo("acme/acme-web".into()));
        let r = call(&crew, list, json!({ "action": "pause", "preference_id": id })).await;
        assert!(r.output.to_string().starts_with("\"Paused"), "{}", r.output);
        assert!(d.store().preferences_of("you", MAIN, false).await.unwrap().is_empty());
        call(&crew, list, json!({ "action": "resume", "preference_id": id })).await;
        let r = call(&crew, list, json!({ "action": "forget", "preference_id": id })).await;
        assert!(r.output.to_string().starts_with("\"Forgot"), "{}", r.output);
        let r = call(&crew, list, json!({ "action": "forget", "preference_id": id })).await;
        assert!(!r.success, "forgotten already");
        let r = call(&crew, list, json!({ "action": "pause", "preference_id": uuid::Uuid::new_v4().to_string() })).await;
        assert!(!r.success);
        let r = call(&crew, prefer, json!({ "when": "x", "then": "y", "scope": "everywhere" })).await;
        assert!(!r.success, "a scope is all, a group or a repository");
    })
    .await;
}

/// Unbound: a nudge applying a preference is recorded as made by it and
/// cites it (with the date), and counts its use; a close match is applied
/// and said; two that apply are named both, the winner over the other; an
/// unknown or forgotten preference is refused before anything is done; and
/// the same nudge with none runs as always.
#[tokio::test(flavor = "multi_thread")]
async fn an_action_by_a_preference_cites_it_and_only_the_citation_is_checked() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", UNBOUND, &[]).await;
        let (_, o) = crew.overseer_service().unwrap();
        assert!(crew.unbound(&o));
        let s = new_session(&d, dir.path(), "acme-web").await;
        let tz = d.timezone();
        let red = kept(
            &d,
            "a session wants to merge with CI red",
            "make it wait for green",
            PrefScope::All,
            2,
        )
        .await;
        let nudge = |extra: Value| {
            let mut v = json!({
                "session_id": s.id.to_string(),
                "message": "Wait for green CI before you merge.",
                "reason": "CI is red.",
            });
            for (k, x) in extra.as_object().unwrap() {
                v[k] = x.clone();
            }
            v
        };
        let r = call(
            &crew,
            "crew.nudge",
            nudge(json!({ "preference": red.id.to_string(), "match": "exact" })),
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        let decisions = d
            .store()
            .decisions_since("desk", epoch(), 10)
            .await
            .unwrap();
        let by_pref = decisions.last().unwrap().clone();
        assert_eq!(by_pref.how, How::Preference);
        assert_eq!(by_pref.preference, Some(red.id));
        assert!(
            by_pref
                .why
                .contains(&format!("per your preference from {}", red.date(tz))),
            "{}",
            by_pref.why
        );
        assert!(
            by_pref.said().contains("make it wait for green"),
            "{}",
            by_pref.said()
        );
        assert_eq!(
            by_pref.line().preference.as_deref(),
            Some(red.id.to_string().as_str())
        );
        assert_eq!(
            d.store()
                .preference(red.id)
                .await
                .unwrap()
                .unwrap()
                .applied_count,
            1
        );

        // A close match, unbound: applied, and said as one.
        let r = call(
            &crew,
            "crew.nudge",
            nudge(json!({ "preference": red.id.to_string(), "match": "close" })),
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        let close = d
            .store()
            .decisions_since("desk", epoch(), 10)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert!(close.why.contains("as a close match"), "{}", close.why);
        assert!(close.why.contains("shout if wrong"), "{}", close.why);

        // Two that apply: the decision names both.
        let staging = kept(
            &d,
            "a session runs staging tests",
            "use the staging key",
            PrefScope::Group("g-acme".into()),
            0,
        )
        .await;
        let r = call(
            &crew,
            "crew.nudge",
            nudge(json!({ "preference": staging.id.to_string(), "over": [red.id.to_string()] })),
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        let both = d
            .store()
            .decisions_since("desk", epoch(), 10)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert!(
            both.why
                .contains(&format!("per your preference from {}", staging.date(tz))),
            "{}",
            both.why
        );
        assert!(
            both.why
                .contains(&format!("over the broader one from {}", red.date(tz))),
            "{}",
            both.why
        );

        // Only the citation is checked: unknown, forgotten, or a bad match
        // are refused before anything is sent.
        let count = |d: Desk| async move {
            d.store()
                .decisions_since("desk", epoch(), 50)
                .await
                .unwrap()
                .len()
        };
        let before = count(d.clone()).await;
        let r = call(
            &crew,
            "crew.nudge",
            nudge(json!({ "preference": uuid::Uuid::new_v4().to_string() })),
        )
        .await;
        assert!(!r.success);
        assert!(
            r.error
                .as_deref()
                .unwrap_or_default()
                .contains("without preference"),
            "{:?}",
            r.error
        );
        d.store()
            .preference_set_state(staging.id, PrefState::Retired)
            .await
            .unwrap();
        let r = call(
            &crew,
            "crew.nudge",
            nudge(json!({ "preference": staging.id.to_string() })),
        )
        .await;
        assert!(
            r.error.as_deref().unwrap_or_default().contains("retired"),
            "{:?}",
            r.error
        );
        let r = call(
            &crew,
            "crew.nudge",
            nudge(json!({ "preference": red.id.to_string(), "over": [staging.id.to_string()] })),
        )
        .await;
        assert!(!r.success, "one set aside must be active too");
        let r = call(
            &crew,
            "crew.nudge",
            nudge(json!({ "preference": red.id.to_string(), "match": "maybe" })),
        )
        .await;
        assert!(!r.success);
        assert_eq!(count(d.clone()).await, before, "nothing was done");
        // The same nudge with none runs unbound as it always did.
        let r = call(&crew, "crew.nudge", nudge(json!({}))).await;
        assert!(r.success, "{:?}", r.error);
        let plain = d
            .store()
            .decisions_since("desk", epoch(), 50)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!((plain.how, plain.preference), (How::Own, None));
    })
    .await;
}

/// Every turn is given the person's preferences, the most specific first:
/// a line turn only those covering its session's repository; unbound it is
/// told to apply a close match without asking, bound to ask when what it
/// does can't be undone. The unbound hint carries no never-list.
#[tokio::test(flavor = "multi_thread")]
async fn every_turn_is_given_the_preferences_that_cover_it() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let acme = base.join("acme-web");
        repo(&acme, "git@github.com:acme/acme-web.git");
        let other = base.join("docs");
        repo(&other, "git@github.com:acme/docs.git");
        let (overseer, crew, d, _llm) = overseeing(&base, pool, "ok", UNBOUND, &[]).await;
        let web = new_session(&d, &acme, "acme-web").await;
        let docs = new_session(&d, &other, "docs").await;
        let any = kept(
            &d,
            "a session wants a key",
            "ask before using any key",
            PrefScope::All,
            5,
        )
        .await;
        let staging = kept(
            &d,
            "a session runs staging tests",
            "use the staging key",
            PrefScope::Repo("acme/acme-web".into()),
            1,
        )
        .await;
        let line = |s: uuid::Uuid| format!("{DESK_CHANNEL}:line:{s}");
        let block = super::super::context::block(&overseer, &line(web.id), PERSON)
            .await
            .expect("a block");
        let (a, b) = (
            block
                .find(&staging.id.to_string())
                .expect("the repository's"),
            block.find(&any.id.to_string()).expect("everyone's"),
        );
        assert!(a < b, "the more specific first: {block}");
        assert!(
            block.contains("apply a close match and say so, never ask"),
            "{block}"
        );
        let block = super::super::context::block(&overseer, &line(docs.id), PERSON)
            .await
            .expect("a block");
        assert!(
            !block.contains(&staging.id.to_string()),
            "another repository's isn't given: {block}"
        );
        assert!(block.contains(&any.id.to_string()), "{block}");
        let dm = super::super::context::block(&overseer, DESK_CHANNEL, PERSON)
            .await
            .unwrap();
        assert!(
            dm.contains(&staging.id.to_string()) && dm.contains(&any.id.to_string()),
            "{dm}"
        );
        // A paused one is no longer given.
        d.store()
            .preference_set_state(any.id, PrefState::Paused)
            .await
            .unwrap();
        let dm = super::super::context::block(&overseer, DESK_CHANNEL, PERSON)
            .await
            .unwrap();
        assert!(!dm.contains(&any.id.to_string()), "{dm}");

        let (_, o) = crew.overseer_service().unwrap();
        let hint = super::super::CrewHub::boss_hint(&o);
        assert!(hint.contains("preferences first"), "{hint}");
        assert!(
            !hint.contains("Never secrets"),
            "unbound: no never-list: {hint}"
        );
    })
    .await;
}

/// Bound, a close match is guidance to ask when what it does can't be
/// undone; the hub applies whatever it is given all the same.
#[tokio::test(flavor = "multi_thread")]
async fn bound_a_close_match_is_asked_when_it_cant_be_undone() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true }\n",
            &[],
        )
        .await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        let red = kept(&d, "a session wants to merge with CI red", "make it wait", PrefScope::All, 0).await;
        let dm = super::super::context::block(&overseer, DESK_CHANNEL, PERSON).await.unwrap();
        assert!(dm.contains("ask the person whether it covers this case"), "{dm}");
        let r = call(
            &crew,
            "crew.nudge",
            json!({ "session_id": s.id.to_string(), "message": "Wait.", "reason": "Red.", "preference": red.id.to_string(), "match": "close" }),
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        let d1 = d.store().decisions_since("desk", epoch(), 10).await.unwrap().pop().unwrap();
        assert_eq!(d1.how, How::Preference);
    })
    .await;
}

/// Bound, what becomes a suggestion because of a setting is a decision
/// that names the setting, escalated.
#[tokio::test(flavor = "multi_thread")]
async fn bound_what_a_setting_leaves_to_the_person_names_it() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true, interrupt = false }\n",
            &[],
        )
        .await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        let r = call(
            &crew,
            "crew.interrupt",
            json!({ "session_id": s.id.to_string(), "reason": "It's going the wrong way." }),
        )
        .await;
        assert!(r.output.to_string().contains("suggested"), "{}", r.output);
        let left = d.store().decisions_since("desk", epoch(), 10).await.unwrap().pop().expect("a decision");
        assert_eq!(left.how, How::Escalated);
        assert!(left.what.starts_with("left it to you:"), "{}", left.what);
        assert!(left.why.contains("I couldn't do it myself"), "{}", left.why);
        assert!(left.why.to_lowercase().contains("interrupt"), "names the setting: {}", left.why);
    })
    .await;
}
