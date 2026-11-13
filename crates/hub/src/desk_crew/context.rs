//! The one block every turn of a boss is given, the same in its DM, on a
//! session's line and in a work turn: what Desk keeps of each session in
//! its scope (an account's line each, and the whole account of the session
//! the turn is about) and what it told the person lately, read from the DM
//! archive so a rollover loses none of it, what it owes the person (its
//! open items, kept in Postgres), and the person's preferences that cover
//! what the turn is about. Capped at about 3k tokens: past that, quiet
//! sessions drop to one short line first.

use chrono::{DateTime, Utc};
use uuid::Uuid;
use workforce_common::channels::DESK_CHANNEL;
use workforce_desk::account::Account;
use workforce_desk::model::SessionState;

use super::watcher::reported;
use crate::serve::AgentService;

/// The block's most, about 3k tokens.
pub(super) const CAP_CHARS: usize = 12_000;
/// A session's line, about 80 tokens.
const ENTRY_CHARS: usize = 320;
/// A quiet session's line once the block is over its cap.
const SHORT_CHARS: usize = 100;
/// The whole account of the session the turn is about, at most.
const ABOUT_CHARS: usize = 4_000;
/// The boss's own posts it is reminded of.
pub(super) const POSTS: usize = 10;
const POST_CHARS: usize = 240;

/// One session in the boss's scope.
#[derive(Debug, Clone)]
pub(super) struct Entry {
    pub id: Uuid,
    pub name: String,
    pub state: SessionState,
    /// Its last turn failed.
    pub failed: bool,
    pub account: Option<Account>,
}

/// One of the boss's own DM posts.
#[derive(Debug, Clone)]
pub(super) struct Post {
    pub at: DateTime<Utc>,
    pub text: String,
}

/// Whether a session is quietly between turns or done: what drops first.
/// One interrupted, or whose last turn failed, wants attention and keeps
/// its line.
fn quiet(e: &Entry) -> bool {
    !e.failed
        && matches!(
            e.state,
            SessionState::Idle | SessionState::Done | SessionState::Stopped | SessionState::Closed
        )
}

fn clip(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.chars().count() > max {
        true => format!("{}…", flat.chars().take(max).collect::<String>()),
        false => flat,
    }
}

fn line(e: &Entry, short: bool, tz: chrono_tz::Tz) -> String {
    let state = match e.failed {
        true => format!("{}, its last turn failed", e.state.as_str()),
        false => e.state.as_str().to_string(),
    };
    let head = match &e.account {
        Some(a) => format!(
            "- {} ({state}, session_id {}, as of {})",
            e.name,
            e.id,
            a.written_at.with_timezone(&tz).format("%H:%M")
        ),
        None => format!("- {} ({state}, session_id {})", e.name, e.id),
    };
    match (&e.account, short) {
        (None, _) => format!("{head}: no account yet"),
        (Some(a), true) if !a.purpose.is_empty() => {
            format!("{head}: {}", clip(&a.purpose, SHORT_CHARS))
        }
        (Some(_), true) => head,
        (Some(a), false) => format!("{head}: {}", clip(&a.line(), ENTRY_CHARS)),
    }
}

/// The block's text, unwrapped: `entries` in the boss's scope, the whole
/// account of `about` (the session the turn is about), and `posts` (oldest
/// first) at the person's time `tz`. None when there is nothing to say.
pub(super) fn body(
    entries: &[Entry],
    about: Option<Uuid>,
    posts: &[Post],
    open: Option<&str>,
    tz: chrono_tz::Tz,
) -> Option<String> {
    if entries.is_empty() && posts.is_empty() && open.is_none() {
        return None;
    }
    let mut head = String::new();
    // What it owes the person comes first: it is never dropped for room.
    if let Some(open) = open {
        head.push_str(open.trim_end());
        head.push('\n');
    }
    let mut headed = None;
    if let Some(a) = about
        .and_then(|id| entries.iter().find(|e| e.id == id))
        .and_then(|e| e.account.as_ref().map(|a| (e, a)))
    {
        let render = a.1.render();
        let render: String = match render.chars().count() > ABOUT_CHARS {
            true => format!("{}…", render.chars().take(ABOUT_CHARS).collect::<String>()),
            false => render,
        };
        if !head.is_empty() {
            head.push('\n');
        }
        head.push_str(&format!(
            "The session this turn is about, {} (as of {}):\n{}\n",
            a.0.name,
            a.1.written_at.with_timezone(&tz).format("%H:%M"),
            render.trim_end()
        ));
        headed = Some(a.0.id);
    }
    let mut tail = String::new();
    if !posts.is_empty() {
        tail.push_str("\nWhat you told the person lately, oldest first:\n");
        for p in posts {
            tail.push_str(&format!(
                "- {} {}\n",
                p.at.with_timezone(&tz).format("%H:%M"),
                clip(&p.text, POST_CHARS)
            ));
        }
    }
    // Working sessions first, then the quiet ones.
    // The session the turn is about is listed too when it has no account
    // to give whole.
    let mut shown: Vec<&Entry> = entries.iter().filter(|e| Some(e.id) != headed).collect();
    shown.sort_by_key(|e| quiet(e));
    let room = CAP_CHARS.saturating_sub(head.len() + tail.len());
    let mut lines: Vec<String> = shown.iter().map(|e| line(e, false, tz)).collect();
    if lines.iter().map(|l| l.len() + 1).sum::<usize>() > room {
        // Over the cap: quiet sessions drop to one short line first.
        lines = shown.iter().map(|e| line(e, quiet(e), tz)).collect();
    }
    let mut sessions = String::new();
    let mut left = 0usize;
    for l in &lines {
        if sessions.len() + l.len() + 1 > room {
            left += 1;
            continue;
        }
        sessions.push_str(l);
        sessions.push('\n');
    }
    if left > 0 {
        sessions.push_str(&format!("- and {left} more (crew.overview lists them)\n"));
    }
    let mut out = head;
    if !sessions.is_empty() {
        out.push_str("\nThe sessions you oversee, from their accounts:\n");
        out.push_str(&sessions);
    }
    out.push_str(&tail);
    Some(out)
}

/// The block for a turn of `service` on `channel` (its DM, or a session's
/// line, where its line and work turns run), when it is a boss. None off
/// those, for an agent that oversees nothing, or with nothing to say.
pub(crate) async fn block(service: &AgentService, channel: &str, owner: &str) -> Option<String> {
    let about = match channel {
        DESK_CHANNEL => None,
        c if c.starts_with(&format!("{DESK_CHANNEL}:checkin:")) => None,
        c => Some(
            c.strip_prefix(DESK_CHANNEL)?
                .strip_prefix(":line:")?
                .parse::<Uuid>()
                .ok()?,
        ),
    };
    let crew = super::for_service(service)?;
    let (boss, o) = crew.overseer_named(service.agent_id.as_str())?;
    let seen = crew.desk.seen_by(&o.scope);
    // The cheap tests first: the hub-internal one reads Desk's state.
    let views: Vec<_> = crew
        .desk
        .list()
        .into_iter()
        .filter(|v| v.state != SessionState::Closed)
        .filter(|v| seen.sees(v.group.as_ref().map(|g| g.id.as_str())))
        .filter(|v| !workforce_desk::ledger::agent_turn(&v.cwd, &v.name))
        .filter(|v| !crew.desk.hub_internal(v.id))
        .collect();
    let ids: Vec<Uuid> = views.iter().map(|v| v.id).collect();
    let accounts = match crew.desk.session_accounts_of(&ids).await {
        Ok(a) => a,
        Err(e) => {
            tracing::info!(error = %e, "the sessions' accounts couldn't be read for a boss's turn");
            Vec::new()
        }
    };
    let entries: Vec<Entry> = views
        .iter()
        .map(|v| Entry {
            id: v.id,
            name: v.label().to_string(),
            state: v.state,
            failed: v.failure.is_some(),
            account: accounts.iter().find(|a| a.session == v.id).cloned(),
        })
        .collect();
    let posts = recent_posts(&boss, owner);
    let open = crew.items_context(boss.agent_id.as_str()).await;
    // The person's preferences that cover what the turn is about: like
    // what it owes them, never dropped for room.
    let prefs = crew
        .preferences_context(
            &workforce_desk::preference::boss_scope_of(o.scope.group()),
            about,
            crew.unbound_given(&boss, &o),
        )
        .await;
    let (listed, how) = match prefs {
        Some(p) => (Some(p.listed), Some(p.how)),
        None => (None, None),
    };
    let open = match (open, listed) {
        (Some(open), Some(listed)) => Some(format!("{}\n\n{listed}", open.trim_end())),
        (open, listed) => open.or(listed),
    };
    let body = body(
        &entries,
        about,
        &posts,
        open.as_deref(),
        crew.desk.timezone(),
    )?;
    let how = how.map(|h| format!("\n\n{h}")).unwrap_or_default();
    Some(format!(
        "What Desk keeps of your crew now, the same in every turn of yours (read crew.account \
         before you say anything specific of one session):\n{}{how}",
        reported(&body, "Desk's notes on your crew")
    ))
}

/// The boss's own last [`POSTS`] posts in its DM with `owner` (the person
/// the turn is for), oldest first: from the DM's archive, so they survive
/// its rollover.
fn recent_posts(boss: &AgentService, owner: &str) -> Vec<Post> {
    let speaker = workforce_desk::Speaker::agent(boss.agent_id.as_str(), &super::name_of(boss));
    let page = match crate::desk_conversations::page(
        boss.sessions.as_ref(),
        &speaker,
        owner,
        DESK_CHANNEL,
        workforce_desk::Cursor::Newest,
        POSTS * 4,
    ) {
        Ok(p) => p,
        Err(e) => {
            tracing::info!(error = %e, "a boss's recent posts couldn't be read");
            return Vec::new();
        }
    };
    let mut posts: Vec<Post> = page
        .messages
        .into_iter()
        .filter(|m| m.from == workforce_desk::Author::Agent && !m.text.trim().is_empty())
        .map(|m| Post {
            at: m.at,
            text: m.text,
        })
        .collect();
    let from = posts.len().saturating_sub(POSTS);
    posts.drain(..from);
    posts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, state: SessionState, purpose: &str) -> Entry {
        let id = Uuid::new_v4();
        let mut a = Account::new(id, "", Utc::now());
        a.purpose = purpose.to_string();
        a.now = format!("{name} is {} ", "busy with a long step".repeat(4));
        a.next = "The next step, within the hour.".into();
        Entry {
            id,
            name: name.into(),
            state,
            failed: false,
            account: Some(a),
        }
    }

    #[test]
    fn past_the_cap_quiet_sessions_drop_to_one_line_first() {
        let mut entries: Vec<Entry> = (0..60)
            .map(|i| entry(&format!("idle-{i}"), SessionState::Idle, "Tidied the docs."))
            .collect();
        entries.extend((0..12).map(|i| {
            entry(
                &format!("busy-{i}"),
                SessionState::Working,
                "Fix the login page.",
            )
        }));
        let about = entries[65].id;
        let text = body(&entries, Some(about), &[], None, chrono_tz::UTC).unwrap();
        assert!(text.len() <= CAP_CHARS + 200, "{}", text.len());
        assert!(
            text.contains("The session this turn is about, busy-5"),
            "{text}"
        );
        for i in [0, 11] {
            let l = text
                .lines()
                .find(|l| l.starts_with(&format!("- busy-{i} ")))
                .unwrap_or_else(|| panic!("busy-{i} is kept: {text}"));
            assert!(
                l.contains("Now:"),
                "a working session keeps its whole line: {l}"
            );
        }
        let idle = text.lines().find(|l| l.starts_with("- idle-0 ")).unwrap();
        assert!(
            !idle.contains("Now:"),
            "a quiet one is one short line: {idle}"
        );
        assert!(text.contains("Tidied the docs."));
    }

    #[test]
    fn under_the_cap_every_session_keeps_its_line_and_posts_are_timed() {
        let entries = vec![
            entry("web", SessionState::Working, "Fix the login page."),
            entry("docs", SessionState::Done, "Tidy the docs."),
        ];
        let posts = vec![Post {
            at: "2026-11-12T19:40:00Z".parse().unwrap(),
            text: "web is fixing the login page;\nCI next.".into(),
        }];
        let text = body(&entries, None, &posts, None, chrono_tz::UTC).unwrap();
        assert!(
            text.lines().filter(|l| l.contains("Now:")).count() == 2,
            "{text}"
        );
        assert!(
            text.contains("- 19:40 web is fixing the login page; CI next."),
            "{text}"
        );
        assert!(body(&[], None, &[], None, chrono_tz::UTC).is_none());
    }

    #[test]
    fn the_session_a_turn_is_about_is_kept_without_an_account_and_clipped_with_a_long_one() {
        let mut fresh = entry("new-one", SessionState::Working, "");
        fresh.account = None;
        let id = fresh.id;
        let text = body(&[fresh], Some(id), &[], None, chrono_tz::UTC).unwrap();
        assert!(text.contains("- new-one (working, session_id"), "{text}");
        assert!(text.contains("no account yet"));
        let mut long = entry("busy", SessionState::Working, "Fix the login page.");
        if let Some(a) = long.account.as_mut() {
            a.done = (0..20)
                .map(|i| workforce_desk::account::Done {
                    text: format!("step {i} {}", "with many words ".repeat(25)),
                    refs: vec![],
                    at: Utc::now(),
                })
                .collect();
        }
        let id = long.id;
        let mut entries = vec![long];
        entries.extend(
            (0..20).map(|i| entry(&format!("other-{i}"), SessionState::Working, "Other work.")),
        );
        let text = body(&entries, Some(id), &[], None, chrono_tz::UTC).unwrap();
        assert!(text.len() <= CAP_CHARS + 400, "{}", text.len());
        assert!(
            text.contains("- other-0 "),
            "the others still have room: {text}"
        );
    }
}
