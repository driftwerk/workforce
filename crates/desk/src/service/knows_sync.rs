//! What to send an agent now (Sees K3, `designs/agent-sees.md` §3 and §4
//! in workforce-desk): of what its rule covers, what it doesn't hold yet,
//! or holds as it was before it changed. Sending is the hub's; recording
//! what was sent is [`Desk::knows_mark_sent`].

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use super::{Desk, DeskError};
use crate::knows::{self, Rule};
use crate::knows_collect::{Scope, Turn};
use crate::knows_items::{self as items, Outgoing, Where, ACCOUNT, DECISION, NEED};
use crate::knows_store::Sent;
use crate::memory::Redactor;

fn internal(e: eyre::Report) -> DeskError {
    DeskError::Internal(e)
}

/// The most hops between sessions Desk follows back to what a message
/// carried in, and the most earlier exchanges of a session it looks at.
const HOPS: usize = 4;
const EARLIER: i64 = 20;

/// The fence Desk and the hub put around outside content.
fn fenced(text: &str) -> bool {
    text.contains(&format!("<<<{}", crate::memory::render::MARKER))
}

/// How much one call gathers, and how far back.
#[derive(Debug, Clone, Copy)]
pub struct Gather {
    pub items: usize,
    pub bytes: usize,
    pub backfill: Duration,
}

/// Whether `out` still fits under `cap`, counting `o`.
fn fits(out: &[Outgoing], bytes: usize, o: &Outgoing, cap: &Gather) -> bool {
    out.len() < cap.items && (out.is_empty() || bytes + o.bytes() <= cap.bytes)
}

impl Desk {
    /// What `agent`, under `rule`, should be sent now, every string
    /// redacted by `redactor`: at most `cap.items` items and about
    /// `cap.bytes`, accounts, needs and decisions first, then board activity,
    /// then (with transcripts on) exchanges, oldest first.
    pub async fn knows_pending(
        &self,
        agent: &str,
        rule: &Rule,
        cap: &Gather,
        redactor: &dyn Redactor,
        home: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Vec<Outgoing>, DeskError> {
        let store = self.store();
        let world = self.knows_world().await?;
        let covered = knows::resolve(agent, rule, &world);
        let groups_off: HashSet<String> = store
            .knows_groups_off()
            .await
            .map_err(internal)?
            .into_iter()
            .collect();
        let names: HashMap<&str, &str> = world
            .layout
            .groups
            .iter()
            .map(|g| (g.id.as_str(), g.name.as_str()))
            .collect();
        let mut place_of: HashMap<Uuid, Where> = HashMap::new();
        // A session in any group whose harvest is off is off, whatever
        // other group also has it.
        let mut session_off: HashSet<Uuid> = HashSet::new();
        for g in &world.layout.groups {
            for item in &g.items {
                if let Some(id) = item.strip_prefix("session:").and_then(|s| s.parse().ok()) {
                    if groups_off.contains(&g.id) {
                        session_off.insert(id);
                    }
                    place_of.insert(
                        id,
                        Where {
                            group: Some(g.id.clone()),
                            group_name: Some(g.name.clone()),
                        },
                    );
                }
            }
        }
        let place = |s: Option<Uuid>| {
            s.and_then(|s| place_of.get(&s).cloned())
                .unwrap_or_default()
        };
        let named = |g: Option<&str>| Where {
            group: g.map(str::to_string),
            group_name: g.and_then(|g| names.get(g)).map(|n| n.to_string()),
        };
        // Of a group whose harvest is off, nothing.
        let off = |w: &Where| w.group.as_ref().is_some_and(|g| groups_off.contains(g));
        let off_session = |s: Option<Uuid>| s.is_some_and(|s| session_off.contains(&s));
        let since = now - cap.backfill;

        let states = store
            .knows_sent_states(agent, &[ACCOUNT, NEED, DECISION])
            .await
            .map_err(internal)?;
        // A changed item, or one never sent, or sent and deleted; never one
        // being deleted.
        let changed =
            |o: &Outgoing| match states.get(&(o.held.kind.clone(), o.held.reference.clone())) {
                None => true,
                Some(s) if s.deleting => false,
                Some(s) => s.deleted || s.hash != o.hash,
            };

        let mut out: Vec<Outgoing> = Vec::new();
        let mut bytes = 0usize;
        let mut push = |out: &mut Vec<Outgoing>, o: Outgoing| -> bool {
            if !fits(out, bytes, &o, cap) {
                return false;
            }
            bytes += o.bytes();
            out.push(o);
            true
        };

        for a in store.knows_accounts().await.map_err(internal)? {
            let w = place(Some(a.session));
            if off(&w)
                || off_session(Some(a.session))
                || !covered.covers(Some(a.session), w.group.as_deref(), None, None)
            {
                continue;
            }
            let o = items::account(&a, &w, home, redactor);
            if changed(&o) && !push(&mut out, o) {
                return Ok(out);
            }
        }
        for n in store.knows_needs(since).await.map_err(internal)? {
            let w = place(n.session);
            // Every session it is about must be covered and on, not only
            // the one it names.
            let about_all = n.sessions.iter().all(|s| {
                !off_session(Some(*s))
                    && !off(&place(Some(*s)))
                    && covered.covers(Some(*s), place(Some(*s)).group.as_deref(), None, None)
            });
            if off(&w)
                || off_session(n.session)
                || !about_all
                || !covered.covers(n.session, w.group.as_deref(), None, Some(n.boss.as_str()))
            {
                continue;
            }
            let o = items::need(&n, &n.boss, &w, redactor);
            if changed(&o) && !push(&mut out, o) {
                return Ok(out);
            }
        }
        for d in store.knows_decisions(since).await.map_err(internal)? {
            let w = place(d.session);
            if off(&w)
                || off_session(d.session)
                || !covered.covers(d.session, w.group.as_deref(), None, Some(d.boss.as_str()))
            {
                continue;
            }
            let o = items::decision(&d, &d.boss, &w, redactor);
            if changed(&o) && !push(&mut out, o) {
                return Ok(out);
            }
        }

        let scope = Scope {
            everything: covered.everything,
            sessions: covered.sessions.iter().copied().collect(),
            boards: covered.boards.iter().copied().collect(),
            groups: covered.groups.iter().cloned().collect(),
            groups_off: groups_off.iter().cloned().collect(),
            sessions_off: session_off.iter().copied().collect(),
        };
        let room = |out: &Vec<Outgoing>| i64::try_from(cap.items - out.len()).unwrap_or(0);
        if room(&out) > 0 {
            for b in store
                .knows_board_activity(agent, &scope, since, room(&out))
                .await
                .map_err(internal)?
            {
                let o = items::board(&b, &named(b.group.as_deref()), redactor);
                if !push(&mut out, o) {
                    return Ok(out);
                }
            }
        }
        if rule.transcripts && room(&out) > 0 {
            let mut judged = HashMap::new();
            for mut e in store
                .knows_exchanges(agent, &scope, since, room(&out))
                .await
                .map_err(internal)?
            {
                e.outside = self
                    .exchange_outside(&Turn::from(&e), 0, &mut judged)
                    .await
                    .map_err(internal)?;
                // Its place now, else where it was harvested.
                let w = place_of
                    .get(&e.session)
                    .cloned()
                    .unwrap_or_else(|| named(e.group.as_deref()));
                let o = items::exchange(&e, &w, redactor);
                if !push(&mut out, o) {
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }

    /// Why exchange `t` may carry outside content, each reason once; none
    /// when it is the person's own work. What it did (a step or command
    /// reading outside content), what it was asked: a message on Desk's
    /// line carrying some in (a boss's turn that had read some, a ticket
    /// others wrote, a session whose exchange that sent it carried some),
    /// a first prompt that did, or text fenced as outside content; and an
    /// engine's own prompt (a wake-up, a compaction) carries in what an
    /// earlier exchange of its session did. `depth` hops are followed so
    /// far; `judged` keeps what was judged.
    pub(crate) fn exchange_outside<'a>(
        &'a self,
        t: &'a Turn,
        depth: usize,
        judged: &'a mut HashMap<String, Vec<String>>,
    ) -> futures::future::BoxFuture<'a, eyre::Result<Vec<String>>> {
        Box::pin(async move {
            if let Some(known) = judged.get(&t.reference) {
                return Ok(known.clone());
            }
            let store = self.store();
            let mut why = crate::knows_outside::read_outside(&t.steps, &t.commands);
            let earlier = store.turns_before(t.session, t.at, EARLIER).await?;
            let delivered = match crate::line::is_delivery(&t.ask) {
                Some(mark) => Some(store.delivered(t.session, &mark).await?),
                None => None,
            };
            let start = match earlier.is_empty() {
                true => store.session_start(t.session).await?,
                false => None,
            };
            match (delivered, start) {
                (Some(Some(d)), _) => {
                    why.extend(d.outside);
                    if d.from.kind == crate::crew::PartyKind::Session {
                        let sent = match (depth < HOPS, d.from.id.parse::<Uuid>()) {
                            (true, Ok(from)) => store.turns_before(from, d.at, 1).await?,
                            _ => vec![],
                        };
                        let carried = match sent.first() {
                            Some(s) => !self
                                .exchange_outside(s, depth + 1, judged)
                                .await?
                                .is_empty(),
                            None => true,
                        };
                        if carried {
                            why.push(format!(
                                "a message from {} whose work may carry outside content",
                                d.from.name
                            ));
                        }
                    }
                }
                (Some(None), _) => why.push("a message Desk no longer holds".into()),
                (None, Some(started)) => why.extend(started),
                (None, None) if fenced(&t.ask) => {
                    why.push("a prompt fenced as outside content".into())
                }
                (None, None) => {}
            }
            if t.asker == "unknown" && !earlier.is_empty() {
                let mut carried = depth >= HOPS;
                for e in &earlier {
                    if carried {
                        break;
                    }
                    carried = !self
                        .exchange_outside(e, depth + 1, judged)
                        .await?
                        .is_empty();
                }
                if carried {
                    why.push("an earlier exchange of its session may carry outside content".into());
                }
            }
            let why = why.into_iter().fold(Vec::new(), |mut said, w| {
                if !said.contains(&w) {
                    said.push(w);
                }
                said
            });
            judged.insert(t.reference.clone(), why.clone());
            Ok(why)
        })
    }

    /// The bytes sent to `agent` in the day up to `now`, for its limit
    /// (kept across restarts, by the hour: never less than was sent).
    pub async fn knows_spent_day(&self, agent: &str, now: DateTime<Utc>) -> Result<u64, DeskError> {
        let bytes = self
            .store()
            .knows_spent_day(agent, now)
            .await
            .map_err(internal)?;
        Ok(u64::try_from(bytes).unwrap_or(0))
    }

    /// `items` and `bytes` were sent to `agent` at `now`, counted for its
    /// day's limit.
    pub async fn knows_spend(
        &self,
        agent: &str,
        now: DateTime<Utc>,
        items: usize,
        bytes: usize,
    ) -> Result<(), DeskError> {
        self.store()
            .knows_spent_add(
                agent,
                now,
                i64::try_from(items).unwrap_or(i64::MAX),
                i64::try_from(bytes).unwrap_or(i64::MAX),
            )
            .await
            .map_err(internal)
    }

    /// `agent`'s hub took `sent`: recorded, so it isn't sent again and a
    /// narrowing can offer to delete it.
    pub async fn knows_mark_sent(
        &self,
        agent: &str,
        sent: &[Outgoing],
        now: DateTime<Utc>,
    ) -> Result<(), DeskError> {
        let rows: Vec<Sent> = sent
            .iter()
            .map(|o| Sent {
                held: o.held.clone(),
                bytes: i32::try_from(o.bytes()).unwrap_or(i32::MAX),
                hash: o.hash.clone(),
            })
            .collect();
        self.store()
            .knows_sent_put(agent, &rows, now)
            .await
            .map_err(internal)?;
        // A session forgotten while this was on its way: taken back too.
        self.store().knows_forget_late().await.map_err(internal)
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::collections::BTreeMap;

    use tokio_util::sync::CancellationToken;
    use workforce_postgres::test_support::with_test_pool;

    use super::*;
    use crate::knows::{Digest, TRANSCRIPT};
    use crate::knows_items::BOARD;
    use crate::memory::Redacted;
    use crate::service::{Group, Layout};
    use crate::store::{build_pool, DeskStore};

    /// Hides `hunter2`, as the hub's patterns hide a secret.
    struct Hide;
    impl Redactor for Hide {
        fn stored<'a>(&self, text: &'a str) -> Redacted<'a> {
            match text.contains("hunter2") {
                true => Redacted {
                    text: Cow::Owned(text.replace("hunter2", "[REDACTED PASSWORD]")),
                    count: 1,
                },
                false => Redacted {
                    text: Cow::Borrowed(text),
                    count: 0,
                },
            }
        }
    }

    const CAP: Gather = Gather {
        items: 256,
        bytes: 1024 * 1024,
        backfill: Duration::days(14),
    };

    async fn session(store: &DeskStore, id: Uuid, name: &str, memory: &str) {
        let client = store.pool().get().await.unwrap();
        client
            .execute(
                "INSERT INTO sessions (id, engine, name, cwd, origin, state, approvals, memory, \
                   created_at, updated_at, state_at) \
                 VALUES ($1, 'claude', $2, '/Users/me/dev/x', 'desk', 'stopped', 'desk', $3, now(), now(), now())",
                &[&id, &name, &memory],
            )
            .await
            .unwrap();
        client
            .execute(
                "INSERT INTO session_accounts (session, purpose, now, written_at) \
                 VALUES ($1, $2, 'testing with hunter2', now())",
                &[&id, &format!("{name}'s purpose")],
            )
            .await
            .unwrap();
        client
            .execute(
                "INSERT INTO memory_exchanges (id, engine, key, session, group_id, project, folder, \
                   seq, at, ended, asker, ask, answer, steps) \
                 VALUES ($1, 'claude', $2, $3, NULL, 'x', '/x', 1, now(), now(), 'person', \
                   'what of ' || $4, 'the password is hunter2', '[]')",
                &[&Uuid::new_v4(), &format!("k-{name}"), &id, &name],
            )
            .await
            .unwrap();
    }

    async fn desk(pool: workforce_postgres::PostgresPool, dir: &std::path::Path) -> Desk {
        Desk::start(
            DeskStore::from_pool(pool),
            crate::service::Options {
                programs: crate::testing::programs("/bin/false".into(), "/bin/false".into()),
                env: BTreeMap::new(),
                run_dir: dir.join("run"),
                scrollback_bytes: 1024,
                approval_timeout: std::time::Duration::from_secs(30),
                restore: crate::service::RestorePolicy::Off,
                adopt_every: None,
                holder: None,
                hub_internal: Default::default(),
                account_keys: None,
                line_reach: crate::line::Reach::Group,
                memory: Default::default(),
                bosses: Default::default(),
                sight: Default::default(),
            },
            Default::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap()
    }

    fn refs(out: &[Outgoing]) -> Vec<(String, String)> {
        let mut r: Vec<(String, String)> = out
            .iter()
            .map(|o| (o.held.kind.clone(), o.held.reference.clone()))
            .collect();
        r.sort();
        r
    }

    /// One exchange of `session`, `mins` minutes ago.
    #[allow(clippy::too_many_arguments)]
    async fn turn(
        store: &DeskStore,
        session: Uuid,
        key: &str,
        mins: i64,
        asker: &str,
        ask: &str,
        steps: serde_json::Value,
    ) {
        let at = Utc::now() - Duration::minutes(mins);
        store
            .pool()
            .get()
            .await
            .unwrap()
            .execute(
                "INSERT INTO memory_exchanges (id, engine, key, session, group_id, project, folder, \
                   seq, at, ended, asker, ask, answer, steps) \
                 VALUES ($1, 'claude', $2, $3, NULL, 'x', '/x', $4, $5, $5, $6, $7, 'done', $8)",
                &[&Uuid::new_v4(), &key, &session, &(100 - mins as i32), &at, &asker, &ask, &steps],
            )
            .await
            .unwrap();
    }

    /// A message on Desk's line to `to`, from `from`, `mins` minutes ago,
    /// carrying `outside` in: what its exchange is asked with.
    async fn told(
        store: &DeskStore,
        from: crate::crew::Party,
        to: Uuid,
        mins: i64,
        outside: Option<&str>,
    ) -> String {
        let to_party = crate::crew::Party {
            kind: crate::crew::PartyKind::Session,
            id: to.to_string(),
            name: "to".into(),
            engine: None,
            origin: None,
        };
        let mut m = crate::line::LineMessage::new(
            from,
            to_party,
            crate::line::LineKind::Tell,
            "Go on.".into(),
            1,
        );
        m.at = Utc::now() - Duration::minutes(mins);
        m.outside = outside.map(str::to_string);
        store.put_line(&m).await.unwrap();
        format!("{} Sarah tells you: Go on.", crate::line::marker(m.id))
    }

    /// What Desk says each exchange carries in from outside, by its key.
    async fn judged(d: &Desk, sessions: &[Uuid]) -> BTreeMap<String, serde_json::Value> {
        let places: Vec<String> = sessions.iter().map(|s| format!("session:{s}")).collect();
        let rule = Rule::new(&places, true, Digest::Off).unwrap();
        d.knows_pending("patchwork/lattice", &rule, &CAP, &Hide, None, Utc::now())
            .await
            .unwrap()
            .into_iter()
            .filter(|o| o.held.kind == TRANSCRIPT)
            .map(|o| (o.held.reference, o.body["outside"].clone()))
            .collect()
    }

    /// Boss-run work on the person's own tickets carries nothing in; work
    /// started on a ticket others wrote, that read the web, that a boss's
    /// turn sent after reading outside content, or that a session passed on
    /// from work that did, carries that in, and so does an engine's own
    /// prompt after it.
    #[tokio::test]
    async fn the_outside_content_an_exchange_carries_in_is_followed_to_where_it_came_from() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::with_pool(pool.clone()).await.unwrap();
            let ids: Vec<Uuid> = (10..16).map(Uuid::from_u128).collect();
            let [own, theirs, fetched, armed, sender, passed] = ids[..] else {
                unreachable!()
            };
            for (id, name) in ids
                .iter()
                .zip(["own", "theirs", "fetched", "armed", "sender", "passed"])
            {
                session(&store, *id, name, "on").await;
            }
            let boss = crate::crew::Party::manager("sarah", "Sarah");
            let none = serde_json::json!([]);

            // The person's own tickets, run by their boss.
            store.put_session_start(own, None).await.unwrap();
            turn(
                &store,
                own,
                "own-1",
                50,
                "person",
                "ENG-1 is yours: Fix the login",
                none.clone(),
            )
            .await;
            let steer = told(&store, boss.clone(), own, 40, None).await;
            turn(&store, own, "own-2", 39, "desk", &steer, none.clone()).await;
            turn(
                &store,
                own,
                "own-3",
                30,
                "unknown",
                "Continue from where you left off.",
                none.clone(),
            )
            .await;

            // A ticket a teammate wrote.
            store
                .put_session_start(
                    theirs,
                    Some("ENG-2, a Linear issue not all the person's words"),
                )
                .await
                .unwrap();
            turn(
                &store,
                theirs,
                "theirs-1",
                50,
                "person",
                "ENG-2 is yours",
                none.clone(),
            )
            .await;

            // A page read, and an engine's prompt after it.
            turn(
                &store,
                fetched,
                "fetched-1",
                50,
                "person",
                "read the docs",
                serde_json::json!([{"tool": "WebFetch", "input": "https://docs.example"}]),
            )
            .await;
            turn(
                &store,
                fetched,
                "fetched-2",
                40,
                "unknown",
                "Continue from where you left off.",
                none.clone(),
            )
            .await;

            // A boss's turn that had read the web.
            let rushed = told(&store, boss.clone(), armed, 40, Some("web.fetch")).await;
            turn(&store, armed, "armed-1", 39, "desk", &rushed, none.clone()).await;

            // A session passing on what its web read said.
            turn(
                &store,
                sender,
                "sender-1",
                50,
                "person",
                "find out",
                serde_json::json!([{"tool": "WebSearch", "input": "rotate keys"}]),
            )
            .await;
            let from = crate::crew::Party {
                kind: crate::crew::PartyKind::Session,
                id: sender.to_string(),
                name: "sender".into(),
                engine: None,
                origin: None,
            };
            let hop = told(&store, from, passed, 45, None).await;
            turn(
                &store,
                passed,
                "passed-1",
                44,
                "session",
                &hop,
                none.clone(),
            )
            .await;

            // Prompts an agent typed in: one whose turn had read the web.
            let typed = |outside: Option<&'static str>, to: Uuid, mins: i64| {
                let store = &store;
                async move {
                    let mark = Uuid::new_v4();
                    store
                        .put_typed_prompt(
                            mark,
                            to,
                            &crate::crew::Party::agent("scout", "Scout"),
                            outside,
                            Utc::now() - Duration::minutes(mins),
                        )
                        .await
                        .unwrap();
                    format!("{} run the deploy", crate::line::marker(mark))
                }
            };
            let calm = typed(None, own, 20).await;
            turn(&store, own, "own-4", 19, "desk", &calm, none.clone()).await;
            let rushed_in = typed(Some("web.fetch"), armed, 20).await;
            turn(
                &store,
                armed,
                "armed-2",
                19,
                "desk",
                &rushed_in,
                none.clone(),
            )
            .await;

            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let said = judged(&d, &ids).await;
            let says = |key: &str| said[&format!("claude:{key}")].clone();
            for key in ["own-1", "own-2", "own-3", "own-4", "k-own"] {
                assert_eq!(says(key), none, "{key}");
            }
            assert_eq!(
                says("theirs-1"),
                serde_json::json!(["ENG-2, a Linear issue not all the person's words"])
            );
            assert_eq!(says("fetched-1"), serde_json::json!(["WebFetch"]));
            assert_eq!(
                says("fetched-2"),
                serde_json::json!(["an earlier exchange of its session may carry outside content"])
            );
            assert_eq!(says("armed-1"), serde_json::json!(["web.fetch"]));
            assert_eq!(says("armed-2"), serde_json::json!(["web.fetch"]));
            assert_eq!(
                says("passed-1"),
                serde_json::json!(["a message from sender whose work may carry outside content"])
            );

            // What a boss says carries in what its turn has read, as its hub
            // says, unless it says what it carries itself.
            let by = crate::crew::By::manager("sarah", "Sarah", "");
            assert_eq!(d.carried_by(&by), None);
            d.read_outside_with(std::sync::Arc::new(|| Some("web.fetch".into())));
            assert_eq!(d.carried_by(&by).as_deref(), Some("web.fetch"));
            let handing = crate::crew::By {
                outside: Some("ENG-2".into()),
                ..by
            };
            assert_eq!(d.carried_by(&handing).as_deref(), Some("ENG-2"));
        })
        .await;
    }

    /// An exchange sent before Desk said what it carries in goes once more,
    /// saying it, and then not again.
    #[tokio::test]
    async fn an_exchange_sent_before_desk_said_what_it_carries_goes_once_more() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::with_pool(pool.clone()).await.unwrap();
            let s = Uuid::from_u128(30);
            session(&store, s, "atlas", "on").await;
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool.clone(), dir.path()).await;
            let rule = Rule::new(&[format!("session:{s}")], true, Digest::Off).unwrap();
            let pending = || async {
                d.knows_pending("patchwork/lattice", &rule, &CAP, &Hide, None, Utc::now())
                    .await
                    .unwrap()
                    .into_iter()
                    .filter(|o| o.held.kind == TRANSCRIPT)
                    .collect::<Vec<_>>()
            };
            let sent = pending().await;
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].body["outside"], serde_json::json!([]));
            d.knows_mark_sent("patchwork/lattice", &sent, Utc::now())
                .await
                .unwrap();
            assert!(pending().await.is_empty());
            // As it was sent before Desk said.
            pool.get()
                .await
                .unwrap()
                .execute("UPDATE knows_sent SET outside_said = false", &[])
                .await
                .unwrap();
            let again = pending().await;
            assert_eq!(again.len(), 1, "sent once more, saying it");
            d.knows_mark_sent("patchwork/lattice", &again, Utc::now())
                .await
                .unwrap();
            assert!(pending().await.is_empty(), "and then not again");
        })
        .await;
    }

    #[tokio::test]
    async fn an_agent_is_sent_only_what_its_rule_covers_redacted_once_and_again_when_changed() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::with_pool(pool.clone()).await.unwrap();
            let (pw, acme, off) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
            session(&store, pw, "atlas", "on").await;
            session(&store, acme, "acme-billing", "on").await;
            session(&store, off, "private", "off").await;
            store
                .put_layout(&Layout {
                    groups: vec![
                        Group {
                            id: "patchwork".into(),
                            name: "Patchwork".into(),
                            items: vec![format!("session:{pw}"), format!("session:{off}")],
                            ..Group::default()
                        },
                        Group {
                            id: "acme".into(),
                            name: "Acme".into(),
                            items: vec![format!("session:{acme}")],
                            ..Group::default()
                        },
                    ],
                    ..Layout::default()
                })
                .await
                .unwrap();
            let dir = tempfile::tempdir().unwrap();
            let d = desk(pool, dir.path()).await;
            let now = Utc::now();
            let lattice = "patchwork/lattice";

            // The Patchwork group, transcripts on: atlas alone, never Acme,
            // never a session whose memory is off.
            let rule = Rule::new(&["group:patchwork".into()], true, Digest::Off).unwrap();
            let out = d
                .knows_pending(lattice, &rule, &CAP, &Hide, Some("/Users/me"), now)
                .await
                .unwrap();
            assert_eq!(
                refs(&out),
                vec![
                    ("account".to_string(), pw.to_string()),
                    (TRANSCRIPT.to_string(), "claude:k-atlas".to_string()),
                ]
            );
            let all = serde_json::to_string(&out.iter().map(|o| &o.body).collect::<Vec<_>>()).unwrap();
            assert!(!all.contains("hunter2"), "{all}");
            assert!(!all.contains("acme") && !all.contains("Acme"), "{all}");
            assert!(!all.contains("private"), "{all}");
            assert_eq!(out[0].group_name.as_deref(), Some("Patchwork"));
            assert_eq!(out[0].body["folder"], "~/dev/x");

            // Sent: nothing more, until the account changes.
            d.knows_mark_sent(lattice, &out, now).await.unwrap();
            let again = d
                .knows_pending(lattice, &rule, &CAP, &Hide, Some("/Users/me"), now)
                .await
                .unwrap();
            assert!(again.is_empty(), "{:?}", refs(&again));
            store
                .pool()
                .get()
                .await
                .unwrap()
                .execute(
                    "UPDATE session_accounts SET now = 'reviewing', written_at = now() WHERE session = $1",
                    &[&pw],
                )
                .await
                .unwrap();
            let changed = d
                .knows_pending(lattice, &rule, &CAP, &Hide, Some("/Users/me"), now)
                .await
                .unwrap();
            assert_eq!(refs(&changed), vec![("account".to_string(), pw.to_string())]);

            // Transcripts off: only the account; everything: Acme too, and
            // still never the session whose memory is off.
            let summaries = Rule::new(&[], false, Digest::Off).unwrap();
            let allan = "driftwerk/allan";
            let out = d
                .knows_pending(allan, &summaries, &CAP, &Hide, None, now)
                .await
                .unwrap();
            assert!(out.is_empty(), "own work alone, and Allan runs nothing");
            // A boss's question naming no one session, about the private one
            // among others: never sent.
            store
                .pool()
                .get()
                .await
                .unwrap()
                .execute(
                    "INSERT INTO boss_open_items (id, boss, kind, session, sessions, text, dedupe_key, \
                       opened_at, changed_at) \
                     VALUES ($1, 'allan', 'ask', NULL, $2, 'about private and atlas', 'k', now(), now())",
                    &[&Uuid::from_u128(9), &vec![off, acme]],
                )
                .await
                .unwrap();
            let everything = Rule::new(&["all".into()], false, Digest::Live).unwrap();
            let out = d
                .knows_pending(allan, &everything, &CAP, &Hide, None, now)
                .await
                .unwrap();
            assert_eq!(
                refs(&out),
                vec![
                    ("account".to_string(), pw.to_string()),
                    ("account".to_string(), acme.to_string()),
                ]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
            );
            assert!(out.iter().all(|o| o.held.kind != TRANSCRIPT && o.held.kind != BOARD));

            // A cap of one: one at a time, the rest next time.
            let one = Gather { items: 1, ..CAP };
            let first = d
                .knows_pending(allan, &everything, &one, &Hide, None, now)
                .await
                .unwrap();
            assert_eq!(first.len(), 1);

            // Forgetting atlas takes it back from Lattice, without asking.
            d.knows_mark_sent(lattice, &changed, now).await.unwrap();
            store.forget_memory(pw).await.unwrap();
            let deletes = store.knows_pending_deletes().await.unwrap();
            assert_eq!(deletes.len(), 1);
            assert_eq!(deletes[0].0, lattice);
            assert_eq!(deletes[0].1.detail["why"], "forgotten");
            let deleting = store.knows_deleting(lattice, deletes[0].1.id).await.unwrap();
            assert_eq!(deleting.len(), 2, "its account and its exchange");
            // And nothing of it is sent while that is pending.
            let out = d
                .knows_pending(lattice, &rule, &CAP, &Hide, None, now)
                .await
                .unwrap();
            assert!(out.is_empty(), "{:?}", refs(&out));
        })
        .await;
    }
}
