//! Markers. Those recording that memories switched to a collection are
//! kept as Weaviate keeps them, in [`MARKERS`], so that a marker imported
//! from there is found and a later import into the same tenant finds the
//! collection as it would make it: nothing else is kept there. Those
//! recording that an index is complete are workforce's own, kept in
//! [`INDEX_MARKERS`].

use eyre::Result;
use serde_json::json;
use uuid::Uuid;
use vellev_client::api::Change;
use vellev_core::{CollectionName, DataType, Embedding, EmbeddingProvider, ObjectId, Property};

use crate::{classified, collection, is_conflict, Tenants};

/// The collection markers are kept in, named as in Weaviate, so a marker
/// imported from there is found.
pub const MARKERS: &str = "WorkforceMemorySwitch";

/// [`MARKERS`] exactly as vellev's importer makes it from Weaviate's class
/// (`vellev/src/import.rs`, `collection()` and `copied_embedding()`): text
/// filterable and searchable, as Weaviate indexes it by default; vectors
/// supplied by the client under the importer's placeholder for a class
/// without a vectorizer, `weaviate:none`, one dimension, as a class whose
/// objects carry no vector is given.
fn imported_shape() -> Result<vellev_core::Collection> {
    let vectorless = Embedding {
        provider: EmbeddingProvider::None,
        model: "weaviate:none".to_string(),
        dimensions: 1,
        query_prefix: None,
        document_prefix: None,
        properties: Vec::new(),
    };
    Ok(collection(
        CollectionName::new(MARKERS)?,
        vec![
            Property::new("collection", DataType::Text).searchable(),
            Property::new("switchedAt", DataType::Date),
        ],
        vectorless,
    ))
}

/// The marker's id: a stable UUID from the collection name (FNV-1a).
pub fn marker_id(name: &str) -> Uuid {
    let fnv = |seed: u64| {
        name.bytes().fold(seed, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
    };
    let bits =
        (u128::from(fnv(0xcbf2_9ce4_8422_2325)) << 64) | u128::from(fnv(0x8422_2325_cbf2_9ce4));
    Uuid::from_u128(bits)
}

/// The collection index markers are kept in: that a derived index, such
/// as the artifacts', holds everything it is derived from.
pub const INDEX_MARKERS: &str = "WorkforceIndexMarkers";

/// [`INDEX_MARKERS`]: markers named by what they mark, with no vectors.
fn index_shape() -> Result<vellev_core::Collection> {
    let vectorless = Embedding {
        provider: EmbeddingProvider::None,
        model: "none".to_string(),
        dimensions: 1,
        query_prefix: None,
        document_prefix: None,
        properties: Vec::new(),
    };
    Ok(collection(
        CollectionName::new(INDEX_MARKERS)?,
        vec![
            Property::new("collection", DataType::Text),
            Property::new("switchedAt", DataType::Date),
        ],
        vectorless,
    ))
}

/// One of the two collections of markers, and how it is made.
#[derive(Clone, Copy)]
struct Kept {
    name: &'static str,
    shape: fn() -> Result<vellev_core::Collection>,
}

const SWITCHES: Kept = Kept {
    name: MARKERS,
    shape: imported_shape,
};

const INDEXES: Kept = Kept {
    name: INDEX_MARKERS,
    shape: index_shape,
};

impl Kept {
    fn records(self, at: &Tenants) -> Result<vellev_client::Records> {
        Ok(at.reads.collection(&CollectionName::new(self.name)?))
    }

    async fn marked(self, at: &Tenants, name: &str) -> Result<bool> {
        let held = self.records(at)?.get(ObjectId(marker_id(name))).await;
        Ok(held.map_err(|e| classified(e, at))?.is_some())
    }

    /// Make the collection, unless the node holds it already.
    async fn made(self, at: &Tenants) -> Result<vellev_client::Records> {
        let records = self.records(at)?;
        if records
            .schema()
            .await
            .map_err(|e| classified(e, at))?
            .is_none()
        {
            match records.create(&(self.shape)()?).await {
                Err(e) if !is_conflict(&e) => return Err(classified(e, at)),
                _ => {}
            }
        }
        Ok(records)
    }

    /// Insert `value` under `what`, unless something is recorded there
    /// already.
    async fn insert(self, at: &Tenants, what: &str, value: &str) -> Result<()> {
        let marker = Change::Insert {
            collection: CollectionName::new(self.name)?,
            id: ObjectId(marker_id(what)),
            properties: json!({
                "collection": value,
                "switchedAt": chrono::Utc::now().to_rfc3339(),
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
            vector: None,
        };
        match at.reads.apply(vec![marker]).await {
            Err(e) if !is_conflict(&e) => Err(classified(e, at)),
            _ => Ok(()),
        }
    }

    async fn mark(self, at: &Tenants, name: &str) -> Result<()> {
        self.made(at).await?;
        self.insert(at, name, name).await
    }

    /// Record `owner` under `what`, unless something is recorded there
    /// already; returns what is then recorded.
    async fn claim(self, at: &Tenants, what: &str, owner: &str) -> Result<String> {
        let records = self.made(at).await?;
        self.insert(at, what, owner).await?;
        let held = records
            .get(ObjectId(marker_id(what)))
            .await
            .map_err(|e| classified(e, at))?;
        held.and_then(|held| Some(held.properties.get("collection")?.as_str()?.to_string()))
            .ok_or_else(|| eyre::eyre!("the claim on '{what}' cannot be read back"))
    }

    async fn unmark(self, at: &Tenants, name: &str) -> Result<()> {
        match self.records(at)?.delete(ObjectId(marker_id(name))).await {
            Err(e) if !e.is_not_found() => Err(classified(e, at)),
            _ => Ok(()),
        }
    }
}

/// Whether memories are marked as switched to `name` in the tenant `at`.
pub async fn marked(at: &Tenants, name: &str) -> Result<bool> {
    SWITCHES.marked(at, name).await
}

/// Mark memories as switched to `name`; idempotent, keeping when it was
/// first marked.
pub async fn mark(at: &Tenants, name: &str) -> Result<()> {
    SWITCHES.mark(at, name).await
}

/// Take `name`'s switch marker away, if it has one.
pub async fn unmark(at: &Tenants, name: &str) -> Result<()> {
    SWITCHES.unmark(at, name).await
}

/// The key memories' promotion to `name` is marked under, in
/// [`INDEX_MARKERS`]: workforce's own collection, which no import from
/// Weaviate brings, so that only a promotion can make it.
fn promotion(name: &str) -> String {
    format!("promoted:{name}")
}

/// Whether memories have been promoted to `name` in the tenant `at`:
/// served from it, after it was a shadow replica of Weaviate (DW-676).
pub async fn promoted(at: &Tenants, name: &str) -> Result<bool> {
    INDEXES.marked(at, &promotion(name)).await
}

/// Mark memories as promoted to `name`; idempotent.
pub async fn mark_promoted(at: &Tenants, name: &str) -> Result<()> {
    INDEXES.mark(at, &promotion(name)).await
}

/// Take away the mark of memories' promotion to `name`, if it has one: a
/// rollback, once everything written there since is carried back.
pub async fn unmark_promoted(at: &Tenants, name: &str) -> Result<()> {
    INDEXES.unmark(at, &promotion(name)).await
}

/// Whether the index `name` is marked complete in the tenant `at`.
pub async fn index_marked(at: &Tenants, name: &str) -> Result<bool> {
    INDEXES.marked(at, name).await
}

/// Mark the index `name` complete; idempotent.
pub async fn index_mark(at: &Tenants, name: &str) -> Result<()> {
    INDEXES.mark(at, name).await
}

/// Take the index `name`'s marker away, if it has one.
pub async fn index_unmark(at: &Tenants, name: &str) -> Result<()> {
    INDEXES.unmark(at, name).await
}

/// Record that `what` in the tenant `at` is `owner`'s, unless it is
/// recorded as another's already, and return whose it is: for what only
/// one owner may keep in a tenant, as one code graph keeps its code
/// vectors.
pub async fn index_claim(at: &Tenants, what: &str, owner: &str) -> Result<String> {
    INDEXES.claim(at, what, owner).await
}
