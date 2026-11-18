//! Fixtures are built from the shapes the hub's GitHub board tools answer
//! in (themselves GitHub's documented GraphQL and REST shapes), never
//! recorded from a live call.

use super::*;
use crate::board::{RankMove, TicketState};
use crate::board_sync::Who;

fn states(pairs: &[(TicketState, &str)]) -> BTreeMap<TicketState, String> {
    pairs.iter().map(|(c, s)| (*c, s.to_string())).collect()
}

fn full_item() -> Value {
    json!({
        "item_id": "PVTI_lADOAAA",
        "type": "ISSUE",
        "updated_at": "2026-11-18T10:00:00Z",
        "repo": "Acme/Web",
        "number": 12,
        "issue_id": "I_kwDOA",
        "url": "https://github.com/Acme/Web/issues/12",
        "title": "Fix the login page",
        "body": "<<<EXTERNAL_UNTRUSTED_CONTENT_ab12>>>\n[External content from GitHub issue body]\nIt breaks on Safari.\n<<</EXTERNAL_UNTRUSTED_CONTENT_ab12>>>",
        "state": "OPEN",
        "state_reason": null,
        "labels": ["bug", "p1"],
        "assignees": [{"login": "sam", "name": "Sam Lee"}],
        "milestone": {"title": "v2", "number": 3, "due_on": "2026-12-01T00:00:00Z"},
        "issue_type": "Bug",
        "parent": {"repo": "Acme/Web", "number": 4, "title": "Auth revamp"},
        "author": "kim",
        "created_at": "2026-11-01T09:00:00Z",
        "status": "In Progress"
    })
}

#[test]
fn targets_are_a_project_or_a_repository() {
    assert_eq!(
        target_of("acme/projects/7"),
        Some(Target::Project {
            owner: "acme".into(),
            number: 7
        })
    );
    assert_eq!(
        target_of("acme/web.app"),
        Some(Target::Repo {
            owner: "acme".into(),
            repo: "web.app".into()
        })
    );
    for bad in [
        "acme",
        "acme/projects/0",
        "-acme/web",
        "acme/projects/x",
        "a/b/c",
        "acme/..",
        "",
    ] {
        assert!(check_target(bad).is_err(), "{bad}");
    }
}

#[test]
fn keys_are_owner_repo_number_lowercased() {
    assert_eq!(parse_key(" Acme/Web#12 ").as_deref(), Some("acme/web#12"));
    assert_eq!(parse_key("acme/web#0"), None);
    assert_eq!(parse_key("ACME-12"), None);
    assert_eq!(
        crate::board_sync::parse_any_key("acme/web#3").as_deref(),
        Some("acme/web#3")
    );
    assert_eq!(
        crate::board_sync::parse_any_key("acme-3").as_deref(),
        Some("ACME-3")
    );
}

#[test]
fn a_repository_board_takes_only_its_own_issues() {
    assert!(key_in_target("acme/web#1", "Acme/Web"));
    assert!(!key_in_target("acme/api#1", "acme/web"));
    // A project spans repositories: its listing says what is on it.
    assert!(key_in_target("acme/api#1", "acme/projects/7"));
    assert!(crate::board_sync::key_in_target("acme/web#1", "acme/web"));
    assert!(!crate::board_sync::key_in_target("ACME-1", "acme/web"));
    assert!(crate::board_sync::key_in_target("ACME-1", "ACME"));
}

#[test]
fn a_lean_listing_keeps_only_issues_in_order() {
    let answer = json!({
        "items": [
            {"item_id": "PVTI_1", "type": "ISSUE", "updated_at": "2026-11-18T10:00:00Z", "repo": "acme/web", "number": 3},
            {"item_id": "PVTI_2", "type": "PULL_REQUEST", "updated_at": "2026-11-18T10:00:00Z", "repo": "acme/web", "number": 4},
            {"item_id": "PVTI_3", "type": "DRAFT_ISSUE", "updated_at": "2026-11-18T10:00:00Z", "repo": null, "number": null},
            {"item_id": "PVTI_4", "type": "REDACTED", "updated_at": "2026-11-18T10:00:00Z"},
            {"item_id": "PVTI_5", "type": "ISSUE", "updated_at": "2026-11-18T11:00:00Z", "repo": "acme/api", "number": 1}
        ],
        "page_info": {"has_next_page": true, "end_cursor": "Y3Vy"}
    });
    let (items, next) = lean_items(&answer);
    let keys: Vec<&str> = items.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(keys, ["acme/web#3", "acme/api#1"]);
    assert_eq!(items[0].item_id, "PVTI_1");
    assert_eq!(next.as_deref(), Some("Y3Vy"));
    let (_, last) =
        lean_items(&json!({"items": [], "page_info": {"has_next_page": false, "end_cursor": "x"}}));
    assert_eq!(last, None);
}

#[test]
fn a_project_item_reads_as_its_issue_with_status_and_place() {
    let map = states(&[
        (TicketState::Doing, "In Progress"),
        (TicketState::Done, "Done"),
    ]);
    let issue = issue_of(
        &full_item(),
        &map,
        true,
        Some(crate::board::github_rank_of(2)),
    )
    .unwrap();
    assert_eq!(issue.key, "acme/web#12");
    assert_eq!(issue.id.as_deref(), Some("PVTI_lADOAAA"));
    assert_eq!(issue.title, "Fix the login page");
    assert_eq!(issue.body.as_deref(), Some("It breaks on Safari."));
    assert_eq!(issue.state, "In Progress");
    assert_eq!(issue.column(&map), Some(TicketState::Doing));
    assert_eq!(issue.closed, Some(false));
    assert_eq!(issue.labels, ["bug", "p1"]);
    assert_eq!(
        issue.assignee,
        Some(Some(Assignee {
            id: "sam".into(),
            name: "Sam Lee".into()
        }))
    );
    assert_eq!(issue.facts.kind.as_deref(), Some("Bug"));
    assert_eq!(
        issue.facts.sprint.as_ref().and_then(|s| s.name.as_deref()),
        Some("v2")
    );
    assert_eq!(
        issue.facts.parent.as_ref().map(|p| p.key.as_str()),
        Some("acme/web#4")
    );
    assert_eq!(issue.upstream_rank.as_deref(), Some("0000002048"));
}

#[test]
fn a_closed_issue_without_a_mapped_status_is_done_or_dropped() {
    let map = states(&[(TicketState::Todo, "Todo")]);
    let mut v = full_item();
    v["status"] = Value::Null;
    v["state"] = json!("CLOSED");
    v["state_reason"] = json!("COMPLETED");
    assert_eq!(
        issue_of(&v, &map, true, None).unwrap().column(&map),
        Some(TicketState::Done)
    );
    v["state_reason"] = json!("NOT_PLANNED");
    assert_eq!(
        issue_of(&v, &map, true, None).unwrap().column(&map),
        Some(TicketState::Dropped)
    );
}

#[test]
fn a_repository_issue_takes_a_mapped_label_as_its_column() {
    let map = states(&[
        (TicketState::Todo, "Open"),
        (TicketState::Doing, "label:p1"),
        (TicketState::Done, "Closed"),
    ]);
    let mut v = full_item();
    v.as_object_mut().unwrap().remove("item_id");
    v.as_object_mut().unwrap().remove("type");
    v.as_object_mut().unwrap().remove("status");
    let issue = issue_of(&v, &map, false, None).unwrap();
    assert_eq!(issue.state, "label:p1");
    assert_eq!(issue.id, None);
    assert_eq!(issue.column(&map), Some(TicketState::Doing));
    v["labels"] = json!(["bug"]);
    assert_eq!(
        issue_of(&v, &map, false, None).unwrap().column(&map),
        Some(TicketState::Todo)
    );
    v["state"] = json!("CLOSED");
    assert_eq!(
        issue_of(&v, &map, false, None).unwrap().column(&map),
        Some(TicketState::Done)
    );
}

#[test]
fn non_issues_are_not_read() {
    let mut v = full_item();
    v["type"] = json!("PULL_REQUEST");
    assert!(issue_of(&v, &BTreeMap::new(), true, None).is_none());
}

#[test]
fn ranks_sort_bytewise_as_the_project_orders() {
    let a = crate::board::github_rank_of(9);
    let b = crate::board::github_rank_of(10);
    assert!(a.as_bytes() < b.as_bytes());
    let mid = crate::board::place_between(Some(&a), Some(&b)).unwrap();
    assert!(a < mid && mid < b, "{a} {mid} {b}");
}

#[test]
fn filters_become_a_project_query_or_repo_arguments() {
    assert_eq!(
        project_query(
            &Filter::All {
                include_done: false
            },
            None
        ),
        None
    );
    assert_eq!(
        project_query(&Filter::Label("in progress".into()), None).as_deref(),
        Some("label:\"in progress\"")
    );
    assert_eq!(
        project_query(&Filter::Milestone("v2".into()), None).as_deref(),
        Some("milestone:\"v2\"")
    );
    assert_eq!(
        project_query(&Filter::AssignedToMe, Some("sam")).as_deref(),
        Some("assignee:sam")
    );
    let args = repo_args(
        "acme",
        "web",
        &Filter::Label("bug".into()),
        None,
        Some("2026-11-18T10:00:00Z"),
        2,
    );
    assert_eq!(args["labels"], "bug");
    assert_eq!(args["since"], "2026-11-18T10:00:00Z");
    assert_eq!(args["state"], "all");
    assert_eq!(args["page"], 2);
    let open = repo_args(
        "acme",
        "web",
        &Filter::All {
            include_done: false,
        },
        None,
        None,
        1,
    );
    assert_eq!(open["state"], "open");
    assert!(check_query("label:bug -status:Done").is_ok());
    assert!(check_query("label:\"bug").is_err());
    assert!(check_query("a\nb").is_err());
}

#[test]
fn github_filters_are_checked() {
    use crate::board::System;
    use crate::board_sync::check_filter;
    assert!(check_filter(System::Github, &Filter::Milestone("v2".into()), None).is_ok());
    assert!(check_filter(System::Jira, &Filter::Milestone("v2".into()), None).is_err());
    assert!(check_filter(System::Github, &Filter::AssignedToMe, None).is_err());
    assert!(check_filter(System::Github, &Filter::AssignedToMe, Some("sam")).is_ok());
    assert!(check_filter(System::Github, &Filter::CurrentCycle, None).is_err());
}

fn sync(target: &str) -> crate::board::Sync {
    serde_json::from_value(json!({
        "link": "6f1c2a4e-0b7d-4c55-9a51-3d2e8f9b1c07",
        "system": "github",
        "target": target,
        "targetId": "PVT_kwDOA",
        "via": {"kind": "thisHub"},
        "person": "sam",
        "filter": {"kind": "all", "value": {}},
        "states": {}
    }))
    .unwrap()
}

#[test]
fn a_project_move_sets_status_and_closes_at_done() {
    let s = sync("acme/projects/7");
    let calls = move_calls(
        &s,
        "acme/web#12",
        Some("PVTI_1"),
        TicketState::Doing,
        "In Progress",
        Some("Todo"),
        false,
    );
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "github.project_item_status");
    assert_eq!(calls[0].1["status"], "In Progress");
    assert_eq!(calls[0].1["item_id"], "PVTI_1");
    let done = move_calls(
        &s,
        "acme/web#12",
        Some("PVTI_1"),
        TicketState::Dropped,
        "Done",
        None,
        false,
    );
    assert_eq!(done[1].0, "github.issue_update");
    assert_eq!(done[1].1["state"], "closed");
    assert_eq!(done[1].1["state_reason"], "not_planned");
    let back = move_calls(
        &s,
        "acme/web#12",
        Some("PVTI_1"),
        TicketState::Todo,
        "Todo",
        Some("Done"),
        true,
    );
    assert_eq!(back[1].1["state"], "open");
}

#[test]
fn a_label_column_adds_and_removes_never_replaces() {
    let s = sync("acme/web");
    let calls = move_calls(
        &s,
        "acme/web#12",
        None,
        TicketState::Review,
        "label:review",
        Some("label:doing"),
        false,
    );
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "github.issue_labels");
    assert_eq!(calls[0].1["add"], json!(["review"]));
    assert_eq!(calls[0].1["remove"], json!(["doing"]));
    assert!(calls[0].1.get("labels").is_none());
    let closed = move_calls(
        &s,
        "acme/web#12",
        None,
        TicketState::Done,
        "Closed",
        Some("label:review"),
        false,
    );
    assert_eq!(closed[0].1["remove"], json!(["review"]));
    assert_eq!(closed[1].1["state"], "closed");
}

#[test]
fn a_drop_moves_after_an_item_or_to_the_top() {
    let after = RankMove {
        after_item: Some("PVTI_9".into()),
        ..Default::default()
    };
    let args = order_call(Some("PVT_1"), Some("PVTI_2"), &after).unwrap();
    assert_eq!(args["after_id"], "PVTI_9");
    let top = RankMove {
        to_top: true,
        ..Default::default()
    };
    assert!(order_call(Some("PVT_1"), Some("PVTI_2"), &top).unwrap()["after_id"].is_null());
    assert!(order_call(Some("PVT_1"), Some("PVTI_2"), &RankMove::default()).is_none());
    assert!(order_call(Some("PVT_1"), None, &top).is_none());
}

#[test]
fn only_the_persons_own_key_says_who_they_are() {
    let me = viewer_of(&json!({"login": "sam", "name": "Sam Lee", "personal": true}));
    assert_eq!(me.person().map(|a| a.id.as_str()), Some("sam"));
    assert!(matches!(
        viewer_of(&json!({"login": "bot", "personal": false})),
        Who::Shared(_)
    ));
    assert!(matches!(viewer_of(&json!({"login": "sam"})), Who::Unsaid));
}

#[test]
fn states_come_from_status_options_or_open_closed_and_labels() {
    let p = project_states(
        &json!({"status": {"field_id": "F", "name": "Status", "options": [{"id": "1", "name": "Todo"}, {"id": "2", "name": "In Progress"}, {"id": "3", "name": "Done"}]}}),
    );
    let prefill = crate::board_sync::prefill_states(&p);
    assert_eq!(
        prefill.get(&TicketState::Todo).map(String::as_str),
        Some("Todo")
    );
    assert_eq!(
        prefill.get(&TicketState::Doing).map(String::as_str),
        Some("In Progress")
    );
    assert_eq!(
        prefill.get(&TicketState::Done).map(String::as_str),
        Some("Done")
    );
    let r = repo_states(&json!({"labels": [{"name": "doing", "color": "fff"}]}));
    let names: Vec<&str> = r.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["Open", "Closed", "label:doing"]);
    let prefill = crate::board_sync::prefill_states(&r);
    assert_eq!(
        prefill.get(&TicketState::Todo).map(String::as_str),
        Some("Open")
    );
    assert_eq!(
        prefill.get(&TicketState::Done).map(String::as_str),
        Some("Closed")
    );
}
