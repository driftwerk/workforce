//! A boss's updates (overseer H7): nothing changed is no post, a result
//! goes out within the gather, decisions read as their rows, the composer
//! off or failing gives the plain update, Suggest's check-in is the one
//! post, and a restart neither loses nor repeats an update.

use super::super::actions::{Action, Target};
use super::super::updates::UpdateWriter;
use super::*;
use workforce_common::channels::desk::{DeliveryKind, DeskPlace, DeskTarget};
use workforce_desk::account::{Account, Done, Ref, RefKind};
use workforce_desk::open_item::{NewItem, OpenKind};

/// A writer that answers `answer`, or fails.
struct Writer(Option<String>);

#[async_trait::async_trait]
impl UpdateWriter for Writer {
    async fn write(&self, _: &str, _: &str, _: &str, user: &str) -> eyre::Result<String> {
        match &self.0 {
            // Each session given, by its id, in the boss's words.
            Some(said) => {
                let ids: Vec<String> = user
                    .lines()
                    .filter_map(|l| l.strip_prefix("Session "))
                    .filter_map(|l| l.split_whitespace().next())
                    .map(|id| format!("{{\"id\": \"{id}\", \"text\": \"{said}\"}}"))
                    .collect();
                Ok(format!("{{\"sessions\": [{}]}}", ids.join(", ")))
            }
            None => eyre::bail!("the model is down"),
        }
    }
}

/// A writer that gives each decision `action` and `why`, and each
/// session `said`.
struct Deciding {
    action: String,
    why: String,
}

#[async_trait::async_trait]
impl UpdateWriter for Deciding {
    async fn write(&self, _: &str, _: &str, system: &str, user: &str) -> eyre::Result<String> {
        assert!(system.contains("at most 70 characters"));
        let ids = |prefix: &str| -> Vec<String> {
            user.lines()
                .filter_map(|l| l.strip_prefix(prefix))
                .filter_map(|l| l.split_whitespace().next())
                .map(str::to_string)
                .collect()
        };
        let sessions: Vec<String> = ids("Session ")
            .iter()
            .map(|id| format!("{{\"id\": \"{id}\", \"text\": \"It merged.\"}}"))
            .collect();
        let decisions: Vec<serde_json::Value> = ids("Decision ")
            .iter()
            .map(|id| serde_json::json!({"id": id, "action": self.action, "why": self.why}))
            .collect();
        Ok(format!(
            "{{\"sessions\": [{}], \"decisions\": {}}}",
            sessions.join(", "),
            serde_json::Value::from(decisions)
        ))
    }
}

/// A writer that counts its calls.
struct Counting(std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl UpdateWriter for Counting {
    async fn write(&self, _: &str, _: &str, _: &str, _: &str) -> eyre::Result<String> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok("{\"sessions\": []}".into())
    }
}

/// The DM's messages delivered as updates.
fn updates(overseer: &AgentService) -> Vec<workforce_common::session::SessionMessage> {
    overseer
        .sessions
        .find(DESK_CHANNEL, PERSON)
        .unwrap()
        .map(|s| s.messages)
        .unwrap_or_default()
        .into_iter()
        .filter(|m| {
            m.desk
                .as_ref()
                .and_then(|d| d.delivery)
                .is_some_and(|k| k == DeliveryKind::Update)
        })
        .collect()
}

/// Keeps `s`'s account with `done` (each with a PR ref when it names one),
/// `now` and `next`, over whatever it had.
async fn account(d: &Desk, s: uuid::Uuid, done: &[&str], now: &str, next: &str) {
    let mut a = d
        .session_accounts_of(&[s])
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap_or_else(|| Account::new(s, "", chrono::Utc::now()));
    for text in done {
        a.done.push(Done {
            text: (*text).into(),
            refs: text
                .contains("PR")
                .then(|| Ref {
                    kind: RefKind::Pr,
                    id: "#622".into(),
                    url: None,
                })
                .into_iter()
                .collect(),
            at: chrono::Utc::now(),
        });
    }
    a.now = now.into();
    a.next = next.into();
    d.store().session_account_put(&a).await.unwrap();
}

/// The boss's state is begun before anything is done, so all of it is news.
async fn begun(crew: &Arc<CrewHub>) {
    crew.look_at_updates().await;
    tokio::time::sleep(Duration::from_millis(20)).await;
}

const ACT: &str = "[oversight]\nenabled = true\nlevel = \"act\"\nact = { nudge = true }\n";

/// What a session does this minute isn't news; a PR it opened is, sent
/// within the gather, in the writer's words, with its ref.
#[tokio::test(flavor = "multi_thread")]
async fn nothing_changed_is_no_post_and_a_result_goes_out_within_the_gather() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        crew.set_update_writer(Arc::new(Writer(Some(
            "PR #622 is up; I'll merge it once CI is green.".into(),
        ))));
        begun(&crew).await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        account(&d, s.id, &[], "Reading the code", "").await;
        // Told once, with nothing done: then only its now moves.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        account(&d, s.id, &[], "Writing the endpoint", "").await;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(
            updates(&overseer).is_empty(),
            "what it does this minute is no news"
        );

        let started = std::time::Instant::now();
        account(
            &d,
            s.id,
            &["Opened PR #622 for the lead picker"],
            "Waiting on CI",
            "Merge it",
        )
        .await;
        let posted = eventually("the update", async || updates(&overseer).into_iter().next()).await;
        // 3 minutes of gather at 100 ms a minute, and a tick.
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "{:?}",
            started.elapsed()
        );
        assert!(
            posted
                .content
                .starts_with("**acme-web** — PR #622 is up; I'll merge it once CI is green."),
            "{}",
            posted.content
        );
        let extras = d
            .store()
            .extras(std::slice::from_ref(&posted.id))
            .await
            .unwrap()
            .remove(&posted.id)
            .and_then(|e| e.update)
            .expect("the update's extras");
        assert_eq!(extras.sessions.len(), 1);
        assert_eq!(extras.sessions[0].session_id, s.id.to_string());
        assert_eq!(extras.sessions[0].refs[0].id, "#622");
        // Told: nothing new is no second update.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(updates(&overseer).len(), 1);
    })
    .await;
}

/// A decision reads as its row, word for word, in the text and the extras;
/// the composer off gives the plain update, and so does a writer that
/// fails.
#[tokio::test(flavor = "multi_thread")]
async fn decisions_read_as_their_rows_and_the_plain_update_stands_in() {
    for (toml, writer) in [
        (
            format!("{ACT}updates = {{ enabled = false }}\n"),
            Some("unused".to_string()),
        ),
        (ACT.to_string(), None),
    ] {
        with_test_pool(workforce_desk::store::build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", &toml, &[]).await;
            crew.set_update_writer(Arc::new(Writer(writer)));
            begun(&crew).await;
            let s = new_session(&d, dir.path(), "acme-web").await;
            let t0 = chrono::Utc::now() - chrono::Duration::minutes(1);
            crew.perform(
                &Action::Nudge {
                    to: Target::Session(s.id),
                    message: "Wait for green CI before merging.".into(),
                    reply_to: None,
                },
                crew.by_overseer("CI is red on main.").unwrap(),
            )
            .await
            .unwrap();
            account(&d, s.id, &["Merged PR #622"], "", "Deploy it").await;
            let row = d
                .store()
                .decisions_since("desk", t0, 10)
                .await
                .unwrap()
                .pop()
                .expect("the decision");
            let posted =
                eventually("the update", async || updates(&overseer).into_iter().next()).await;
            assert!(posted.content.contains(&row.said()), "{}", posted.content);
            assert!(
                posted
                    .content
                    .starts_with("**acme-web** — Done: Merged PR #622. Next: Deploy it."),
                "written plainly: {}",
                posted.content
            );
            // Its extras are kept just after it posts.
            let extras = eventually("its extras", async || {
                d.store()
                    .extras(std::slice::from_ref(&posted.id))
                    .await
                    .unwrap()
                    .remove(&posted.id)
                    .and_then(|e| e.update)
            })
            .await;
            assert_eq!(extras.decisions, vec![row.line()]);
            // Laid out plainly: a short line that never says the message,
            // which is kept whole beside it.
            assert_eq!(row.brief, "Sent acme-web a message.");
            assert_eq!(row.brief_why, "CI is red on main.");
            assert_eq!(
                row.message.as_deref(),
                Some("Wait for green CI before merging.")
            );
            assert_eq!(row.kind, workforce_desk::decision::DecisionKind::Acted);
            let l = &extras.decisions[0];
            assert_eq!(l.action, "Sent acme-web a message.");
            assert!(!l.action.contains("green CI") && !l.why.contains("green CI"));
            assert_eq!(l.message, row.message);
        })
        .await;
    }
}

/// The composer writes each decision's short line and why, never with the
/// message in them (one that does, or runs past 70 characters, is written
/// plainly), and what needs the person is laid out as the boss gave it.
#[tokio::test(flavor = "multi_thread")]
async fn decisions_and_needs_you_are_laid_out() {
    let message = "Hold the release until the migration check passes on staging.";
    for (action, why, expect_action, expect_why) in [
        (
            "Put a hold on tonight's release.",
            "The migration check failed on staging.",
            "Put a hold on tonight's release.",
            "The migration check failed on staging.",
        ),
        (
            "Told acme-web to hold the release until the migration check passes",
            "It said: hold the release until the migration check passes.",
            "Sent acme-web a message.",
            "The migration check failed on staging.",
        ),
    ] {
        with_test_pool(workforce_desk::store::build_pool, |pool| async move {
            let dir = tempfile::tempdir().unwrap();
            let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
            crew.set_update_writer(Arc::new(Deciding {
                action: action.into(),
                why: why.into(),
            }));
            begun(&crew).await;
            let s = new_session(&d, dir.path(), "acme-web").await;
            let mut ask = NewItem::ask(
                "desk",
                OpenKind::Ask,
                Some(s.id),
                "Two staging records need a decision before the next PR. Which way?",
            );
            ask.options = vec!["Deactivate the roles".into(), "Leave both".into()];
            ask.recommendation = Some("Deactivate the roles; they can be put back.".into());
            ask.layout = workforce_desk::open_item::AskLayout {
                question: Some("Two staging records need a decision.".into()),
                points: vec!["A user with no organisation.".into()],
                recommended: Some("Deactivate the roles".into()),
                not_blocking: Some("The docs PR doesn't depend on it.".into()),
            };
            d.store().item_open(&ask).await.unwrap();
            crew.perform(
                &Action::Nudge {
                    to: Target::Session(s.id),
                    message: message.into(),
                    reply_to: None,
                },
                crew.by_overseer("The migration check failed on staging. Twice.")
                    .unwrap(),
            )
            .await
            .unwrap();
            let posted =
                eventually("the update", async || updates(&overseer).into_iter().next()).await;
            let u = eventually("its extras", async || {
                d.store()
                    .extras(std::slice::from_ref(&posted.id))
                    .await
                    .unwrap()
                    .remove(&posted.id)
                    .and_then(|e| e.update)
            })
            .await;
            let l = &u.decisions[0];
            assert_eq!(
                (l.action.as_str(), l.why.as_str()),
                (expect_action, expect_why)
            );
            assert!(l.action.chars().count() <= 70);
            assert_eq!(l.message.as_deref(), Some(message));
            // The text the chat falls back on still says it in full.
            assert!(posted.content.contains(message), "{}", posted.content);
            let n = &u.needs_you[0];
            assert_eq!(n.question, "Two staging records need a decision.");
            assert_eq!(n.items, vec!["A user with no organisation.".to_string()]);
            assert_eq!(
                n.options
                    .iter()
                    .filter(|o| o.recommended)
                    .map(|o| o.label.as_str())
                    .collect::<Vec<_>>(),
                vec!["Deactivate the roles"]
            );
            assert_eq!(
                n.not_blocking.as_deref(),
                Some("The docs PR doesn't depend on it.")
            );
            assert_eq!(u.waiting[0].options.len(), 2, "waiting stays as it was");
        })
        .await;
    }
}

/// At Suggest the check-in is the one post: no update of its own.
#[tokio::test(flavor = "multi_thread")]
async fn at_suggest_the_check_in_is_the_one_post() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(
            dir.path(),
            pool,
            "Here is where things are.",
            "[oversight]\nenabled = true\nlevel = \"suggest\"\n",
            &[],
        )
        .await;
        begun(&crew).await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        account(&d, s.id, &["Opened PR #622"], "", "Merge it").await;
        let check_in = eventually("the update's check-in", async || {
            d.store()
                .latest_check_in("desk")
                .await
                .ok()
                .flatten()
                .filter(|c| {
                    c.prompt.contains("Your update to the person is due") && c.reply.is_some()
                })
        })
        .await;
        assert!(
            check_in
                .prompt
                .contains("**acme-web** — Done: Opened PR #622."),
            "{}",
            check_in.prompt
        );
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(updates(&overseer).is_empty(), "no update of its own");
        // Told once: the same check-in is still the latest.
        assert_eq!(
            d.store()
                .latest_check_in("desk")
                .await
                .unwrap()
                .map(|c| c.id),
            Some(check_in.id),
            "told once"
        );
        // Its message, in the DM, is the one post.
        let dm = overseer
            .sessions
            .find(DESK_CHANNEL, PERSON)
            .unwrap()
            .unwrap();
        assert_eq!(
            dm.messages
                .iter()
                .filter(|m| m.content.contains("Here is where things are."))
                .count(),
            1
        );
    })
    .await;
}

/// An update written but not sent when the hub stopped goes out once
/// after; one already in the DM isn't sent again.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_neither_loses_nor_repeats_an_update() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        begun(&crew).await;
        let pending = |text: &str| workforce_desk::update::Pending {
            id: uuid::Uuid::new_v4(),
            text: text.into(),
            update: Default::default(),
            told: Default::default(),
            restated: vec![],
            cutoff: None,
            handed: false,
            sent: None,
            away: None,
        };
        // Kept before it went: sent once.
        d.store()
            .update_pend("desk", &pending("Kept, not sent."))
            .await
            .unwrap();
        crew.look_at_updates().await;
        crew.look_at_updates().await;
        eventually("sent", async || updates(&overseer).into_iter().next()).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(updates(&overseer).len(), 1);
        let state = d
            .store()
            .update_state("desk", chrono::Utc::now())
            .await
            .unwrap();
        assert!(state.pending.is_none() && state.last_update_at.is_some());

        // Sent, but the hub stopped before it was marked: not sent again.
        let p = pending("Sent, not marked.");
        let target =
            DeskTarget::of_turn(&DeskPlace::Dm { thread: None }, DESK_CHANNEL, None, false);
        let mut delivery =
            crate::desk_surface::Delivery::text(DeliveryKind::Update, p.text.clone());
        delivery.request_id = Some(p.id.to_string());
        crate::desk_surface::deliver(&overseer, PERSON, &target, delivery)
            .await
            .unwrap();
        d.store().update_pend("desk", &p).await.unwrap();
        crew.look_at_updates().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(updates(&overseer).len(), 2);
        assert!(d
            .store()
            .update_state("desk", chrono::Utc::now())
            .await
            .unwrap()
            .pending
            .is_none());
    })
    .await;
}

/// An update whose only news is the times the boss left a session alone
/// asks the model nothing: those read as their rows, marked to fold.
#[tokio::test(flavor = "multi_thread")]
async fn left_alone_only_asks_the_composer_nothing() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (overseer, crew, d, _llm) = overseeing(dir.path(), pool, "ok", ACT, &[]).await;
        let writer = Arc::new(Counting(Default::default()));
        crew.set_update_writer(writer.clone());
        begun(&crew).await;
        let s = new_session(&d, dir.path(), "acme-web").await;
        crew.record_nothing("desk", s.id, true, "It is still running its tests.")
            .await;
        let posted = eventually("the update", async || updates(&overseer).into_iter().next()).await;
        let u = eventually("its extras", async || {
            d.store()
                .extras(std::slice::from_ref(&posted.id))
                .await
                .unwrap()
                .remove(&posted.id)
                .and_then(|e| e.update)
        })
        .await;
        let l = &u.decisions[0];
        assert_eq!(l.kind, workforce_desk::decision::DecisionKind::LeftAlone);
        assert_eq!(l.action, "Let acme-web wait.");
        assert_eq!(l.why, "It is still running its tests.");
        assert_eq!(l.message, None);
        assert_eq!(writer.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    })
    .await;
}
