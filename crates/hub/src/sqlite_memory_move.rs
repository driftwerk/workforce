//! `workforce memory move-sqlite`: moves the memories a hub kept in its
//! SQLite file into vellev, the store `[memory] backend = "vellev"` serves
//! them from. Desk runs it once, with its hub stopped, when its local hub
//! moves to its own memory store.
//!
//! Every memory is copied under its own id (forgotten ones too, still
//! forgotten), so a second run puts the same memories in the same places.
//! The node embeds each one. Every memory copied is then read back and
//! compared: the move is complete only when all of them are there as they
//! were. The SQLite file is only read.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use eyre::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use workforce_common::settings::WorkforceConfig;
use workforce_memory::admin::Switchable;
use workforce_memory::sqlite::{SqliteMemoryConfig, SqliteMemoryStore};

/// Ids read back per request.
const READ_BACK: usize = 100;

/// What a move found and did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    /// Memories in the SQLite file, forgotten ones included.
    pub in_sqlite: u64,
    /// Those with nothing in them to embed (empty content), left behind.
    pub unmovable: u64,
    /// Copied into vellev (0 on a dry run).
    pub moved: u64,
    /// Read back from vellev as they were in SQLite.
    pub verified: u64,
}

impl Counts {
    /// Every memory with something in it is in vellev, as it was.
    pub fn complete(&self) -> bool {
        self.moved + self.unmovable == self.in_sqlite && self.verified == self.moved
    }
}

/// The properties a memory read back must hold as it was copied.
const COMPARED: [&str; 7] = [
    "memoryId",
    "content",
    "layer",
    "owner",
    "visibility",
    "identityId",
    "deleted",
];

/// Whether `held` holds what `sent` was sent with.
fn same(sent: &Value, held: &Value) -> bool {
    COMPARED.iter().all(|key| {
        let (a, b) = (&sent["properties"][*key], &held["properties"][*key]);
        // A missing value and null are the same absence.
        a == b || (a.is_null() && b.is_null())
    })
}

/// Moves the memories of the SQLite file at `from` into the vellev store
/// `config` names; with `apply` false, only counts them.
pub async fn run(config: &WorkforceConfig, from: &Path, apply: bool) -> Result<Counts> {
    if !config.memory.is_vellev() {
        bail!("move-sqlite moves memories into vellev: run it with [memory] backend = \"vellev\"");
    }
    if !from.is_file() {
        bail!("no SQLite memory file at {}", from.display());
    }
    // Opened as the hub opens it, without an embedder: only read.
    let sqlite = SqliteMemoryStore::open(&SqliteMemoryConfig {
        db_path: from.to_string_lossy().into_owned(),
        embedding_provider: "none".into(),
        ..Default::default()
    })?;
    let export = sqlite.export_objects()?;
    let mut counts = Counts {
        in_sqlite: export.total as u64,
        unmovable: export.unmovable as u64,
        ..Counts::default()
    };
    if !apply {
        return Ok(counts);
    }
    let target = crate::embeddings::base_store(config)?;
    crate::state::ensure_memory_store_ready(
        target.as_ref(),
        None,
        Duration::from_secs(config.vellev.connect_timeout_secs),
        "vellev",
    )
    .await?;
    if !export.objects.is_empty() {
        counts.moved = target.import(&export.objects).await? as u64;
    }
    counts.verified = read_back(target.as_ref(), &export.objects).await?;
    Ok(counts)
}

/// How many of `sent` vellev holds as they were sent.
async fn read_back(target: &dyn Switchable, sent: &[Value]) -> Result<u64> {
    let by_id: BTreeMap<&str, &Value> = sent
        .iter()
        .filter_map(|o| Some((o["id"].as_str()?, o)))
        .collect();
    let ids: Vec<String> = by_id.keys().map(|id| id.to_string()).collect();
    let mut verified = 0;
    for chunk in ids.chunks(READ_BACK) {
        for held in target.objects_by_id(chunk, false).await? {
            let matches = held["id"]
                .as_str()
                .and_then(|id| by_id.get(id))
                .is_some_and(|sent| same(sent, &held));
            if matches {
                verified += 1;
            } else {
                tracing::warn!(id = %held["id"], "a memory read back differs from what was moved");
            }
        }
    }
    Ok(verified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_move_is_complete_only_when_every_memory_is_back_as_it_was() {
        let done = Counts {
            in_sqlite: 5,
            unmovable: 1,
            moved: 4,
            verified: 4,
        };
        assert!(done.complete());
        assert!(!Counts {
            verified: 3,
            ..done
        }
        .complete());
        assert!(!Counts { moved: 3, ..done }.complete());
        assert!(Counts::default().complete());
    }

    #[test]
    fn a_memory_read_back_must_hold_what_was_sent() {
        let sent = json!({"id": "a", "properties": {
            "memoryId": "m", "content": "Acme ships on Fridays", "layer": "episodic",
            "owner": "desk", "visibility": "private", "identityId": null, "deleted": false,
        }});
        let mut held = sent.clone();
        held["properties"]["accessCount"] = json!(3);
        held["properties"]
            .as_object_mut()
            .unwrap()
            .remove("identityId");
        assert!(same(&sent, &held));
        held["properties"]["deleted"] = json!(true);
        assert!(!same(&sent, &held));
    }

    #[tokio::test]
    async fn a_dry_run_counts_what_sqlite_holds_and_what_it_cannot_move() {
        use workforce_memory::store::MemoryStore;
        use workforce_memory::types::{Memory, MemoryLayer};
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memory.db");
        let store = SqliteMemoryStore::open(&SqliteMemoryConfig {
            db_path: db.to_string_lossy().into_owned(),
            embedding_provider: "none".into(),
            ..Default::default()
        })
        .unwrap();
        let agent = workforce_core::types::AgentId::new("desk");
        let kept = Memory::new(
            "Acme's invoices go out on the first Monday".to_string(),
            MemoryLayer::Semantic,
            agent.clone(),
        );
        let forgotten = Memory::new(
            "an old note".to_string(),
            MemoryLayer::Episodic,
            agent.clone(),
        );
        store.remember(kept.clone()).await.unwrap();
        store.remember(forgotten.clone()).await.unwrap();
        store.forget(&forgotten.id).await.unwrap();
        drop(store);

        let mut config = WorkforceConfig::default();
        config.memory.backend = "vellev".into();
        let counts = run(&config, &db, false).await.unwrap();
        assert_eq!(
            counts,
            Counts {
                in_sqlite: 2,
                unmovable: 0,
                moved: 0,
                verified: 0
            }
        );

        let export = SqliteMemoryStore::open(&SqliteMemoryConfig {
            db_path: db.to_string_lossy().into_owned(),
            embedding_provider: "none".into(),
            ..Default::default()
        })
        .unwrap()
        .export_objects()
        .unwrap();
        let gone = export
            .objects
            .iter()
            .find(|o| o["properties"]["memoryId"] == forgotten.id.0.as_str())
            .unwrap();
        assert_eq!(gone["properties"]["deleted"], json!(true));
        assert_eq!(gone["id"], json!(forgotten.id.0));

        config.memory.backend = "sqlite".into();
        assert!(run(&config, &db, false).await.is_err());
    }
}
