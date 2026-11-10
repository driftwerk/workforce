//! The kill switch, grants and activity in Postgres (`desk` schema, V020).
//!
//! `arguments` reach here already tokenised by the caller
//! (`redact::tokenize_secrets` on the hub): nothing in this crate sees a
//! secret to strip. `person` and `arguments` are personal, erased with the
//! person by [`DeskStore::erase_access_person`].

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use eyre::{eyre, Result};
use serde::Serialize;
use serde_json::Value;
use tokio_postgres::Row;
use uuid::Uuid;
use workforce_core::computer::Ability;

use super::policy::{Grant, RunOf};
use crate::store::DeskStore;

/// How long activity is kept.
pub const ACTIVITY_KEPT_DAYS: i64 = 90;
/// Most rows one page of activity holds.
pub const PAGE: i64 = 200;

/// A grant to keep, as an answer from Desk gives it.
#[derive(Debug, Clone, PartialEq)]
pub struct NewGrant {
    pub hub: String,
    pub agent: String,
    pub person: String,
    pub ability: Ability,
    pub folder: Option<PathBuf>,
    pub connector: Option<String>,
    pub session_id: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub run: Option<RunOf>,
    /// The job's name, as Settings shows the grant.
    pub run_name: Option<String>,
}

/// A grant as Settings lists it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KeptGrant {
    pub grant: Grant,
    pub run_name: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// One decided call, to record.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Activity {
    pub hub: String,
    pub agent: String,
    pub person: Option<String>,
    pub session_id: Option<String>,
    pub request_id: Option<String>,
    /// read | write | delete | exec | session | connector
    pub kind: String,
    /// What it did, as the person reads it: a path, a command, "Linear ·
    /// create issue".
    pub doing: String,
    pub folder: Option<String>,
    pub connector: Option<String>,
    pub tool: String,
    pub vault_source: Option<String>,
    /// rule | grant | session | person | classifier
    pub decided_by: String,
    pub outcome: Value,
    pub reason: Option<String>,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<i32>,
    /// What the call was given, as the gate summarised it: its tool, its
    /// targets and a short redacted line, never file contents.
    pub arguments: Value,
    /// The paired device whose answer decided it, where a person answered.
    pub device: Option<String>,
}

/// A recorded call.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ActivityRow {
    pub id: Uuid,
    pub at: DateTime<Utc>,
    #[serde(flatten)]
    pub activity: Activity,
}

/// Which activity a page is of, and where it starts: after `before`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActivityFilter {
    pub agent: Option<String>,
    pub kind: Option<String>,
    pub before: Option<(DateTime<Utc>, Uuid)>,
}

const GRANT_COLUMNS: &str = "id, hub, agent, person, ability, folder, connector, session_id, \
     run_kind, run_id, run_name, expires_at, created_at";

fn kept_grant(row: &Row) -> Result<KeptGrant> {
    let ability: String = row.try_get("ability")?;
    let run = match (
        row.try_get::<_, Option<String>>("run_kind")?,
        row.try_get::<_, Option<String>>("run_id")?,
    ) {
        (Some(kind), Some(id)) => Some(RunOf { kind, id }),
        _ => None,
    };
    Ok(KeptGrant {
        grant: Grant {
            id: row.try_get("id")?,
            hub: row.try_get("hub")?,
            agent: row.try_get("agent")?,
            person: row.try_get("person")?,
            ability: Ability::parse(&ability).ok_or_else(|| eyre!("unknown ability {ability}"))?,
            folder: row
                .try_get::<_, Option<String>>("folder")?
                .map(PathBuf::from),
            connector: row.try_get("connector")?,
            session_id: row.try_get("session_id")?,
            run,
        },
        run_name: row.try_get("run_name")?,
        expires_at: row.try_get("expires_at")?,
        created_at: row.try_get("created_at")?,
    })
}

fn activity_row(row: &Row) -> Result<ActivityRow> {
    Ok(ActivityRow {
        id: row.try_get("id")?,
        at: row.try_get("at")?,
        activity: Activity {
            hub: row.try_get("hub")?,
            agent: row.try_get("agent")?,
            person: row.try_get("person")?,
            session_id: row.try_get("session_id")?,
            request_id: row.try_get("request_id")?,
            kind: row.try_get("kind")?,
            doing: row.try_get("doing")?,
            folder: row.try_get("folder")?,
            connector: row.try_get("connector")?,
            tool: row.try_get("tool")?,
            vault_source: row.try_get("vault_source")?,
            decided_by: row.try_get("decided_by")?,
            outcome: row.try_get("outcome")?,
            reason: row.try_get("reason")?,
            exit_code: row.try_get("exit_code")?,
            duration_ms: row.try_get("duration_ms")?,
            arguments: row.try_get("arguments")?,
            device: row.try_get("device")?,
        },
    })
}

fn path_text(path: &Option<PathBuf>) -> Option<String> {
    path.as_ref().map(|p| p.to_string_lossy().into_owned())
}

impl DeskStore {
    /// Which state of the kill switch and grants is current: changes with
    /// every write to them through this store.
    pub fn access_version(&self) -> u64 {
        self.access_version
            .load(std::sync::atomic::Ordering::Acquire)
    }

    fn access_changed(&self) {
        self.access_version
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    /// Whether the person stopped all access to this computer. No row yet
    /// is not stopped.
    pub async fn access_stopped(&self) -> Result<bool> {
        let client = self.pool().get().await?;
        Ok(client
            .query_opt("SELECT stopped FROM access_settings WHERE id", &[])
            .await?
            .map(|r| r.try_get(0))
            .transpose()?
            .unwrap_or(false))
    }

    pub async fn set_access_stopped(&self, stopped: bool) -> Result<()> {
        let client = self.pool().get().await?;
        self.access_changed();
        client
            .execute(
                "INSERT INTO access_settings (id, stopped, updated_at) VALUES (true, $1, now())
                 ON CONFLICT (id) DO UPDATE SET stopped = EXCLUDED.stopped, updated_at = now()",
                &[&stopped],
            )
            .await?;
        Ok(())
    }

    /// The grants that hold now for `agent` on `hub`: expired ones are left
    /// out, so the verdict needs no clock.
    pub async fn live_grants(&self, hub: &str, agent: &str) -> Result<Vec<Grant>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                &format!(
                    "SELECT {GRANT_COLUMNS} FROM access_grants
                     WHERE hub = $1 AND agent = $2 AND (expires_at IS NULL OR expires_at > now())"
                ),
                &[&hub, &agent],
            )
            .await?;
        rows.iter().map(|r| Ok(kept_grant(r)?.grant)).collect()
    }

    /// Every live grant, or one agent's, newest first, as Settings lists them.
    pub async fn grants(&self, agent: Option<&str>) -> Result<Vec<KeptGrant>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                &format!(
                    "SELECT {GRANT_COLUMNS} FROM access_grants
                     WHERE ($1::text IS NULL OR agent = $1)
                       AND (expires_at IS NULL OR expires_at > now())
                     ORDER BY created_at DESC, id"
                ),
                &[&agent],
            )
            .await?;
        rows.iter().map(kept_grant).collect()
    }

    pub async fn add_grant(&self, new: &NewGrant) -> Result<Grant> {
        let id = Uuid::new_v4();
        let client = self.pool().get().await?;
        self.access_changed();
        client
            .execute(
                "INSERT INTO access_grants
                   (id, hub, agent, person, ability, folder, connector, session_id, expires_at,
                    run_kind, run_id, run_name)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
                &[
                    &id,
                    &new.hub,
                    &new.agent,
                    &new.person,
                    &new.ability.as_str(),
                    &path_text(&new.folder),
                    &new.connector,
                    &new.session_id,
                    &new.expires_at,
                    &new.run.as_ref().map(|r| r.kind.clone()),
                    &new.run.as_ref().map(|r| r.id.clone()),
                    &new.run_name,
                ],
            )
            .await?;
        Ok(Grant {
            id,
            hub: new.hub.clone(),
            agent: new.agent.clone(),
            person: new.person.clone(),
            ability: new.ability,
            folder: new.folder.clone(),
            connector: new.connector.clone(),
            session_id: new.session_id.clone(),
            run: new.run.clone(),
        })
    }

    /// Whether there was such a grant.
    pub async fn revoke_grant(&self, id: Uuid) -> Result<bool> {
        let client = self.pool().get().await?;
        self.access_changed();
        Ok(client
            .execute("DELETE FROM access_grants WHERE id = $1", &[&id])
            .await?
            > 0)
    }

    /// Drops a session's allowances: it rolled over, or the person pressed
    /// "Forget".
    pub async fn forget_session_grants(&self, session_id: &str) -> Result<u64> {
        let client = self.pool().get().await?;
        self.access_changed();
        Ok(client
            .execute(
                "DELETE FROM access_grants WHERE session_id = $1",
                &[&session_id],
            )
            .await?)
    }

    /// Drops every "Allow for this session" a browser holds, for `agent` or
    /// for every agent: the person pressed Stop on it.
    pub async fn forget_browser_grants(&self, agent: Option<&str>) -> Result<u64> {
        let client = self.pool().get().await?;
        self.access_changed();
        Ok(client
            .execute(
                "DELETE FROM access_grants
                  WHERE session_id IS NOT NULL
                    AND (connector = 'browser' OR connector LIKE 'browser:%')
                    AND ($1::TEXT IS NULL OR agent = $1)",
                &[&agent],
            )
            .await?)
    }

    pub async fn record_activity(&self, a: &Activity) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO access_activity
                   (id, hub, agent, person, session_id, request_id, kind, doing, folder, connector,
                    tool, vault_source, decided_by, outcome, reason, exit_code, duration_ms,
                    arguments, device)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                         $17, $18, $19)",
                &[
                    &id,
                    &a.hub,
                    &a.agent,
                    &a.person,
                    &a.session_id,
                    &a.request_id,
                    &a.kind,
                    &a.doing,
                    &a.folder,
                    &a.connector,
                    &a.tool,
                    &a.vault_source,
                    &a.decided_by,
                    &a.outcome,
                    &a.reason,
                    &a.exit_code,
                    &a.duration_ms,
                    &a.arguments,
                    &a.device,
                ],
            )
            .await?;
        Ok(id)
    }

    /// A page of activity, newest first.
    pub async fn activity(&self, filter: &ActivityFilter) -> Result<Vec<ActivityRow>> {
        let client = self.pool().get().await?;
        let (before_at, before_id) = filter.before.unzip();
        let rows = client
            .query(
                "SELECT * FROM access_activity
                 WHERE ($1::text IS NULL OR agent = $1)
                   AND ($2::text IS NULL OR kind = $2)
                   AND ($3::timestamptz IS NULL OR (at, id) < ($3, $4))
                 ORDER BY at DESC, id DESC
                 LIMIT $5",
                &[&filter.agent, &filter.kind, &before_at, &before_id, &PAGE],
            )
            .await?;
        rows.iter().map(activity_row).collect()
    }

    /// Drops activity older than `before` and grants that have expired: at
    /// start and daily, with `before` [`ACTIVITY_KEPT_DAYS`] ago.
    pub async fn prune_access(&self, before: DateTime<Utc>) -> Result<u64> {
        let client = self.pool().get().await?;
        self.access_changed();
        let activity = client
            .execute("DELETE FROM access_activity WHERE at < $1", &[&before])
            .await?;
        let grants = client
            .execute(
                "DELETE FROM access_grants WHERE expires_at IS NOT NULL AND expires_at <= now()",
                &[],
            )
            .await?;
        Ok(activity + grants)
    }

    /// Erases what is kept of `person`: their grants and the activity of
    /// their turns.
    pub async fn erase_access_person(&self, person: &str) -> Result<u64> {
        let client = self.pool().get().await?;
        self.access_changed();
        let grants = client
            .execute("DELETE FROM access_grants WHERE person = $1", &[&person])
            .await?;
        let activity = client
            .execute("DELETE FROM access_activity WHERE person = $1", &[&person])
            .await?;
        Ok(grants + activity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::build_pool;
    use chrono::Duration;
    use serde_json::json;
    use workforce_postgres::test_support::with_test_pool;

    fn new_grant(agent: &str, session: Option<&str>, expires: Option<DateTime<Utc>>) -> NewGrant {
        NewGrant {
            hub: "local".into(),
            agent: agent.into(),
            person: "sarah".into(),
            ability: Ability::Run,
            folder: Some(PathBuf::from("/Users/sarah/dev/acme-web")),
            connector: None,
            session_id: session.map(str::to_string),
            expires_at: expires,
            run: None,
            run_name: None,
        }
    }

    fn activity(agent: &str, kind: &str, person: &str) -> Activity {
        Activity {
            hub: "local".into(),
            agent: agent.into(),
            person: Some(person.into()),
            session_id: Some("s1".into()),
            request_id: None,
            kind: kind.into(),
            doing: "npm test".into(),
            folder: Some("~/dev/acme-web".into()),
            connector: None,
            tool: "exec.run".into(),
            vault_source: None,
            decided_by: "person".into(),
            outcome: json!({"ran": true}),
            reason: None,
            exit_code: Some(0),
            duration_ms: Some(12),
            arguments: json!({"command": "npm test"}),
            device: Some("desk-device".into()),
        }
    }

    #[tokio::test]
    async fn the_kill_switch_starts_off_and_holds_what_was_set() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            assert!(!store.access_stopped().await.unwrap());
            store.set_access_stopped(true).await.unwrap();
            assert!(store.access_stopped().await.unwrap());
            store.set_access_stopped(false).await.unwrap();
            assert!(!store.access_stopped().await.unwrap());
        })
        .await;
    }

    #[tokio::test]
    async fn grants_round_trip_and_only_live_ones_reach_the_verdict() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let mut standing = new_grant("allan", None, None);
            standing.run = Some(RunOf {
                kind: "job".into(),
                id: "j1".into(),
            });
            standing.run_name = Some("Morning build".into());
            let kept = store.add_grant(&standing).await.unwrap();
            let session = store
                .add_grant(&new_grant(
                    "allan",
                    Some("s1"),
                    Some(Utc::now() + Duration::hours(12)),
                ))
                .await
                .unwrap();
            store
                .add_grant(&new_grant(
                    "allan",
                    Some("s0"),
                    Some(Utc::now() - Duration::minutes(1)),
                ))
                .await
                .unwrap();
            store
                .add_grant(&new_grant("ben", None, None))
                .await
                .unwrap();

            let mut live = store.live_grants("local", "allan").await.unwrap();
            live.sort_by_key(|g| g.session_id.clone());
            assert_eq!(live, vec![kept.clone(), session.clone()]);
            assert!(store.live_grants("acme", "allan").await.unwrap().is_empty());

            let listed = store.grants(Some("allan")).await.unwrap();
            assert_eq!(listed.len(), 2);
            let job = listed.iter().find(|g| g.grant.id == kept.id).unwrap();
            assert_eq!(job.run_name.as_deref(), Some("Morning build"));
            assert_eq!(store.grants(None).await.unwrap().len(), 3);

            assert_eq!(store.forget_session_grants("s1").await.unwrap(), 1);
            assert!(store.revoke_grant(kept.id).await.unwrap());
            assert!(!store.revoke_grant(kept.id).await.unwrap());
            assert!(store
                .live_grants("local", "allan")
                .await
                .unwrap()
                .is_empty());
            // The expired one is still there until pruned.
            assert_eq!(
                store
                    .prune_access(Utc::now() - Duration::days(90))
                    .await
                    .unwrap(),
                1
            );
        })
        .await;
    }

    #[tokio::test]
    async fn stopping_the_browser_forgets_its_session_allowances_only() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let site = |agent: &str, connector: &str, session: Option<&str>| NewGrant {
                ability: Ability::ConnectorWrite,
                folder: None,
                connector: Some(connector.into()),
                ..new_grant(agent, session, Some(Utc::now() + Duration::hours(12)))
            };
            store
                .add_grant(&site("allan", "browser:shop.example", Some("s1")))
                .await
                .unwrap();
            store
                .add_grant(&site("ben", "browser:docs.example", Some("s2")))
                .await
                .unwrap();
            let linear = store
                .add_grant(&site("allan", "linear", Some("s1")))
                .await
                .unwrap();
            let standing = store
                .add_grant(&new_grant("allan", None, None))
                .await
                .unwrap();

            assert_eq!(store.forget_browser_grants(Some("allan")).await.unwrap(), 1);
            assert_eq!(store.forget_browser_grants(None).await.unwrap(), 1);
            let mut left: Vec<Uuid> = store
                .live_grants("local", "allan")
                .await
                .unwrap()
                .into_iter()
                .map(|g| g.id)
                .collect();
            left.sort();
            let mut kept = vec![linear.id, standing.id];
            kept.sort();
            assert_eq!(left, kept, "another connector and standing grants stay");
        })
        .await;
    }

    #[tokio::test]
    async fn activity_pages_newest_first_prunes_and_erases_by_person() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let mut ids = vec![];
            for (agent, kind, person) in [
                ("allan", "exec", "sarah"),
                ("allan", "write", "sarah"),
                ("ben", "exec", "tom"),
            ] {
                ids.push(
                    store
                        .record_activity(&activity(agent, kind, person))
                        .await
                        .unwrap(),
                );
            }
            let all = store.activity(&ActivityFilter::default()).await.unwrap();
            assert_eq!(all.len(), 3);
            assert!(all
                .windows(2)
                .all(|w| (w[0].at, w[0].id) > (w[1].at, w[1].id)));
            assert_eq!(all[0].activity.arguments, json!({"command": "npm test"}));

            let allan_exec = store
                .activity(&ActivityFilter {
                    agent: Some("allan".into()),
                    kind: Some("exec".into()),
                    before: None,
                })
                .await
                .unwrap();
            assert_eq!(allan_exec.len(), 1);
            assert_eq!(allan_exec[0].id, ids[0]);

            let after_first = store
                .activity(&ActivityFilter {
                    before: Some((all[0].at, all[0].id)),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(after_first, all[1..].to_vec());

            store
                .add_grant(&new_grant("allan", None, None))
                .await
                .unwrap();
            assert_eq!(store.erase_access_person("sarah").await.unwrap(), 3);
            let left = store.activity(&ActivityFilter::default()).await.unwrap();
            assert_eq!(left.len(), 1);
            assert_eq!(left[0].activity.person.as_deref(), Some("tom"));

            assert_eq!(
                store
                    .prune_access(Utc::now() + Duration::seconds(1))
                    .await
                    .unwrap(),
                1
            );
            assert!(store
                .activity(&ActivityFilter::default())
                .await
                .unwrap()
                .is_empty());
        })
        .await;
    }
}
