//! Moving the edge's memories out of the hub's stores (DW-676 (b)), once.
//!
//! The edge once kept its memories in the hub's Weaviate class,
//! `WorkforceMemory`. The hub's switch to its own embeddings copied them
//! into its bge class, and the shadow into vellev's replica of that. An
//! edge customer's erasure purged `WorkforceMemory` alone, so the copies in
//! the other two outlived it.
//!
//! This moves them, in two steps, each re-run safely as often as needed:
//!
//! - **copy**, with the edge stopped: every edge-owned object of
//!   `WorkforceMemory` into the edge's own vellev tenant, under its own id,
//!   read back and checked, and only then [`MOVED_MARKER`] written there.
//!   Refused once the marker is written: from then on the edge runs on that
//!   tenant, and a copy again would bring back what a customer has erased
//!   there since, or overwrite newer edits with old ones.
//! - **purge**, refused until the marker is written: every memory an
//!   edge-owned object holds, in any of the three stores, purged from all
//!   of them; vellev's replica and the bge class together under the
//!   replica's lock, as every erasure of the hub's is (D1 (a)), then
//!   `WorkforceMemory`; then, after the hub's syncs have had time to bring
//!   any back, looked for again, and purged again if found.
//!
//! An object is the edge's when its owner is one of the edge's agents and
//! its identity one the edge writes under ([`is_edge_identity`]). Only
//! `WorkforceMemory`'s are copied: an edge object found only in the bge
//! class or the replica is a copy whose original the edge erased, and is
//! purged, never brought back. A purge is by memory id, so a memory also
//! held by an object that is not the edge's is refused before anything is
//! written. Nothing is ever printed of an object but its ids and counts.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde_json::Value;
use workforce_common::settings::WorkforceConfig;
use workforce_memory::admin::{
    normalised, same_properties, Cursor, SyncSource, SyncTarget, IMPORT_BATCH_SIZE,
};
use workforce_memory::edge_owner::{is_customer_owner, is_edge_identity};
use workforce_memory::store::MemoryStore;
use workforce_memory::types::MemoryId;
use workforce_memory::vellev::{VellevStore, DEFAULT_COLLECTION};
use workforce_memory::weaviate::WeaviateStore;

/// The index marker, in the edge's tenant, that its memories were copied
/// there and checked.
pub const MOVED_MARKER: &str = "edge-memories-moved-from-weaviate";

/// Ids shown of any list in a report; the rest are counted.
const SHOWN: usize = 10;

/// The three stores of the hub's an edge object may be held in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HubStore {
    /// `WorkforceMemory`, where the edge wrote.
    Weaviate,
    /// The class of the hub's own embeddings.
    Embedded,
    /// vellev's replica of that class.
    Replica,
}

impl HubStore {
    pub const ALL: [HubStore; 3] = [HubStore::Weaviate, HubStore::Embedded, HubStore::Replica];

    pub fn label(self) -> &'static str {
        match self {
            HubStore::Weaviate => "WorkforceMemory",
            HubStore::Embedded => "the hub's embedded class",
            HubStore::Replica => "vellev's replica",
        }
    }
}

/// What the three stores hold of the edge's, as a survey found it.
#[derive(Debug, Default, Clone)]
pub struct Survey {
    /// Edge objects of `WorkforceMemory`, as a store is to hold them.
    pub to_copy: Vec<Value>,
    /// Memory ids held by an edge object in any store.
    pub to_purge: BTreeSet<String>,
    /// Edge objects in each store.
    pub held: BTreeMap<HubStore, usize>,
    /// Memory ids of customers' memories in `WorkforceMemory` whose customer
    /// the edge no longer knows (erased there): purged, never copied.
    pub erased: BTreeSet<String>,
    /// Memory ids held by edge objects of the bge class or the replica and
    /// by none of `WorkforceMemory`'s: copies the edge erased.
    pub survivors: BTreeSet<String>,
    /// Memory ids held by an edge object and by one that is not the edge's.
    pub conflicts: BTreeSet<String>,
    /// Ids of edge objects without a memory id, which no purge can reach.
    pub unpurgeable: Vec<String>,
    /// Ids of edge objects of `WorkforceMemory` no store would take (no
    /// content to embed).
    pub uncopyable: Vec<String>,
    /// Objects of an edge agent under an identity the edge does not write
    /// under: left alone, and counted.
    pub unclassified: usize,
}

impl Survey {
    /// `Err` naming what makes the survey unsafe to act on.
    pub fn refusal(&self) -> Option<String> {
        let mut said = Vec::new();
        if !self.conflicts.is_empty() {
            said.push(format!(
                "{} memories are held both by the edge's objects and by others' (a purge is by memory id): {}",
                self.conflicts.len(),
                shown(self.conflicts.iter())
            ));
        }
        if !self.unpurgeable.is_empty() {
            said.push(format!(
                "{} edge objects have no memory id, so no purge reaches them: {}",
                self.unpurgeable.len(),
                shown(self.unpurgeable.iter())
            ));
        }
        if !self.uncopyable.is_empty() {
            said.push(format!(
                "{} edge objects of WorkforceMemory have no content a store would take: {}",
                self.uncopyable.len(),
                shown(self.uncopyable.iter())
            ));
        }
        (!said.is_empty()).then(|| said.join("; "))
    }
}

fn shown<'a>(ids: impl Iterator<Item = &'a String>) -> String {
    let ids: Vec<&String> = ids.collect();
    let mut text = ids
        .iter()
        .take(SHOWN)
        .map(|id| id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if ids.len() > SHOWN {
        text.push_str(&format!(" and {} more", ids.len() - SHOWN));
    }
    text
}

/// A survey under way, object by object.
#[derive(Debug)]
pub struct Tally {
    agents: BTreeSet<String>,
    /// The customers the edge still knows, when copying.
    known: Option<BTreeSet<String>>,
    survey: Survey,
    /// Memory ids held by objects that are not the edge's.
    others: BTreeSet<String>,
    /// Memory ids held by edge objects of `WorkforceMemory`.
    in_weaviate: BTreeSet<String>,
}

impl Tally {
    pub fn new(agents: &[String], known: Option<&BTreeSet<String>>) -> Self {
        Self {
            agents: agents.iter().cloned().collect(),
            known: known.cloned(),
            survey: Survey::default(),
            others: BTreeSet::new(),
            in_weaviate: BTreeSet::new(),
        }
    }

    /// Count `object`, held in `store`.
    pub fn see(&mut self, store: HubStore, object: &Value) {
        let properties = &object["properties"];
        let memory_id = properties["memoryId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        let ours = properties["owner"]
            .as_str()
            .is_some_and(|owner| self.agents.contains(owner));
        let identity = properties["identityId"].as_str().unwrap_or_default();
        let edge = ours && is_edge_identity(identity);
        if !edge {
            if ours {
                self.survey.unclassified += 1;
            }
            if let Some(memory_id) = memory_id {
                self.others.insert(memory_id);
            }
            return;
        }
        *self.survey.held.entry(store).or_default() += 1;
        let Some(memory_id) = memory_id else {
            self.survey
                .unpurgeable
                .push(object["id"].as_str().unwrap_or_default().to_string());
            return;
        };
        self.survey.to_purge.insert(memory_id.clone());
        if store == HubStore::Weaviate {
            self.in_weaviate.insert(memory_id.clone());
            let forgotten = is_customer_owner(identity)
                && self
                    .known
                    .as_ref()
                    .is_some_and(|known| !known.contains(identity));
            if forgotten {
                self.survey.erased.insert(memory_id);
                return;
            }
            match normalised(object) {
                Some(object) => self.survey.to_copy.push(object),
                None => self
                    .survey
                    .uncopyable
                    .push(object["id"].as_str().unwrap_or_default().to_string()),
            }
        }
    }

    pub fn finish(mut self) -> Survey {
        self.survey.conflicts = self
            .survey
            .to_purge
            .intersection(&self.others)
            .cloned()
            .collect();
        self.survey.survivors = self
            .survey
            .to_purge
            .difference(&self.in_weaviate)
            .cloned()
            .collect();
        self.survey
    }
}

/// Whether the edge's memories were copied to its tenant and checked.
#[async_trait]
pub trait Marker: Send + Sync {
    async fn marked(&self) -> Result<bool>;
    async fn mark(&self) -> Result<()>;
}

/// Purges memories from one or more stores in one call.
#[async_trait]
pub trait Purges: Send + Sync {
    async fn purge(&self, ids: &[MemoryId]) -> Result<usize>;
}

/// A memory store's own purge.
pub struct StorePurge(pub Arc<dyn MemoryStore>);

#[async_trait]
impl Purges for StorePurge {
    async fn purge(&self, ids: &[MemoryId]) -> Result<usize> {
        self.0.purge_memories(ids).await
    }
}

/// Everything a move reads and writes.
pub struct Stores {
    /// The hub's three stores, read without vectors.
    pub hub: Vec<(HubStore, Arc<dyn SyncSource>)>,
    /// The bge class and vellev's replica, purged together under the
    /// replica's lock.
    pub purge_embedded: Arc<dyn Purges>,
    /// `WorkforceMemory`, purged after them.
    pub purge_weaviate: Arc<dyn Purges>,
    /// The edge's own collection in its tenant.
    pub edge: Arc<dyn SyncTarget>,
    pub marker: Arc<dyn Marker>,
}

/// Read every object of the hub's three stores.
/// `known`, when copying: the customers the edge still knows; a customer's
/// memory of any other is taken as erased.
pub async fn survey(
    stores: &Stores,
    agents: &[String],
    known: Option<&BTreeSet<String>>,
) -> Result<Survey> {
    let mut tally = Tally::new(agents, known);
    for (which, source) in &stores.hub {
        let mut cursor = Cursor::new(source.as_ref(), false);
        while let Some(object) = cursor.next().await? {
            tally.see(*which, &object);
        }
    }
    Ok(tally.finish())
}

/// What a copy found and did.
#[derive(Debug, Default)]
pub struct Copied {
    pub survey: Survey,
    /// Objects the edge's tenant held before, all of them earlier copies.
    pub already: usize,
    /// Objects sent (none on a dry run).
    pub sent: usize,
    pub applied: bool,
}

/// The copy step; with `apply` false, nothing is written. `known` is every
/// customer the edge still knows (its `edge_analytics.chat_identities`):
/// only their memories are copied, since the hub's carry-back may have put
/// back into `WorkforceMemory` a memory the edge erased.
pub async fn copy(
    stores: &Stores,
    agents: &[String],
    known: &BTreeSet<String>,
    apply: bool,
) -> Result<Copied> {
    if stores.marker.marked().await? {
        return Err(eyre!(
            "the edge's memories were copied to its tenant already ({MOVED_MARKER} is set), and the edge may have written there since; copying again could bring back what a customer erased. Run the purge step"
        ));
    }
    let survey = survey(stores, agents, Some(known)).await?;
    if let Some(refusal) = survey.refusal() {
        return Err(eyre!("nothing was copied: {refusal}"));
    }
    let wanted: BTreeSet<&str> = survey
        .to_copy
        .iter()
        .filter_map(|o| o["id"].as_str())
        .collect();
    // A crashed copy left some there, all of them ours; anything else is
    // the edge's own, written since it ran on the tenant, and would be
    // overwritten or mixed in.
    let mut stray = Vec::new();
    let mut already = 0;
    let mut held = Cursor::new(stores.edge.as_ref(), false);
    while let Some(object) = held.next().await? {
        let id = object["id"].as_str().unwrap_or_default().to_string();
        match wanted.contains(id.as_str()) {
            true => already += 1,
            false => stray.push(id),
        }
    }
    if !stray.is_empty() {
        return Err(eyre!(
            "nothing was copied: the edge's tenant holds {} objects that are not copies of WorkforceMemory's: {}. Has the edge run on it already?",
            stray.len(),
            shown(stray.iter())
        ));
    }
    let mut copied = Copied {
        survey,
        already,
        sent: 0,
        applied: apply,
    };
    if !apply {
        return Ok(copied);
    }
    for batch in copied.survey.to_copy.chunks(IMPORT_BATCH_SIZE) {
        stores.edge.upsert(batch).await?;
        copied.sent += batch.len();
    }
    verify_copied(stores.edge.as_ref(), &copied.survey.to_copy).await?;
    stores.marker.mark().await?;
    Ok(copied)
}

/// `Err` unless the edge's tenant holds each of `objects` under its id,
/// with the same properties.
async fn verify_copied(edge: &dyn SyncTarget, objects: &[Value]) -> Result<()> {
    let mut missing = Vec::new();
    let mut differing = Vec::new();
    for batch in objects.chunks(IMPORT_BATCH_SIZE) {
        let ids: Vec<String> = batch
            .iter()
            .filter_map(|o| o["id"].as_str().map(str::to_string))
            .collect();
        let held: BTreeMap<String, Value> = edge
            .objects_by_id(&ids, false)
            .await?
            .into_iter()
            .filter_map(|o| Some((o["id"].as_str()?.to_string(), o)))
            .collect();
        for object in batch {
            let id = object["id"].as_str().unwrap_or_default().to_string();
            match held.get(&id) {
                None => missing.push(id),
                Some(copy) if !same_properties(&copy["properties"], &object["properties"]) => {
                    differing.push(id)
                }
                Some(_) => {}
            }
        }
    }
    if missing.is_empty() && differing.is_empty() {
        return Ok(());
    }
    Err(eyre!(
        "the copy did not check out, so nothing is marked and nothing will be purged: {} missing from the edge's tenant ({}), {} held differently ({})",
        missing.len(),
        shown(missing.iter()),
        differing.len(),
        shown(differing.iter())
    ))
}

/// What a purge found and did.
#[derive(Debug, Default)]
pub struct Purged {
    pub survey: Survey,
    /// Memory ids purged.
    pub purged: usize,
    pub applied: bool,
}

/// The purge step; with `apply` false, nothing is written. Run only with
/// every hub on `[embeddings] keep_previous = false` ([`open`] refuses
/// otherwise), so that no sync of the hub's carries a memory between its
/// classes while it runs; it looks again after, and fails if any is held.
pub async fn purge(stores: &Stores, agents: &[String], apply: bool) -> Result<Purged> {
    if !stores.marker.marked().await? {
        return Err(eyre!(
            "the edge's memories are not marked as copied to its tenant ({MOVED_MARKER}); nothing is purged before a checked copy. Run the copy step first"
        ));
    }
    let first = survey(stores, agents, None).await?;
    if let Some(refusal) = first.refusal() {
        return Err(eyre!("nothing was purged: {refusal}"));
    }
    let ids: Vec<MemoryId> = first.to_purge.iter().cloned().map(MemoryId).collect();
    let mut purged = Purged {
        survey: first,
        purged: 0,
        applied: apply,
    };
    if !apply || ids.is_empty() {
        return Ok(purged);
    }
    stores.purge_embedded.purge(&ids).await?;
    stores.purge_weaviate.purge(&ids).await?;
    purged.purged = ids.len();
    let left = survey(stores, agents, None).await?;
    if let Some(refusal) = left.refusal() {
        return Err(eyre!("purging stopped: {refusal}"));
    }
    if !left.to_purge.is_empty() {
        return Err(eyre!(
            "{} edge memories are still held after the purge: {}. A sync of the hub's brought them back: is every hub replica on keep_previous = false? Run the purge again once it is",
            left.to_purge.len(),
            shown(left.to_purge.iter())
        ));
    }
    Ok(purged)
}

/// The edge's own vellev tenant, as its `[vellev]` names it: the copy
/// makes the collection exactly as the edge's boot does.
#[derive(Debug, Clone)]
pub struct EdgeTenant {
    pub url: String,
    /// A key of the tenant's own; never printed.
    pub api_key: String,
    pub tenant: String,
    pub model: String,
    pub dimensions: u32,
    pub query_prefix: Option<String>,
}

/// [`MOVED_MARKER`] in the edge's tenant.
struct TenantMarker(workforce_vellev::Tenants);

#[async_trait]
impl Marker for TenantMarker {
    async fn marked(&self) -> Result<bool> {
        workforce_vellev::index_marked(&self.0.reads, MOVED_MARKER).await
    }
    async fn mark(&self) -> Result<()> {
        workforce_vellev::index_mark(&self.0.writes, MOVED_MARKER).await
    }
}

/// `Err` naming why `agents` cannot be taken for the edge's in this hub:
/// none named, or one the hub runs (a purge would reach its memories of
/// customers it serves under `ext:` keys).
pub fn check_agents(config: &WorkforceConfig, agents: &[String]) -> Result<()> {
    if agents.is_empty() {
        return Err(eyre!(
            "name the edge's agents (--agent), as its [[agents]] id"
        ));
    }
    let agents_dir = std::path::Path::new(&config.hub.data_dir).join("agents");
    for agent in agents {
        let configured = config.agents.iter().any(|a| &a.id == agent)
            || config
                .teams
                .iter()
                .any(|t| t.members.iter().any(|m| &m.id == agent));
        if configured || agents_dir.join(agent).exists() {
            return Err(eyre!(
                "{agent:?} is one of this hub's agents, not only the edge's: its memories cannot be told apart by owner"
            ));
        }
    }
    Ok(())
}

/// The stores a move reads and writes, from the hub's config and the
/// edge's tenant. Refused unless memories are shadowed (the only setup
/// with all three stores), the replica is not promoted, the edge's tenant
/// is not the hub's, and `agents` are the edge's alone.
pub async fn open(
    config: &WorkforceConfig,
    edge: &EdgeTenant,
    agents: &[String],
) -> Result<Stores> {
    use workforce_memory::admin::MemoryAdmin;
    if config.memory.backend != workforce_common::settings::SHADOWED {
        return Err(eyre!(
            "[memory] backend is {:?}: the move reads WorkforceMemory, the hub's embedded class and vellev's replica, which only \"shadow\" keeps",
            config.memory.backend
        ));
    }
    if config.embeddings.keep_previous {
        return Err(eyre!(
            "[embeddings] keep_previous is on: the hub carries memories back into WorkforceMemory, which brings back what the edge erased and races a purge. Turn it off on every hub replica (and restart them) first"
        ));
    }
    if edge.tenant.eq_ignore_ascii_case(&config.vellev.tenant) {
        return Err(eyre!(
            "the edge's tenant {:?} is the hub's; the edge needs a tenant of its own",
            edge.tenant
        ));
    }
    check_agents(config, agents)?;
    let (replica, embedded) = crate::embeddings::shadow_stores(config).await?;
    if replica.promoted().await? {
        return Err(eyre!(
            "memories have been promoted to vellev's replica; this move is for the shadow week, before promotion"
        ));
    }
    let weaviate = Arc::new(WeaviateStore::new(
        &config.weaviate.url,
        config.weaviate.api_key.as_deref(),
    ));
    let lock = crate::memory_erasure::Exclusive {
        lock: crate::memory_shadow::replica_lock(config)?,
        wait: crate::memory_erasure::ERASURE_LOCK_WAIT,
        hold: crate::memory_erasure::ERASURE_LOCK_HOLD,
    };
    let embedded_store: Arc<dyn MemoryStore> = embedded.clone();
    let replica_store: Arc<dyn MemoryStore> = replica.clone();
    let shadowed = crate::memory_erasure::ShadowedMemory::new(
        embedded_store,
        Arc::new(crate::memory_erasure::Replica(replica_store)),
        Arc::new(lock),
    );
    let edge_store = VellevStore::new(&edge.url, &edge.api_key, &edge.tenant, DEFAULT_COLLECTION)?
        .with_local_model(&edge.model, edge.dimensions, edge.query_prefix.as_deref());
    edge_store.ensure_collection().await?;
    let marker = TenantMarker(workforce_vellev::connect(
        &edge.url,
        &edge.api_key,
        &edge.tenant,
    )?);
    let weaviate_source: Arc<dyn SyncSource> = weaviate.clone();
    let embedded_source: Arc<dyn SyncSource> = embedded;
    let replica_source: Arc<dyn SyncSource> = replica;
    Ok(Stores {
        hub: vec![
            (HubStore::Weaviate, weaviate_source),
            (HubStore::Embedded, embedded_source),
            (HubStore::Replica, replica_source),
        ],
        purge_embedded: Arc::new(StorePurge(Arc::new(shadowed))),
        purge_weaviate: Arc::new(StorePurge(weaviate)),
        edge: Arc::new(edge_store),
        marker: Arc::new(marker),
    })
}

#[cfg(test)]
#[path = "edge_memory_move_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "edge_memory_move_e2e_tests.rs"]
mod e2e_tests;
