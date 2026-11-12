//! What the sessions of a remote boss's group ask the person, sent up its
//! link so they can answer from anywhere, and the answers that come back
//! (remote bosses design §7; [`workforce_desk::boss_ask`]).
//!
//! On this engine, for each link an uplink carries:
//! - a request open in one of the grant's sessions that is the person's to
//!   answer (a session the boss doesn't run, or one whose badge says it
//!   waits on them) goes up once as an `ask`, with a fresh nonce kept here
//!   only as its hash;
//! - an ask whose request was answered here, or whose time ran out, or
//!   whose grant no longer serves the boss, is closed and said so
//!   (`ask_closed`), so its cards are rewritten;
//! - a `decision` is taken only as [`workforce_desk::boss_ask::accept_remote`]
//!   says, and only once (its row is closed as it is taken), and then
//!   answers the request as the person, or stops the session;
//! - on a new socket every open ask goes up again with a new nonce: the
//!   old one is good no longer.

use std::sync::Arc;

use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_desk::boss_ask::{self, Choice, Class, Decision};
use workforce_desk::boss_ask_store::NewAsk;
use workforce_desk::crew::By;
use workforce_desk::grant::{BossRef, Grant};
use workforce_desk::grant_store::CallRow;
use workforce_desk::sees::{self, Part};

use super::actions::Action;
use super::CrewHub;

/// How long an ask may be answered from elsewhere.
pub const ASK_MINUTES: i64 = 30;

/// The most of a request's words an ask carries.
const DETAIL_CHARS: usize = 1_000;

/// A fresh nonce: 32 random bytes, as hex.
fn nonce() -> String {
    use rand::RngCore as _;
    let mut b = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

fn closed(ask: Uuid, how: &str) -> Value {
    json!({"type": "ask_closed", "askId": ask, "how": how})
}

impl CrewHub {
    /// What goes up `link` (to `boss`, the remote boss of `group`) about
    /// asks now: new asks, and asks closed since. With `again`, on a new
    /// socket, every ask still open goes up again with a new nonce.
    pub(crate) async fn asks_up(
        self: &Arc<Self>,
        link: Uuid,
        group: &str,
        boss: &BossRef,
        again: bool,
    ) -> Vec<Value> {
        let store = self.desk.store();
        let open = match store.boss_asks_open(link).await {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "a boss link's asks couldn't be read"
                );
                return Vec::new();
            }
        };
        let grant = self
            .desk
            .grant_live(group)
            .ok()
            .filter(|g| self.serves_remote(g, boss));
        let mut out = Vec::new();
        let now = Utc::now();
        for ask in &open {
            let pending = self
                .desk
                .view(ask.session)
                .ok()
                .and_then(|v| v.pending)
                .map(|p| p.id);
            let member = grant
                .as_ref()
                .is_some_and(|g| g.members.contains(&ask.session) || g.born.contains(&ask.session));
            let how = if !member {
                Some("withdrawn")
            } else if now >= ask.expires_at {
                Some("expired")
            } else if pending != Some(ask.approval) {
                Some("answered_in_desk")
            } else {
                None
            };
            match how {
                Some(how) => {
                    if store.boss_ask_close(ask.id, how).await.unwrap_or(false) {
                        out.push(closed(ask.id, how));
                    }
                }
                None if again => {
                    let n = nonce();
                    if store
                        .boss_ask_renonce(ask.id, &boss_ask::nonce_hash(&n))
                        .await
                        .unwrap_or(false)
                    {
                        // As it was asked: its class and options are the
                        // kept ones.
                        if let Some(g) = &grant {
                            if let Some(f) = self.ask_frame(
                                g,
                                link,
                                ask.id,
                                ask.session,
                                &n,
                                ask.expires_at,
                                ask.class,
                                &ask.options,
                            ) {
                                out.push(f);
                            }
                        }
                    }
                }
                None => {}
            }
        }
        let Some(g) = grant else {
            return out;
        };
        let party = boss.party_id();
        for session in g.members.iter().chain(g.born.iter()) {
            let Ok(view) = self.desk.view(*session) else {
                continue;
            };
            let Some(p) = view.pending.as_ref() else {
                continue;
            };
            if !p.answerable || p.request.is_none() || open.iter().any(|a| a.approval == p.id) {
                continue;
            }
            // While the boss has it, it is the boss's to answer: once it
            // leaves it, or won't answer, the badge says it waits on the
            // person ([`CrewHub::owed_remote`]), and it goes up.
            let bosses = self
                .desk
                .charge_of(*session)
                .is_some_and(|c| c.live() && c.boss == party);
            if bosses && view.waits_on_you.is_none() {
                continue;
            }
            let (class, _) = self.class_of(&g, &view);
            let options = boss_ask::options(class);
            let n = nonce();
            let hash = boss_ask::nonce_hash(&n);
            let expires_at = now + chrono::Duration::minutes(ASK_MINUTES);
            let made = store
                .boss_ask_open(&NewAsk {
                    link_id: link,
                    group: &g.group,
                    session: *session,
                    approval: p.id,
                    nonce_hash: &hash,
                    class,
                    options: &options,
                    expires_at,
                })
                .await;
            match made {
                Ok(Some(id)) => {
                    if let Some(f) =
                        self.ask_frame(&g, link, id, *session, &n, expires_at, class, &options)
                    {
                        out.push(f);
                    }
                }
                // Asked before on this link: once a request.
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(error = format!("{e:#}"), "an ask couldn't be kept");
                }
            }
        }
        out
    }

    /// Whether the request `view` waits on is answered in Desk only, and
    /// the hard limit it runs into. Fails closed: a request this engine
    /// can't read the whole of (Codex's, a tool it doesn't know) is
    /// answered in Desk.
    fn class_of(
        &self,
        g: &Grant,
        view: &workforce_desk::model::SessionView,
    ) -> (Class, Option<workforce_desk::never::Never>) {
        let Some(p) = view.pending.as_ref() else {
            return (Class::DeskOnly, None);
        };
        // What Codex says it will do isn't what runs.
        if !p.choices.is_empty() {
            return (Class::DeskOnly, None);
        }
        let shipping_needs_desk = g
            .act
            .get("shipping_needs_desk")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let home = dirs_home();
        let cwd = std::path::PathBuf::from(&view.cwd);
        let scratch = [std::path::PathBuf::from("/tmp")];
        let at = workforce_desk::never::Where {
            cwd: Some(&cwd),
            own: Some(&cwd),
            home: &home,
            scratch: &scratch,
        };
        // The tool and what it was given, as the engine's hook said them;
        // the request's label is for people.
        let Some(r) = p.request.as_ref() else {
            return (Class::DeskOnly, None);
        };
        let input = |k: &str| r.input[k].as_str().unwrap_or_default().to_string();
        let (never, ships) = match r.tool.as_str() {
            "Bash" | "shell" | "exec_command" => {
                let command = match input("command") {
                    c if c.is_empty() => p.detail.clone(),
                    c => c,
                };
                (
                    workforce_desk::never::command(&command, &at),
                    boss_ask::ships(&command),
                )
            }
            "Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "Read" => {
                let path = match input("file_path") {
                    f if f.is_empty() => input("notebook_path"),
                    f => f,
                };
                if path.is_empty() {
                    return (Class::DeskOnly, None);
                }
                // What runs on its own later: CI, hooks, an agent's settings.
                if [
                    ".github/",
                    ".git/",
                    ".claude/",
                    ".codex/",
                    ".husky/",
                    ".gitlab-ci",
                ]
                .iter()
                .any(|w| path.contains(w))
                {
                    return (Class::DeskOnly, None);
                }
                (
                    workforce_desk::never::path(std::path::Path::new(&path))
                        .or_else(|| workforce_desk::never::said(&p.detail)),
                    false,
                )
            }
            _ => return (Class::DeskOnly, None),
        };
        (boss_ask::class_of(never, ships, shipping_needs_desk), never)
    }

    /// The `ask` frame for the request `session` waits on, of `class` with
    /// `options`, its words as the grant's Sees shows its question (never
    /// the words of a request for secrets); none when it no longer waits.
    #[allow(clippy::too_many_arguments)]
    fn ask_frame(
        &self,
        g: &Grant,
        link: Uuid,
        ask: Uuid,
        session: Uuid,
        nonce: &str,
        expires_at: chrono::DateTime<Utc>,
        class: Class,
        options: &[Choice],
    ) -> Option<Value> {
        let view = self.desk.view(session).ok()?;
        let p = view.pending.as_ref()?;
        let shows = sees::shows(g.sees, Part::Question);
        let secret = self.class_of(g, &view).1 == Some(workforce_desk::never::Never::Secrets);
        let detail = (shows && !secret).then(|| {
            sees::capped(
                &workforce_common::redact::redact_secrets(&p.detail),
                DETAIL_CHARS,
            )
        });
        let summary = if shows {
            format!(
                "{} asks to use {}",
                view.label(),
                p.request
                    .as_ref()
                    .map_or(p.tool.as_str(), |r| r.tool.as_str())
            )
        } else {
            format!("A session in {} asks for your answer", g.group)
        };
        Some(json!({
            "type": "ask",
            "askId": ask,
            "nonce": nonce,
            "linkId": link,
            "class": class.as_str(),
            "summary": summary,
            "detail": detail,
            "options": options.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
            "expiresAt": expires_at,
            "subject": session,
        }))
    }

    /// Takes `frame`, a `decision` come on `link` (to `boss`, the remote
    /// boss of `group`), whose person the boss hub said is `person`, as
    /// [`boss_ask::accept_remote`] says, once; what goes back: the ask
    /// closed, or why the answer wasn't taken.
    pub(crate) async fn remote_decision(
        self: &Arc<Self>,
        link: Uuid,
        group: &str,
        boss: &BossRef,
        person: Option<&str>,
        frame: &Value,
    ) -> Value {
        let refused = |ask: Value, why: boss_ask::Refusal| json!({"type": "decision_refused", "askId": ask, "code": why.as_str(), "why": why.say()});
        let Ok(d) = serde_json::from_value::<Decision>(frame.clone()) else {
            return refused(frame["askId"].clone(), boss_ask::Refusal::NotAnOption);
        };
        let store = self.desk.store();
        let ask = match store.boss_ask(d.ask_id).await {
            Ok(Some(a)) => a,
            // An ask this engine never made says nothing of others.
            _ => return refused(json!(d.ask_id), boss_ask::Refusal::OtherLink),
        };
        let choice = match boss_ask::accept_remote(&ask, &d, link, person, Utc::now()) {
            Ok(c) => c,
            Err(boss_ask::Refusal::NotOpen) => {
                self.audit_decision(boss, group, &d, "refused", "not_open")
                    .await;
                return closed(ask.id, &ask.state);
            }
            Err(why) => {
                self.audit_decision(boss, group, &d, "refused", why.as_str())
                    .await;
                return refused(json!(ask.id), why);
            }
        };
        // Still the boss's group, and the request still the one asked.
        let live = self.desk.grant_live(group).ok().filter(|g| {
            g.group == ask.group
                && self.serves_remote(g, boss)
                && (g.members.contains(&ask.session) || g.born.contains(&ask.session))
        });
        if live.is_none() {
            if store
                .boss_ask_close(ask.id, "withdrawn")
                .await
                .unwrap_or(false)
            {
                return closed(ask.id, "withdrawn");
            }
            return refused(json!(ask.id), boss_ask::Refusal::NotOpen);
        }
        let pending = self.desk.view(ask.session).ok().and_then(|v| v.pending);
        if pending.as_ref().map(|p| p.id) != Some(ask.approval) {
            let _ = store.boss_ask_close(ask.id, "answered_in_desk").await;
            return closed(ask.id, "answered_in_desk");
        }
        // Taken once: the row is closed as it is taken.
        let hash = boss_ask::nonce_hash(&d.nonce);
        match store.boss_ask_take(ask.id, &hash, choice, &d.decider).await {
            Ok(true) => {}
            Ok(false) => return refused(json!(ask.id), boss_ask::Refusal::NotOpen),
            Err(e) => {
                tracing::warn!(error = format!("{e:#}"), "an answer couldn't be taken");
                return refused(json!(ask.id), boss_ask::Refusal::NotOpen);
            }
        }
        let action = match choice {
            Choice::Allow => Action::Approve {
                session: ask.session,
                approval: ask.approval,
            },
            Choice::Deny => Action::Deny {
                session: ask.session,
                approval: ask.approval,
            },
            Choice::Stop => Action::Stop {
                session: ask.session,
            },
        };
        let mut by = By::you();
        by.reason = Some(format!("answered through {}", boss_name_of(boss)));
        let done = match choice {
            Choice::Stop => {
                // Refused first, so nothing it asked runs as it stops.
                let deny = Action::Deny {
                    session: ask.session,
                    approval: ask.approval,
                };
                let _ = self.perform(&deny, by.clone()).await;
                self.perform(&action, by).await
            }
            _ => self.perform(&action, by).await,
        };
        let outcome = if done.is_ok() { "ok" } else { "refused" };
        self.audit_decision(boss, group, &d, outcome, choice.as_str())
            .await;
        if let Err(why) = done {
            // Taken, so never again from elsewhere: the request stays open
            // in Desk, and the card says to answer it there.
            tracing::info!(ask = %ask.id, %why, "a remote answer was taken but couldn't be applied");
            return refused(json!(ask.id), boss_ask::Refusal::NotApplied);
        }
        json!({"type": "ask_closed", "askId": ask.id, "how": "decided", "choice": choice.as_str()})
    }

    /// Closes the ask on `link` for `approval`, the boss having answered
    /// it itself.
    pub(crate) async fn answered_by_boss(&self, link: Option<&str>, approval: Uuid) {
        let Some(link) = link.and_then(|l| l.parse::<Uuid>().ok()) else {
            return;
        };
        let store = self.desk.store();
        if let Ok(open) = store.boss_asks_open(link).await {
            for a in open.iter().filter(|a| a.approval == approval) {
                let _ = store.boss_ask_close(a.id, "answered_by_boss").await;
            }
        }
    }

    async fn audit_decision(
        &self,
        boss: &BossRef,
        group: &str,
        d: &Decision,
        outcome: &'static str,
        said: &str,
    ) {
        let row = CallRow {
            caller: format!("boss:{}", boss.party_id()),
            boss: Some(boss.key()),
            group: Some(group.to_string()),
            tool: "boss.decision",
            input: json!({"askId": d.ask_id, "choice": d.choice, "via": d.decider.via}).to_string(),
            input_hash: boss_ask::nonce_hash(&d.ask_id.to_string()),
            outcome,
            said: Some(said.to_string()),
            result: json!({}),
        };
        if let Err(e) = self.desk.store().boss_call(&row).await {
            tracing::warn!(error = format!("{e:#}"), "a remote answer wasn't audited");
        }
    }
}

fn boss_name_of(boss: &BossRef) -> String {
    match boss {
        BossRef::Remote { hub, agent } => format!("{agent} ({hub})"),
        BossRef::Local { agent } => agent.clone(),
    }
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/"))
}
