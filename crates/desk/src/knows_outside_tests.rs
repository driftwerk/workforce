use super::*;
use crate::board::{External, Seen, Sync, System, TicketState};
use crate::crew::Party;
use crate::ticket_comments::{Author, AuthorKind};
use chrono::Utc;
use serde_json::json;
use uuid::Uuid;

/// Every step and command is read, however many there are, and each that
/// reads outside content is named once.
#[test]
fn an_exchange_reads_outside_content_by_its_steps_and_commands_all_of_them() {
    let mut steps: Vec<Value> = (0..60)
        .map(|i| json!({"tool": "Bash", "input": format!("cargo test {i}")}))
        .collect();
    steps.push(json!({"tool": "WebFetch", "input": "https://docs.example"}));
    steps.push(json!({"tool": "WebFetch", "input": "https://docs.example/2"}));
    steps.push(json!({"tool": "linear__get_issue", "input": "{}"}));
    let mut commands: Vec<String> = (0..60).map(|i| format!("cargo test {i}")).collect();
    commands.push("cd web && curl -s https://api.example/v1".into());
    assert_eq!(
        read_outside(&Value::Array(steps), &commands),
        [
            "WebFetch",
            "linear__get_issue",
            "cd web && curl -s https://api.example/v1"
        ]
    );
    let own: Vec<String> = (0..60).map(|i| format!("cargo test {i}")).collect();
    assert!(read_outside(&json!([{"tool": "Edit"}, {"tool": "Bash"}]), &own).is_empty());
}

/// `gh` reads outside content only where it reads what others wrote.
#[test]
fn gh_reads_outside_content_only_for_others_words() {
    let reads = |command: &str| !read_outside(&json!([]), &[command.to_string()]).is_empty();
    for others in [
        "gh issue view 12",
        "gh pr view 648 --comments",
        "gh pr diff 648",
        "gh api repos/acme/web/issues/12/comments",
        "gh release view v1.2",
        "gh gist view abc",
        "git push && gh pr view --json body",
    ] {
        assert!(reads(others), "{others}");
    }
    for own in [
        "gh pr create --title 'Fix login' --body 'Fixes it'",
        "gh pr checks 648",
        "gh pr merge 648 --squash",
        "gh run watch",
        "gh auth status",
        "git log --oneline",
    ] {
        assert!(!reads(own), "{own}");
    }
}

fn board(me: Option<&str>) -> Board {
    let now = Utc::now();
    let mut sync: Sync = serde_json::from_value(json!({
        "link": Uuid::nil(), "system": System::Linear, "target": "ENG",
        "via": { "kind": "thisHub" }, "person": "you", "filter": { "kind": "all", "value": {} },
        "states": {}
    }))
    .unwrap();
    sync.me = me.map(str::to_string);
    Board {
        id: Uuid::nil(),
        name: "Engineering".into(),
        group: None,
        prefix: Some("ENG".into()),
        sync: Some(sync),
        next_number: 2,
        created_at: now,
        updated_at: now,
    }
}

fn ticket(by: Party) -> Ticket {
    let now = Utc::now();
    Ticket {
        id: Uuid::new_v4(),
        board: Uuid::nil(),
        number: 1,
        title: "Fix the login".into(),
        body: "It times out.".into(),
        state: TicketState::Todo,
        rank: 1.0,
        assignee: None,
        needs: vec![],
        links: vec![],
        external: None,
        created_by: by,
        pinned: false,
        snoozed_until: None,
        upstream_rank: None,
        snoozed_at: None,
        state_at: now,
        created_at: now,
        updated_at: now,
        version: 0,
    }
}

fn issue(reporter: &str, comments: u32) -> Ticket {
    let mut t = ticket(Party::you());
    let mut details = crate::board::Details::default();
    details.facts.reporter_id = Some(reporter.into());
    details.facts.comments = Some(comments);
    t.external = Some(External {
        system: System::Linear,
        key: "ENG-482".into(),
        id: Some("i-482".into()),
        url: "https://linear.example/ENG-482".into(),
        remote_updated_at: Utc::now(),
        synced: true,
        noted: None,
        link: Uuid::nil(),
        made_here: false,
        kept: false,
        flagged: false,
        seen: Seen {
            title: t.title.clone(),
            body: t.body.clone(),
            state: Some(TicketState::Todo),
            needs: vec![],
        },
        assignee: None,
        clipped: false,
        html_read: 0,
        html_prints: vec![],
        details,
        dev: None,
        rank_move: None,
        rank_stale: false,
    });
    t
}

fn comment(by: &str) -> Comment {
    Comment {
        id: Uuid::new_v4(),
        remote_id: Some(format!("c-{by}")),
        author: Author {
            kind: AuthorKind::Person,
            name: by.into(),
            id: Some(by.into()),
            on: "Linear".into(),
        },
        posted_by: None,
        body: "Looks right.".into(),
        flagged: false,
        at: Utc::now(),
        edited_at: None,
        only_here: false,
        parent: None,
    }
}

/// A ticket made here is the person's words when they, a boss or Desk
/// made it; a session's or another agent's is not.
#[test]
fn a_ticket_made_here_is_the_persons_unless_a_session_wrote_it() {
    let b = board(None);
    for own in [
        Party::you(),
        Party::manager("sarah", "Sarah"),
        Party::desk(),
    ] {
        assert_eq!(ticket_outside(&ticket(own), &b, &[]), None);
    }
    let by_session = Party {
        kind: PartyKind::Session,
        id: Uuid::new_v4().to_string(),
        name: "acme-web".into(),
        engine: None,
        origin: None,
    };
    assert!(ticket_outside(&ticket(by_session), &b, &[]).is_some());
}

/// A synced issue is the person's words only when Desk knows who they
/// are there, they wrote it, and every comment on it is theirs and read.
#[test]
fn a_synced_issue_is_the_persons_only_when_they_wrote_it_and_all_its_comments() {
    let mine = board(Some("u-me"));
    assert_eq!(ticket_outside(&issue("u-me", 0), &mine, &[]), None);
    assert_eq!(
        ticket_outside(&issue("u-me", 1), &mine, &[comment("u-me")]),
        None
    );
    assert!(
        ticket_outside(&issue("u-me", 2), &mine, &[comment("u-me")]).is_some(),
        "a comment not read"
    );
    assert!(
        ticket_outside(&issue("u-me", 1), &mine, &[comment("u-teammate")]).is_some(),
        "a teammate's comment"
    );
    assert!(
        ticket_outside(&issue("u-customer", 0), &mine, &[]).is_some(),
        "someone else's issue"
    );
    assert!(
        ticket_outside(&issue("u-me", 0), &board(None), &[]).is_some(),
        "the person not known there"
    );
}
