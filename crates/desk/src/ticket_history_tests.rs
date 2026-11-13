//! Reading an issue's history: Jira's changelog and Linear's history, each
//! change said as Desk shows it, and who made it told apart.

use serde_json::{json, Value};
use workforce_postgres::test_support::with_test_pool;

use super::*;
use crate::store::build_pool;

const WRAPPED: &str = "<<<EXTERNAL_UNTRUSTED_CONTENT_ab12>>>\n[External content from jira.get_issue.changelog]\nIn Review\n<<</EXTERNAL_UNTRUSTED_CONTENT_ab12>>>";

fn item(field: &str, from: Value, to: Value) -> Value {
    json!({ "field": field, "field_id": field, "from": from, "to": to })
}

fn entry(at: &str, author: &str, account: &str, kind: &str, items: Vec<Value>) -> Value {
    json!({ "id": at, "created": at, "author": author, "author_account_id": account,
        "author_account_type": kind, "items": items })
}

fn jira_answer(histories: Vec<Value>) -> Value {
    json!({ "key": "ACME-1", "created": "2026-11-01T09:00:00.000+0000",
        "reporter": { "display_name": "Dana Kim", "account_id": "acc-dana" },
        "changelog": { "total": histories.len(), "histories": histories } })
}

/// Each kind of change Jira's changelog says comes out as Desk shows it,
/// its values unwrapped; noise (rank, time logged) is left out, and a
/// whole text says only that it changed.
#[test]
fn jira_s_changelog_says_each_kind_of_change() {
    let answer = jira_answer(vec![
        entry(
            "2026-11-02T09:00:00.000+0000",
            "Dana Kim",
            "acc-dana",
            "atlassian",
            vec![
                item("status", json!("To Do"), json!(WRAPPED)),
                item("assignee", Value::Null, json!("Sam")),
                item("priority", json!("Medium"), json!("High")),
                item("Sprint", Value::Null, json!("Sprint 12")),
                item("Story Points", json!("3"), json!("5")),
                item("IssueParentAssociation", Value::Null, json!("ACME-9")),
                item("Link", Value::Null, json!("This issue blocks ACME-2")),
                item("Fix Version", json!("1.0"), json!("1.1")),
                item("description", json!("old words"), json!("new words")),
                item("Rank", Value::Null, json!("Ranked higher")),
                item("timespent", Value::Null, json!("3600")),
            ],
        ),
        entry(
            "2026-11-03T09:00:00.000+0000",
            "Dana Kim",
            "acc-dana",
            "atlassian",
            vec![item("Link", json!("This issue blocks ACME-2"), Value::Null)],
        ),
    ]);
    let got = jira_history(&answer, None).unwrap();
    let said: Vec<Value> = got.iter().map(|e| json!(e)).collect();
    let at = "2026-11-02T09:00:00Z";
    assert_eq!(
        said,
        vec![
            json!({ "at": "2026-11-01T09:00:00Z", "actor": "Dana Kim", "by": "remote", "kind": "created" }),
            json!({ "at": at, "actor": "Dana Kim", "by": "remote", "kind": "state", "from": "To Do", "to": "In Review" }),
            json!({ "at": at, "actor": "Dana Kim", "by": "remote", "kind": "assignee", "to": "Sam" }),
            json!({ "at": at, "actor": "Dana Kim", "by": "remote", "kind": "priority", "from": "Medium", "to": "High" }),
            json!({ "at": at, "actor": "Dana Kim", "by": "remote", "kind": "sprint", "to": "Sprint 12" }),
            json!({ "at": at, "actor": "Dana Kim", "by": "remote", "kind": "points", "from": "3", "to": "5" }),
            json!({ "at": at, "actor": "Dana Kim", "by": "remote", "kind": "parent", "to": "ACME-9" }),
            json!({ "at": at, "actor": "Dana Kim", "by": "remote", "kind": "link", "to": "ACME-2" }),
            json!({ "at": at, "actor": "Dana Kim", "by": "remote", "kind": "field", "field": "Fix Version", "from": "1.0", "to": "1.1" }),
            json!({ "at": at, "actor": "Dana Kim", "by": "remote", "kind": "field", "field": "description" }),
            json!({ "at": "2026-11-03T09:00:00Z", "actor": "Dana Kim", "by": "remote", "kind": "field", "field": "link", "from": "ACME-2" }),
        ]
    );
}

/// A change by the account Desk writes with is Desk's; GitHub's app is
/// GitHub; any other app or person is someone there.
#[test]
fn jira_s_changes_are_told_apart_by_who_made_them() {
    let status = || vec![item("status", json!("To Do"), json!("In Progress"))];
    let answer = jira_answer(vec![
        entry(
            "2026-11-02T09:00:00.000+0000",
            "Hub key",
            "acc-key",
            "atlassian",
            status(),
        ),
        entry(
            "2026-11-02T10:00:00.000+0000",
            "GitHub for Jira",
            "acc-gh",
            "app",
            status(),
        ),
        entry(
            "2026-11-02T11:00:00.000+0000",
            "Automation for Jira",
            "acc-auto",
            "app",
            status(),
        ),
        entry(
            "2026-11-02T12:00:00.000+0000",
            "GitHub fan",
            "acc-fan",
            "atlassian",
            status(),
        ),
    ]);
    let by = |w: Option<&str>| -> Vec<HistoryBy> {
        jira_history(&answer, w)
            .unwrap()
            .iter()
            .skip(1)
            .map(|e| e.by)
            .collect()
    };
    use HistoryBy::*;
    assert_eq!(by(Some("acc-key")), [Desk, Github, Remote, Remote]);
    assert_eq!(by(None), [Remote, Github, Remote, Remote]);
}

/// Only the newest are kept, oldest first; an answer with no changelog (an
/// older hub) is no history, so what was kept stays.
#[test]
fn jira_s_history_is_held_to_its_newest() {
    let histories: Vec<Value> = (0..120)
        .map(|n| {
            entry(
                &format!("2026-11-02T{:02}:{:02}:00.000+0000", n / 60, n % 60),
                "Dana Kim",
                "acc-dana",
                "atlassian",
                vec![item("priority", json!("Low"), json!("High"))],
            )
        })
        .collect();
    let got = jira_history(&jira_answer(histories), None).unwrap();
    assert_eq!(got.len(), HISTORY_MAX);
    assert!(got.windows(2).all(|w| w[0].at <= w[1].at));
    assert_eq!(got[0].kind, HistoryKind::Priority, "the oldest, made, goes");
    assert_eq!(jira_history(&json!({ "key": "ACME-1" }), None), None);
}

/// The account Desk writes with is named only for a shared key.
#[test]
fn the_writer_is_only_a_shared_key_s_account() {
    assert_eq!(
        writer_of(
            System::Jira,
            &json!({ "account_id": "acc-key", "personal": false })
        ),
        Some("acc-key".into())
    );
    assert_eq!(
        writer_of(
            System::Jira,
            &json!({ "account_id": "acc-sam", "personal": true })
        ),
        None
    );
    assert_eq!(
        writer_of(System::Linear, &json!({ "id": "u-key", "personal": false })),
        Some("u-key".into())
    );
    assert_eq!(writer_of(System::Linear, &json!({ "id": "u-key" })), None);
}

/// Each kind of change Linear's history says comes out as Desk shows it:
/// GitHub's bot moving it is GitHub's, a PR it linked is `#N`, and the
/// account Desk writes with is Desk.
#[test]
fn linear_s_history_says_each_kind_of_change() {
    let answer = json!({ "identifier": "ATL-7", "created_at": "2026-11-01T09:00:00.000Z",
        "creator": { "id": "u-key", "name": "Hub key" },
        "history": [
            { "createdAt": "2026-11-02T09:00:00.000Z", "actor": { "id": "u-dana", "name": "Dana Kim" },
              "fromState": { "name": "Todo", "type": "unstarted" }, "toState": { "name": "In Progress", "type": "started" },
              "fromAssignee": null, "toAssignee": { "id": "u-sam", "name": "Sam" },
              "fromPriority": 3, "toPriority": 1, "fromEstimate": 2, "toEstimate": 5,
              "fromCycle": null, "toCycle": { "number": 12, "name": null },
              "fromParent": null, "toParent": { "identifier": "ATL-1" },
              "toTitle": "<<<EXTERNAL_UNTRUSTED_CONTENT_x>>>\n[External content from Linear issue title]\nNew title\n<<</EXTERNAL_UNTRUSTED_CONTENT_x>>>",
              "fromTitle": "Old title",
              "addedLabels": [{ "name": "web" }], "removedLabels": [],
              "relationChanges": [{ "identifier": "ATL-3", "type": "blocks" }],
              "updatedDescription": true },
            { "createdAt": "2026-11-03T09:00:00.000Z", "actor": null, "botActor": { "name": "GitHub", "type": "github" },
              "fromState": { "name": "In Progress" }, "toState": { "name": "Done" } },
            { "createdAt": "2026-11-03T08:00:00.000Z", "actor": null, "botActor": null,
              "attachment": { "title": "Fix the login", "url": "https://github.com/acme/web/pull/248", "sourceType": "github" } },
            { "createdAt": "2026-11-04T09:00:00.000Z", "actor": { "id": "u-key", "name": "Hub key" },
              "fromPriority": 1, "toPriority": 1, "fromEstimate": null, "toEstimate": null },
            { "createdAt": "2026-11-04T10:00:00.000Z", "actor": { "id": "u-key", "name": "Hub key" },
              "fromAssignee": { "id": "u-sam", "name": "Sam" }, "toAssignee": null },
        ] });
    let got = linear_history(&answer, Some("u-key")).unwrap();
    let said: Vec<Value> = got.iter().map(|e| json!(e)).collect();
    let at = "2026-11-02T09:00:00Z";
    let dana = |v: Value| {
        let mut v = v;
        v["at"] = json!(at);
        v["actor"] = json!("Dana Kim");
        v["by"] = json!("remote");
        v
    };
    assert_eq!(
        said,
        vec![
            json!({ "at": "2026-11-01T09:00:00Z", "actor": "Hub key", "by": "desk", "kind": "created" }),
            dana(json!({ "kind": "state", "from": "Todo", "to": "In Progress" })),
            dana(json!({ "kind": "assignee", "to": "Sam" })),
            dana(json!({ "kind": "priority", "from": "Medium", "to": "Urgent" })),
            dana(json!({ "kind": "points", "from": "2", "to": "5" })),
            dana(json!({ "kind": "sprint", "to": "Cycle 12" })),
            dana(json!({ "kind": "parent", "to": "ATL-1" })),
            dana(
                json!({ "kind": "field", "field": "title", "from": "Old title", "to": "New title" })
            ),
            dana(json!({ "kind": "field", "field": "labels", "to": "web" })),
            dana(json!({ "kind": "link", "to": "ATL-3" })),
            dana(json!({ "kind": "field", "field": "description" })),
            json!({ "at": "2026-11-03T08:00:00Z", "by": "github", "kind": "link", "to": "#248" }),
            json!({ "at": "2026-11-03T09:00:00Z", "actor": "GitHub", "by": "github", "kind": "state", "from": "In Progress", "to": "Done" }),
            json!({ "at": "2026-11-04T10:00:00Z", "actor": "Hub key", "by": "desk", "kind": "assignee", "from": "Sam" }),
        ],
        "an entry that changed nothing says nothing"
    );
    assert_eq!(
        linear_history(&json!({ "identifier": "ATL-7" }), None),
        None
    );
}

/// A pull request is named by its number only from GitHub's own address.
#[test]
fn a_pr_is_named_by_its_number() {
    assert_eq!(
        pr_number("https://github.com/acme/web/pull/5850"),
        Some("#5850".into())
    );
    assert_eq!(
        pr_number("https://github.com/acme/web/pull/5850/files"),
        Some("#5850".into())
    );
    assert_eq!(pr_number("https://github.com/acme/web/issues/12"), None);
    assert_eq!(pr_number("https://evil.example/acme/web/pull/1"), None);
}

/// The calls that read it: Jira's issue with its changelog, Linear's tool.
#[test]
fn history_is_read_with_the_issue_or_linear_s_tool() {
    assert_eq!(
        history_call(System::Jira, "ACME-1"),
        (
            "jira.get_issue",
            json!({ "issue_key": "ACME-1", "changelog": true })
        )
    );
    assert_eq!(
        history_call(System::Linear, "ATL-7").0,
        "linear.issue_history"
    );
}

async fn a_ticket(store: &DeskStore) -> Uuid {
    let client = store.pool().get().await.unwrap();
    let board = Uuid::new_v4();
    let ticket = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO boards (id, name, prefix, created_at, updated_at) VALUES ($1, 'B', 'HIS', now(), now())",
            &[&board],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO tickets (id, board, number, title, state, rank, created_by, state_at, created_at, updated_at) \
             VALUES ($1, $2, 1, 'T', 'todo', 1, '{\"kind\":\"you\",\"id\":\"you\",\"name\":\"You\"}', now(), now(), now())",
            &[&ticket, &board],
        )
        .await
        .unwrap();
    ticket
}

/// History is kept per ticket and replaced by the next read.
#[tokio::test]
async fn history_is_kept_and_replaced() {
    with_test_pool(build_pool, |pool| async move {
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool);
        let ticket = a_ticket(&store).await;
        assert_eq!(store.history_of(ticket).await.unwrap(), None);
        let first = jira_history(
            &jira_answer(vec![entry(
                "2026-11-02T09:00:00.000+0000",
                "Dana Kim",
                "acc-dana",
                "atlassian",
                vec![item("status", json!("To Do"), json!("Done"))],
            )]),
            None,
        )
        .unwrap();
        store
            .put_history(ticket, &first, chrono::Utc::now())
            .await
            .unwrap();
        assert_eq!(store.history_of(ticket).await.unwrap(), Some(first));
        store
            .put_history(ticket, &[], chrono::Utc::now())
            .await
            .unwrap();
        assert_eq!(store.history_of(ticket).await.unwrap(), Some(vec![]));
    })
    .await;
}
