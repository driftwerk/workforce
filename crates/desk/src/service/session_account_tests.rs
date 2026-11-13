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
use crate::memory::derive::fake::FakeDeriver;
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
    /// It may refresh an account and read notes in one call.
    shares: Mutex<bool>,
    /// What each such call answers, the first first; an `Err` fails it.
    shared_answers: Mutex<Vec<Result<String, String>>>,
    /// What each such call was told and given.
    shared_asked: Mutex<Vec<(String, String)>>,
    shared_slow: Mutex<Duration>,
    /// Runs as such a call is made.
    shared_during: Mutex<Option<Box<dyn Fn() -> tokio::task::JoinHandle<()> + Send + Sync>>>,
}

impl Fake {
    fn answering(answers: &[&str]) -> Arc<Self> {
        let f = Self::default();
        *f.answers.lock() = answers.iter().rev().map(|a| a.to_string()).collect();
        Arc::new(f)
    }

    fn calls(&self) -> usize {
        self.asked.lock().len() + self.shared_asked.lock().len()
    }

    /// It shares reads with notes, answering `shared` in turn.
    fn sharing(self: Arc<Self>, shared: Vec<Result<String, String>>) -> Arc<Self> {
        *self.shares.lock() = true;
        *self.shared_answers.lock() = shared;
        self
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

    fn shares_reads(&self) -> bool {
        *self.shares.lock()
    }

    async fn answer_shared(
        &self,
        _session: Uuid,
        _engine: Engine,
        system: &str,
        user: &str,
    ) -> eyre::Result<crate::memory::derive::Answered> {
        self.shared_asked
            .lock()
            .push((system.to_string(), user.to_string()));
        let during = self.shared_during.lock().as_ref().map(|f| f());
        if let Some(h) = during {
            h.await.unwrap();
        }
        let slow = *self.shared_slow.lock();
        tokio::time::sleep(slow).await;
        let next = {
            let mut answers = self.shared_answers.lock();
            match answers.is_empty() {
                true => Err("no answer left".to_string()),
                false => answers.remove(0),
            }
        };
        next.map(|text| crate::memory::derive::Answered {
            text,
            model: "fake-shared".into(),
        })
        .map_err(|e| eyre::eyre!(e))
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
    start_with(pool, dir, fake, None, |_| {}).await
}

/// As [`start`], with a model that notes decisions too, and options.
async fn start_with(
    pool: &workforce_postgres::PostgresPool,
    dir: &std::path::Path,
    fake: &Arc<Fake>,
    deriver: Option<&Arc<FakeDeriver>>,
    tune: impl FnOnce(&mut super::Options),
) -> (Desk, Uuid, tokio_util::sync::CancellationToken) {
    DeskStore::migrate(pool).await.unwrap();
    let store = DeskStore::from_pool(pool.clone());
    let r = record_fixture("login-fix");
    store.put(&r).await.unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut opts = crate::testing::options(dir);
    tune(&mut opts);
    let d = Desk::start(
        DeskStore::from_pool(pool.clone()),
        opts,
        super::Floor {
            accountant: Some(Arc::clone(fake) as Arc<dyn Accountant>),
            deriver: deriver.map(|d| Arc::clone(d) as Arc<dyn crate::memory::derive::Deriver>),
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

#[tokio::test]
async fn a_reader_gets_a_fresh_account_as_kept_and_a_stale_one_refreshed_once_in_two_minutes() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::answering(&[ANSWER, ANSWER, ANSWER]);
        let (d, id, _cancel) = start(&pool, dir.path(), &fake).await;
        let store = DeskStore::from_pool(pool.clone());
        // Its turn end writes it first.
        harvested(
            &store,
            id,
            &[exchange("x1", 10, &"Long work. ".repeat(60), 2)],
        )
        .await;
        d.account_refresh(id, Moment::TurnEnd)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fake.calls(), 1);
        // Fresh: given as kept, with no call.
        let fresh = d.account_read(id).await.unwrap();
        assert!(fresh.account.is_some() && !fresh.refreshed && !fresh.stale);
        assert_eq!(fake.calls(), 1);
        // A turn ended since it was written: refreshed before it is read.
        harvested(
            &store,
            id,
            &[exchange("x2", 1, &"More work. ".repeat(60), 2)],
        )
        .await;
        d.0.accounts.ended.lock().insert(id, Utc::now());
        let read = d.account_read(id).await.unwrap();
        assert!(read.refreshed && !read.stale);
        assert_eq!(fake.calls(), 2);
        // Stale again within two minutes: the kept one, said to be stale.
        harvested(
            &store,
            id,
            &[exchange("x3", 0, &"Even more. ".repeat(60), 2)],
        )
        .await;
        d.0.accounts.ended.lock().insert(id, Utc::now());
        let floor = d.account_read(id).await.unwrap();
        assert!(!floor.refreshed && floor.stale, "{floor:?}");
        assert_eq!(fake.calls(), 2, "one reader's refresh per two minutes");
    })
    .await;
}

#[tokio::test]
async fn a_question_is_answered_from_the_conversation_or_not_at_all() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::answering(&[
            "It signed in with the key named DEPLOY_TOKEN: \"using DEPLOY_TOKEN from the vault\".",
            "Not in the record.",
        ]);
        let (d, id, _cancel) = start(&pool, dir.path(), &fake).await;
        let store = DeskStore::from_pool(pool.clone());
        let mut x = exchange("x1", 10, "Deployed, using DEPLOY_TOKEN from the vault.", 1);
        x.ask = "Deploy the login fix".into();
        harvested(&store, id, &[x]).await;
        let crate::AccountAnswer::Answered(answer) = d
            .account_question(id, "Which API key did it use?")
            .await
            .unwrap()
        else {
            panic!("answered")
        };
        assert!(answer.contains("DEPLOY_TOKEN"));
        let (_, call, asked) = fake.asked.lock()[0].clone();
        assert_eq!(call, Call::Question);
        assert!(
            asked.contains("using DEPLOY_TOKEN from the vault"),
            "from the exchanges"
        );
        assert!(asked.contains("Which API key did it use?"));
        assert_eq!(
            d.account_question(id, "What did it eat?").await.unwrap(),
            crate::AccountAnswer::NotInRecord,
            "what the record doesn't say isn't answered, however it is said"
        );
        // Past the questions' share no model is asked.
        d.set_account_limits(Limits {
            cheap_calls_per_day: 500,
            account_questions_per_day: 0,
        });
        assert_eq!(
            d.account_question(id, "Again?").await.unwrap(),
            crate::AccountAnswer::Unavailable,
            "past the share it couldn't be looked up, which isn't the record saying nothing"
        );
        assert_eq!(fake.calls(), 2);
    })
    .await;
}

/// One answer to both jobs: the account, and two notes of which one cites
/// nothing the record holds.
fn both_answer() -> String {
    let account: serde_json::Value = serde_json::from_str(ANSWER).unwrap();
    serde_json::json!({
        "account": account,
        "notes": {"add": [
            {"kind": "decision", "text": "Keep the old cookie name for the mobile app, signed with SECRET-abc123.", "sources": ["x1"]},
            {"kind": "decision", "text": "Ship on Friday.", "sources": []},
        ]},
    })
    .to_string()
}

async fn derived_to(store: &DeskStore, session: Uuid) -> i32 {
    let client = store.pool().get().await.unwrap();
    client
        .query_opt(
            "SELECT derived_to FROM memory_derive WHERE session = $1",
            &[&session],
        )
        .await
        .unwrap()
        .map_or(0, |r| r.get(0))
}

async fn due_at(store: &DeskStore, session: Uuid) -> Option<chrono::DateTime<Utc>> {
    let client = store.pool().get().await.unwrap();
    client
        .query_one(
            "SELECT due_at FROM memory_derive WHERE session = $1",
            &[&session],
        )
        .await
        .unwrap()
        .get(0)
}

/// Lets the derivation worker go idle after starting, before anything is
/// harvested: it then looks again only when poked or after half a minute.
async fn worker_idle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// Pokes the worker until `check` holds.
async fn until(d: &Desk, what: &str, mut check: impl AsyncFnMut() -> bool) {
    for _ in 0..600 {
        if check().await {
            return;
        }
        d.derive_poke();
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("never: {what}");
}

/// Lets the worker do what it would.
async fn settle(d: &Desk) {
    for _ in 0..8 {
        d.derive_poke();
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn a_turn_end_with_noting_due_reads_its_exchanges_once_for_both() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::answering(&[]).sharing(vec![Ok(both_answer())]);
        let deriver = Arc::new(FakeDeriver::default());
        let (d, id, _cancel) = start_with(&pool, dir.path(), &fake, Some(&deriver), |_| {}).await;
        worker_idle().await;
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
        assert_eq!(fake.calls(), 1, "one call");
        assert!(
            fake.asked.lock().is_empty(),
            "the account isn't asked alone"
        );
        let (system, user) = fake.shared_asked.lock()[0].clone();
        assert!(system.contains("running account") && system.contains("keep notes"));
        assert!(user.contains("The account so far") && user.contains("New exchanges:"));
        assert!(user.contains("Please do x1"));
        assert!(
            !user.contains("Answer with the JSON object only."),
            "the wrapper's shape is the only one asked for"
        );
        // The account is written from its part.
        assert_eq!(a.purpose, "Fix the login page so people can sign in again.");
        assert!(a.by_model);
        // The notes as the worker keeps them: the one citing nothing goes.
        let notes = d.session_memory(id).await.unwrap().notes;
        assert_eq!(notes.counts.decisions, 1);
        assert_eq!(
            notes.decisions[0].text,
            "Keep the old cookie name for the mobile app, signed with [REDACTED TEST_KEY].",
            "kept as safely as the worker keeps its own"
        );
        assert!(notes.decisions[0].noted_by_desk);
        let by: String = {
            let client = store.pool().get().await.unwrap();
            client
                .query_one("SELECT by FROM memory_items WHERE session = $1", &[&id])
                .await
                .unwrap()
                .get(0)
        };
        assert_eq!(by, "model:fake-shared");
        assert_eq!(
            derived_to(&store, id).await,
            1,
            "derivation is moved past it"
        );
        // Counted once against each day's ceiling.
        let day = d.derive_day();
        assert_eq!(store.cheap_calls(day).await.unwrap(), 1);
        assert_eq!(store.derive_calls(day).await.unwrap(), 1);
        // The worker has nothing of them left to read.
        settle(&d).await;
        assert_eq!(deriver.calls(), 0, "the same exchanges are read once");
        assert_eq!(derived_to(&store, id).await, 1);
    })
    .await;
}

#[tokio::test]
async fn an_account_with_noting_held_or_behind_is_refreshed_alone_and_noting_reads_its_own() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::answering(&[ANSWER, ANSWER]).sharing(vec![Ok(both_answer())]);
        let deriver = Arc::new(FakeDeriver::default());
        let (d, id, _cancel) = start_with(&pool, dir.path(), &fake, Some(&deriver), |_| {}).await;
        worker_idle().await;
        let store = DeskStore::from_pool(pool.clone());
        harvested(
            &store,
            id,
            &[exchange("x1", 10, &"I found the bug. ".repeat(40), 3)],
        )
        .await;
        // A worker holds the session's reading: the account doesn't wait.
        let held = store.derive_claim(300.0).await.unwrap().expect("it is due");
        assert_eq!(held.session, id);
        let started = std::time::Instant::now();
        d.account_refresh(id, Moment::TurnEnd)
            .await
            .unwrap()
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "promptly");
        assert_eq!(fake.asked.lock().len(), 1, "alone");
        assert!(fake.shared_asked.lock().is_empty());
        // The worker lets it go; the next turn reads x2 only, and noting,
        // behind at x1, isn't shared.
        store.derive_release(&held, Some(Utc::now())).await.unwrap();
        harvested(
            &store,
            id,
            &[exchange("x2", 5, &"Fixed it. ".repeat(60), 2)],
        )
        .await;
        d.account_refresh(id, Moment::TurnEnd)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fake.asked.lock().len(), 2, "alone again");
        assert!(!fake.asked.lock()[1].2.contains("Please do x1"));
        assert!(fake.shared_asked.lock().is_empty(), "noting is behind");
        let day = d.derive_day();
        assert_eq!(store.derive_calls(day).await.unwrap(), 0);
        // The worker reads its own backlog, both at once.
        until(&d, "noting reads its backlog", async || {
            derived_to(&store, id).await == 2
        })
        .await;
        assert_eq!(deriver.calls(), 1);
        let asked = deriver.asked.lock()[0].1.clone();
        assert!(asked.contains("Please do x1") && asked.contains("Please do x2"));
    })
    .await;
}

#[tokio::test]
async fn an_account_with_memory_paused_is_refreshed_alone_and_sends_nothing_for_notes() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::answering(&[ANSWER]).sharing(vec![Ok(both_answer())]);
        let deriver = Arc::new(FakeDeriver::default());
        let (d, id, _cancel) = start_with(&pool, dir.path(), &fake, Some(&deriver), |_| {}).await;
        worker_idle().await;
        let store = DeskStore::from_pool(pool.clone());
        store
            .put_setting(crate::memory_store::PAUSED, &serde_json::json!(true))
            .await
            .unwrap();
        harvested(
            &store,
            id,
            &[exchange("x1", 10, &"I found the bug. ".repeat(40), 3)],
        )
        .await;
        let was = due_at(&store, id).await;
        d.account_refresh(id, Moment::TurnEnd)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fake.calls(), 1);
        assert!(fake.shared_asked.lock().is_empty(), "alone");
        assert!(
            !fake.asked.lock()[0].2.contains("New exchanges:"),
            "nothing for notes"
        );
        assert_eq!(
            due_at(&store, id).await,
            was,
            "the claim went back to its place"
        );
        settle(&d).await;
        assert_eq!(deriver.calls(), 0);
        assert_eq!(derived_to(&store, id).await, 0);
    })
    .await;
}

#[tokio::test]
async fn an_account_with_noting_off_for_the_desk_is_refreshed_alone() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::answering(&[ANSWER]).sharing(vec![Ok(both_answer())]);
        let deriver = Arc::new(FakeDeriver::default());
        let (d, id, _cancel) = start_with(&pool, dir.path(), &fake, Some(&deriver), |o| {
            o.memory.derive = false;
        })
        .await;
        worker_idle().await;
        let store = DeskStore::from_pool(pool.clone());
        harvested(
            &store,
            id,
            &[exchange("x1", 10, &"I found the bug. ".repeat(40), 3)],
        )
        .await;
        d.account_refresh(id, Moment::TurnEnd)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fake.calls(), 1);
        assert!(fake.shared_asked.lock().is_empty(), "alone");
        assert!(!fake.asked.lock()[0].2.contains("New exchanges:"));
        settle(&d).await;
        assert_eq!(deriver.calls(), 0);
    })
    .await;
}

#[tokio::test]
async fn a_failed_shared_call_still_writes_the_account_and_leaves_the_notes_to_the_worker() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::answering(&[ANSWER]).sharing(vec![Err("the model is down".into())]);
        let deriver = Arc::new(FakeDeriver::default());
        let (d, id, _cancel) = start_with(&pool, dir.path(), &fake, Some(&deriver), |_| {}).await;
        worker_idle().await;
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
        assert_eq!(fake.shared_asked.lock().len(), 1);
        assert_eq!(fake.asked.lock().len(), 1, "the account is asked alone");
        assert_eq!(a.purpose, "Fix the login page so people can sign in again.");
        assert!(a.by_model);
        // The failed call is given back on both counts.
        let day = d.derive_day();
        assert_eq!(store.cheap_calls(day).await.unwrap(), 1);
        assert_eq!(store.derive_calls(day).await.unwrap(), 0);
        until(&d, "the worker reads the notes", async || {
            derived_to(&store, id).await == 1
        })
        .await;
        assert_eq!(deriver.calls(), 1);
    })
    .await;
}

#[tokio::test]
async fn a_shared_answer_whose_notes_dont_parse_keeps_the_account_and_the_worker_reads_them() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        // The account's object alone.
        let fake = Fake::answering(&[]).sharing(vec![Ok(ANSWER.to_string())]);
        let deriver = Arc::new(FakeDeriver::default());
        let (d, id, _cancel) = start_with(&pool, dir.path(), &fake, Some(&deriver), |_| {}).await;
        worker_idle().await;
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
        assert_eq!(a.purpose, "Fix the login page so people can sign in again.");
        assert_eq!(derived_to(&store, id).await, 0, "no notes kept");
        until(&d, "the worker reads the notes", async || {
            derived_to(&store, id).await == 1
        })
        .await;
        assert_eq!(deriver.calls(), 1);
    })
    .await;
}

#[tokio::test]
async fn a_turn_end_during_a_shared_read_doesnt_let_the_worker_read_it_too() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::answering(&[]).sharing(vec![Ok(both_answer())]);
        let deriver = Arc::new(FakeDeriver::default());
        let (d, id, _cancel) = start_with(&pool, dir.path(), &fake, Some(&deriver), |_| {}).await;
        worker_idle().await;
        let store = DeskStore::from_pool(pool.clone());
        harvested(
            &store,
            id,
            &[exchange("x1", 10, &"I found the bug. ".repeat(40), 3)],
        )
        .await;
        // Another turn ends while the call runs, and the worker is poked.
        let (during, pool2) = (d.clone(), pool.clone());
        *fake.shared_during.lock() = Some(Box::new(move || {
            let (d, pool) = (during.clone(), pool2.clone());
            tokio::spawn(async move {
                let store = DeskStore::from_pool(pool);
                harvested(&store, id, &[exchange("x2", 1, &"More. ".repeat(60), 1)]).await;
                assert!(due_at(&store, id).await.unwrap() <= Utc::now(), "due again");
                settle(&d).await;
            })
        }));
        d.account_refresh(id, Moment::TurnEnd)
            .await
            .unwrap()
            .unwrap();
        settle(&d).await;
        assert_eq!(deriver.calls(), 0, "the worker left it to the shared read");
        assert_eq!(derived_to(&store, id).await, 1);
        assert!(
            due_at(&store, id).await.unwrap() > Utc::now() + chrono::Duration::minutes(5),
            "what came meanwhile waits out the debounce"
        );
    })
    .await;
}

#[tokio::test]
async fn past_the_cheap_ceiling_a_shared_read_isnt_made_and_noting_keeps_its_place() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::answering(&[]).sharing(vec![Ok(both_answer())]);
        let deriver = Arc::new(FakeDeriver::default());
        let (d, id, _cancel) = start_with(&pool, dir.path(), &fake, Some(&deriver), |_| {}).await;
        d.set_account_limits(Limits {
            cheap_calls_per_day: 0,
            account_questions_per_day: 0,
        });
        worker_idle().await;
        let store = DeskStore::from_pool(pool.clone());
        harvested(
            &store,
            id,
            &[exchange("x1", 10, &"I found the bug. ".repeat(40), 3)],
        )
        .await;
        let was = due_at(&store, id).await;
        let a = d
            .account_refresh(id, Moment::TurnEnd)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fake.calls(), 0);
        assert!(!a.by_model, "said plainly");
        let day = d.derive_day();
        assert_eq!(store.derive_calls(day).await.unwrap(), 0);
        assert_eq!(due_at(&store, id).await, was, "back in its place");
        until(&d, "the worker reads the notes", async || {
            derived_to(&store, id).await == 1
        })
        .await;
        assert_eq!(deriver.calls(), 1);
    })
    .await;
}

#[tokio::test]
async fn past_the_days_readings_for_notes_the_account_is_refreshed_alone() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::answering(&[ANSWER]).sharing(vec![Ok(both_answer())]);
        let deriver = Arc::new(FakeDeriver::default());
        let (d, id, _cancel) = start_with(&pool, dir.path(), &fake, Some(&deriver), |o| {
            o.memory.derive_calls_per_day = 0;
        })
        .await;
        worker_idle().await;
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
        assert!(fake.shared_asked.lock().is_empty());
        assert_eq!(fake.asked.lock().len(), 1, "alone");
        assert!(a.by_model);
        let day = d.derive_day();
        assert_eq!(store.cheap_calls(day).await.unwrap(), 1);
        assert_eq!(store.derive_calls(day).await.unwrap(), 0);
        settle(&d).await;
        assert_eq!(deriver.calls(), 0);
    })
    .await;
}

#[tokio::test]
async fn a_shared_read_that_takes_too_long_says_the_account_plainly_and_stays_counted() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::answering(&[]).sharing(vec![Ok(both_answer())]);
        *fake.shared_slow.lock() = Duration::from_secs(2);
        let deriver = Arc::new(FakeDeriver::default());
        let (d, id, _cancel) = start_with(&pool, dir.path(), &fake, Some(&deriver), |_| {}).await;
        *d.0.accounts.shared_within.lock() = Duration::from_millis(200);
        worker_idle().await;
        let store = DeskStore::from_pool(pool.clone());
        harvested(
            &store,
            id,
            &[exchange("x1", 10, &"I found the bug. ".repeat(40), 3)],
        )
        .await;
        let was = due_at(&store, id).await;
        let a = d
            .account_refresh(id, Moment::TurnEnd)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fake.calls(), 1);
        assert!(!a.by_model, "said plainly");
        assert!(a.version > 0, "still written");
        let day = d.derive_day();
        assert_eq!(
            store.cheap_calls(day).await.unwrap(),
            1,
            "it may have been spent"
        );
        assert_eq!(store.derive_calls(day).await.unwrap(), 1);
        assert_eq!(due_at(&store, id).await, was, "back in its place");
        until(&d, "the worker reads the notes", async || {
            derived_to(&store, id).await == 1
        })
        .await;
        assert_eq!(deriver.calls(), 1);
    })
    .await;
}

#[tokio::test]
async fn a_shared_read_covers_as_far_as_the_account_reads_and_the_rest_is_due_at_once() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::answering(&[]).sharing(vec![
            Ok(both_answer()),
            Ok(both_answer()),
            Ok(both_answer()),
        ]);
        let deriver = Arc::new(FakeDeriver::default());
        let (d, id, _cancel) = start_with(&pool, dir.path(), &fake, Some(&deriver), |_| {}).await;
        worker_idle().await;
        let store = DeskStore::from_pool(pool.clone());
        harvested(&store, id, &[exchange("x0", 60, &"Began. ".repeat(60), 1)]).await;
        d.account_refresh(id, Moment::TurnEnd)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(derived_to(&store, id).await, 1);
        // Ten long exchanges: more than one account refresh reads, fewer
        // than one reading for notes would.
        let long: Vec<Exchange> = (0..10)
            .map(|i| {
                exchange(
                    &format!("y{i:02}"),
                    30 - i,
                    &"Worked on it. ".repeat(200),
                    1,
                )
            })
            .collect();
        harvested(&store, id, &long).await;
        // Due now, without poking the worker.
        assert!(store.derive_due_now(id).await.unwrap());
        d.account_refresh(id, Moment::TurnEnd)
            .await
            .unwrap()
            .unwrap();
        let shared: Vec<String> = fake
            .shared_asked
            .lock()
            .iter()
            .map(|a| a.1.clone())
            .collect();
        assert_eq!(
            shared.len(),
            3,
            "the backlog was due at once and shared again"
        );
        let notes_of = |u: &str| u[u.find("For the notes:").unwrap()..].to_string();
        assert!(notes_of(&shared[1]).contains("Please do y00"));
        assert!(
            !notes_of(&shared[1]).contains("Please do y09"),
            "only as far as the account read"
        );
        assert!(notes_of(&shared[2]).contains("Please do y09"));
        assert!(
            !notes_of(&shared[2]).contains("Please do y00"),
            "each read once"
        );
        assert_eq!(derived_to(&store, id).await, 11);
        settle(&d).await;
        assert_eq!(deriver.calls(), 0);
        assert!(fake.asked.lock().is_empty());
    })
    .await;
}
