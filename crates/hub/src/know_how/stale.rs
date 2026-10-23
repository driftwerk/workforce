//! Re-checks: the checks that admitted an entry, run again once what they
//! check against has moved. After the Metabase schema sync, a table that is
//! gone makes its entries stale and a column that is gone is dropped from
//! them; after code index maintenance, a moved file is followed by its
//! symbol or made stale. Stale entries are not shown, and a turn or a later
//! re-check that finds their names again brings them back. Entries nobody
//! produced, was shown or matched for 90 days are deleted. Each pass runs on
//! the fleet's leader only: the store is shared.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use workforce_common::metabase_schema::{DbSchema, InstanceSchema};
use workforce_common::settings::{ProgramsMode, WorkforceConfig};

use super::{code, sql, Entry, Kind, KnowHow, Revision};

/// How long an entry may go unused before it is deleted.
pub const UNUSED_DAYS: i64 = 90;

/// What a re-check changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Rechecked {
    pub revised: usize,
    pub stale: usize,
}

/// An entry of a data kind against its database's schema now: stale when a
/// table it names is gone (or the database is), its gone columns dropped;
/// a query shape or correction naming a gone column is stale too, and so is
/// a correction whose wrong name has since appeared. `None` when nothing
/// changed.
pub(super) fn recheck_data(e: &Entry, db: Option<&DbSchema>) -> Option<Revision> {
    let Some(db) = db else {
        return revise(e, Vec::new(), None, true);
    };
    // `Some(present)` when the table's columns are known; columns are only
    // described for some tables, and an undescribed one proves nothing.
    let column = |c: &str| -> Option<bool> {
        let (table, name) = c.rsplit_once('.')?;
        let known = db.columns.get(table).filter(|k| !k.is_empty())?;
        Some(known.iter().any(|k| k.eq_ignore_ascii_case(name)))
    };
    let tables_there = e.tables.iter().all(|t| db.tables.contains(t));
    let gone: Vec<String> = e
        .columns
        .iter()
        .filter(|c| column(c) == Some(false))
        .cloned()
        .collect();
    match e.kind {
        Kind::DataLocation => revise(e, gone, None, !tables_there),
        Kind::QueryShape => revise(e, Vec::new(), None, !tables_there || !gone.is_empty()),
        Kind::SchemaCorrection => {
            let wrong = e.wrong.as_deref().unwrap_or_default();
            let appeared = match e.columns.is_empty() {
                true => sql::knows_table(wrong, db),
                false => column(wrong) == Some(true),
            };
            revise(
                e,
                Vec::new(),
                None,
                !tables_there || !gone.is_empty() || appeared,
            )
        }
        Kind::CodeLocation => None,
    }
}

/// The revision of `e`, if it changes anything.
pub(super) fn revise(
    e: &Entry,
    gone: Vec<String>,
    moved: Option<(String, String)>,
    stale: bool,
) -> Option<Revision> {
    let changed = !gone.is_empty() || moved.is_some() || stale != e.stale;
    changed.then_some(Revision {
        id: e.id,
        gone,
        moved,
        stale,
    })
}

impl KnowHow {
    /// Re-check every data entry against the schema store.
    pub async fn recheck_schemas(&self) -> eyre::Result<Rechecked> {
        let mut done = Rechecked::default();
        let mut instances: HashMap<String, Option<InstanceSchema>> = HashMap::new();
        for agent in self.store.agents().await? {
            let checked = Utc::now();
            let mut revised = Vec::new();
            for e in self.store.entries(&agent).await? {
                if e.kind == Kind::CodeLocation {
                    continue;
                }
                if let std::collections::hash_map::Entry::Vacant(slot) =
                    instances.entry(e.site.clone())
                {
                    slot.insert(self.instance_schema(&e.site).await);
                }
                // Nothing stored for the site (never synced, or another
                // site now) says nothing about its names.
                let Some(instance) = instances[&e.site]
                    .as_ref()
                    .filter(|i| !i.databases.is_empty())
                else {
                    continue;
                };
                revised.extend(recheck_data(&e, instance.databases.get(&e.database_id)));
            }
            done = self.revise_all(&agent, revised, checked, done).await;
        }
        Ok(done)
    }

    /// Re-check every code location against the code index.
    pub async fn recheck_code(&self) -> eyre::Result<Rechecked> {
        let mut done = Rechecked::default();
        let Some(index) = self.code() else {
            return Ok(done);
        };
        for agent in self.store.agents().await? {
            let checked = Utc::now();
            let mut revised = Vec::new();
            for e in self.store.entries(&agent).await? {
                if e.kind == Kind::CodeLocation {
                    revised.extend(code::recheck(&e, index.as_ref()).await);
                }
            }
            done = self.revise_all(&agent, revised, checked, done).await;
        }
        Ok(done)
    }

    /// Write each revision; one that fails is logged and left for the next
    /// pass.
    async fn revise_all(
        &self,
        agent: &str,
        revised: Vec<Revision>,
        checked: DateTime<Utc>,
        mut done: Rechecked,
    ) -> Rechecked {
        if revised.is_empty() {
            return done;
        }
        for r in &revised {
            match self.store.revise(agent, r, checked).await {
                Ok(true) => {
                    done.revised += 1;
                    done.stale += usize::from(r.stale);
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(target: "know_how", %agent, entry = r.id, error = %e, "re-checking an entry failed")
                }
            }
        }
        self.forget_snapshot(agent);
        done
    }

    /// Delete entries unused for [`UNUSED_DAYS`].
    pub async fn prune(&self) -> eyre::Result<usize> {
        let pruned = self
            .store
            .prune_unused(Utc::now() - chrono::Duration::days(UNUSED_DAYS))
            .await?;
        if pruned > 0 {
            self.snapshots.lock().clear();
        }
        Ok(pruned)
    }
}

/// After the Metabase schema sync's fetch, on the leader.
pub async fn after_schema_sync(config: &WorkforceConfig) {
    let Some(kh) = leading(config).await else {
        return;
    };
    let rechecked = kh.recheck_schemas().await;
    finish(&kh, "schema", rechecked).await;
}

/// After a code index maintenance pass, on the leader.
pub async fn after_code_maintenance(config: &WorkforceConfig) {
    let Some(kh) = leading(config).await else {
        return;
    };
    let rechecked = kh.recheck_code().await;
    finish(&kh, "code", rechecked).await;
}

async fn leading(config: &WorkforceConfig) -> Option<std::sync::Arc<KnowHow>> {
    (config.know_how.mode != ProgramsMode::Off && crate::health_gauges::is_leader())
        .then_some(())?;
    Some(KnowHow::shared(config).await)
}

async fn finish(kh: &KnowHow, what: &str, rechecked: eyre::Result<Rechecked>) {
    let pruned = kh.prune().await;
    match (rechecked, pruned) {
        (Ok(r), Ok(pruned)) => tracing::info!(
            target: "know_how",
            pass = what,
            revised = r.revised,
            stale = r.stale,
            pruned,
            "know-how re-checked"
        ),
        (r, p) => tracing::warn!(
            target: "know_how",
            pass = what,
            recheck_error = ?r.err(),
            prune_error = ?p.err(),
            "know-how re-check failed"
        ),
    }
}
