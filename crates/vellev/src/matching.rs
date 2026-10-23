//! Whether a collection holds what it should, without reading it.
//!
//! An index derived from something else (artifacts from Postgres, code
//! vectors from the code graph) is compared with its source by a key and a
//! hash kept on each object in two filterable properties. A page of the
//! source is checked by one count: of the objects whose key and hash are
//! both one of the page's pairs. If that is the page's length, the page is
//! held exactly, and nothing was read. Only a page that is not is listed,
//! and only by its own keys, to tell which of it differs.

use std::collections::{HashMap, HashSet};

use vellev_client::api::Page;
use vellev_client::{Records, Result};
use vellev_core::ObjectId;
use vellev_index::filter::Filter;

use crate::Cap;

/// How a page of wanted `(key, hash)` pairs differs from what a collection
/// holds.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Unmatched {
    /// The wanted keys the collection lacks, or holds under another hash.
    pub differing: Vec<String>,
    /// Objects held under a wanted key with another hash than wanted, or
    /// a second time: an earlier version, kept under another id than the
    /// one that replaces it. Each with its key.
    pub stale: Vec<(ObjectId, String)>,
    /// How many objects the collection holds under the wanted keys, within
    /// the scope: summed over the pages of a source, what the collection
    /// holds beyond it is held under keys the source does not have.
    pub held: u64,
}

impl Unmatched {
    pub fn is_empty(&self) -> bool {
        self.differing.is_empty() && self.stale.is_empty()
    }
}

fn eq(property: &str, value: &str) -> Filter {
    Filter::Eq {
        property: property.to_string(),
        value: value.into(),
    }
}

fn within(scope: Option<&Filter>, any: Vec<Filter>) -> Filter {
    let any = Filter::Or(any);
    match scope {
        Some(scope) => Filter::And(vec![scope.clone(), any]),
        None => any,
    }
}

/// How `wanted`, a page of `(key, hash)` pairs with no key twice, differs
/// from what `records` holds within `scope`: by the properties named `key`
/// and `hash`, both filterable. Nothing is listed unless something
/// differs.
pub async fn unmatched(
    records: &Records,
    scope: Option<&Filter>,
    (key, hash): (&str, &str),
    wanted: &[(String, String)],
    cap: &Cap,
) -> Result<Unmatched> {
    if wanted.is_empty() {
        return Ok(Unmatched::default());
    }
    let held = |held| Unmatched {
        held,
        ..Unmatched::default()
    };
    let exactly = wanted
        .iter()
        .map(|(k, h)| Filter::And(vec![eq(key, k), eq(hash, h)]))
        .collect();
    let keyed: Vec<Filter> = wanted.iter().map(|(k, _)| eq(key, k)).collect();
    let (held_exactly, held_at_all) = (
        records.count(Some(&within(scope, exactly))).await?,
        records.count(Some(&within(scope, keyed.clone()))).await?,
    );
    if held_exactly == wanted.len() as u64 && held_at_all == held_exactly {
        return Ok(held(held_at_all));
    }
    // Which: only this page's keys are listed.
    let by_key: HashMap<&str, &str> = wanted
        .iter()
        .map(|(k, h)| (k.as_str(), h.as_str()))
        .collect();
    let filter = within(scope, keyed);
    let mut right: HashSet<String> = HashSet::new();
    let mut stale = Vec::new();
    let mut after = None;
    loop {
        let listed = cap
            .ask(usize::MAX, |limit| {
                let page = Page {
                    filter: Some(filter.clone()),
                    after,
                    limit,
                    vectors: false,
                };
                async move { records.list(&page).await }
            })
            .await?;
        for object in &listed.objects {
            let property = |name: &str| object.properties.get(name).and_then(|v| v.as_str());
            let Some(held_key) = property(key) else {
                continue;
            };
            if by_key.get(held_key).copied() == property(hash) && !right.contains(held_key) {
                right.insert(held_key.to_string());
            } else {
                stale.push((object.id, held_key.to_string()));
            }
        }
        after = listed.next;
        if after.is_none() {
            break;
        }
    }
    Ok(Unmatched {
        differing: wanted
            .iter()
            .filter(|(k, _)| !right.contains(k))
            .map(|(k, _)| k.clone())
            .collect(),
        stale,
        held: held_at_all,
    })
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use vellev_client::api::Change;
    use vellev_core::{CollectionName, DataType, Property};

    use super::*;
    use crate::testing::Node;
    use crate::{collection, connect};

    /// A page held exactly is told by counting alone, however long; one
    /// that is not is told which of its keys differ and which objects are
    /// earlier versions; and what is held outside the scope does not count.
    #[tokio::test]
    async fn a_page_is_compared_by_key_and_hash() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let tenants = connect(&node.url, &node.key, &node.tenant).unwrap();
        let vectorless = vellev_core::Embedding {
            provider: vellev_core::EmbeddingProvider::None,
            model: "none".into(),
            dimensions: 1,
            query_prefix: None,
            document_prefix: None,
            properties: Vec::new(),
        };
        let held = collection(
            CollectionName::new("Held").unwrap(),
            vec![
                Property::new("repo", DataType::Text),
                Property::new("key", DataType::Text),
                Property::new("hash", DataType::Text),
            ],
            vectorless,
        );
        let records = tenants.reads.collection(&held.name);
        records.create(&held).await.unwrap();
        let put = |id: u128, repo: &str, key: &str, hash: &str| Change::Upsert {
            collection: held.name.clone(),
            id: ObjectId(uuid::Uuid::from_u128(id)),
            properties: serde_json::json!({ "repo": repo, "key": key, "hash": hash })
                .as_object()
                .cloned()
                .unwrap(),
            vector: None,
        };
        let page: Vec<(String, String)> = (0..300)
            .map(|n| (format!("k{n:03}"), format!("h{n}")))
            .collect();
        let changes: Vec<Change> = page
            .iter()
            .enumerate()
            .map(|(n, (key, hash))| put(n as u128 + 1, "r", key, hash))
            .collect();
        for chunk in changes.chunks(100) {
            tenants.writes.apply(chunk.to_vec()).await.unwrap();
        }
        let scope = eq("repo", "r");
        let cap = Cap::default();
        let compare = |wanted: Vec<(String, String)>| {
            let (records, scope, cap) = (&records, &scope, &cap);
            async move {
                unmatched(records, Some(scope), ("key", "hash"), &wanted, cap)
                    .await
                    .unwrap()
            }
        };
        let agreed = compare(page.clone()).await;
        assert!(agreed.is_empty(), "300 pairs in one count");
        assert_eq!(agreed.held, 300, "and how many are held under them");
        assert!(compare(Vec::new()).await.is_empty());

        // One changed, one missing, one held twice (an earlier version
        // under another id), and the same keys in another repo.
        let mut wanted = page.clone();
        wanted[5].1 = "another".into();
        wanted.push(("k999".into(), "h999".into()));
        tenants
            .writes
            .apply(vec![
                put(9001, "r", "k007", "earlier"),
                put(9002, "other", "k999", "h999"),
                put(9003, "other", "k005", "another"),
            ])
            .await
            .unwrap();
        let found = compare(wanted).await;
        assert_eq!(found.differing, ["k005", "k999"]);
        let mut stale = found.stale.clone();
        stale.sort();
        assert_eq!(
            stale,
            [
                (ObjectId(uuid::Uuid::from_u128(6)), "k005".to_string()),
                (ObjectId(uuid::Uuid::from_u128(9001)), "k007".to_string())
            ],
            "the changed one's old version, and the one held twice"
        );
        // Every wanted pair held, and an earlier version of one of them
        // besides: told by the two counts differing, and found.
        let beside = compare(page[6..9].to_vec()).await;
        assert!(beside.differing.is_empty(), "{beside:?}");
        assert_eq!(
            beside.stale,
            [(ObjectId(uuid::Uuid::from_u128(9001)), "k007".to_string())]
        );
        assert_eq!(beside.held, 4);
        // The very same pair held twice, under two ids: one is the one
        // wanted, the other is one too many.
        tenants
            .writes
            .apply(vec![put(9004, "r", "k010", "h10")])
            .await
            .unwrap();
        let twice = compare(page[10..12].to_vec()).await;
        assert!(twice.differing.is_empty(), "{twice:?}");
        assert_eq!(twice.stale.len(), 1, "{twice:?}");
        assert_eq!(twice.stale[0].1, "k010");
        assert_eq!(twice.held, 3);
        let unscoped = unmatched(&records, None, ("key", "hash"), &page[..3], &cap)
            .await
            .unwrap();
        assert!(unscoped.is_empty());
    }
}
