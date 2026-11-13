//! What a linked engine's sessions ask the person, on the boss hub
//! (remote bosses design §7, RB7b): each `ask` frame becomes a routed
//! approval only the link's person answers, carded in the group's Slack
//! thread and listed on Desk; their answer goes back down the link as a
//! `decision` with the ask's nonce; the engine decides whether to take it.
//!
//! This hub is trusted to say who answered, and nothing more:
//! - the person, agent, group and engine are the live link's, never the
//!   frame's;
//! - an ask the engine says is `desk_only` offers no Allow, and Allow is
//!   refused here before anything is recorded (the engine refuses it too);
//! - a nonce is kept in memory only, for the ask's link; after a restart
//!   the engine sends each open ask again with a new one, and an answer
//!   already given goes down again with it;
//! - an `ask_closed` from the engine closes the card: answered in Desk, by
//!   the boss, run out, or withdrawn.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Weak};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_agent::runner::PendingApprovalCore;
use workforce_core::policy::PermissionChoice;

use super::auth::LiveLink;
use super::uplink::Uplinks;
use crate::approvals::store::PendingApprovalStore;
use crate::approvals::store::{ApprovalOutcome, PendingApproval};
use crate::gateway::GatewayState;

/// The tool name an ask's routed approval is kept under: no turn resumes
/// from it.
pub const TOOL: &str = "desk_boss.ask";

/// The longest an ask is answerable here.
const MAX_MINUTES: i64 = 60;

/// The most of a summary and of the detail an ask's card shows.
const SUMMARY_CHARS: usize = 300;
const DETAIL_CHARS: usize = 1_000;

/// The most asks one link may have open here at once; the rest wait on
/// the engine, which sends them again on its next socket.
pub const OPEN_PER_LINK: usize = 20;

/// The most times an answer is sent again to an engine that had no room.
const BUSY_TRIES: u8 = 5;

/// An open ask, as this hub holds it in memory.
struct Held {
    link: Uuid,
    /// The nonce it last came with.
    nonce: String,
    uplinks: Weak<Uplinks>,
    expires_at: DateTime<Utc>,
    /// The person's answer, once given: (choice, decider).
    decided: Option<(String, Value)>,
    busy: u8,
}

/// The asks this hub holds answers for, in memory.
#[derive(Default)]
pub struct Asks {
    held: parking_lot::Mutex<HashMap<Uuid, Held>>,
}

/// This hub's asks: one table, whichever routed approval answers them.
static ASKS: LazyLock<Asks> = LazyLock::new(Asks::default);

/// The routed approvals' store, as [`crate::approvals::decide`] reads it.
fn approvals_store(state: &GatewayState) -> Option<Arc<dyn PendingApprovalStore>> {
    state
        .registry
        .resolve(None)?
        .pending_approval_store
        .read()
        .clone()
}

/// The routed approval's id for `ask`.
pub fn request_id(ask: Uuid) -> String {
    format!("dba_{ask}")
}

/// Text from an engine, for a card: control characters out, capped.
fn plain(s: &str, max: usize) -> String {
    let s: String = s
        .chars()
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .collect();
    let s = workforce_common::redact::redact_secrets(&s);
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s,
    }
}

/// An `ask` frame, read: none when it isn't one.
#[derive(Debug, Clone, PartialEq)]
pub struct Asked {
    pub ask: Uuid,
    pub nonce: String,
    pub desk_only: bool,
    pub options: Vec<&'static str>,
    pub summary: String,
    pub detail: Option<String>,
    pub subject: Uuid,
    pub expires_at: DateTime<Utc>,
}

impl Asked {
    pub fn of(frame: &Value, now: DateTime<Utc>) -> Option<Self> {
        let ask = frame["askId"].as_str()?.parse().ok()?;
        let nonce = frame["nonce"].as_str()?;
        if nonce.len() != 64 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let desk_only = match frame["class"].as_str()? {
            "anywhere" => false,
            "desk_only" => true,
            _ => return None,
        };
        let mut options: Vec<&'static str> = Vec::new();
        for o in frame["options"].as_array()? {
            let o = match o.as_str()? {
                "allow" if !desk_only => "allow",
                "allow" => continue,
                "deny" => "deny",
                "stop" => "stop",
                _ => continue,
            };
            if !options.contains(&o) {
                options.push(o);
            }
        }
        if options.is_empty() {
            return None;
        }
        let summary = plain(frame["summary"].as_str()?, SUMMARY_CHARS);
        if summary.trim().is_empty() {
            return None;
        }
        let expires_at = frame["expiresAt"]
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc))?
            .min(now + chrono::Duration::minutes(MAX_MINUTES));
        if expires_at <= now {
            return None;
        }
        Some(Self {
            ask,
            nonce: nonce.to_string(),
            desk_only,
            options,
            summary,
            detail: frame["detail"]
                .as_str()
                .map(|d| plain(d, DETAIL_CHARS))
                .filter(|d| !d.trim().is_empty()),
            subject: frame["subject"].as_str()?.parse().ok()?,
            expires_at,
        })
    }
}

/// The choice a card's press gives an ask, or none for one it doesn't
/// offer: Allow once, Deny, or (its third button) Stop the session.
pub fn choice_of(choice: PermissionChoice) -> Option<&'static str> {
    match choice {
        PermissionChoice::AllowOnce => Some("allow"),
        PermissionChoice::DenyOnce => Some("deny"),
        PermissionChoice::DenyAlways => Some("stop"),
        _ => None,
    }
}

/// Why `row`, an ask's approval, can't be answered with `choice` here;
/// none when it can.
pub fn refuses(row: &PendingApproval, choice: PermissionChoice) -> Option<&'static str> {
    let Some(c) = choice_of(choice) else {
        return Some("That isn't one of its answers.");
    };
    let offered = row.tool_input["options"]
        .as_array()
        .is_some_and(|o| o.iter().any(|x| x == c));
    if offered {
        None
    } else if c == "allow" {
        Some("Decide this in Desk.")
    } else {
        Some("That isn't one of its answers.")
    }
}

impl Asks {
    /// Holds `asked`, come on `link`, with the nonce it came with now:
    /// false when another link holds it, or the link has as many open as
    /// it may. What has run out is let go first.
    fn hold(&self, asked: &Asked, link: Uuid, uplinks: &Arc<Uplinks>) -> bool {
        let mut held = self.held.lock();
        let now = Utc::now();
        held.retain(|_, h| h.expires_at > now);
        if let Some(h) = held.get_mut(&asked.ask) {
            if h.link != link {
                return false;
            }
            h.nonce = asked.nonce.clone();
            h.uplinks = Arc::downgrade(uplinks);
            return true;
        }
        if held.values().filter(|h| h.link == link).count() >= OPEN_PER_LINK {
            return false;
        }
        held.insert(
            asked.ask,
            Held {
                link,
                nonce: asked.nonce.clone(),
                uplinks: Arc::downgrade(uplinks),
                expires_at: asked.expires_at,
                decided: None,
                busy: 0,
            },
        );
        true
    }

    /// Keeps nothing more of `ask`.
    pub fn forget(&self, ask: Uuid) {
        self.held.lock().remove(&ask);
    }
}

/// `s`, an engine's words, as plain Slack text: its `&`, `<` and `>`
/// escaped so no link, mention or channel markup in it works, and no
/// formatting marks; its lines kept.
fn slack_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '*' | '_' | '~' | '`' => {}
            c => out.push(c),
        }
    }
    out
}

/// The card an ask is shown with in Slack.
fn blocks(asked: &Asked, live: &LiveLink, hub: &str, request: &str) -> Vec<Value> {
    let mut text = format!(
        ":raising_hand: *{}*\n{} on {}, through {}",
        slack_text(&asked.summary),
        super::slack::slack_plain(&live.link.group_name),
        slack_text(&live.link.engine_id),
        slack_text(&live.agent_id)
    );
    if let Some(d) = &asked.detail {
        text.push_str(&format!("\n> {}", slack_text(d).replace('\n', "\n> ")));
    }
    let mut out = vec![json!({"type": "section", "text": {"type": "mrkdwn", "text": text}})];
    if asked.desk_only {
        out.push(json!({"type": "context", "elements": [{"type": "mrkdwn",
            "text": "Decide this in Desk: allowing it can't be done from here."}]}));
    } else {
        out.push(json!({"type": "context", "elements": [{"type": "mrkdwn",
            "text": format!("Answering here trusts {hub} to pass on your answer.")}]}));
    }
    let button = |label: &str, style: Option<&str>, action: &str| {
        let mut b = json!({"type": "button", "text": {"type": "plain_text", "text": label},
                           "action_id": action, "value": request});
        if let Some(s) = style {
            b["style"] = json!(s);
        }
        b
    };
    let mut elements = Vec::new();
    for o in &asked.options {
        elements.push(match *o {
            "allow" => button("Allow once", Some("primary"), "perm_allow_once"),
            "deny" => button("Deny", Some("danger"), "perm_deny_once"),
            _ => button("Stop the session", Some("danger"), "perm_deny_always"),
        });
    }
    out.push(json!({"type": "actions", "elements": elements}));
    out
}

/// Takes `frame`, an `ask` come on `live`'s link: keeps its routed
/// approval for the link's person and cards it, once an ask; an ask
/// already answered here has its answer sent down again with the nonce it
/// now has. Err when the frame isn't an ask.
pub async fn asked(
    state: &Arc<GatewayState>,
    live: &LiveLink,
    frame: &Value,
    hub: &str,
) -> Result<(), ()> {
    let asked = Asked::of(frame, Utc::now()).ok_or(())?;
    let link = live.link.link_id;
    let Some(service) = state.registry.get(&live.agent_id) else {
        return Ok(());
    };
    let Some(store) = approvals_store(state) else {
        return Ok(());
    };
    let person = live.person.id.to_string();
    let request = request_id(asked.ask);
    // One ask is one link's: another link's id is never taken over, and
    // its nonce is held only once the kept row says it is this link's.
    let row = match store.get(&request) {
        Ok(row) => row,
        Err(e) => {
            tracing::warn!(error = %e, "an ask's approval couldn't be read");
            return Ok(());
        }
    };
    if row
        .as_ref()
        .is_some_and(|r| r.tool_input["linkId"] != json!(link))
    {
        return Err(());
    }
    if !ASKS.hold(&asked, link, &state.uplinks) {
        return Ok(());
    }
    match row {
        Some(row) if !row.is_pending() => {
            // Answered here before it came again: the answer goes down
            // with its new nonce.
            let kept = ASKS
                .held
                .lock()
                .get(&asked.ask)
                .and_then(|h| h.decided.clone());
            let given = kept.or_else(|| {
                let choice = match ApprovalOutcome::from_str_lossy(&row.status)? {
                    ApprovalOutcome::Approved => "allow",
                    ApprovalOutcome::Denied => "deny",
                    ApprovalOutcome::Expired => return None,
                };
                let decider = json!({"identityId": row.resolved_by.clone().unwrap_or_default(),
                                     "via": "desk"});
                Some((choice.to_string(), decider))
            });
            match given {
                Some((choice, decider)) => {
                    send_decision(asked.ask, &choice, decider);
                }
                // Run out here: never answered.
                None => ASKS.forget(asked.ask),
            }
            return Ok(());
        }
        Some(_) => return Ok(()),
        None => {}
    }
    let key = (
        live.person.id,
        live.agent_id.clone(),
        live.link.group_key.clone(),
    );
    let place = super::wake::place(state, &key);
    // (workspace, channel, thread) of the group's Slack DM thread, while it
    // is the person's own.
    let thread = match &place {
        Some(super::wake::Place::Thread(k))
            if super::slack::thread_is_theirs(&service, &person, k).await =>
        {
            super::slack::dm_thread(k)
                .map(|(w, c, ts)| (w.to_string(), c.to_string(), ts.to_string()))
        }
        _ => None,
    };
    let ttl = (asked.expires_at - Utc::now())
        .to_std()
        .unwrap_or(std::time::Duration::from_secs(60));
    let core = PendingApprovalCore {
        request_id: request.clone(),
        agent_id: live.agent_id.clone(),
        session_key: super::conversation::boss_channel(&live.link.group_key),
        channel_id: thread
            .as_ref()
            .map(|(_, c, _)| c.clone())
            .unwrap_or_default(),
        requester_user_id: person.clone(),
        tool_name: TOOL.to_string(),
        tool_input: json!({
            "linkId": link,
            "askId": asked.ask,
            "engine": live.link.engine_id,
            "group": live.link.group_name,
            "subject": asked.subject,
            "class": if asked.desk_only { "desk_only" } else { "anywhere" },
            "options": asked.options,
            "summary": asked.summary,
            "detail": asked.detail,
        }),
        reason: asked.summary.clone(),
        allow_self_approval: true,
        approver_user_ids: vec![person.clone()],
        ttl,
    };
    if let Err(e) = store.insert_core(
        &core,
        thread.as_ref().map(|(w, _, _)| w.as_str()),
        thread.as_ref().map(|(_, _, ts)| ts.as_str()),
        asked.expires_at,
    ) {
        tracing::warn!(error = %e, "an ask's approval couldn't be kept");
        return Ok(());
    }
    // Desk lists it for the person.
    if let Err(e) = store.add_target(&request, &person, "") {
        tracing::warn!(error = %e, "an ask's approver couldn't be kept");
    }
    crate::approvals::approvers_changed([person.as_str()]);
    // Slack: in the group's thread, while it is the person's own DM.
    let Some((workspace, channel, ts)) = thread else {
        return Ok(());
    };
    let Some(slack_id) = live
        .person
        .linked_ids
        .get("slack")
        .filter(|s| !s.is_empty())
        .cloned()
    else {
        return Ok(());
    };
    let surface = {
        let key = workforce_common::channels::SurfaceKey::workspace("slack", workspace);
        service.surface_registry.read().await.get(&key).cloned()
    };
    let Some(surface) = surface else {
        return Ok(());
    };
    if let Err(e) = store.add_target(&request, &person, &slack_id) {
        tracing::warn!(error = %e, "an ask's Slack approver couldn't be kept");
        return Ok(());
    }
    let sent = surface
        .send_message(&workforce_common::channels::OutboundMessage {
            channel: channel.clone(),
            text: format!(
                "{} — answer in this thread's card, or in Desk.",
                slack_text(&asked.summary)
            ),
            thread_id: Some(ts),
            username: None,
            icon_url: None,
            blocks: Some(blocks(&asked, live, hub, &request)),
        })
        .await;
    match sent {
        Ok(sent) => {
            if let Err(e) = store.set_target_message(&request, &person, &channel, &sent.message_id)
            {
                tracing::warn!(error = %e, "an ask's card couldn't be kept");
            }
        }
        Err(e) => tracing::warn!(error = %e, "an ask's card couldn't be posted"),
    }
    Ok(())
}

/// Sends the answer to `ask` down its link, when a socket carries it;
/// kept to send again until the engine closes the ask. Whether it went.
fn send_decision(ask: Uuid, choice: &str, decider: Value) -> bool {
    let (link, nonce, uplinks) = {
        let mut held = ASKS.held.lock();
        let Some(h) = held.get_mut(&ask) else {
            return false;
        };
        h.decided = Some((choice.to_string(), decider.clone()));
        (h.link, h.nonce.clone(), h.uplinks.clone())
    };
    let Some(uplinks) = uplinks.upgrade() else {
        return false;
    };
    uplinks.notify_link(
        link,
        json!({"type": "decision", "askId": ask, "linkId": link, "nonce": nonce,
               "choice": choice, "decider": decider, "decidedAt": Utc::now()}),
    )
}

/// The person answered `row`, an ask's approval, with `choice`, as
/// `decider` (their identity) by `via`: it goes down the link. Whether it
/// went now; else it goes when the engine sends the ask again.
pub fn decided(
    row: &PendingApproval,
    choice: PermissionChoice,
    decider: &str,
    via: &str,
    slack: Option<&str>,
) -> bool {
    let Some(ask) = row.tool_input["askId"]
        .as_str()
        .and_then(|a| a.parse().ok())
    else {
        return false;
    };
    let Some(choice) = choice_of(choice) else {
        return false;
    };
    let mut who = json!({"identityId": decider, "via": via});
    if let Some(s) = slack {
        who["slackUserId"] = json!(s);
    }
    send_decision(ask, choice, who)
}

/// Takes an `ask_closed` or `decision_refused` frame come on `live`'s
/// link, for an ask that link's: its answer is no longer sent, and its card
/// says what came of it. The engine's words are said as its: a card the
/// person answered keeps their answer.
pub async fn closed(state: &Arc<GatewayState>, live: &LiveLink, frame: &Value) {
    let Some(ask) = frame["askId"].as_str().and_then(|a| a.parse::<Uuid>().ok()) else {
        return;
    };
    let link = live.link.link_id;
    let Some(service) = state.registry.get(&live.agent_id) else {
        return;
    };
    let Some(store) = approvals_store(state) else {
        return;
    };
    let request = request_id(ask);
    let Ok(Some(row)) = store.get(&request) else {
        return;
    };
    if row.tool_input["linkId"] != json!(link) {
        return;
    }
    let refused = frame["type"] == "decision_refused";
    // The engine had no room for it just then: the same answer, again, a
    // few times.
    if refused && frame["code"] == "busy" {
        let again = {
            let mut held = ASKS.held.lock();
            held.get_mut(&ask).and_then(|h| {
                h.busy += 1;
                (h.busy <= BUSY_TRIES).then(|| h.decided.clone()).flatten()
            })
        };
        if let Some((choice, decider)) = again {
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let still = ASKS
                    .held
                    .lock()
                    .get(&ask)
                    .is_some_and(|h| h.decided.is_some());
                if still {
                    send_decision(ask, &choice, decider);
                }
            });
        }
        return;
    }
    if refused {
        // Not taken: the person's answer stays theirs, and the card says
        // it wasn't taken.
        if let Some(h) = ASKS.held.lock().get_mut(&ask) {
            h.decided = None;
        }
        let why = plain(
            frame["why"].as_str().unwrap_or("the engine refused it."),
            200,
        );
        rewrite(
            &service,
            &store,
            &row,
            &format!(
                "Your answer wasn't taken. The engine says: {}",
                slack_text(&why)
            ),
        )
        .await;
        return;
    }
    ASKS.forget(ask);
    if !row.is_pending() {
        return;
    }
    let said = match frame["how"].as_str().unwrap_or_default() {
        "answered_in_desk" => "Answered in Desk.",
        "answered_by_boss" => "The boss answered it.",
        "expired" => "It waited too long: answer it in Desk.",
        _ => "No longer asked.",
    };
    if store
        .claim(&request, ApprovalOutcome::Expired, "")
        .unwrap_or(false)
    {
        crate::approvals::resolved(store.as_ref(), &request);
    }
    rewrite(&service, &store, &row, &format!("The engine says: {said}")).await;
}

/// What a card says once its person answered it here with `choice`, and
/// whether it went down the link now.
pub fn said_of(choice: PermissionChoice, went: bool) -> String {
    let what = match choice_of(choice) {
        Some("allow") => "Allowed",
        Some("stop") => "Denied, and the session asked to stop",
        _ => "Denied",
    };
    match went {
        true => format!("{what}: passed on."),
        false => format!("{what}: passed on when the engine is back."),
    }
}

/// Rewrites `row`'s cards once it waited too long here.
pub async fn rewrite_expired(
    service: &Arc<crate::serve::AgentService>,
    store: &Arc<dyn PendingApprovalStore>,
    row: &PendingApproval,
) {
    if let Some(ask) = row.tool_input["askId"]
        .as_str()
        .and_then(|a| a.parse::<Uuid>().ok())
    {
        ASKS.forget(ask);
    }
    rewrite(
        service,
        store,
        row,
        "It waited too long: answer it in Desk.",
    )
    .await;
}

/// Rewrites `row`'s cards once its person answered it with `choice`.
pub async fn rewrite_decided(
    service: &Arc<crate::serve::AgentService>,
    store: &Arc<dyn PendingApprovalStore>,
    row: &PendingApproval,
    choice: PermissionChoice,
    went: bool,
) {
    rewrite(service, store, row, &said_of(choice, went)).await;
}

/// Rewrites every card of `row` to say `said`, without its buttons.
async fn rewrite(
    service: &Arc<crate::serve::AgentService>,
    store: &Arc<dyn PendingApprovalStore>,
    row: &PendingApproval,
    said: &str,
) {
    let surface = {
        let surfaces = service.surface_registry.read().await;
        match &row.workspace_id {
            Some(w) => surfaces
                .get(&workforce_common::channels::SurfaceKey::workspace(
                    "slack",
                    w.clone(),
                ))
                .cloned(),
            None => None,
        }
    };
    let Some(surface) = surface else {
        return;
    };
    let summary = slack_text(row.tool_input["summary"].as_str().unwrap_or_default());
    let text = format!("*{summary}*\n{said}");
    for t in store.targets(&row.request_id).unwrap_or_default() {
        let (Some(dm), Some(ts)) = (&t.dm_channel, &t.message_ts) else {
            continue;
        };
        let out = workforce_common::channels::OutboundMessage {
            channel: dm.clone(),
            text: text.clone(),
            thread_id: None,
            username: None,
            icon_url: None,
            blocks: Some(vec![
                json!({"type": "section", "text": {"type": "mrkdwn", "text": text}}),
            ]),
        };
        if let Err(e) = surface.update_message(dm, ts, &out).await {
            tracing::warn!(error = %e, "an ask's card couldn't be rewritten");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> Value {
        json!({"type": "ask", "askId": Uuid::from_u128(1), "nonce": "ab".repeat(32),
               "linkId": Uuid::from_u128(2), "class": "anywhere",
               "summary": "acme-api asks to use Bash", "detail": "cargo build\u{7}",
               "options": ["allow", "deny", "stop", "always"],
               "expiresAt": (Utc::now() + chrono::Duration::hours(5)).to_rfc3339(),
               "subject": Uuid::from_u128(3)})
    }

    #[test]
    fn an_ask_is_read_as_this_hub_will_show_it_and_no_more() {
        let now = Utc::now();
        let a = Asked::of(&frame(), now).unwrap();
        assert_eq!(a.options, vec!["allow", "deny", "stop"]);
        assert!(!a.detail.as_deref().unwrap().contains('\u{7}'));
        assert!(a.expires_at <= now + chrono::Duration::minutes(MAX_MINUTES));
        // Desk only: no Allow, whatever it lists.
        let mut f = frame();
        f["class"] = json!("desk_only");
        assert_eq!(Asked::of(&f, now).unwrap().options, vec!["deny", "stop"]);
        // What isn't an ask isn't taken.
        for (k, v) in [
            ("nonce", json!("short")),
            ("class", json!("whatever")),
            ("options", json!(["always"])),
            ("summary", json!("  ")),
            (
                "expiresAt",
                json!((now - chrono::Duration::minutes(1)).to_rfc3339()),
            ),
            ("subject", json!("devbox")),
        ] {
            let mut f = frame();
            f[k] = v;
            assert!(Asked::of(&f, now).is_none(), "{k}");
        }
    }

    #[test]
    fn an_engines_words_are_plain_in_a_card() {
        let s =
            slack_text("<https://evil.example|Approve in Desk> <!channel> <@U1> *ok* & `x`\nnext");
        assert!(!s.contains('<') && !s.contains('>') && !s.contains('*') && !s.contains('`'));
        assert!(s.contains("&lt;https://evil.example|Approve in Desk&gt;"));
        assert!(s.contains("&amp;") && s.contains('\n'));
        assert_eq!(
            said_of(PermissionChoice::DenyAlways, false),
            "Denied, and the session asked to stop: passed on when the engine is back."
        );
    }

    #[test]
    fn a_link_holds_its_own_asks_and_no_more_than_its_share() {
        let asks = Asks::default();
        let uplinks = Arc::new(Uplinks::default());
        let (a, b) = (Uuid::from_u128(10), Uuid::from_u128(11));
        let now = Utc::now();
        let mut asked = Asked::of(&frame(), now).unwrap();
        assert!(asks.hold(&asked, a, &uplinks));
        // Another link never takes it over, nor its nonce.
        let mut other = asked.clone();
        other.nonce = "cd".repeat(32);
        assert!(!asks.hold(&other, b, &uplinks));
        assert_eq!(asks.held.lock()[&asked.ask].nonce, "ab".repeat(32));
        // Its share open at once, then no more until one closes.
        for i in 1..OPEN_PER_LINK as u128 {
            asked.ask = Uuid::from_u128(100 + i);
            assert!(asks.hold(&asked, a, &uplinks), "{i}");
        }
        asked.ask = Uuid::from_u128(999);
        assert!(!asks.hold(&asked, a, &uplinks));
        assert!(
            asks.hold(&asked, b, &uplinks),
            "another link's share is its own"
        );
        asks.forget(Uuid::from_u128(101));
        asked.ask = Uuid::from_u128(998);
        assert!(asks.hold(&asked, a, &uplinks));
        // What has run out is let go.
        asks.held
            .lock()
            .values_mut()
            .for_each(|h| h.expires_at = now);
        asked.ask = Uuid::from_u128(997);
        assert!(asks.hold(&asked, a, &uplinks));
        assert_eq!(asks.held.lock().len(), 1);
    }

    #[test]
    fn a_card_offers_only_what_its_ask_does() {
        let row = |options: Value| PendingApproval {
            request_id: "dba_x".into(),
            agent_id: "allan".into(),
            session_key: "desk:boss:k".into(),
            workspace_id: None,
            channel_id: None,
            thread_ts: None,
            requester_id: "sam".into(),
            tool_name: TOOL.into(),
            tool_input: json!({"options": options}),
            reason: String::new(),
            allow_self_approval: true,
            status: "pending".into(),
            resolved_by: None,
            channel_notice: None,
            channel_notice_ts: None,
            created_at: Utc::now(),
            expires_at: Utc::now(),
        };
        let anywhere = row(json!(["allow", "deny", "stop"]));
        assert_eq!(refuses(&anywhere, PermissionChoice::AllowOnce), None);
        assert_eq!(refuses(&anywhere, PermissionChoice::DenyAlways), None);
        assert!(refuses(&anywhere, PermissionChoice::AllowAlways).is_some());
        let desk_only = row(json!(["deny", "stop"]));
        assert_eq!(
            refuses(&desk_only, PermissionChoice::AllowOnce),
            Some("Decide this in Desk.")
        );
        assert_eq!(refuses(&desk_only, PermissionChoice::DenyOnce), None);
    }
}
