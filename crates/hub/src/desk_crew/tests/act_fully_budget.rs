//! Act fully's daily budget counts real money only: what an API key (or
//! Bedrock) paid for, never what the person's Claude or ChatGPT plans ran,
//! however much that would cost at API prices. And an overseer asked to
//! change its own limits can't: it posts a card the person approves, which
//! changes them through the person's own path.

use super::*;
use workforce_common::oversight::Level;
use workforce_common::persistence::CostRecord;
use workforce_common::subscription::Billing as HubBilling;
use workforce_desk::ledger::{Billing, Priced, Spend, Tokens};

const FULL: &str = "[oversight]\nenabled = true\nlevel = \"full\"\n\
                    [oversight.full]\nrepos = [\"acme/acme-web\"]\nbudget_usd_per_day = 200\n";

/// A call of `usd` in `session`'s ledger, paid as `billing`.
async fn spent(d: &Desk, session: uuid::Uuid, billing: Billing, usd: f64) {
    d.store()
        .ledger_write(
            session,
            Engine::Claude,
            None,
            &[workforce_desk::ledger_store::Kept {
                spend: Spend {
                    key: uuid::Uuid::new_v4().to_string(),
                    at: chrono::Utc::now(),
                    model: Some("claude-opus-5-5".into()),
                    tokens: Tokens {
                        input: 100,
                        ..Default::default()
                    },
                    usd: Some(usd),
                    priced: Priced::Reported,
                    billing: None,
                },
                billing,
                account: None,
            }],
        )
        .await
        .unwrap();
}

/// Claude Code seen signed in as `via` now.
async fn keep_sign_in(d: &Desk, via: workforce_desk::engine::Via) {
    d.store()
        .keep_sign_in(&workforce_desk::ledger_store::KeptSignIn {
            engine: Engine::Claude,
            via,
            who: None,
            since: chrono::Utc::now() - chrono::Duration::days(1),
        })
        .await
        .unwrap();
}

/// A model call of the hub agent `agent`'s, of `usd`, paid as `billing`.
fn hub_call(service: &AgentService, agent: &str, billing: HubBilling, usd: f64) {
    assert!(service
        .telemetry_store
        .record_cost(&CostRecord {
            agent_id: agent.into(),
            model: "claude-opus-5-5".into(),
            cost_usd: usd,
            billing,
            ledger_key: Some(uuid::Uuid::new_v4().to_string()),
            ..Default::default()
        })
        .unwrap());
}

/// The reporter's day: the overseer on a claude-code plan, its sessions on
/// a Claude plan, five times the budget at API prices, and Act fully still
/// in force. Real money (a key's call, a metered hub call) still counts,
/// and spent, it says what stopped and how much of the budget went.
#[tokio::test(flavor = "multi_thread")]
async fn plans_never_spend_act_fullys_budget_and_real_money_does() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let (overseer, crew, d, _llm) = overseeing(&base, pool, "ok", FULL, &[]).await;
        let id = overseer.agent_id.as_str().to_string();
        let s = new_session(&d, &base, "acme-web").await;
        let level = async || {
            crew.read_spent_today(&crew.overseers()).await;
            let (_, o) = crew.overseer_service().unwrap();
            crew.level_now(&id, &o)
        };

        // $1,000 of plan use, at API-equivalent prices: the sessions' and
        // the overseer's own claude-code turns.
        spent(&d, s.id, Billing::Plan, 600.0).await;
        hub_call(&overseer, &id, HubBilling::Subscription, 400.0);
        // A call Desk can't say the billing of, on an engine only ever seen
        // signed in to a plan (as after a restart, before it is read
        // again), is a plan's.
        keep_sign_in(&d, workforce_desk::engine::Via::Plan).await;
        spent(&d, s.id, Billing::Unknown, 50.0).await;
        assert_eq!(
            level().await,
            (Level::Full, None),
            "plans are never real money"
        );
        let shown = d.full_budget_of(&id).unwrap();
        assert_eq!(shown.spent_usd, Some(0.0));
        assert_eq!(shown.budget_usd, 200.0);
        assert!(!shown.spent);

        // Real money: a key's call in its sessions, and its own metered
        // calls, which it counts once.
        spent(&d, s.id, Billing::Key, 120.0).await;
        hub_call(&overseer, &id, HubBilling::Metered, 60.0);
        assert_eq!(level().await, (Level::Full, None));
        assert_eq!(d.full_budget_of(&id).unwrap().spent_usd, Some(180.0));
        // Once it has been seen on a key, an unknown call could be one.
        keep_sign_in(&d, workforce_desk::engine::Via::Key).await;
        assert_eq!(level().await.0, Level::Act, "$50 unknown now counts");
        assert_eq!(d.full_budget_of(&id).unwrap().spent_usd, Some(230.0));
        hub_call(&overseer, &id, HubBilling::Metered, 30.0);
        let (now, why) = level().await;
        assert_eq!(now, Level::Act);
        let why = why.unwrap();
        assert!(
            why.contains("$260.00 of $200.00 in real money today"),
            "{why}"
        );
        assert!(why.contains("plans aren't counted"), "{why}");
        assert!(
            why.contains("stopped merging, tagging, releasing and running workflows"),
            "{why}"
        );
        assert!(d.full_budget_of(&id).unwrap().spent);
        assert!(d.bosses().iter().all(|b| b.level == "act"));
    })
    .await;
}

/// Asked to raise its own budget, the overseer can't: its config tool
/// refuses, its crew tool only posts a card (unbound too), the card done
/// by anyone but the person is refused, and accepted by the person it
/// writes the change to the overseer's config.toml. Clearing starts
/// today's count again.
#[tokio::test(flavor = "multi_thread")]
async fn an_overseer_asks_for_its_own_limits_on_a_card_the_person_approves() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let budgeted = super::unbound::UNBOUND.replace(
            "anywhere = true\n",
            "anywhere = true\nbudget_usd_per_day = 200\n",
        );
        let (overseer, crew, d, _llm) = overseeing(&base, pool, "ok", &budgeted, &[]).await;
        let id = overseer.agent_id.as_str().to_string();
        eventually("the overseer's tools", async || {
            overseer
                .tools
                .has(super::super::own_settings::NAME)
                .then_some(())
        })
        .await;
        let s = new_session(&d, &base, "acme-web").await;
        spent(&d, s.id, Billing::Key, 250.0).await;
        crew.read_spent_today(&crew.overseers()).await;
        let (_, o) = crew.overseer_service().unwrap();
        assert!(!crew.unbound(&o), "spent: not unbound now");

        // Unbound with budget to spare, it still only asks.
        let (_, mut free) = crew.overseer_service().unwrap();
        free.full.budget_cents_per_day = None;
        assert!(crew.unbound(&free));
        let change = super::super::actions::Action::ChangeOversight {
            agent: id.clone(),
            change: super::super::own_settings::Change::parse(&json!({"budget_usd_per_day": 5000}))
                .unwrap(),
        };
        assert!(matches!(
            crew.gate(&change, &free),
            super::super::actions::Gate::Suggest(_)
        ));
        let by_itself = crew
            .perform(&change, crew.by_boss(&overseer, "The person asked."))
            .await
            .unwrap_err();
        assert!(by_itself.contains("Only the person"), "{by_itself}");

        let asked = overseer
            .tools
            .get(super::super::own_settings::NAME)
            .unwrap()
            .execute(json!({
                "budget_usd_per_day": 5000, "clear_spent_today": true,
                "agent_id": "someone-else",
                "reason": "You asked me to raise it to $5k a day."
            }))
            .await
            .unwrap();
        let note = asked.output["note"].as_str().unwrap_or_default();
        assert!(note.contains("your own limits are the person's"), "{asked:?}");
        let text = asked.output["suggested"].as_str().unwrap();
        assert!(
            text.starts_with("Set Act fully's daily budget to $5,000.00 of real money; clear what Act fully's budget has counted today"),
            "{text}"
        );
        assert!(text.ends_with("Only you can change them."), "{text}");
        // Nothing changed yet.
        let before = std::fs::read_to_string(
            overseer
                .workspace
                .join(workforce_common::config_files::WORKSPACE_CONFIG),
        )
        .unwrap();
        assert!(before.contains("budget_usd_per_day = 200"));

        let suggestion = d
            .store()
            .suggestion(asked.output["suggestion_id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(suggestion.action["input"]["agent_id"], id, "only ever its own");
        // What was spent before the clear is past it.
        tokio::time::sleep(Duration::from_millis(20)).await;
        crew.accept(&suggestion).await.unwrap();
        let after = std::fs::read_to_string(
            overseer
                .workspace
                .join(workforce_common::config_files::WORKSPACE_CONFIG),
        )
        .unwrap();
        assert!(after.contains("budget_usd_per_day = 5000"), "{after}");
        assert!(after.contains("budget_cleared_at = "), "{after}");
        crew.read_spent_today(&crew.overseers()).await;
        let (_, o) = crew.overseer_service().unwrap();
        assert_eq!(o.full.budget_cents_per_day, Some(500_000));
        assert!(crew.unbound(&o), "raised and cleared: unbound again");
        let shown = d.full_budget_of(&id).unwrap();
        assert_eq!(shown.spent_usd, Some(0.0), "counted from the clear");
        assert!(shown.cleared_at.is_some());

        // The config tool refuses it outright, and says why.
        let config = crate::config_tool::ConfigTool::new(Arc::clone(&overseer), None);
        let refused = workforce_agent::tools::Tool::execute(
            &config,
            json!({"action": "set", "key": "oversight.full.budget_usd_per_day", "value": 9000}),
        )
        .await
        .unwrap();
        let said = format!("{:?} {}", refused.error, refused.output);
        assert!(said.contains("you can't change your own limits"), "{said}");
        assert!(said.contains("crew.ask_oversight_change"), "{said}");
    })
    .await;
}

/// A cleared time ahead of the clock counts from now, not from then: the
/// budget never stops counting.
#[test]
fn a_cleared_time_is_never_after_now() {
    let now = chrono::Utc::now();
    let midnight = now - chrono::Duration::hours(3);
    let mut full = workforce_common::oversight::Full::default();
    assert_eq!(super::super::counted_from(midnight, now, &full), midnight);
    full.budget_cleared_at = Some(now + chrono::Duration::days(30));
    assert_eq!(super::super::counted_from(midnight, now, &full), now);
    full.budget_cleared_at = Some(now - chrono::Duration::hours(1));
    assert_eq!(
        super::super::counted_from(midnight, now, &full),
        now - chrono::Duration::hours(1)
    );
    full.budget_cleared_at = Some(midnight - chrono::Duration::hours(1));
    assert_eq!(super::super::counted_from(midnight, now, &full), midnight, "yesterday's");
}
