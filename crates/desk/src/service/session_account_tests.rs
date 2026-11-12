//! Accounts against Postgres and a fake model: a turn's end is read from
//! its exchanges once, two refreshes at once make one call, a write that
//! loses merges and never overwrites, a short turn and a spent day are
//! said plainly, and the floor's gist is handed over without identifiers.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use parking_lot::Mutex;
use uuid::Uuid;
use workforce_postgres::test_support::with_test_pool;

use super::{AccountMoment as Moment, Boss, Desk};
use crate::account::{Account, Accountant, Call, Limits};
use crate::engine::Engine;
use crate::ledger_store::Cursor;
use crate::memory::record::{Asker, Exchange, Step};
use crate::memory_store::RecordWrite;
use crate::scope::Scope;
use crate::store::{build_pool, tests::record_fixture, DeskStore};

/// A gist handed over: the session, its purpose and now, by the model.
type Handed = (Uuid, Option<String>, Option<String>, bool);

/// A model for tests: it answers what it is given, slowly when told, and
/// keeps what it was asked and the gists it was handed.
#[derive(Default)]
struct Fake {
    answers: Mutex<Vec<String>>,
    asked: Mutex<Vec<(Uuid, Call, String)>>,
    gists: Mutex<Vec<Handed>>,
    slow: Mutex<Duration>,
    /// Runs as the call is made: another writer, in the middle.
    during: Mutex<Option<Box<dyn Fn() -> tokio::task::JoinHandle<()> + Send + Sync>>>,
}

impl Fake {
    fn answering(answers: &[&str]) -> Arc<Self> {
        let f = Self::default();
        *f.answers.lock() = answers.iter().rev().map(|a| a.to_string()).collect();
        Arc::new(f)
    }

    fn calls(&self) -> usize {
        self.asked.lock().len()
    }
}

#[async_trait::async_trait]
impl Accountant for Fake {
    async fn answer(
        &self,
        session: Uuid,
        _engine: Engine,
        _system: &str,
        user: &str,
        call: Call,
    ) -> eyre::Result<String> {
        self.asked.lock().push((session, call, user.to_string()));
        let during = self.during.lock().as_ref().map(|f| f());
        if let Some(h) = during {
            h.await.unwrap();
        }
        let slow = *self.slow.lock();
        tokio::time::sleep(slow).await;
        self.answers
            .lock()
            .pop()
            .ok_or_else(|| eyre::eyre!("no answer left"))
    }

    async fn gist(
        &self,
        session: Uuid,
        purpose: Option<String>,
        now: Option<String>,
        by_model: bool,
    ) {
        self.gists.lock().push((session, purpose, now, by_model));
    }
}

const ANSWER: &str = r##"{"purpose": "Fix the login page so people can sign in again.",
  "plan": "Find the bug, fix it, open a PR.",
  "done": [{"text": "Opened the fix for review", "refs": [{"kind": "pr", "id": "#622"}]}],
  "now": "Merged PR #622 and is waiting for CI on main.",
  "waiting_on": "CI", "next": "Deploy once CI is green, within the hour.",
  "decisions": [{"what": "Kept the old cookie name", "why": "the mobile app reads it"}]}"##;

fn exchange(key: &str, minutes_ago: i64, answer: &str, steps: usize) -> Exchange {
    let at = Utc::now() - chrono::Duration::minutes(minutes_ago);
    Exchange {
        key: key.into(),
        at,
        ended: Some(at + chrono::Duration::seconds(30)),
        asker: Asker::Person,
        ask: format!("Please do {key}"),
        answer: answer.into(),
        steps: (0..steps)
            .map(|i| Step {
                id: format!("{key}-{i}"),
                tool: "Bash".into(),
                input: "cargo test -p login".into(),
                result: "ok".into(),
                failed: false,
            })
            .collect(),
        files: Default::default(),
        commands: Vec::new(),
        errors: Vec::new(),
        compaction: false,
        redacted: 0,
        flagged: Default::default(),
    }
}

async fn harvested(store: &DeskStore, session: Uuid, closed: &[Exchange]) {
    store
        .record_write(&RecordWrite {
            session,
            engine: Engine::Claude,
            source: "/records/t.jsonl",
            cursor: &Cursor::default(),
            rows: &[],
            closed,
            open: None,
            project: "/dev/acme-web",
            folder: "/dev/acme-web",
            name: "acme-web",
            cli_session_id: None,
        })
        .await
        .unwrap();
}

/// A desk with one session a boss oversees, and its model.
async fn start(
    pool: &workforce_postgres::PostgresPool,
    dir: &std::path::Path,
    fake: &Arc<Fake>,
) -> (Desk, Uuid, tokio_util::sync::CancellationToken) {
    DeskStore::migrate(pool).await.unwrap();
    let store = DeskStore::from_pool(pool.clone());
    let r = record_fixture("login-fix");
    store.put(&r).await.unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let d = Desk::start(
        DeskStore::from_pool(pool.clone()),
        crate::testing::options(dir),
        super::Floor {
            accountant: Some(Arc::clone(fake) as Arc<dyn Accountant>),
            redactor: Some(Arc::new(crate::memory::record::tests::Plain)),
            ..Default::default()
        },
        cancel.clone(),
    )
    .await
    .unwrap();
    d.set_bosses(vec![Boss {
        agent: "boss".into(),
        name: "Desk".into(),
        scope: Scope::Ungrouped,
        level: "act".into(),
    }]);
    (d, r.id, cancel)
}

async fn kept(d: &Desk, id: Uuid) -> Account {
    d.session_account(id)
        .await
        .unwrap()
        .expect("an account is kept")
}

#[tokio::test]
async fn a_turn_end_is_read_from_its_exchanges_once_and_hands_the_floor_a_plain_gist() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::answering(&[ANSWER]);
        let (d, id, _cancel) = start(&pool, dir.path(), &fake).await;
        assert!(d.account_kept_for(id));
        let store = DeskStore::from_pool(pool.clone());
        harvested(
            &store,
            id,
            &[exchange("x1", 10, &"I found the bug. ".repeat(40), 3)],
        )
        .await;
        let a = d
            .account_refresh(id, Moment::TurnEnd)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fake.calls(), 1);
        let (_, call, asked) = fake.asked.lock()[0].clone();
        assert_eq!(call, Call::Refresh);
        assert!(
            asked.contains("EXTERNAL_UNTRUSTED_CONTENT_"),
            "the record is wrapped"
        );
        assert!(asked.contains("Please do x1"));
        assert!(asked.contains("fix the auth race"), "its goal is given");
        assert_eq!(a.purpose, "Fix the login page so people can sign in again.");
        assert_eq!(a.done[0].refs[0].id, "#622", "the account keeps its refs");
        assert_eq!(a.waiting_on.as_deref(), Some("CI"));
        assert!(a.by_model);
        assert!(a.asked_by.starts_with("you"), "{}", a.asked_by);
        assert_eq!(a.version, 1);
        // The floor's gist from the same answer, without the ids.
        let gists = fake.gists.lock().clone();
        assert_eq!(gists.len(), 1);
        let now = gists[0].2.clone().unwrap();
        assert!(!now.contains("#622"), "{now}");
        assert!(gists[0].3);
        // Nothing new: no call, and the account stands.
        let again = d
            .account_refresh(id, Moment::TurnEnd)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fake.calls(), 1, "an exchange is read once");
        assert_eq!(again.version, 1);
    })
    .await;
}

#[tokio::test]
async fn a_backlog_is_read_oldest_first_and_none_is_passed_over() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::answering(&[ANSWER, ANSWER, ANSWER, ANSWER]);
        let (d, id, _cancel) = start(&pool, dir.path(), &fake).await;
        let store = DeskStore::from_pool(pool.clone());
        harvested(&store, id, &[exchange("a00", 40, &"Began. ".repeat(60), 1)]).await;
        d.account_refresh(id, Moment::TurnEnd)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fake.calls(), 1);
        // More than one refresh reads: a long gap, or refreshes that failed.
        let backlog: Vec<Exchange> = (0..20)
            .map(|i| exchange(&format!("b{i:02}"), 30 - i, &"Worked on. ".repeat(60), 1))
            .collect();
        harvested(&store, id, &backlog).await;
        d.account_refresh(id, Moment::TurnEnd)
            .await
            .unwrap()
            .unwrap();
        let asked: Vec<String> = fake.asked.lock().iter().map(|a| a.2.clone()).collect();
        assert_eq!(asked.len(), 3, "the backlog is caught up on in rounds");
        assert!(asked[1].contains("Please do b00"), "the oldest first");
        assert!(!asked[1].contains("Please do b19"));
        assert!(asked[2].contains("Please do b19"));
        assert!(!asked[2].contains("Please do b00"), "each read once");
        d.account_refresh(id, Moment::TurnEnd)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fake.calls(), 3, "nothing is left unread");
        assert_eq!(fake.gists.lock().len(), 3, "one gist per turn end");
    })
    .await;
}

#[tokio::test]
async fn two_refreshes_at_once_make_one_call() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::answering(&[ANSWER, ANSWER]);
        *fake.slow.lock() = Duration::from_millis(300);
        let (d, id, _cancel) = start(&pool, dir.path(), &fake).await;
        let store = DeskStore::from_pool(pool.clone());
        harvested(
            &store,
            id,
            &[exchange("x1", 10, &"Long work. ".repeat(60), 2)],
        )
        .await;
        let (a, b) = tokio::join!(
            d.account_refresh(id, Moment::Read),
            d.account_refresh(id, Moment::Read)
        );
        assert_eq!(fake.calls(), 1, "the second caller waits for the first's");
        assert_eq!(a.unwrap().unwrap().version, b.unwrap().unwrap().version);
        // A turn that ends during a refresh gets one more after it.
        harvested(
            &store,
            id,
            &[exchange("x2", 1, &"More work. ".repeat(60), 2)],
        )
        .await;
        let first = d.account_refresh(id, Moment::Read);
        let ended = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            d.account_refresh(id, Moment::TurnEnd).await
        };
        let (_, after) = tokio::join!(first, ended);
        assert_eq!(after.unwrap().unwrap().version, 2);
        assert_eq!(fake.calls(), 2, "the second read finds x2 read already");
    })
    .await;
}

#[tokio::test]
async fn a_refresh_that_loses_its_write_merges_onto_the_newer_account() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::answering(&[ANSWER]);
        let (d, id, _cancel) = start(&pool, dir.path(), &fake).await;
        let store = DeskStore::from_pool(pool.clone());
        harvested(&store, id, &[exchange("x1", 10, &"Work. ".repeat(80), 2)]).await;
        // Another writer keeps an account while the model answers.
        let rival = pool.clone();
        *fake.during.lock() = Some(Box::new(move || {
            let rival = DeskStore::from_pool(rival.clone());
            tokio::spawn(async move {
                let mut a = Account::new(id, "", crate::crew::now());
                a.done.push(crate::account::Done {
                    text: "Wrote the failing test".into(),
                    refs: vec![],
                    at: crate::crew::now(),
                });
                a.purpose = "Rival purpose.".into();
                rival.session_account_put(&a).await.unwrap();
            })
        }));
        let a = d
            .account_refresh(id, Moment::TurnEnd)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(a.version, 2, "written onto the rival's version");
        let texts: Vec<&str> = a.done.iter().map(|d| d.text.as_str()).collect();
        assert_eq!(
            texts,
            ["Wrote the failing test", "Opened the fix for review"],
            "nothing the rival kept is lost"
        );
        assert_eq!(kept(&d, id).await, a);
    })
    .await;
}

#[tokio::test]
async fn a_short_turn_and_a_spent_day_are_said_plainly() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::answering(&[ANSWER]);
        let (d, id, _cancel) = start(&pool, dir.path(), &fake).await;
        let store = DeskStore::from_pool(pool.clone());
        harvested(&store, id, &[exchange("x1", 10, "Done: it was a typo.", 0)]).await;
        let a = d
            .account_refresh(id, Moment::TurnEnd)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fake.calls(), 0, "a short turn asks no model");
        assert_eq!(a.now, "Finished: Done: it was a typo.");
        assert!(!a.by_model);
        // Past the day's ceiling: plainly, and the day says so.
        d.set_account_limits(Limits {
            cheap_calls_per_day: 0,
            account_questions_per_day: 0,
        });
        harvested(
            &store,
            id,
            &[exchange("x2", 5, &"A long answer. ".repeat(60), 4)],
        )
        .await;
        let a = d
            .account_refresh(id, Moment::TurnEnd)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fake.calls(), 0, "past the ceiling no model is asked");
        assert!(a.now.starts_with("Finished: A long answer."), "{}", a.now);
        assert!(!a.by_model);
        assert!(d.account_thin().await);
        assert_eq!(fake.gists.lock().len(), 2, "the floor still gets its gist");
    })
    .await;
}

#[tokio::test]
async fn a_session_no_boss_oversees_gets_no_account() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::answering(&[ANSWER]);
        let (d, id, _cancel) = start(&pool, dir.path(), &fake).await;
        d.set_bosses(vec![]);
        assert!(!d.account_kept_for(id));
        let store = DeskStore::from_pool(pool.clone());
        harvested(&store, id, &[exchange("x1", 10, &"Work. ".repeat(80), 2)]).await;
        assert!(d
            .account_refresh(id, Moment::TurnEnd)
            .await
            .unwrap()
            .is_none());
        assert_eq!(fake.calls(), 0);
        assert!(
            fake.gists.lock().is_empty(),
            "the floor writes its own gist"
        );
    })
    .await;
}

#[tokio::test]
async fn forgetting_a_session_forgets_its_account() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::answering(&[ANSWER]);
        let (d, id, _cancel) = start(&pool, dir.path(), &fake).await;
        let store = DeskStore::from_pool(pool.clone());
        harvested(&store, id, &[exchange("x1", 10, &"Work. ".repeat(80), 2)]).await;
        d.account_refresh(id, Moment::TurnEnd).await.unwrap();
        assert!(d.session_account(id).await.unwrap().is_some());
        store.forget_memory(id).await.unwrap();
        assert!(d.session_account(id).await.unwrap().is_none());
    })
    .await;
}

/// The harvest's timing a refresh relies on, as the code has it: a
/// working session's record is read on its tool calls at most once a
/// minute, keeping the exchange still open; its turn's end reads it again,
/// and that read refreshes the account.
#[tokio::test]
async fn an_open_exchange_is_kept_mid_turn_and_the_turns_end_refreshes_the_account() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool.clone());
        let fixture = include_str!("../../fixtures/ledger/claude.jsonl");
        let lines: Vec<&str> = fixture.lines().collect();
        let t = dir.path().join("t.jsonl");
        // The ask and its first tool call: the turn still runs.
        std::fs::write(&t, lines[..4].join("\n") + "\n").unwrap();
        let mut r = record_fixture("login-race");
        r.cwd = dir.path().to_string_lossy().into_owned();
        r.transcript = Some(t.to_string_lossy().into_owned());
        store.put(&r).await.unwrap();
        let fake = Fake::answering(&[ANSWER]);
        let cancel = tokio_util::sync::CancellationToken::new();
        let d = Desk::start(
            DeskStore::from_pool(pool.clone()),
            crate::testing::options(dir.path()),
            super::Floor {
                accountant: Some(Arc::clone(&fake) as Arc<dyn Accountant>),
                redactor: Some(Arc::new(crate::memory::record::tests::Plain)),
                ..Default::default()
            },
            cancel.clone(),
        )
        .await
        .unwrap();
        d.set_bosses(vec![Boss {
            agent: "boss".into(),
            name: "Desk".into(),
            scope: Scope::Ungrouped,
            level: "act".into(),
        }]);
        let open = || async {
            let client = store.pool().get().await.unwrap();
            client
                .query_opt(
                    "SELECT ended IS NULL, length(answer) FROM memory_exchanges WHERE session = $1",
                    &[&r.id],
                )
                .await
                .unwrap()
                .map(|row| (row.get::<_, bool>(0), row.get::<_, i32>(1)))
        };
        // A tool call while it works reads the record: the exchange is
        // kept, still open.
        d.spend_read_while_working(r.id);
        let mut kept = None;
        for _ in 0..200 {
            kept = open().await;
            if kept.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(kept.map(|k| k.0), Some(true), "kept mid-turn, still open");
        // The turn ends: its reading closes the exchange and refreshes the
        // account from it.
        std::fs::write(&t, fixture).unwrap();
        d.spend_read_ending(r.id);
        let mut account = None;
        for _ in 0..400 {
            account = d.session_account(r.id).await.unwrap();
            if account.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let account = account.expect("the turn's end wrote the account");
        assert_eq!(
            account.purpose,
            "Fix the login page so people can sign in again."
        );
        assert!(fake.asked.lock()[0]
            .2
            .contains("Fix the login race in acme-web"));
        assert_eq!(
            open().await.map(|k| k.0),
            Some(false),
            "closed at the turn's end"
        );
        cancel.cancel();
    })
    .await;
}
