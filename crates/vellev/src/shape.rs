//! Whether a collection found on the node can serve as a store wants it.

use eyre::Result;
use tracing::warn;
use vellev_core::{Collection, Distance, Embedding, EmbeddingProvider};

/// How vellev's importer names the model of a class it copied from
/// Weaviate with its vectors: `weaviate:<vectorizer>`.
const IMPORTED_MODEL_PREFIX: &str = "weaviate:";

/// A collection that cannot serve as wanted, and why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "vellev collection '{collection}' has {} schema mismatch(es): {}",
    problems.len(),
    problems.join("; ")
)]
pub struct Refused {
    pub collection: String,
    pub problems: Vec<String>,
}

/// Whether `found` holds vectors the importer copied from Weaviate, under
/// its placeholder for a model, where the client supplies the vectors.
fn imported(found: &Collection, wanted: &Embedding) -> bool {
    wanted.provider == EmbeddingProvider::None
        && found.embedding.model.starts_with(IMPORTED_MODEL_PREFIX)
}

/// What keeps `found` from serving as `wanted` describes: a property
/// missing, of another type or not indexed as `wanted` needs, another
/// measure, or another embedding. Indexing a property more than needed,
/// as a collection imported from Weaviate does, is no mismatch, and
/// neither is the importer's placeholder for the model of vectors of the
/// right size that the client supplies. A collection holding vectors
/// copied from Weaviate that the node must embed queries for is refused
/// with `remedy`, which only its store can name: derived data is rebuilt
/// from source, memories are not.
pub fn mismatches(found: &Collection, wanted: &Collection, remedy: &str) -> Vec<String> {
    let properties = wanted.properties.iter().filter_map(|want| {
        let name = &want.name;
        match found.property(name) {
            None => Some(format!("property '{name}' is missing")),
            Some(have) if have.data_type != want.data_type => Some(format!(
                "property '{name}' is {}, expected {}",
                have.data_type, want.data_type
            )),
            Some(have) if want.filterable && !have.filterable => {
                Some(format!("property '{name}' is not filterable"))
            }
            Some(have) if want.searchable && !have.searchable => {
                Some(format!("property '{name}' is not searchable"))
            }
            Some(_) => None,
        }
    });
    let distance = (found.distance != wanted.distance).then(|| {
        format!(
            "distance is {:?}, expected {}",
            found.distance,
            distance_name(wanted.distance)
        )
    });
    let (have, want) = (&found.embedding, &wanted.embedding);
    let node_embeds = want.provider != EmbeddingProvider::None;
    let provider = match (node_embeds, have.provider == EmbeddingProvider::None) {
        (true, true) if have.model.starts_with(IMPORTED_MODEL_PREFIX) => Some(format!(
            "it holds vectors copied from Weaviate ('{}') that the node cannot embed queries to match; {remedy}",
            have.model
        )),
        (true, true) => Some("the collection does not embed for itself".to_string()),
        (false, false) => Some("the collection embeds for itself, not with the hub's vectors".to_string()),
        _ => None,
    };
    let embedding = [
        provider,
        (have.model != want.model && !imported(found, want))
            .then(|| format!("it embeds with '{}', expected '{}'", have.model, want.model)),
        (have.dimensions != want.dimensions).then(|| {
            format!(
                "vectors have {} dimensions, expected {}",
                have.dimensions, want.dimensions
            )
        }),
        (node_embeds && have.properties != want.properties).then(|| {
            format!(
                "it embeds {:?}, expected {:?}",
                have.properties, want.properties
            )
        }),
        (node_embeds && have.query_prefix != want.query_prefix).then(|| {
            format!(
                "its query prefix is {:?}, expected {:?}",
                have.query_prefix, want.query_prefix
            )
        }),
        (node_embeds && have.document_prefix != want.document_prefix).then(|| {
            format!(
                "its document prefix is {:?}, expected {:?}",
                have.document_prefix, want.document_prefix
            )
        }),
    ];
    properties
        .chain(distance)
        .chain(embedding.into_iter().flatten())
        .collect()
}

fn distance_name(distance: Distance) -> &'static str {
    match distance {
        Distance::Cosine => "cosine",
        Distance::Dot => "dot",
        Distance::L2 => "l2",
    }
}

/// [`Refused`] naming every [`mismatches`] of `found`, if it has any.
pub fn checked(found: &Collection, wanted: &Collection, remedy: &str) -> Result<()> {
    let problems = mismatches(found, wanted, remedy);
    if !problems.is_empty() {
        return Err(eyre::Report::new(Refused {
            collection: found.name.to_string(),
            problems,
        }));
    }
    if imported(found, &wanted.embedding) {
        warn!(
            collection = %found.name,
            recorded = %found.embedding.model,
            model = %wanted.embedding.model,
            "collection imported from Weaviate with its vectors; taking them to be this model's, as their size matches"
        );
    }
    Ok(())
}
