//! Deleting by a filter, however many objects it matches.
//!
//! This is the one place a delete by filter is sent. vellev refuses one
//! that matches more than [`DELETE_CAP`] objects unless it says how many it
//! may delete (from 0.2; earlier nodes delete them all in one commit). The
//! 0.1 client cannot say, and a node older than 0.2 refuses a request that
//! does: so a delete that matches more than the cap is done here by id, a
//! page at a time, which every node takes.
//!
//! With vellev-client 0.2, against nodes all of 0.2 or later, [`one`] is
//! to send `limit: Some(DELETE_CAP)` under a new request id each time,
//! and be sent again, summing the counts, until a page deletes fewer than
//! the limit; the listing by id below then goes.

use vellev_client::api::{Change, Code, Outcome, Page};
use vellev_client::{Error, Result, Tenant};
use vellev_core::CollectionName;
use vellev_index::filter::Filter;

use crate::Cap;

/// The most objects one delete by filter may match without saying how
/// many it may delete.
pub const DELETE_CAP: u64 = 8192;

/// Delete every object of `collection` passing `filter`, and return how
/// many went. `reads` and `writes` are one tenant, as reads and as writes
/// reach it (see [`crate::Tenants`]). Whatever the count, no single commit
/// deletes more than [`DELETE_CAP`] objects.
pub async fn delete_where(
    reads: &Tenant,
    writes: &Tenant,
    collection: &CollectionName,
    filter: &Filter,
) -> Result<u64> {
    delete_within(reads, writes, collection, filter, DELETE_CAP).await
}

/// As [`delete_where`], with `cap` for [`DELETE_CAP`].
async fn delete_within(
    reads: &Tenant,
    writes: &Tenant,
    collection: &CollectionName,
    filter: &Filter,
    cap: u64,
) -> Result<u64> {
    let records = reads.collection(collection);
    if records.count(Some(filter)).await? <= cap {
        match one(writes, collection, filter).await {
            // More match by now than did when they were counted.
            Err(error) if error.code() == Some(Code::Invalid) => {}
            done => return done,
        }
    }
    by_id(reads, writes, collection, filter, &Cap::default()).await
}

/// Delete what passes `filter` by id, a page at a time, until a listing
/// finds none left. Returns how many deletes took something away, as the
/// node answered each. A page is as large as the node gives (`cap` learns
/// it from a refusal), and the pages are counted against how many matched
/// at the start: a delete that keeps finding the same objects there, as
/// if the node took none of them away, is given up rather than gone on
/// with for ever.
async fn by_id(
    reads: &Tenant,
    writes: &Tenant,
    collection: &CollectionName,
    filter: &Filter,
    cap: &Cap,
) -> Result<u64> {
    let records = &reads.collection(collection);
    let matched = records.count(Some(filter)).await?;
    let mut deleted = 0;
    let mut listed_in_all = 0;
    loop {
        let listed = cap
            .ask(usize::MAX, |limit| {
                let page = Page {
                    filter: Some(filter.clone()),
                    after: None,
                    limit,
                    vectors: false,
                };
                async move { records.list(&page).await }
            })
            .await?;
        if listed.objects.is_empty() {
            return Ok(deleted);
        }
        listed_in_all += listed.objects.len() as u64;
        // Twice what matched, and a page over: writers may add to what
        // matches meanwhile, but not without end.
        if listed_in_all > matched * 2 + u64::from(cap.limit(usize::MAX)) {
            return Err(Error::Unusable(
                "a delete by id kept finding objects it had deleted; given up",
            ));
        }
        let by_id = listed
            .objects
            .into_iter()
            .map(|object| Change::Delete {
                collection: collection.clone(),
                id: object.id,
            })
            .collect();
        let written = writes.apply(by_id).await?;
        deleted += written
            .outcomes
            .iter()
            .map(|outcome| match outcome {
                Outcome::Deleted { count } => *count,
                Outcome::Unchanged => 0,
                _ => 1,
            })
            .sum::<u64>();
    }
}

/// One delete by filter, of everything that passes it.
async fn one(writes: &Tenant, collection: &CollectionName, filter: &Filter) -> Result<u64> {
    let delete = Change::DeleteWhere {
        collection: collection.clone(),
        filter: filter.clone(),
    };
    let written = writes.apply(vec![delete]).await?;
    match written.outcomes.first() {
        Some(Outcome::Deleted { count }) => Ok(*count),
        _ => Err(Error::Unusable(
            "vellev answered a delete by filter with something else than how many it deleted",
        )),
    }
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use vellev_core::{DataType, ObjectId, Property};

    use super::*;
    use crate::testing::Node;
    use crate::{collection, connect};

    fn owned_by(owner: &str) -> Filter {
        Filter::Eq {
            property: "owner".to_string(),
            value: owner.into(),
        }
    }

    /// A delete that matches more than the cap goes by id, a page at a
    /// time, and one within it in one step: either way all that match go,
    /// counted, and nothing else.
    #[tokio::test]
    async fn everything_matching_is_deleted_whether_or_not_it_is_within_the_cap() {
        let Some(node) = Node::fresh().await else {
            return;
        };
        let tenants = connect(&node.url, &node.key, &node.tenant).unwrap();
        let vectors = vellev_core::Embedding {
            provider: vellev_core::EmbeddingProvider::None,
            model: "notes".into(),
            dimensions: 2,
            query_prefix: None,
            document_prefix: None,
            properties: Vec::new(),
        };
        let notes = collection(
            CollectionName::new("Notes").unwrap(),
            vec![Property::new("owner", DataType::Text)],
            vectors,
        );
        let records = tenants.reads.collection(&notes.name);
        records.create(&notes).await.unwrap();
        let fill = || async {
            let changes = (0..7u128)
                .map(|n| Change::Upsert {
                    collection: notes.name.clone(),
                    id: ObjectId(uuid::Uuid::from_u128(n + 1)),
                    properties: serde_json::json!({ "owner": if n < 5 { "ada" } else { "mo" } })
                        .as_object()
                        .cloned()
                        .unwrap(),
                    vector: Some(vec![1.0, 0.0]),
                })
                .collect();
            tenants.writes.apply(changes).await.unwrap();
        };
        for cap in [2, 5, DELETE_CAP] {
            fill().await;
            let deleted = delete_within(
                &tenants.reads,
                &tenants.writes,
                &notes.name,
                &owned_by("ada"),
                cap,
            )
            .await
            .unwrap();
            assert_eq!(deleted, 5, "cap {cap}");
            assert_eq!(records.count(Some(&owned_by("ada"))).await.unwrap(), 0);
            assert_eq!(records.count(None).await.unwrap(), 2, "the rest stay");
        }
        let ada = owned_by("ada");
        let none = delete_where(&tenants.reads, &tenants.writes, &notes.name, &ada).await;
        assert_eq!(none.unwrap(), 0);
    }

    /// A node that refuses a delete for matching too many, more having
    /// come to match since they were counted, is not asked again the same
    /// way: the delete goes by id.
    #[tokio::test]
    async fn a_delete_refused_for_matching_too_many_goes_by_id() {
        use axum::response::IntoResponse;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listed = std::sync::Arc::new(AtomicUsize::new(0));
        let by_id = std::sync::Arc::new(AtomicUsize::new(0));
        let (lists, deletes) = (listed.clone(), by_id.clone());
        let app =
            axum::Router::new().fallback(move |uri: axum::http::Uri, body: axum::body::Bytes| {
                let (lists, deletes) = (lists.clone(), deletes.clone());
                async move {
                    let path = uri.path();
                    let body = String::from_utf8_lossy(&body).to_string();
                    if path.ends_with("/count") {
                        return axum::Json(serde_json::json!({ "count": 3 })).into_response();
                    }
                    if path.ends_with("/list") {
                        let objects: Vec<_> = match lists.fetch_add(1, Ordering::SeqCst) {
                            0 => (1..=3u128)
                                .map(|n| {
                                    serde_json::json!({
                                        "id": uuid::Uuid::from_u128(n), "properties": {}
                                    })
                                })
                                .collect(),
                            _ => Vec::new(),
                        };
                        return axum::Json(serde_json::json!({ "objects": objects }))
                            .into_response();
                    }
                    if body.contains("delete_where") {
                        let refused = serde_json::json!({ "error": {
                            "code": "invalid",
                            "message": "the filter matches 9000 objects; say how many to delete"
                        }});
                        return (axum::http::StatusCode::BAD_REQUEST, axum::Json(refused))
                            .into_response();
                    }
                    deletes.fetch_add(body.matches("\"op\":\"delete\"").count(), Ordering::SeqCst);
                    let outcomes: Vec<_> = (0..body.matches("\"op\":\"delete\"").count())
                        .map(|_| serde_json::json!({ "outcome": "applied" }))
                        .collect();
                    let written = serde_json::json!({
                        "incarnation": 1, "position": 1, "outcomes": outcomes
                    });
                    axum::Json(written).into_response()
                }
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let tenants = connect(&url, "key", "harbor").unwrap();
        let notes = CollectionName::new("Notes").unwrap();
        let deleted = delete_where(&tenants.reads, &tenants.writes, &notes, &owned_by("ada"))
            .await
            .unwrap();
        assert_eq!(deleted, 3);
        assert_eq!(by_id.load(Ordering::SeqCst), 3, "each by its id");
        assert_eq!(listed.load(Ordering::SeqCst), 2, "until none is left");
    }

    /// A delete counted as matching more than the cap is never sent as one
    /// delete by filter: it goes by id from the start.
    #[tokio::test]
    async fn a_delete_matching_more_than_the_cap_is_never_sent_as_one() {
        use axum::response::IntoResponse;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let unbounded = std::sync::Arc::new(AtomicUsize::new(0));
        let listed = std::sync::Arc::new(AtomicUsize::new(0));
        let (asked, lists) = (unbounded.clone(), listed.clone());
        let app =
            axum::Router::new().fallback(move |uri: axum::http::Uri, body: axum::body::Bytes| {
                let (asked, lists) = (asked.clone(), lists.clone());
                async move {
                    let path = uri.path();
                    if path.ends_with("/count") {
                        let count = serde_json::json!({ "count": DELETE_CAP + 1 });
                        return axum::Json(count).into_response();
                    }
                    if path.ends_with("/list") {
                        let objects: Vec<_> = match lists.fetch_add(1, Ordering::SeqCst) {
                            0 => vec![serde_json::json!({
                                "id": uuid::Uuid::from_u128(1), "properties": {}
                            })],
                            _ => Vec::new(),
                        };
                        return axum::Json(serde_json::json!({ "objects": objects }))
                            .into_response();
                    }
                    if String::from_utf8_lossy(&body).contains("delete_where") {
                        asked.fetch_add(1, Ordering::SeqCst);
                    }
                    // The object was gone already: nothing was deleted.
                    let written = serde_json::json!({
                        "incarnation": 1, "position": 1,
                        "outcomes": [{ "outcome": "unchanged" }]
                    });
                    axum::Json(written).into_response()
                }
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let tenants = connect(&url, "key", "harbor").unwrap();
        let notes = CollectionName::new("Notes").unwrap();
        let ada = owned_by("ada");
        let deleted = delete_where(&tenants.reads, &tenants.writes, &notes, &ada).await;
        assert_eq!(
            deleted.unwrap(),
            0,
            "counted as the node answered, not as listed"
        );
        assert_eq!(unbounded.load(Ordering::SeqCst), 0, "no delete by filter");
        assert_eq!(listed.load(Ordering::SeqCst), 2);
    }

    /// A node that goes on listing what it was told to delete is given up
    /// on, not asked for ever; and a page is as large as the node gives.
    #[tokio::test]
    async fn a_delete_by_id_that_takes_nothing_away_is_given_up() {
        use axum::response::IntoResponse;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listed = std::sync::Arc::new(AtomicUsize::new(0));
        let lists = listed.clone();
        let app =
            axum::Router::new().fallback(move |uri: axum::http::Uri, body: axum::body::Bytes| {
                let lists = lists.clone();
                async move {
                    let path = uri.path();
                    if path.ends_with("/count") {
                        let count = serde_json::json!({ "count": DELETE_CAP + 1 });
                        return axum::Json(count).into_response();
                    }
                    if path.ends_with("/list") {
                        let asked: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        if asked["limit"].as_u64().unwrap() > 50 {
                            let refused = serde_json::json!({ "error": {
                                "code": "invalid",
                                "message": "limit must be from 1 to 50, not 1000"
                            }});
                            return (axum::http::StatusCode::BAD_REQUEST, axum::Json(refused))
                                .into_response();
                        }
                        lists.fetch_add(1, Ordering::SeqCst);
                        let objects: Vec<_> = (1..=50u128)
                            .map(|n| {
                                serde_json::json!({
                                    "id": uuid::Uuid::from_u128(n), "properties": {}
                                })
                            })
                            .collect();
                        return axum::Json(serde_json::json!({ "objects": objects }))
                            .into_response();
                    }
                    let written = serde_json::json!({
                        "incarnation": 1, "position": 1, "outcomes": []
                    });
                    axum::Json(written).into_response()
                }
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let tenants = connect(&url, "key", "harbor").unwrap();
        let notes = CollectionName::new("Notes").unwrap();
        let ada = owned_by("ada");
        let deleting = delete_where(&tenants.reads, &tenants.writes, &notes, &ada);
        let given_up = tokio::time::timeout(std::time::Duration::from_secs(20), deleting)
            .await
            .expect("given up, not gone on with for ever")
            .unwrap_err()
            .to_string();
        assert!(given_up.contains("given up"), "{given_up}");
        let pages = listed.load(Ordering::SeqCst) as u64;
        assert!(
            pages > 2 && pages * 50 <= (DELETE_CAP + 1) * 2 + 100,
            "{pages} pages of 50"
        );
    }
}
