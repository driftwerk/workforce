//! The boss learning from the person (overseer H10): "always" turns an
//! answer into a preference for the case its ask said it settles; undoing
//! a decision a preference made is an overrule, counted, asked about, and
//! after two the preference is set aside; and the needs it handled itself
//! are counted, in all and per day, adding up.

use super::unbound::UNBOUND;
use super::*;
use workforce_desk::decision::{BossDecision, How, Need};
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

/// An ask with generalizes and a scope, answered from its button, then
/// "always": one preference, the person's answer for that case and scope,
/// kept once. One whose ask settles no case has nothing to keep.
#[tokio::test(flavor = "multi_thread")]
async fn always_turns_an_answer_into_a_scoped_preference_once() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"act\"\n",
            &[],
        )
        .await;
        let r = call(
            &crew,
            super::super::items::ASK,
            json!({
                "text": "Merge acme-web's fix with CI red on the flaky e2e job?",
                "options": ["Merge", "Wait for green"],
                "generalizes": "When a session wants to merge with CI red",
                "scope": "repo:acme/acme-web",
            }),
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        let item = d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.generalizes.is_some())
            .unwrap();
        // Its card proposes the case and what it would cover.
        let m = item.mention();
        assert_eq!(
            m.generalizes.as_deref(),
            Some("When a session wants to merge with CI red")
        );
        assert_eq!(m.prefer_scope.as_deref(), Some("repo:acme/acme-web"));
        assert!(
            crew.always_item(item.id, None).await.is_err(),
            "not before it is answered"
        );
        crew.answer_item(item.id, "Wait for green").await.unwrap();
        let said = crew.always_item(item.id, None).await.unwrap();
        assert!(said.starts_with("From now on"), "{said}");
        let p = d
            .store()
            .preference_from_item(item.id)
            .await
            .unwrap()
            .expect("kept");
        assert_eq!(p.source, PrefSource::Asked);
        assert_eq!(p.when_text, "a session wants to merge with CI red");
        assert_eq!(p.then_text, "Wait for green");
        assert_eq!(p.scope, PrefScope::Repo("acme/acme-web".into()));
        let again = crew.always_item(item.id, None).await.unwrap();
        assert!(again.starts_with("Already kept"), "{again}");
        assert_eq!(
            d.store()
                .preferences_of("you", MAIN, false)
                .await
                .unwrap()
                .len(),
            1
        );
        // The boss keeping one from the same ask in other words replaces
        // the kept one: one preference per ask.
        let r = call(
            &crew,
            super::super::preferences::PREFER,
            json!({
                "when": "CI is red on a merge",
                "then": "Wait for green",
                "scope": "all",
                "item_id": item.id.to_string(),
            }),
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        assert!(r.output.to_string().contains("It replaces"), "{}", r.output);
        let kept = d.store().preferences_of("you", MAIN, false).await.unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].when_text, "CI is red on a merge");
        // The person keeping one from the same ask in Desk's settings
        // replaces it too; one of their own is said, for the main boss.
        use workforce_desk::crew::Crew;
        let mine = crew
            .keep_preference(
                "a deploy is asked for on a Friday",
                "wait for Monday",
                "all",
                Some(item.id),
                None,
            )
            .await
            .unwrap();
        assert_eq!(mine.source, PrefSource::Asked);
        assert_eq!(mine.boss_scope, MAIN);
        let own = crew
            .keep_preference("a key is asked for", "use the staging key", "", None, None)
            .await
            .unwrap();
        assert_eq!(own.source, PrefSource::Said);
        let kept = d.store().preferences_of("you", MAIN, false).await.unwrap();
        assert_eq!(kept.len(), 2, "{kept:?}");
        assert!(kept.iter().any(|p| p.id == mine.id) && kept.iter().any(|p| p.id == own.id));
        assert!(crew
            .keep_preference("x", "y", "", None, Some("no-such-group"))
            .await
            .is_err());
        assert!(crew
            .keep_preference(" ", "y", "", None, None)
            .await
            .is_err());
        assert!(crew
            .keep_preference("x", "y", "everywhere", None, None)
            .await
            .is_err());
        for p in [&mine, &own] {
            d.store()
                .preference_set_state(p.id, PrefState::Retired)
                .await
                .unwrap();
        }

        // An ask that settles no case keeps nothing.
        call(&crew, super::super::items::ASK, json!({ "text": "Lunch?" })).await;
        let plain = d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.text == "Lunch?")
            .unwrap();
        crew.answer_item(plain.id, "Yes").await.unwrap();
        assert!(crew.always_item(plain.id, None).await.is_err());
        // A bad scope is said to the boss when it asks.
        let r = call(
            &crew,
            super::super::items::ASK,
            json!({ "text": "x?", "generalizes": "x", "scope": "everywhere" }),
        )
        .await;
        assert!(!r.success);
    })
    .await;
}

/// Undoing a decision a preference made is an overrule: counted, asked
/// about once ("Change it", "Keep it", "Forget it"); "Keep it" keeps it and
/// starts the count again; two overrules set it aside.
#[tokio::test(flavor = "multi_thread")]
async fn an_undo_of_a_preferences_decision_is_an_overrule() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", UNBOUND, &[]).await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        let p = Preference::new(
            MAIN,
            "a session wants to merge with CI red",
            "make it wait for green",
            PrefScope::All,
            PrefSource::Said,
        );
        d.store().preference_put(&p).await.unwrap();
        let apply = || async {
            let r = call(
                &crew,
                "crew.nudge",
                json!({ "session_id": s.id.to_string(), "message": "Wait for green CI.", "reason": "CI is red.", "preference": p.id.to_string() }),
            )
            .await;
            assert!(r.success, "{:?}", r.error);
            d.store()
                .decisions_since("desk", epoch(), 50)
                .await
                .unwrap()
                .into_iter()
                .rfind(|x| x.how == How::Preference)
                .unwrap()
        };
        let first = apply().await;
        crew.undo(first.id).await.unwrap();
        let now = d.store().preference(p.id).await.unwrap().unwrap();
        assert_eq!((now.overruled_count, now.state), (1, PrefState::Active));
        let asked = d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.dedupe_key == format!("overrule:{}", p.id))
            .expect("asked whether to change it");
        assert!(asked.text.contains("Should I change that preference?"), "{}", asked.text);
        assert_eq!(asked.options, vec!["Change it", "Keep it", "Forget it"]);
        let kept = crew.answer_item(asked.id, "Keep it").await.unwrap();
        assert!(kept.starts_with("Kept"), "{kept}");
        assert_eq!(d.store().preference(p.id).await.unwrap().unwrap().overruled_count, 0);

        // Twice: set aside.
        for _ in 0..2 {
            let x = apply().await;
            crew.undo(x.id).await.unwrap();
        }
        let now = d.store().preference(p.id).await.unwrap().unwrap();
        assert_eq!((now.overruled_count, now.state), (2, PrefState::Paused));
        let asked = d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .into_iter()
            .filter(|i| i.dedupe_key == format!("overrule:{}", p.id))
            .collect::<Vec<_>>();
        assert_eq!(asked.len(), 1, "asked once while it is open: {asked:?}");
        assert!(
            asked[0].text.contains("set it aside") && asked[0].text.contains(&p.id.to_string()),
            "says it set it aside, and which: {}",
            asked[0].text
        );
        // "Change it" goes on to the boss for the new words: nothing here
        // changes it.
        crew.answer_item(asked[0].id, "Change it").await.unwrap();
        assert_eq!(
            d.store().preference(p.id).await.unwrap().unwrap().state,
            PrefState::Paused
        );

        // Back on, undone again: "Forget it" forgets it.
        d.store()
            .preference_set_state(p.id, PrefState::Active)
            .await
            .unwrap();
        let x = apply().await;
        crew.undo(x.id).await.unwrap();
        let asked: Vec<_> = d
            .store()
            .items_not_closed(Some("desk"))
            .await
            .unwrap()
            .into_iter()
            .filter(|i| i.dedupe_key == format!("overrule:{}", p.id))
            .collect();
        let forgot = crew.answer_item(asked[0].id, "Forget it").await.unwrap();
        assert!(forgot.starts_with("Forgot"), "{forgot}");
        assert_eq!(
            d.store().preference(p.id).await.unwrap().unwrap().state,
            PrefState::Retired
        );
    })
    .await;
}

/// The needs it handled itself, by preference, escalated, and the person
/// got to first, since a time and per day: they add up, and say so.
#[tokio::test(flavor = "multi_thread")]
async fn the_handled_counts_add_up_in_all_and_per_day() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "ok",
            "[oversight]\nenabled = true\nlevel = \"act\"\n",
            &[],
        )
        .await;
        let yesterday = chrono::Utc::now() - chrono::Duration::days(1);
        for (need, how, at) in [
            (Need::Approval, How::Own, None),
            (Need::Question, How::Own, None),
            (Need::Question, How::Preference, None),
            (Need::Question, How::Escalated, None),
            (Need::Stall, How::PersonFirst, None),
            (Need::Approval, How::Own, Some(yesterday)),
            // Not needs: never counted.
            (Need::TurnEnd, How::Own, None),
            (Need::Choice, How::Own, None),
        ] {
            let mut x = BossDecision::new("desk", need, "did it", "", how);
            if let Some(at) = at {
                x.at = at;
            }
            d.store().decision_put(&x).await.unwrap();
        }
        let today = crew.handled_counts(None, None).await.unwrap();
        assert_eq!(today["needs"], 5, "{today}");
        assert_eq!(today["yourself"], 3);
        assert_eq!(today["preference"], 1);
        let parts = ["own", "preference", "escalated", "personFirst"]
            .iter()
            .map(|k| today[*k].as_u64().unwrap())
            .sum::<u64>();
        assert_eq!(parts, 5, "the counts add up: {today}");
        assert_eq!(
            today["said"],
            "handled 3 of 5 myself, 1 by your preferences"
        );
        let week = crew
            .handled_counts(
                Some("desk"),
                Some(chrono::Utc::now() - chrono::Duration::days(7)),
            )
            .await
            .unwrap();
        assert_eq!(week["needs"], 6);
        let days = week["days"].as_array().unwrap();
        assert_eq!(days.len(), 2, "{week}");
        assert_eq!(
            days.iter()
                .map(|d| d["needs"].as_u64().unwrap())
                .sum::<u64>(),
            6
        );
        assert!(crew.handled_counts(Some("nobody"), None).await.is_none());
    })
    .await;
}
