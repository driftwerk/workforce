use super::*;
use crate::board::{External, Seen, Sync, System, TicketState};
use crate::crew::Party;
use crate::ticket_comments::{Author, AuthorKind};
use crate::ticket_history::{HistoryBy, HistoryEvent};
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
        "gh -R acme/web pr view 648",
        "gh --repo acme/web issue list",
        "gh --hostname ghe.acme.dev --repo=acme/web pr diff 9",
        "gh search issues 'login timeout'",
        "gh repo view acme/web",
        "gh pr list --state open",
        "gh pr checkout 648",
        "gh run view 991 --log-failed",
        "gh run view --log",
        "bash -c \"curl -s https://api.example/v1\"",
        "sh -c 'wget https://example.com/x'",
    ] {
        assert!(reads(others), "{others}");
    }
    for own in [
        "gh pr create --title 'Fix login' --body 'Fixes it'",
        "gh pr checks 648",
        "gh pr merge 648 --squash",
        "gh run watch",
        "gh auth status",
        "gh -R acme/issue pr create --fill",
        "gh --repo acme/api pr checks 648",
        "git log --oneline",
        "echo \"done\"",
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

fn none() -> HashMap<Uuid, Option<String>> {
    HashMap::new()
}

fn by<'a>(comments: &'a [Comment], stamps: &'a HashMap<Uuid, Option<String>>) -> Writers<'a> {
    Writers {
        comments,
        history: None,
        stamps,
    }
}

fn posted(by: Party) -> Comment {
    Comment {
        remote_id: None,
        posted_by: Some(by),
        only_here: true,
        ..comment("here")
    }
}

/// A ticket made here is the person's words when they, a boss whose turn
/// had read nothing, or Desk made it; a session's or another agent's is
/// not, nor a boss's that had read outside content or never said.
#[test]
fn a_ticket_made_here_is_the_persons_unless_a_session_or_an_armed_boss_wrote_it() {
    let b = board(None);
    let boss = Party::manager("sarah", "Sarah");
    for own in [Party::you(), boss.clone(), Party::desk()] {
        let t = ticket(own);
        let stamps = HashMap::from([(t.id, None)]);
        assert_eq!(ticket_outside(&t, &b, by(&[], &stamps)), None);
    }
    assert_eq!(
        ticket_outside(&ticket(Party::you()), &b, by(&[], &none())),
        None
    );
    let by_session = Party {
        kind: PartyKind::Session,
        id: Uuid::new_v4().to_string(),
        name: "acme-web".into(),
        engine: None,
        origin: None,
    };
    assert!(ticket_outside(&ticket(by_session.clone()), &b, by(&[], &none())).is_some());

    let armed = ticket(boss.clone());
    let stamps = HashMap::from([(armed.id, Some("web.fetch".to_string()))]);
    assert_eq!(
        ticket_outside(&armed, &b, by(&[], &stamps)).as_deref(),
        Some("ENG-1, written by a boss's turn that had read web.fetch")
    );
    assert!(
        ticket_outside(&ticket(boss.clone()), &b, by(&[], &none())).is_some(),
        "a boss's from before Desk said"
    );
    // A person's ticket an armed boss edited.
    let edited = ticket(Party::you());
    let stamps = HashMap::from([(edited.id, Some("web.fetch".to_string()))]);
    assert!(ticket_outside(&edited, &b, by(&[], &stamps)).is_some());

    // Its bosses' comments count; a session's note on it is its own work.
    let t = ticket(Party::you());
    let note = posted(by_session);
    assert_eq!(ticket_outside(&t, &b, by(&[note], &none())), None);
    let said = posted(boss.clone());
    let stamps = HashMap::from([(said.id, None)]);
    assert_eq!(
        ticket_outside(&t, &b, by(std::slice::from_ref(&said), &stamps)),
        None
    );
    let stamps = HashMap::from([(said.id, Some("web.fetch".to_string()))]);
    assert!(ticket_outside(&t, &b, by(std::slice::from_ref(&said), &stamps)).is_some());
    assert!(
        ticket_outside(&t, &b, by(&[said], &none())).is_some(),
        "a boss's comment from before Desk said"
    );
}

/// A synced issue is the person's words only when Desk knows who they
/// are there, they wrote it, every comment on it is theirs, a person's,
/// and read, and no one else edited its description.
#[test]
fn a_synced_issue_is_the_persons_only_when_they_wrote_it_and_all_its_comments() {
    let mine = board(Some("u-me"));
    let alone = |t: &Ticket, comments: &[Comment]| ticket_outside(t, &mine, by(comments, &none()));
    assert_eq!(alone(&issue("u-me", 0), &[]), None);
    assert_eq!(alone(&issue("u-me", 1), &[comment("u-me")]), None);
    assert!(
        alone(&issue("u-me", 2), &[comment("u-me")]).is_some(),
        "a comment not read"
    );
    assert!(
        alone(&issue("u-me", 1), &[comment("u-teammate")]).is_some(),
        "a teammate's comment"
    );
    let app = Comment {
        author: Author {
            kind: AuthorKind::Agent,
            ..comment("u-me").author
        },
        ..comment("u-me")
    };
    assert!(
        alone(&issue("u-me", 1), &[app]).is_some(),
        "an app writing as the person"
    );
    assert!(
        alone(&issue("u-customer", 0), &[]).is_some(),
        "someone else's issue"
    );
    assert!(
        ticket_outside(&issue("u-me", 0), &board(None), by(&[], &none())).is_some(),
        "the person not known there"
    );

    // Its history, as last read: a description another edited.
    let edit = |who: HistoryBy| {
        serde_json::from_value::<HistoryEvent>(json!({
            "at": Utc::now(), "by": who, "kind": "field", "field": "description"
        }))
        .unwrap()
    };
    let t = issue("u-me", 0);
    let read = |events: Vec<HistoryEvent>| {
        let stamps = none();
        ticket_outside(
            &t,
            &mine,
            Writers {
                comments: &[],
                history: Some(&events),
                stamps: &stamps,
            },
        )
    };
    assert_eq!(read(vec![edit(HistoryBy::Desk)]), None);
    assert!(
        read(vec![edit(HistoryBy::Remote)]).is_some(),
        "a teammate's edit"
    );
}
