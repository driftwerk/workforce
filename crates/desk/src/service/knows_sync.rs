//! What to send an agent now (Sees K3, `designs/agent-sees.md` §3 and §4
//! in workforce-desk): of what its rule covers, what it doesn't hold yet,
//! or holds as it was before it changed. Sending is the hub's; recording
//! what was sent is [`Desk::knows_mark_sent`].

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use super::{Desk, DeskError};
use crate::knows::{self, Rule};
use crate::knows_collect::Scope;
use crate::knows_items::{self as items, Outgoing, Where, ACCOUNT, DECISION, NEED};
use crate::knows_store::Sent;
use crate::memory::Redactor;

fn internal(e: eyre::Report) -> DeskError {
    DeskError::Internal(e)
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
            for e in store
                .knows_exchanges(agent, &scope, since, room(&out))
                .await
                .map_err(internal)?
            {
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
