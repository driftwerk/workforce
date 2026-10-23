//! One pass over a Metabase instance, refreshing what its databases contain.
//!
//! The in-request path only learns from responses a turn already paid for, so a
//! cold process guesses once per database before it knows anything. This pass
//! runs on a timer and fills the store ahead of the first query.

use std::sync::Arc;

use serde_json::Value;
use workforce_common::metabase_schema::MetabaseSchemaStore;

use super::client::{self, field_names, qualified_table_name, MetabaseClient};

/// Bounds for one pass.
#[derive(Debug, Clone, Copy)]
pub struct SyncCaps {
    /// Tables per database whose columns are fetched, ranked by usage.
    pub tables_with_columns: usize,
    /// Ceiling on HTTP requests, so a large install cannot run unbounded.
    pub max_requests: usize,
}

/// What a pass did, for the log line and for tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub databases: usize,
    pub tables: usize,
    pub columns_fetched: usize,
    pub requests: usize,
    pub errors: usize,
    /// A cap stopped the pass before every ranked table was described.
    pub truncated: bool,
}

impl SyncReport {
    pub fn merge(self, other: Self) -> Self {
        Self {
            databases: self.databases + other.databases,
            tables: self.tables + other.tables,
            columns_fetched: self.columns_fetched + other.columns_fetched,
            requests: self.requests + other.requests,
            errors: self.errors + other.errors,
            truncated: self.truncated || other.truncated,
        }
    }
}

/// The rows of a `/database` response, which Metabase returns either wrapped in
/// `data` or bare depending on version. Mirrors `metabase.database_list`, whose
/// identical fallback is proven against the live instances.
fn database_rows(list: &Value) -> Vec<Value> {
    list.get("data")
        .and_then(|d| d.as_array())
        .cloned()
        .or_else(|| list.as_array().cloned())
        .unwrap_or_default()
}

fn database_ids(rows: &[Value]) -> Vec<i64> {
    rows.iter()
        .filter_map(|row| row.get("id").and_then(Value::as_i64))
        .collect()
}

fn table_ids(metadata: &Value) -> Vec<(String, i64)> {
    metadata
        .get("tables")
        .and_then(|v| v.as_array())
        .map(|tables| {
            tables
                .iter()
                .filter_map(|entry| {
                    Some((
                        qualified_table_name(entry)?,
                        entry.get("id").and_then(Value::as_i64)?,
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Refresh every database on `client`'s instance into `store`.
///
/// Loads what the store already knows first: the pass builds its own client, so
/// without that load the accumulated usage would be invisible and every pass
/// would describe the same arbitrary tables. A client without a configured
/// site has no identity in the store and is skipped.
///
/// Each database is persisted as it completes rather than once at the end, so a
/// pass that dies halfway keeps what it learned. A database that errors is
/// skipped with its stored rows intact.
pub async fn sync_instance(
    client: &Arc<MetabaseClient>,
    store: &Arc<dyn MetabaseSchemaStore>,
    caps: SyncCaps,
) -> SyncReport {
    let Some(key) = client.store_key() else {
        return SyncReport::default();
    };
    let site_url = key.as_str();
    client.reload_from_store().await;
    let Ok(list) = client.get("/database", &[]).await else {
        return SyncReport {
            requests: 1,
            errors: 1,
            ..Default::default()
        };
    };
    let rows = database_rows(&list);
    client.record_engines(&rows);
    let ids = client.databases_by_need(&database_ids(&rows));
    let instance = key.clone();
    if let Err(e) = store
        .upsert_engines(&instance, site_url, &client::engine_pairs(&rows))
        .await
    {
        tracing::warn!(error = %e, instance = %instance, "Metabase engine upsert failed");
    }
    if let Err(e) = store.prune_databases(&instance, &ids).await {
        tracing::warn!(error = %e, instance = %instance, "Metabase schema prune failed");
    }

    let mut report = SyncReport {
        requests: 1,
        ..Default::default()
    };
    for database_id in ids {
        match caps.max_requests.saturating_sub(report.requests) {
            0 => {
                report.truncated = true;
                break;
            }
            remaining => {
                report = report.merge(
                    sync_database(
                        client,
                        store,
                        site_url,
                        &instance,
                        database_id,
                        caps.tables_with_columns,
                        remaining,
                    )
                    .await,
                )
            }
        }
    }
    report
}

async fn sync_database(
    client: &Arc<MetabaseClient>,
    store: &Arc<dyn MetabaseSchemaStore>,
    site_url: &str,
    instance: &str,
    database_id: i64,
    tables_with_columns: usize,
    request_budget: usize,
) -> SyncReport {
    let Ok(metadata) = client
        .get(&format!("/database/{database_id}/metadata"), &[])
        .await
    else {
        return SyncReport {
            requests: 1,
            errors: 1,
            ..Default::default()
        };
    };
    client.record_schema(database_id, &metadata);

    let ids = table_ids(&metadata);
    let ranked: Vec<(String, i64)> = client
        .tables_to_describe(database_id, tables_with_columns)
        .into_iter()
        .filter_map(|table| {
            ids.iter()
                .find(|(name, _)| *name == table)
                .map(|(name, id)| (name.clone(), *id))
        })
        .collect();
    let affordable = request_budget.saturating_sub(1);
    let truncated = ranked.len() > affordable;
    let wanted: Vec<(String, i64)> = ranked.into_iter().take(affordable).collect();

    let (fetched, requests) = describe_tables(client, database_id, wanted).await;

    let schema = client.database_schema(database_id);
    let errors = match store
        .upsert_database(instance, site_url, database_id, &schema)
        .await
    {
        Ok(()) => 0,
        Err(e) => {
            tracing::warn!(error = %e, instance = %instance, database_id, "Metabase schema upsert failed");
            1
        }
    };

    SyncReport {
        databases: 1,
        tables: schema.tables.len(),
        columns_fetched: fetched,
        requests: requests + 1,
        errors,
        truncated,
    }
}

/// Fetch and record columns for each table, returning how many were described
/// and how many requests it cost.
async fn describe_tables(
    client: &Arc<MetabaseClient>,
    database_id: i64,
    wanted: Vec<(String, i64)>,
) -> (usize, usize) {
    let mut fetched = 0usize;
    let mut requests = 0usize;
    for (name, table_id) in wanted {
        requests += 1;
        let Ok(payload) = client
            .get(&format!("/table/{table_id}/query_metadata"), &[])
            .await
        else {
            continue;
        };
        let columns = field_names(&payload);
        if !columns.is_empty() {
            client.record_columns(database_id, &name, columns);
            fetched += 1;
        }
    }
    (fetched, requests)
}

/// Bring every live client's tool schema up to date. A full pass reloads the
/// store into each client before publishing; otherwise only buffered usage is
/// flushed and clients that have never published get their first publish.
pub async fn publish_live_clients(full: bool) -> usize {
    let clients = client::live_clients();
    for client in &clients {
        match full || !client.is_published() || client.knows_no_schema() {
            true => client.reload_and_publish().await,
            false => client.flush_usage().await,
        }
    }
    clients.len()
}

/// The instances worth syncing this pass: one per distinct site, keeping the
/// first client seen for each. Sites are compared normalised, so two agents
/// spelling one server differently sync it once.
///
/// Agents commonly share one Metabase, and syncing it once per agent would
/// multiply the request count for identical results.
pub fn dedupe_by_site_url<T>(candidates: Vec<(String, T)>) -> Vec<(String, T)> {
    let mut seen: Vec<String> = Vec::new();
    candidates
        .into_iter()
        .filter(|(site_url, _)| {
            let site = client::normalised_site(site_url);
            let fresh = !seen.contains(&site);
            if fresh {
                seen.push(site);
            }
            fresh
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::FakeSchemaStore;
    use super::super::{MetabaseSkill, MetabaseSkillConfig};
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use workforce_common::metabase_schema::DbSchema;

    async fn metabase_with_two_databases() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/database"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{ "id": 1, "engine": "postgres" }, { "id": 2, "engine": "postgres" }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/database/1/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tables": [
                    { "id": 9, "name": "people", "schema": "public" },
                    { "id": 10, "name": "shipments", "schema": "public" }
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/table/9/query_metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "fields": [{ "name": "id" }, { "name": "email_address" }]
            })))
            .mount(&server)
            .await;
        server
    }

    fn client_for(server: &MockServer) -> Arc<MetabaseClient> {
        Arc::new(MetabaseClient::new("k", &server.uri(), 2000).expect("client"))
    }

    #[tokio::test]
    async fn a_pass_records_every_database_and_prunes_what_is_gone() {
        let server = metabase_with_two_databases().await;
        Mock::given(method("GET"))
            .and(path("/api/database/2/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tables": [{ "id": 11, "name": "audit" }]
            })))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let store: Arc<dyn MetabaseSchemaStore> = Arc::new(FakeSchemaStore::default());

        let report = sync_instance(
            &client,
            &store,
            SyncCaps {
                tables_with_columns: 25,
                max_requests: 200,
            },
        )
        .await;

        assert_eq!(report.databases, 2);
        assert_eq!(report.tables, 3, "two tables in db 1, one in db 2");
        assert_eq!(report.errors, 0);
        assert!(!report.truncated);
    }

    #[tokio::test]
    async fn a_failing_database_does_not_abort_the_pass() {
        let server = metabase_with_two_databases().await;
        Mock::given(method("GET"))
            .and(path("/api/database/2/metadata"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let fake = Arc::new(FakeSchemaStore::default());
        let store: Arc<dyn MetabaseSchemaStore> = fake.clone();

        let report = sync_instance(
            &client,
            &store,
            SyncCaps {
                tables_with_columns: 25,
                max_requests: 200,
            },
        )
        .await;

        assert_eq!(report.errors, 1, "the broken database is counted");
        assert_eq!(report.databases, 1, "the healthy one still synced");
        let upserted: Vec<i64> = fake
            .upserts
            .lock()
            .unwrap()
            .iter()
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(
            upserted,
            vec![1],
            "a database that could not be read is never written, so its stored rows survive"
        );
        assert_eq!(
            fake.pruned.lock().unwrap().as_slice(),
            &[vec![1, 2]],
            "pruning keeps every database the instance still lists, healthy or not"
        );
    }

    #[tokio::test]
    async fn the_request_cap_truncates_rather_than_overruns() {
        let server = metabase_with_two_databases().await;
        Mock::given(method("GET"))
            .and(path("/api/database/2/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "tables": [] })))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let store: Arc<dyn MetabaseSchemaStore> = Arc::new(FakeSchemaStore::default());

        let report = sync_instance(
            &client,
            &store,
            SyncCaps {
                tables_with_columns: 25,
                max_requests: 2,
            },
        )
        .await;

        assert!(report.truncated, "the cap is reported, not hidden");
        assert!(
            report.requests <= 3,
            "the pass stops near its budget rather than running on: {}",
            report.requests
        );
    }

    #[tokio::test]
    async fn a_pass_persists_engines_for_every_listed_database() {
        let server = metabase_with_two_databases().await;
        let client = client_for(&server);
        let fake = Arc::new(FakeSchemaStore::default());
        let store: Arc<dyn MetabaseSchemaStore> = fake.clone();

        let report = sync_instance(
            &client,
            &store,
            SyncCaps {
                tables_with_columns: 25,
                max_requests: 2,
            },
        )
        .await;

        assert!(report.truncated, "the second database is never described");
        assert_eq!(
            fake.engines.lock().unwrap().as_slice(),
            &[(1, "postgres".to_string()), (2, "postgres".to_string())],
            "every listed database's engine is persisted, described or not"
        );
        let upserts = fake.upserts.lock().unwrap();
        assert_eq!(upserts.len(), 1);
        assert_eq!(upserts[0].0, 1);
        assert_eq!(
            upserts[0].1.engine.as_deref(),
            Some("postgres"),
            "the described database's row carries its engine"
        );
    }

    #[tokio::test]
    async fn columns_are_fetched_for_ranked_tables_only() {
        let server = metabase_with_two_databases().await;
        Mock::given(method("GET"))
            .and(path("/api/database/2/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "tables": [] })))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let fake = Arc::new(FakeSchemaStore::default());
        let store: Arc<dyn MetabaseSchemaStore> = fake.clone();

        let report = sync_instance(
            &client,
            &store,
            SyncCaps {
                tables_with_columns: 1,
                max_requests: 200,
            },
        )
        .await;

        assert_eq!(
            report.columns_fetched, 1,
            "a cold client still describes its ranked table: selecting on \
             already-known columns would mean a fresh database never acquires any"
        );
        let stored: BTreeMap<i64, DbSchema> =
            fake.upserts.lock().unwrap().iter().cloned().collect();
        assert!(
            stored[&1].columns.contains_key("public.people"),
            "the described table's columns reach the store"
        );
    }

    #[tokio::test]
    async fn publish_live_clients_publishes_the_never_published_and_reloads_on_a_full_pass() {
        let skill = MetabaseSkill::new(MetabaseSkillConfig {
            api_key: Some("mb_x".to_string()),
            site_url: Some("https://mb.publish.example.com".to_string()),
            ..MetabaseSkillConfig::default()
        })
        .unwrap();
        let _tools = skill.tools();
        let client = Arc::clone(&skill.clients()["default"]);
        client.record_schema(
            2,
            &json!({"tables": [{"name": "people", "fields": [{"name": "id"}]}]}),
        );
        assert!(!client.is_published());

        publish_live_clients(false).await;
        let first = client.published_description().expect("first publish");
        assert!(first.contains("people(id)"), "{first}");

        client.record_columns(2, "people", vec!["id".into(), "email".into()]);
        publish_live_clients(false).await;
        assert_eq!(
            client.published_description().as_deref(),
            Some(&*first),
            "a reconcile tick leaves an already-published client alone"
        );

        publish_live_clients(true).await;
        let republished = client.published_description().expect("republished");
        assert!(republished.contains("people(id, email)"), "{republished}");
    }

    /// Metabase wraps this list in `data` on some versions and not others; both
    /// must yield ids *and* engines, or a bare-array instance would sync its
    /// databases while silently harvesting no dialects.
    #[test]
    fn database_rows_and_ids_read_both_shapes() {
        let wrapped = json!({"data": [{"id": 2}, {"id": 5}, {"name": "no id"}]});
        assert_eq!(database_ids(&database_rows(&wrapped)), vec![2, 5]);
        let bare = json!([{"id": 7, "engine": "postgres"}]);
        let rows = database_rows(&bare);
        assert_eq!(database_ids(&rows), vec![7]);
        assert_eq!(
            rows.len(),
            1,
            "engines are harvested from the bare shape too"
        );
        assert!(database_rows(&json!({})).is_empty());
    }

    #[test]
    fn table_ids_qualify_by_schema_and_need_an_id() {
        let metadata = serde_json::json!({"tables": [
            {"id": 1, "name": "users", "schema": "public"},
            {"id": 2, "name": "shipments", "schema": ""},
            {"name": "no_id"},
            {"id": 4}
        ]});
        assert_eq!(
            table_ids(&metadata),
            vec![
                ("public.users".to_string(), 1),
                ("shipments".to_string(), 2)
            ]
        );
    }

    #[test]
    fn dedupe_keeps_the_first_client_per_site_url() {
        let deduped = dedupe_by_site_url(vec![
            ("https://mb.example.com".to_string(), "atlas"),
            ("https://mb.example.com".to_string(), "bernard"),
            ("https://other.example.com".to_string(), "candy"),
        ]);
        assert_eq!(
            deduped,
            vec![
                ("https://mb.example.com".to_string(), "atlas"),
                ("https://other.example.com".to_string(), "candy")
            ]
        );
    }

    #[test]
    fn reports_merge_additively_and_latch_truncation() {
        let a = SyncReport {
            databases: 1,
            tables: 10,
            requests: 3,
            ..Default::default()
        };
        let b = SyncReport {
            databases: 1,
            columns_fetched: 2,
            requests: 4,
            errors: 1,
            truncated: true,
            ..Default::default()
        };
        let merged = a.merge(b);
        assert_eq!(merged.databases, 2);
        assert_eq!(merged.tables, 10);
        assert_eq!(merged.requests, 7);
        assert_eq!(merged.errors, 1);
        assert!(merged.truncated);
    }
}
