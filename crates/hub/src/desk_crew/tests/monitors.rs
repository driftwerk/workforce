//! A monitor that tells the person in Desk (`surface = "desk"`): in the DM
//! with the agent that owns it, at once, or held by their cadence through
//! the quiet hours and while they are away, then told as one message; a
//! hub with no Desk refuses it; and an inbox watched end to end.

use super::*;
use workforce_common::channels::desk::DeliveryKind;
use workforce_desk::monitor_held_store::HeldFor;
use workforce_orchestrator::monitor::{DeskDelivery, MonitorDefinition, MonitorEvent, MonitorMeta};

use crate::desk_crew::monitors::MonitorTold;
use crate::monitor::desk::Finding;

/// `agent`'s DM messages told as `kind`.
fn told(
    agent: &AgentService,
    kind: DeliveryKind,
) -> Vec<workforce_common::session::SessionMessage> {
    agent
        .sessions
        .find(DESK_CHANNEL, PERSON)
        .unwrap()
        .map(|s| s.messages)
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.desk.as_ref().and_then(|d| d.delivery) == Some(kind))
        .collect()
}

fn finding(place: DeskDelivery, text: &str) -> Finding {
    Finding {
        key: format!("invoice-emails\n{PERSON}\n{text}"),
        monitor_id: "invoice-emails".into(),
        monitor_name: "Invoice emails".into(),
        place,
        text: text.into(),
    }
}

/// A boss over everything ungrouped whose cadence says `updates` (merged
/// over a minute's gather and away after five), and Scout, an agent of the
/// person's that oversees nothing, whose monitors tell them by it.
async fn scout_under(
    dir: &Path,
    pool: workforce_postgres::PostgresPool,
    updates: impl FnOnce(&chrono_tz::Tz) -> String,
) -> (Arc<AgentService>, Arc<AgentService>, Arc<CrewHub>, Desk) {
    let (scout, _scout_llm) = agent(dir, "scout", "Acme's invoice 42 is due Friday.").await;
    // The hub's clock, as the crew reads it.
    let updates = updates(&scout.config.user_timezone());
    let oversight = format!(
        "[oversight]\nenabled = true\nlevel = \"act\"\nact = {{ nudge = true }}\n\
         updates = {{ every_minutes = 1, max_wait_minutes = 1, away_after_minutes = 5{updates} }}\n"
    );
    let (overseer, crew, d, _llm) =
        overseeing(dir, pool, "ok", &oversight, &[Arc::clone(&scout)]).await;
    // Both mocks must outlive the test; leaking them is the simplest way.
    std::mem::forget(_scout_llm);
    std::mem::forget(_llm);
    (overseer, scout, crew, d)
}

/// The quiet hours as `HH:MM-HH:MM` from an hour ago to an hour from now, on
/// `tz`'s clock: now is inside them.
fn quiet_now(tz: &chrono_tz::Tz) -> String {
    let local = chrono::Utc::now().with_timezone(tz);
    let at = |h: chrono::DateTime<chrono_tz::Tz>| h.format("%H:%M").to_string();
    format!(
        "{}-{}",
        at(local - chrono::Duration::hours(1)),
        at(local + chrono::Duration::hours(1))
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_monitor_tells_the_person_in_its_agents_dm_at_once_or_as_a_notice() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, scout, crew, _d) = scout_under(dir.path(), pool, |_| String::new()).await;

        let now = crew
            .monitor_told(
                &scout,
                PERSON,
                finding(DeskDelivery::Dm, "Invoice from Acme: #42"),
            )
            .await
            .unwrap();
        assert!(matches!(now, MonitorTold::Now(_)), "{now:?}");
        let said = told(&scout, DeliveryKind::Monitor);
        assert_eq!(said.len(), 1, "in Scout's DM, the agent the monitor is of");
        assert_eq!(said[0].content, "Invoice from Acme: #42");
        assert!(
            told(&overseer, DeliveryKind::Monitor).is_empty(),
            "not the boss's"
        );

        // Told as progress is, which this cadence says at once.
        let notice = crew
            .monitor_told(&scout, PERSON, finding(DeskDelivery::Notice, "Globex paid"))
            .await
            .unwrap();
        assert!(matches!(notice, MonitorTold::Now(_)), "{notice:?}");
        assert_eq!(told(&scout, DeliveryKind::Notice)[0].content, "Globex paid");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn in_the_quiet_hours_what_a_monitor_saw_waits_for_the_roundup_after_them() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, scout, crew, d) = scout_under(dir.path(), pool, |tz| {
            format!(", roundup = \"1h\", quiet_hours = \"{}\"", quiet_now(tz))
        })
        .await;

        let held = crew
            .monitor_told(
                &scout,
                PERSON,
                finding(DeskDelivery::Dm, "Invoice from Acme: #42"),
            )
            .await
            .unwrap();
        let MonitorTold::Held { until, why } = held else {
            panic!("told in the quiet hours: {held:?}");
        };
        assert_eq!(why, HeldFor::Quiet);
        assert!(
            until > chrono::Utc::now() + chrono::Duration::minutes(50),
            "the first roundup after the quiet hours: {until}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            told(&scout, DeliveryKind::Monitor).is_empty(),
            "nothing goes in the quiet hours"
        );
        let kept = d.store().monitor_held_for("scout").await.unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].text, "Invoice from Acme: #42");

        // Their end: the crew's own look tells it, saying why it waited.
        let mut due = kept[0].clone();
        due.id = uuid::Uuid::new_v4();
        due.text = "Invoice from Globex: #7".into();
        due.release_at = chrono::Utc::now() - chrono::Duration::seconds(1);
        d.store().monitor_hold(&due).await.unwrap();
        let said = eventually("the held finding", async || {
            told(&scout, DeliveryKind::Monitor).into_iter().next()
        })
        .await;
        assert!(
            said.content.starts_with("Invoice from Globex: #7"),
            "{}",
            said.content
        );
        assert!(
            said.content.contains("held for the quiet hours"),
            "{}",
            said.content
        );
        // The told one leaves the hold once its telling is recorded.
        let left = eventually("only the one still due later", async || {
            let left = d.store().monitor_held_for("scout").await.unwrap();
            (left.len() == 1).then_some(left)
        })
        .await;
        assert_eq!(left[0].text, "Invoice from Acme: #42");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn away_from_the_dm_what_monitors_saw_gathers_into_one_message_for_their_return() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, scout, crew, d) = scout_under(dir.path(), pool, |_| String::new()).await;
        // Read up to now: what Scout posts next is unread, and past the
        // five minutes (500 ms here) that makes them away.
        let read = chrono::Utc::now();
        crew.test_reads.lock().insert("scout".into(), read);
        crew.monitor_told(&scout, PERSON, finding(DeskDelivery::Dm, "First invoice"))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;

        for text in ["Second invoice", "Third invoice"] {
            let held = crew
                .monitor_told(&scout, PERSON, finding(DeskDelivery::Dm, text))
                .await
                .unwrap();
            assert!(
                matches!(
                    held,
                    MonitorTold::Held {
                        why: HeldFor::Away,
                        ..
                    }
                ),
                "{held:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            told(&scout, DeliveryKind::Monitor).len(),
            1,
            "held while away"
        );

        // Back: they read the DM.
        crew.test_reads
            .lock()
            .insert("scout".into(), chrono::Utc::now());
        let back = eventually("the catch-up", async || {
            told(&scout, DeliveryKind::Monitor).into_iter().nth(1)
        })
        .await;
        assert!(
            back.content
                .starts_with("While you were away, your monitors saw:"),
            "{}",
            back.content
        );
        assert!(back.content.contains("Second invoice") && back.content.contains("Third invoice"));
        let forgotten = async || {
            d.store()
                .monitor_held_for("scout")
                .await
                .unwrap()
                .is_empty()
        };
        eventually("what was told forgotten", async || {
            forgotten().await.then_some(())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            told(&scout, DeliveryKind::Monitor).len(),
            2,
            "one message for both"
        );
        assert!(forgotten().await);
    })
    .await;
}

/// What Scout, which oversees nothing, would say on its own waits while the
/// person types to it and while its reply in the DM is being written, as a
/// boss's updates do; it goes when they pause, or the reply ends.
#[tokio::test(flavor = "multi_thread")]
async fn typing_or_a_reply_being_written_holds_a_finding_in_any_agents_dm() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, scout, crew, _d) = scout_under(dir.path(), pool, |_| String::new()).await;
        // A first finding makes the DM.
        crew.monitor_told(&scout, PERSON, finding(DeskDelivery::Dm, "First invoice"))
            .await
            .unwrap();

        assert!(crew.typing_in("scout"), "any agent's DM is typed in");
        let held = crew
            .monitor_told(&scout, PERSON, finding(DeskDelivery::Dm, "While typing"))
            .await
            .unwrap();
        assert!(
            matches!(
                held,
                MonitorTold::Held {
                    why: HeldFor::Typing,
                    ..
                }
            ),
            "{held:?}"
        );
        let said = eventually("told after the pause", async || {
            told(&scout, DeliveryKind::Monitor).into_iter().nth(1)
        })
        .await;
        assert_eq!(said.content, "While typing");

        // Scout's reply in the DM is being written.
        let sid = scout
            .sessions
            .find(DESK_CHANNEL, PERSON)
            .unwrap()
            .expect("the DM")
            .id;
        // Their message begins Scout's reply, as the turn loop records it.
        let turn = crew
            .track(
                &scout,
                DESK_CHANNEL,
                PERSON,
                "What's due this week?",
                Some(sid.as_str()),
                (None, None),
            )
            .expect("a turn in Scout's DM is recorded");
        assert!(
            crew.dm_turns.lock().contains_key(&sid),
            "its reply is being written"
        );
        let held = crew
            .monitor_told(&scout, PERSON, finding(DeskDelivery::Dm, "Mid-reply"))
            .await
            .unwrap();
        assert!(
            matches!(
                held,
                MonitorTold::Held {
                    why: HeldFor::Typing,
                    ..
                }
            ),
            "{held:?}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            told(&scout, DeliveryKind::Monitor).len(),
            2,
            "not into the reply"
        );
        // The reply ends (or the turn is dropped, as a stopped one is).
        drop(turn);
        assert!(!crew.dm_turns.lock().contains_key(&sid));
        let said = eventually("told once the reply ended", async || {
            told(&scout, DeliveryKind::Monitor).into_iter().nth(2)
        })
        .await;
        assert_eq!(said.content, "Mid-reply");
    })
    .await;
}

/// A boss's turn relayed into Scout's DM isn't the person's message: their
/// typing there still holds what Scout's monitors would say.
#[tokio::test(flavor = "multi_thread")]
async fn a_relayed_turn_in_an_agents_dm_leaves_the_persons_typing_hold() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, scout, crew, _d) = scout_under(dir.path(), pool, |_| String::new()).await;
        crew.monitor_told(&scout, PERSON, finding(DeskDelivery::Dm, "First invoice"))
            .await
            .unwrap();
        let sid = scout
            .sessions
            .find(DESK_CHANNEL, PERSON)
            .unwrap()
            .expect("the DM")
            .id;
        assert!(crew.typing_in("scout"));
        crew.relayed_turns.lock().insert(sid.clone());
        let relayed = crew.track(
            &scout,
            DESK_CHANNEL,
            PERSON,
            "Relayed from the boss",
            Some(sid.as_str()),
            (None, None),
        );
        assert!(crew.typing_release("scout").is_some(), "still typing");
        drop(relayed);
        crew.relayed_turns.lock().remove(&sid);
        // Their own message ends it.
        let theirs = crew.track(
            &scout,
            DESK_CHANNEL,
            PERSON,
            "Thanks",
            Some(sid.as_str()),
            (None, None),
        );
        assert!(crew.typing_release("scout").is_none(), "sent");
        drop(theirs);
    })
    .await;
}

/// What comes while older findings wait to be told for the DM goes after
/// them, in one message, not ahead of them.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_finding_waits_behind_older_ones_for_the_same_dm() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, scout, crew, d) = scout_under(dir.path(), pool, |_| String::new()).await;
        let older = workforce_desk::monitor_held_store::HeldFinding {
            id: uuid::Uuid::new_v4(),
            agent: "scout".into(),
            owner: PERSON.into(),
            monitor_id: "invoice-emails".into(),
            monitor_name: "Invoice emails".into(),
            notice: false,
            text: "Older invoice".into(),
            reason: HeldFor::Away,
            held_at: chrono::Utc::now() - chrono::Duration::minutes(5),
            release_at: chrono::Utc::now() - chrono::Duration::seconds(1),
        };
        d.store().monitor_hold(&older).await.unwrap();
        // Being told (taken) by a look that hasn't finished.
        d.store().monitor_held_take(&[older.id]).await.unwrap();
        let held = crew
            .monitor_told(&scout, PERSON, finding(DeskDelivery::Dm, "Newer invoice"))
            .await
            .unwrap();
        assert!(
            matches!(
                held,
                MonitorTold::Held {
                    why: HeldFor::Typing,
                    ..
                }
            ),
            "{held:?}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            told(&scout, DeliveryKind::Monitor).is_empty(),
            "not ahead of the older"
        );

        // That look tells the older and is done: the newer goes after it.
        crew.tell_monitor(&scout, PERSON, false, older.text.clone())
            .await
            .unwrap();
        d.store().monitor_held_done(&[older.id]).await.unwrap();
        crew.update_poke.notify_one();
        let said = eventually("the newer, after", async || {
            told(&scout, DeliveryKind::Monitor).into_iter().nth(1)
        })
        .await;
        let first = &told(&scout, DeliveryKind::Monitor)[0];
        assert_eq!(
            (first.content.as_str(), said.content.as_str()),
            ("Older invoice", "Newer invoice")
        );
    })
    .await;
}

/// A monitor's meta from `action` (TOML), watching a source.
fn meta(action: &str) -> MonitorMeta {
    MonitorDefinition::from_toml(&format!(
        "[monitor]\nid = \"invoice-emails\"\nname = \"Invoice emails\"\n\
         [monitor.source]\ntool = \"email.inbox\"\nitems = \"/messages\"\nitem_id = \"/uid\"\n{action}"
    ))
    .unwrap()
    .monitor
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hub_that_runs_no_desk_refuses_to_tell_anyone_there() {
    let dir = tempfile::tempdir().unwrap();
    let (lone, _llm) = agent(dir.path(), "lone", "ok").await;
    let m = meta(
        "[monitor.action]\ntype = \"notify\"\nsurface = \"desk\"\ntemplate = \"{{json:/subject}}\"\n",
    );
    let ev = MonitorEvent::Observed(workforce_orchestrator::monitor::Observation {
        source: "email.inbox".into(),
        key: "7".into(),
        observed_at: chrono::Utc::now(),
        text: "Invoice".into(),
        value: json!({"subject": "Invoice"}),
        previous: None,
        item: json!({"uid": 7, "subject": "Invoice"}),
    });
    let outcome =
        crate::monitor::desk::deliver(&lone, false, Some(PERSON), &m, &ev, DeskDelivery::Dm, "")
            .await;
    let crate::monitor::ActionOutcome::Terminal(why) = outcome else {
        panic!("a dispatch with no Desk to tell ends for good");
    };
    assert!(why.contains("doesn't run Desk"), "{why}");
    // A hub that runs Desk, before it has started (the catch-up at boot):
    // tried again, not lost.
    let early =
        crate::monitor::desk::deliver(&lone, true, Some(PERSON), &m, &ev, DeskDelivery::Dm, "")
            .await;
    let crate::monitor::ActionOutcome::Transient(why) = early else {
        panic!("a dispatch before Desk starts is tried again");
    };
    assert!(why.contains("hasn't started"), "{why}");
}

/// `email.inbox` over a fake mailbox: each read answers what is in it now,
/// in the tool's own shape.
struct Mailbox(parking_lot::Mutex<Vec<Value>>);

impl Mailbox {
    fn receive(&self, uid: u32, sender: &str, subject: &str) {
        self.0.lock().insert(
            0,
            json!({"uid": uid, "account": "default", "sender": sender, "subject": subject,
                   "date": "Thu, 19 Nov 2026 09:00:00 +0000", "preview": "", "unread": true,
                   "folder": "INBOX"}),
        );
    }
}

#[async_trait::async_trait]
impl workforce_agent::tools::Tool for Mailbox {
    fn name(&self) -> &str {
        "email.inbox"
    }
    fn description(&self) -> &str {
        "Fetch recent messages from one or all email accounts"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {"account": {"type": "string"}, "limit": {"type": "integer"}}})
    }
    async fn execute(&self, _input: Value) -> eyre::Result<workforce_agent::tools::ToolResult> {
        let messages = self.0.lock().clone();
        Ok(workforce_agent::tools::ToolResult::success(
            json!({"messages": messages, "count": messages.len(), "folder": "INBOX", "errors": []}),
            1,
        ))
    }
}

/// Does what the hub's sink does with a Desk delivery, and nothing else.
struct DeskSink(Arc<AgentService>);

#[async_trait::async_trait]
impl crate::monitor::ActionSink for DeskSink {
    async fn dispatch(
        &self,
        monitor: &MonitorMeta,
        owner: Option<&str>,
        ev: &MonitorEvent,
    ) -> crate::monitor::ActionOutcome {
        let place = monitor.action.desk().expect("a Desk monitor");
        crate::monitor::desk::deliver(&self.0, true, owner, monitor, ev, place, "the inbox").await
    }

    async fn tell(
        &self,
        owner: &str,
        _surface: Option<&str>,
        _agent: Option<&str>,
        reason: &str,
        notice: &str,
    ) -> crate::monitor::ActionOutcome {
        crate::monitor::desk::tell_owner(&self.0, true, owner, reason, notice).await
    }
}

/// "Monitor my emails for invoices and tell me here": the monitor the
/// guidance shows, saved as the person's, reads their inbox through the
/// governed path, baselines, then tells them of each new invoice email in
/// Scout's DM, once.
#[tokio::test(flavor = "multi_thread")]
async fn an_inbox_monitor_tells_the_person_of_each_new_invoice_in_desk() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        with_test_pool(
            workforce_orchestrator::monitor::MonitorStore::build_pool,
            |mpool| async move {
                let dir = tempfile::tempdir().unwrap();
                let (_overseer, scout, _crew, _d) =
                    scout_under(dir.path(), pool, |_| String::new()).await;
                let mailbox = Arc::new(Mailbox(parking_lot::Mutex::new(Vec::new())));
                mailbox.receive(1, "Old News <news@example.com>", "Weekly digest");
                scout.register_tool(Arc::clone(&mailbox) as Arc<dyn workforce_agent::tools::Tool>);

                let defs =
                    workforce_orchestrator::workflow_store::WorkflowStore::new(mpool.clone())
                        .await
                        .unwrap();
                let runtime = Arc::new(crate::monitor::MonitorRuntime::new(
                    dir.path().join("monitors"),
                ));
                runtime.set_store(defs);
                runtime
                    .save_definition_source(
                        "invoice-emails",
                        r#"
[monitor]
id = "invoice-emails"
name = "Invoice emails"
[monitor.source]
tool = "email.inbox"
input = { account = "default", limit = 20 }
every_secs = 300
items = "/messages"
item_id = "/uid"
[monitor.condition]
type = "contains"
path = "/subject"
values = ["invoice"]
[monitor.action]
type = "notify"
surface = "desk"
template = "Invoice email from {{json:/sender}}: {{json:/subject}}"
"#,
                        Some(PERSON),
                        workforce_core::access::Visibility::Private,
                        workforce_orchestrator::workflow_store::EditContext::new(
                            "agent_tool",
                            Some(PERSON),
                        ),
                    )
                    .await
                    .unwrap();
                let store = Arc::new(
                    workforce_orchestrator::monitor::MonitorStore::new(mpool)
                        .await
                        .unwrap(),
                );
                let engine = crate::monitor::MonitorEngine::new(
                    Arc::clone(&runtime),
                    Arc::clone(&store),
                    Arc::clone(&scout.surface_registry),
                    Arc::new(DeskSink(Arc::clone(&scout))),
                    crate::monitor::EngineAgent {
                        id: "scout".into(),
                        name: "Scout".into(),
                        is_default: true,
                    },
                    crate::monitor::MonitorEngineConfig::default().on_desk(true),
                )
                .with_invoker(Arc::new(
                    crate::monitor::GovernedInvoker::for_service(Arc::clone(&scout)),
                ));
                let mon = runtime.get("invoice-emails", Some(PERSON)).await.unwrap();
                engine.preflight(&mon.def, Some(PERSON)).await.unwrap();
                let state = || async {
                    store
                        .source_states(std::slice::from_ref(&mon.monitor_key()))
                        .await
                        .unwrap()
                        .remove(&mon.monitor_key())
                };

                let first = engine.poll(&mon, None).await;
                assert_eq!(first.error, None, "read through the governed path");
                assert_eq!(first.fired, 0, "the first read baselines");

                mailbox.receive(
                    2,
                    "Acme Billing <billing@acme.com>",
                    "Invoice 42 for November",
                );
                mailbox.receive(3, "A friend <friend@example.com>", "Lunch?");
                let second = engine.poll(&mon, state().await.as_ref()).await;
                assert_eq!((second.error, second.fired), (None, 1));
                let said = told(&scout, DeliveryKind::Monitor);
                assert_eq!(said.len(), 1);
                assert_eq!(
                    said[0].content,
                    "Invoice email from Acme Billing <billing@acme.com>: Invoice 42 for November"
                );

                // Read again with nothing new: told nothing again.
                let third = engine.poll(&mon, state().await.as_ref()).await;
                assert_eq!(third.fired, 0);
                assert_eq!(told(&scout, DeliveryKind::Monitor).len(), 1);

                // A server hub refuses to save it.
                let server = crate::monitor::MonitorEngine::new(
                    Arc::clone(&runtime),
                    Arc::clone(&store),
                    Arc::clone(&scout.surface_registry),
                    Arc::new(DeskSink(Arc::clone(&scout))),
                    crate::monitor::EngineAgent {
                        id: "scout".into(),
                        name: "Scout".into(),
                        is_default: true,
                    },
                    crate::monitor::MonitorEngineConfig::default(),
                )
                .with_invoker(Arc::new(
                    crate::monitor::GovernedInvoker::for_service(Arc::clone(&scout)),
                ));
                let refused = server.preflight(&mon.def, Some(PERSON)).await.unwrap_err();
                assert!(refused.contains("doesn't run Desk"), "{refused}");
            },
        )
        .await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_tasks_answer_is_what_desk_is_told() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_overseer, scout, _crew, _d) = scout_under(dir.path(), pool, |_| String::new()).await;
        let m = meta(
            "[monitor.action]\ntype = \"agent_task\"\nsurface = \"desk\"\n\
             prompt = \"Say who the invoice is from and when it is due.\"\n",
        );
        let ev = MonitorEvent::Observed(workforce_orchestrator::monitor::Observation {
            source: "email.inbox".into(),
            key: "2".into(),
            observed_at: chrono::Utc::now(),
            text: "Invoice 42".into(),
            value: json!({"uid": 2, "subject": "Invoice 42"}),
            previous: None,
            item: json!({"uid": 2, "subject": "Invoice 42"}),
        });
        let outcome = crate::monitor::desk::deliver(
            &scout,
            true,
            Some(PERSON),
            &m,
            &ev,
            DeskDelivery::Dm,
            "email.inbox",
        )
        .await;
        assert!(
            matches!(outcome, crate::monitor::ActionOutcome::Done(_)),
            "the turn ran and its answer was told"
        );
        let said = told(&scout, DeliveryKind::Monitor);
        assert_eq!(said.len(), 1);
        assert_eq!(said[0].content, "Acme's invoice 42 is due Friday.");
    })
    .await;
}
