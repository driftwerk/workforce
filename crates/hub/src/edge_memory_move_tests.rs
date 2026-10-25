//! The rules the edge's move out of the hub's stores is held to, against
//! stores in memory (the node test drives the real ones).

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use workforce_memory::admin::testing::Held;
use workforce_memory::edge_owner::{external_owner, LESSON_IDENTITY};

use super::*;

const EDGE: &str = "assistant";
const HUB: &str = "allan";

fn customer(n: u32) -> String {
    external_owner("https://accounts.google.com", &format!("sub-{n}"))
}

/// An object `n` holding memory `memory`, of `owner` about `identity`.
fn object(n: u32, memory: &str, owner: &str, identity: &str) -> Value {
    json!({
        "id": format!("00000000-0000-0000-0000-{n:012}"),
        "class": "WorkforceMemory",
        "properties": {
            "memoryId": memory,
            "owner": owner,
            "identityId": identity,
            "content": format!("memory {memory} held by {n}"),
            "createdAt": "2026-01-01T00:00:00Z",
        }
    })
}

fn agents() -> Vec<String> {
    vec![EDGE.to_string()]
}

#[derive(Default)]
struct Flag(AtomicBool);

#[async_trait]
impl Marker for Flag {
    async fn marked(&self) -> Result<bool> {
        Ok(self.0.load(Ordering::SeqCst))
    }
    async fn mark(&self) -> Result<()> {
        self.0.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Purges from the held stores, in order, by memory id; failing after the
/// first store when told to, as a process killed part way.
struct HeldPurge {
    stores: Vec<Arc<Held>>,
    fail_after_first: AtomicBool,
    /// Called after each purge, as a sync of the hub's racing it.
    after: Mutex<Option<Box<dyn FnMut() + Send>>>,
    calls: AtomicUsize,
}

impl HeldPurge {
    fn of(stores: Vec<Arc<Held>>) -> Arc<Self> {
        Arc::new(Self {
            stores,
            fail_after_first: AtomicBool::new(false),
            after: Mutex::new(None),
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl Purges for HeldPurge {
    async fn purge(&self, ids: &[MemoryId]) -> Result<usize> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut purged = 0;
        for (i, store) in self.stores.iter().enumerate() {
            let mut held = store.objects.lock().unwrap();
            let before = held.len();
            held.retain(|o| {
                !ids.iter()
                    .any(|id| o["properties"]["memoryId"] == id.0.as_str())
            });
            purged += before - held.len();
            drop(held);
            if i == 0 && self.fail_after_first.swap(false, Ordering::SeqCst) {
                return Err(eyre!("killed part way"));
            }
        }
        if let Some(after) = self.after.lock().unwrap().as_mut() {
            after();
        }
        Ok(purged)
    }
}

struct World {
    weaviate: Arc<Held>,
    embedded: Arc<Held>,
    replica: Arc<Held>,
    edge: Arc<Held>,
    marker: Arc<Flag>,
    purge_embedded: Arc<HeldPurge>,
    purge_weaviate: Arc<HeldPurge>,
}

impl World {
    /// Customer 1's memory `m1` in all three stores; customer 2's `m2`
    /// erased on the edge, so in the copies only; the edge's lesson `m3`
    /// in `WorkforceMemory`; the hub's `h1` everywhere; an edge object
    /// under another identity, `u1`.
    fn usual() -> Self {
        let c1 = customer(1);
        let c2 = customer(2);
        let weaviate = vec![
            object(1, "m1", EDGE, &c1),
            object(3, "m3", EDGE, LESSON_IDENTITY),
            object(4, "h1", HUB, "user-1"),
            object(5, "u1", EDGE, "something-else"),
        ];
        let copies = || {
            vec![
                object(1, "m1", EDGE, &c1),
                object(2, "m2", EDGE, &c2),
                object(4, "h1", HUB, "user-1"),
            ]
        };
        Self::of(weaviate, copies(), copies())
    }

    fn of(weaviate: Vec<Value>, embedded: Vec<Value>, replica: Vec<Value>) -> Self {
        let weaviate = Arc::new(Held::holding(None, weaviate));
        let embedded = Arc::new(Held::holding(Some("bge"), embedded));
        let replica = Arc::new(Held::holding(Some("bge"), replica));
        Self {
            purge_embedded: HeldPurge::of(vec![Arc::clone(&replica), Arc::clone(&embedded)]),
            purge_weaviate: HeldPurge::of(vec![Arc::clone(&weaviate)]),
            weaviate,
            embedded,
            replica,
            edge: Arc::new(Held::default()),
            marker: Arc::new(Flag::default()),
        }
    }

    fn stores(&self) -> Stores {
        let weaviate: Arc<dyn SyncSource> = self.weaviate.clone();
        let embedded: Arc<dyn SyncSource> = self.embedded.clone();
        let replica: Arc<dyn SyncSource> = self.replica.clone();
        Stores {
            hub: vec![
                (HubStore::Weaviate, weaviate),
                (HubStore::Embedded, embedded),
                (HubStore::Replica, replica),
            ],
            purge_embedded: self.purge_embedded.clone(),
            purge_weaviate: self.purge_weaviate.clone(),
            edge: self.edge.clone(),
            marker: self.marker.clone(),
        }
    }

    fn memories(store: &Held) -> Vec<String> {
        let mut ids: Vec<String> = store
            .objects
            .lock()
            .unwrap()
            .iter()
            .map(|o| o["properties"]["memoryId"].as_str().unwrap().to_string())
            .collect();
        ids.sort();
        ids
    }
}

/// The customers the edge still knows: customer 1 alone; customer 2 was
/// erased on the edge.
fn known() -> BTreeSet<String> {
    [customer(1)].into_iter().collect()
}

#[tokio::test]
async fn a_survey_takes_the_edge_s_objects_and_copies_only_weaviate_s() {
    let world = World::usual();
    let survey = survey(&world.stores(), &agents(), Some(&known()))
        .await
        .unwrap();
    let copied: Vec<&str> = survey
        .to_copy
        .iter()
        .map(|o| o["properties"]["memoryId"].as_str().unwrap())
        .collect();
    assert_eq!(copied, ["m1", "m3"], "the customer's memory and the lesson");
    assert_eq!(
        survey
            .to_purge
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["m1", "m2", "m3"],
        "the erased customer's copies are purged too"
    );
    assert_eq!(
        survey
            .survivors
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["m2"]
    );
    assert_eq!(survey.unclassified, 1, "u1 is left alone and counted");
    assert!(survey.refusal().is_none());
    assert_eq!(survey.held[&HubStore::Weaviate], 2);
    assert_eq!(survey.held[&HubStore::Embedded], 2);
    assert_eq!(survey.held[&HubStore::Replica], 2);
}

#[tokio::test]
async fn a_hub_customer_under_the_same_key_shape_is_not_the_edge_s() {
    // The hub keys external payers `ext:` too; only the owner tells them
    // apart.
    let world = World::of(vec![object(1, "h9", HUB, &customer(7))], vec![], vec![]);
    let survey = survey(&world.stores(), &agents(), Some(&known()))
        .await
        .unwrap();
    assert!(survey.to_copy.is_empty() && survey.to_purge.is_empty());
}

#[tokio::test]
async fn a_memory_shared_with_another_s_object_is_refused_before_anything_is_written() {
    let world = World::usual();
    world
        .embedded
        .objects
        .lock()
        .unwrap()
        .push(object(9, "m1", HUB, "user-1"));
    let err = copy(&world.stores(), &agents(), &known(), true)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("held both"), "{err:#}");
    assert!(world.edge.objects.lock().unwrap().is_empty());
    assert!(!world.marker.marked().await.unwrap());
}

#[tokio::test]
async fn a_dry_run_writes_nothing() {
    let world = World::usual();
    let copied = copy(&world.stores(), &agents(), &known(), false)
        .await
        .unwrap();
    assert_eq!(copied.survey.to_copy.len(), 2);
    assert_eq!(copied.sent, 0);
    assert!(world.edge.objects.lock().unwrap().is_empty());
    assert!(!world.marker.marked().await.unwrap());
    world.marker.mark().await.unwrap();
    let purged = purge(&world.stores(), &agents(), false).await.unwrap();
    assert_eq!(purged.survey.to_purge.len(), 3);
    assert_eq!(world.purge_embedded.calls.load(Ordering::SeqCst), 0);
    assert_eq!(world.purge_weaviate.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_copy_keeps_ids_checks_them_and_marks_the_tenant() {
    let world = World::usual();
    let copied = copy(&world.stores(), &agents(), &known(), true)
        .await
        .unwrap();
    assert_eq!(copied.sent, 2);
    assert_eq!(World::memories(&world.edge), ["m1", "m3"]);
    assert!(world
        .edge
        .get("00000000-0000-0000-0000-000000000001")
        .is_some());
    assert!(world.marker.marked().await.unwrap());
    assert_eq!(
        World::memories(&world.weaviate),
        ["h1", "m1", "m3", "u1"],
        "a copy deletes nothing"
    );
}

#[tokio::test]
async fn a_copy_that_does_not_check_out_is_not_marked_and_nothing_is_purged() {
    let world = World::usual();
    *world.edge.leaves_out.lock().unwrap() = 1;
    let err = copy(&world.stores(), &agents(), &known(), true)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("did not check out"), "{err:#}");
    assert!(!world.marker.marked().await.unwrap());
    let refused = purge(&world.stores(), &agents(), true).await.unwrap_err();
    assert!(
        format!("{refused:#}").contains("copy step first"),
        "{refused:#}"
    );
    assert_eq!(World::memories(&world.weaviate), ["h1", "m1", "m3", "u1"]);
    assert_eq!(World::memories(&world.embedded), ["h1", "m1", "m2"]);
}

#[tokio::test]
async fn a_copy_resumed_after_a_crash_finishes_but_one_into_a_used_tenant_is_refused() {
    let world = World::usual();
    // A crash after the first object was sent.
    world
        .edge
        .objects
        .lock()
        .unwrap()
        .push(normalised(&object(1, "m1", EDGE, &customer(1))).unwrap());
    let copied = copy(&world.stores(), &agents(), &known(), true)
        .await
        .unwrap();
    assert_eq!(copied.already, 1);
    assert!(world.marker.marked().await.unwrap());

    let used = World::usual();
    used.edge
        .objects
        .lock()
        .unwrap()
        .push(object(77, "e1", EDGE, &customer(3)));
    let err = copy(&used.stores(), &agents(), &known(), true)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("not copies"), "{err:#}");
    assert!(!used.marker.marked().await.unwrap());
}

#[tokio::test]
async fn a_purge_takes_the_edge_s_memories_from_every_hub_store_and_leaves_the_hub_s() {
    let world = World::usual();
    copy(&world.stores(), &agents(), &known(), true)
        .await
        .unwrap();
    purge(&world.stores(), &agents(), true).await.unwrap();
    assert_eq!(World::memories(&world.weaviate), ["h1", "u1"]);
    assert_eq!(World::memories(&world.embedded), ["h1"]);
    assert_eq!(World::memories(&world.replica), ["h1"]);
    assert_eq!(
        World::memories(&world.edge),
        ["m1", "m3"],
        "the erased customer's memory is not brought back"
    );
}

#[tokio::test]
async fn once_copied_a_copy_again_is_refused_so_an_erasure_on_the_edge_stays() {
    let world = World::usual();
    copy(&world.stores(), &agents(), &known(), true)
        .await
        .unwrap();
    // The edge, started on its tenant, erases customer 1.
    world
        .edge
        .objects
        .lock()
        .unwrap()
        .retain(|o| o["properties"]["memoryId"] != "m1");
    let err = copy(&world.stores(), &agents(), &known(), true)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("already"), "{err:#}");
    purge(&world.stores(), &agents(), true).await.unwrap();
    assert_eq!(World::memories(&world.edge), ["m3"]);
    assert_eq!(World::memories(&world.weaviate), ["h1", "u1"]);
}

#[tokio::test]
async fn a_purge_killed_part_way_is_finished_by_running_it_again() {
    let world = World::usual();
    copy(&world.stores(), &agents(), &known(), true)
        .await
        .unwrap();
    world
        .purge_embedded
        .fail_after_first
        .store(true, Ordering::SeqCst);
    assert!(purge(&world.stores(), &agents(), true).await.is_err());
    assert_eq!(World::memories(&world.replica), ["h1"], "the replica first");
    assert_eq!(World::memories(&world.weaviate), ["h1", "m1", "m3", "u1"]);
    purge(&world.stores(), &agents(), true).await.unwrap();
    assert_eq!(World::memories(&world.weaviate), ["h1", "u1"]);
    assert_eq!(World::memories(&world.embedded), ["h1"]);
}

#[tokio::test]
async fn a_memory_a_hub_sync_brings_back_fails_the_purge_saying_so() {
    // With keep_previous off nothing should; if anything does, the purge
    // says so rather than reporting success.
    let world = World::usual();
    copy(&world.stores(), &agents(), &known(), true)
        .await
        .unwrap();
    let weaviate = Arc::clone(&world.weaviate);
    *world.purge_weaviate.after.lock().unwrap() = Some(Box::new(move || {
        weaviate
            .objects
            .lock()
            .unwrap()
            .push(object(1, "m1", EDGE, &customer(1)));
    }));
    let err = purge(&world.stores(), &agents(), true).await.unwrap_err();
    assert!(
        format!("{err:#}").contains("still held after the purge"),
        "{err:#}"
    );
}

#[tokio::test]
async fn a_conflict_found_after_the_purge_stops_it_saying_so() {
    let world = World::usual();
    copy(&world.stores(), &agents(), &known(), true)
        .await
        .unwrap();
    let weaviate = Arc::clone(&world.weaviate);
    *world.purge_weaviate.after.lock().unwrap() = Some(Box::new(move || {
        let mut held = weaviate.objects.lock().unwrap();
        held.push(object(1, "m1", EDGE, &customer(1)));
        held.push(object(8, "m1", HUB, "user-1"));
    }));
    let err = purge(&world.stores(), &agents(), true).await.unwrap_err();
    assert!(format!("{err:#}").contains("purging stopped"), "{err:#}");
}

#[tokio::test]
async fn a_customer_the_edge_no_longer_knows_is_purged_and_never_copied() {
    // Customer 1's memory is in WorkforceMemory too, but the edge erased
    // customer 1 (the carry-back may have brought the memory back there).
    let world = World::usual();
    let nobody = BTreeSet::new();
    let copied = copy(&world.stores(), &agents(), &nobody, true)
        .await
        .unwrap();
    assert_eq!(World::memories(&world.edge), ["m3"], "the lesson alone");
    assert_eq!(copied.survey.erased.len(), 1);
    purge(&world.stores(), &agents(), true).await.unwrap();
    assert_eq!(World::memories(&world.weaviate), ["h1", "u1"]);
    assert_eq!(World::memories(&world.edge), ["m3"]);
}

#[tokio::test]
async fn an_edge_agent_s_other_object_sharing_a_memory_is_a_conflict() {
    let world = World::usual();
    world
        .replica
        .objects
        .lock()
        .unwrap()
        .push(object(9, "m1", EDGE, "something-else"));
    let err = copy(&world.stores(), &agents(), &known(), true)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("held both"), "{err:#}");
}

#[tokio::test]
async fn a_copy_held_with_other_content_does_not_check_out() {
    let edge = Held::default();
    edge.objects
        .lock()
        .unwrap()
        .push(object(1, "m1", EDGE, "tampered-identity"));
    let sent = [normalised(&object(1, "m1", EDGE, &customer(1))).unwrap()];
    let err = verify_copied(&edge, &sent).await.unwrap_err();
    assert!(format!("{err:#}").contains("1 held differently"), "{err:#}");
}

#[test]
fn the_hub_s_own_agents_are_refused() {
    let mut config = WorkforceConfig::default();
    config.hub.data_dir = "/nonexistent-edge-move-test".into();
    assert!(check_agents(&config, &[]).is_err());
    assert!(check_agents(&config, &agents()).is_ok());
    config.agents.push(workforce_common::settings::AgentConfig {
        id: EDGE.into(),
        ..Default::default()
    });
    let err = check_agents(&config, &agents()).unwrap_err();
    assert!(
        format!("{err:#}").contains("one of this hub's agents"),
        "{err:#}"
    );
    config.agents.clear();
    config.teams.push(
        toml::from_str(&format!(
            "id = \"t\"\nname = \"t\"\nlead = \"x\"\n[[members]]\nid = \"{EDGE}\"\nrole = \"r\""
        ))
        .unwrap(),
    );
    let err = check_agents(&config, &agents()).unwrap_err();
    assert!(
        format!("{err:#}").contains("one of this hub's agents"),
        "{err:#}"
    );
}

#[tokio::test]
async fn a_move_is_refused_for_the_hub_s_own_tenant_or_without_shadowing() {
    let mut config = WorkforceConfig::default();
    config.hub.data_dir = "/nonexistent-edge-move-test".into();
    config.vellev.tenant = "harbor".into();
    let edge = EdgeTenant {
        url: "http://127.0.0.1:1".into(),
        api_key: "k".into(),
        tenant: "harbor".into(),
        model: "m".into(),
        dimensions: 2,
        query_prefix: None,
    };
    let unshadowed = open(&config, &edge, &agents()).await.err().unwrap();
    assert!(
        format!("{unshadowed:#}").contains("only \"shadow\""),
        "{unshadowed:#}"
    );
    config.memory.backend = workforce_common::settings::SHADOWED.into();
    config.embeddings.keep_previous = false;
    let same = open(&config, &edge, &agents()).await.err().unwrap();
    assert!(format!("{same:#}").contains("is the hub's"), "{same:#}");
}

#[test]
fn an_agent_with_a_workspace_on_the_hub_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("agents").join(EDGE)).unwrap();
    let mut config = WorkforceConfig::default();
    config.hub.data_dir = dir.path().to_string_lossy().into_owned();
    let err = check_agents(&config, &agents()).unwrap_err();
    assert!(
        format!("{err:#}").contains("one of this hub's agents"),
        "{err:#}"
    );
}

#[tokio::test]
async fn a_move_is_refused_while_the_hub_keeps_its_previous_store() {
    let mut config = WorkforceConfig::default();
    config.hub.data_dir = "/nonexistent-edge-move-test".into();
    config.memory.backend = workforce_common::settings::SHADOWED.into();
    config.vellev.tenant = "harbor".into();
    let edge = EdgeTenant {
        url: "http://127.0.0.1:1".into(),
        api_key: "k".into(),
        tenant: "harbor-edge".into(),
        model: "m".into(),
        dimensions: 2,
        query_prefix: None,
    };
    assert!(config.embeddings.keep_previous, "on by default");
    let err = open(&config, &edge, &agents()).await.err().unwrap();
    assert!(format!("{err:#}").contains("keep_previous"), "{err:#}");
    let same = EdgeTenant {
        tenant: "HarBor".into(),
        ..edge
    };
    config.embeddings.keep_previous = false;
    let err = open(&config, &same, &agents()).await.err().unwrap();
    assert!(format!("{err:#}").contains("is the hub's"), "{err:#}");
}
