//! SQLite backend. Mirrors `approvals/store/sqlite.rs`: `Mutex<Connection>`,
//! `init_schema` on open, lossy row parsing (a malformed row is skipped, not
//! fatal).

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use rusqlite::Connection;

use super::{BudgetOverride, BudgetOverrideStore, OverrideAuditEntry, OverrideScope};

pub struct SqliteBudgetOverrideStore {
    conn: Mutex<Connection>,
}

impl SqliteBudgetOverrideStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening budget overrides db at {}", path.display()))?;
        Self::with_connection(conn)
    }

    pub fn in_memory() -> Result<Self> {
        Self::with_connection(Connection::open_in_memory()?)
    }

    fn with_connection(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS budget_overrides (
                 scope TEXT NOT NULL,
                 key TEXT NOT NULL,
                 daily_usd REAL NOT NULL,
                 expires_at TEXT,
                 note TEXT,
                 created_by TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 PRIMARY KEY (scope, key)
             );
             CREATE TABLE IF NOT EXISTS budget_override_audit (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 action TEXT NOT NULL,
                 scope TEXT NOT NULL,
                 key TEXT NOT NULL,
                 daily_usd REAL,
                 expires_at TEXT,
                 actor TEXT NOT NULL,
                 at TEXT NOT NULL
             );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

impl BudgetOverrideStore for SqliteBudgetOverrideStore {
    fn list_active(&self, now: DateTime<Utc>) -> Result<Vec<BudgetOverride>> {
        let conn = self.conn.lock().expect("budget overrides lock");
        let mut stmt = conn.prepare(
            "SELECT scope, key, daily_usd, expires_at, note, created_by, updated_at
             FROM budget_overrides",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, f64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?;
        Ok(rows
            .filter_map(|r| r.ok())
            .filter_map(|(scope, key, daily_usd, expires, note, by, updated)| {
                let scope = OverrideScope::parse(&scope)?;
                let expires_at = match expires {
                    Some(s) => Some(parse_ts(&s)?),
                    None => None,
                };
                Some(BudgetOverride {
                    scope,
                    key,
                    daily_usd,
                    expires_at,
                    note,
                    created_by: by,
                    updated_at: parse_ts(&updated)?,
                })
            })
            .filter(|o| o.expires_at.is_none_or(|e| e > now))
            .collect())
    }

    fn upsert(&self, o: &BudgetOverride) -> Result<()> {
        let mut conn = self.conn.lock().expect("budget overrides lock");
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM budget_overrides WHERE expires_at IS NOT NULL AND expires_at < ?1",
            [o.updated_at.to_rfc3339()],
        )?;
        tx.execute(
            "INSERT INTO budget_overrides
             (scope, key, daily_usd, expires_at, note, created_by, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (scope, key) DO UPDATE SET
                 daily_usd = excluded.daily_usd,
                 expires_at = excluded.expires_at,
                 note = excluded.note,
                 created_by = excluded.created_by,
                 updated_at = excluded.updated_at",
            rusqlite::params![
                o.scope.as_str(),
                o.key,
                o.daily_usd,
                o.expires_at.map(|e| e.to_rfc3339()),
                o.note,
                o.created_by,
                o.updated_at.to_rfc3339(),
            ],
        )?;
        tx.execute(
            "INSERT INTO budget_override_audit
             (action, scope, key, daily_usd, expires_at, actor, at)
             VALUES ('set', ?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                o.scope.as_str(),
                o.key,
                o.daily_usd,
                o.expires_at.map(|e| e.to_rfc3339()),
                o.created_by,
                o.updated_at.to_rfc3339(),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn remove(&self, scope: OverrideScope, key: &str, actor: &str) -> Result<bool> {
        let mut conn = self.conn.lock().expect("budget overrides lock");
        let tx = conn.transaction()?;
        let removed = tx.execute(
            "DELETE FROM budget_overrides WHERE scope = ?1 AND key = ?2",
            rusqlite::params![scope.as_str(), key],
        )?;
        if removed > 0 {
            tx.execute(
                "INSERT INTO budget_override_audit
                 (action, scope, key, daily_usd, expires_at, actor, at)
                 VALUES ('clear', ?1, ?2, NULL, NULL, ?3, ?4)",
                rusqlite::params![scope.as_str(), key, actor, Utc::now().to_rfc3339()],
            )?;
        }
        tx.commit()?;
        Ok(removed > 0)
    }

    fn audit(&self, limit: usize) -> Result<Vec<OverrideAuditEntry>> {
        let conn = self.conn.lock().expect("budget overrides lock");
        let mut stmt = conn.prepare(
            "SELECT action, scope, key, daily_usd, expires_at, actor, at
             FROM budget_override_audit ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<f64>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?;
        Ok(rows
            .filter_map(|r| r.ok())
            .filter_map(|(action, scope, key, daily_usd, expires, actor, at)| {
                Some(OverrideAuditEntry {
                    action,
                    scope: OverrideScope::parse(&scope)?,
                    key,
                    daily_usd,
                    expires_at: expires.and_then(|s| parse_ts(&s)),
                    actor,
                    at: parse_ts(&at)?,
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(scope: OverrideScope, key: &str, usd: f64) -> BudgetOverride {
        BudgetOverride {
            scope,
            key: key.to_string(),
            daily_usd: usd,
            expires_at: None,
            note: None,
            created_by: "admin-uuid".to_string(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn upsert_replaces_and_lists() {
        let s = SqliteBudgetOverrideStore::in_memory().unwrap();
        s.upsert(&row(OverrideScope::Identity, "u1", 100.0))
            .unwrap();
        s.upsert(&row(OverrideScope::Identity, "u1", 150.0))
            .unwrap();
        s.upsert(&row(OverrideScope::Hub, "", 5000.0)).unwrap();
        let active = s.list_active(Utc::now()).unwrap();
        assert_eq!(active.len(), 2);
        let u1 = active
            .iter()
            .find(|o| o.scope == OverrideScope::Identity)
            .unwrap();
        assert_eq!(u1.daily_usd, 150.0);
    }

    #[test]
    fn expired_overrides_are_filtered_and_pruned() {
        let s = SqliteBudgetOverrideStore::in_memory().unwrap();
        let mut expired = row(OverrideScope::Identity, "u1", 100.0);
        expired.expires_at = Some(Utc::now() - chrono::Duration::hours(1));
        s.upsert(&expired).unwrap();
        assert!(s.list_active(Utc::now()).unwrap().is_empty());

        // The next write prunes the lapsed row entirely.
        s.upsert(&row(OverrideScope::Agent, "a", 10.0)).unwrap();
        let active = s.list_active(Utc::now()).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].scope, OverrideScope::Agent);
    }

    #[test]
    fn remove_and_audit_trail() {
        let s = SqliteBudgetOverrideStore::in_memory().unwrap();
        s.upsert(&row(OverrideScope::Agent, "atlas", 900.0))
            .unwrap();
        assert!(s
            .remove(OverrideScope::Agent, "atlas", "admin-uuid")
            .unwrap());
        assert!(!s
            .remove(OverrideScope::Agent, "atlas", "admin-uuid")
            .unwrap());
        assert!(s.list_active(Utc::now()).unwrap().is_empty());

        let audit = s.audit(10).unwrap();
        assert_eq!(audit.len(), 2);
        assert_eq!(audit[0].action, "clear");
        assert_eq!(audit[1].action, "set");
        assert_eq!(audit[1].daily_usd, Some(900.0));
    }
}
