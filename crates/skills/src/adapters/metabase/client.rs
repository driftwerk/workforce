//! Metabase REST API HTTP client wrapper.
//!
//! Auth: `X-API-KEY` header (Metabase v0.49+). All requests target
//! `{site_url}/api{path}`. Error mapping covers the common auth/permission/rate
//! failure modes so tool callers get actionable messages.

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

use eyre::{eyre, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use serde_json::Value;
use workforce_agent::tools::RateLimitedError;
use workforce_common::metabase_schema::{
    DbSchema, InstanceSchema, MetabaseSchemaStore, TableUsage,
};
use workforce_common::settings::SecretStore;

use super::schema_prompt;
use crate::adapters::auth::{field_paths, resolve_companion, resolve_credential};
use crate::adapters::utils::retry_after_secs;

const API_KEY_HEADER: &str = "x-api-key";

/// Thin wrapper around `reqwest::Client` for the Metabase REST API.
///
/// Construct via [`MetabaseClient::new`] for a static API key, or
/// [`MetabaseClient::with_store`] to resolve
/// `skills/metabase/instances/{workspace}/api_key` per request.
pub struct MetabaseClient {
    client: reqwest::Client,
    base_url: Option<String>,
    site_url: Option<String>,
    pub max_query_rows: u64,
    static_auth: Option<HeaderValue>,
    store: Option<Arc<dyn SecretStore>>,
    workspace: String,
    /// Everything this process has learned about the instance's schema, from
    /// responses a turn already paid for and from the store. Live state: read
    /// by [`MetabaseClient::publish`], never by the tool schema directly.
    snapshot: parking_lot::RwLock<InstanceSchema>,
    /// Usage counted since the last flush. Buffered rather than written
    /// through so no query pays for a database round-trip; flushed as deltas,
    /// which compose across replicas instead of clobbering.
    pending_usage: parking_lot::RwLock<BTreeMap<(i64, String), TableUsage>>,
    schema_store: Option<Arc<dyn MetabaseSchemaStore>>,
    /// What the native-query tool schema says about this instance. Written
    /// only by [`MetabaseClient::publish`], so the tool definition — and the
    /// prompt-cache prefix it sits in — holds still between publishes however
    /// much the live snapshot learns in the meantime.
    published: parking_lot::RwLock<Option<Arc<str>>>,
}

/// Clients whose tools are registered somewhere, so the hub can publish to
/// them. Weak: a client lives exactly as long as the skill that built it.
static LIVE: parking_lot::Mutex<Vec<Weak<MetabaseClient>>> = parking_lot::Mutex::new(Vec::new());

/// Track a client whose tools are registered somewhere, so the hub can publish
/// to it. Tracking is idempotent, so however often a skill builds its tools
/// each client is published once per tick.
pub(crate) fn track_live(client: &Arc<MetabaseClient>) {
    let weak = Arc::downgrade(client);
    let mut live = LIVE.lock();
    live.retain(|w| w.strong_count() > 0);
    if !live.iter().any(|w| Weak::ptr_eq(w, &weak)) {
        live.push(weak);
    }
}

/// Every tracked client still alive; dead entries are pruned on the way.
pub fn live_clients() -> Vec<Arc<MetabaseClient>> {
    let mut live = LIVE.lock();
    live.retain(|w| w.strong_count() > 0);
    live.iter().filter_map(Weak::upgrade).collect()
}

/// A Metabase site as the store knows it: no scheme, no trailing slash, the
/// host in lower case. `http://Metabase.example.com/` and
/// `https://metabase.example.com` are one server.
pub fn normalised_site(site_url: &str) -> String {
    let trimmed = site_url.trim();
    let rest = trimmed
        .split_once("://")
        .map_or(trimmed, |(_, rest)| rest)
        .trim_end_matches('/');
    match rest.split_once('/') {
        Some((host, path)) => format!("{}/{path}", host.to_lowercase()),
        None => rest.to_lowercase(),
    }
}

/// Column names out of a `fields` array, as both the database metadata and the
/// per-table endpoint report them.
pub(crate) fn field_names(entry: &Value) -> Vec<String> {
    entry
        .get("fields")
        .and_then(|v| v.as_array())
        .map(|fields| {
            fields
                .iter()
                .filter_map(|f| Some(f.get("name")?.as_str()?.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// A table's name as the snapshot spells it: `schema.name` where the instance
/// reports a schema, the bare name otherwise. Every place that keys on a table
/// goes through here, so columns and usage land on the name the table list
/// carries.
pub(crate) fn qualified_table_name(entry: &Value) -> Option<String> {
    let name = entry.get("name").and_then(Value::as_str)?;
    Some(match entry.get("schema").and_then(Value::as_str) {
        Some(schema) if !schema.is_empty() => format!("{schema}.{name}"),
        _ => name.to_string(),
    })
}

/// `(id, engine)` pairs out of database-list rows; rows lacking either field
/// are ignored.
pub(crate) fn engine_pairs(rows: &[Value]) -> Vec<(i64, String)> {
    rows.iter()
        .filter_map(|row| {
            let id = row.get("id")?.as_i64()?;
            let engine = row.get("engine")?.as_str().filter(|e| !e.is_empty())?;
            Some((id, engine.to_string()))
        })
        .collect()
}

impl MetabaseClient {
    /// Build a new client with a static API key. `site_url` is the
    /// Metabase origin (no `/api` suffix — the client appends `/api`).
    pub fn new(api_key: &str, site_url: &str, max_query_rows: u64) -> Result<Self> {
        let auth =
            HeaderValue::from_str(api_key).map_err(|e| eyre!("Invalid Metabase API key: {e}"))?;
        let client = reqwest::Client::builder().build()?;

        Ok(Self {
            client,
            base_url: Some(api_base(site_url)),
            site_url: Some(site_url.to_string()),
            max_query_rows,
            static_auth: Some(auth),
            store: None,
            workspace: String::new(),
            snapshot: parking_lot::RwLock::new(InstanceSchema::default()),
            pending_usage: parking_lot::RwLock::new(BTreeMap::new()),
            schema_store: None,
            published: parking_lot::RwLock::new(None),
        })
    }

    /// Build a client that resolves its API key per request from `store`.
    pub fn with_store(
        store: Arc<dyn SecretStore>,
        workspace: &str,
        site_url: Option<&str>,
        max_query_rows: u64,
    ) -> Result<Self> {
        let client = reqwest::Client::builder().build()?;
        Ok(Self {
            client,
            base_url: site_url.map(api_base),
            site_url: site_url.map(str::to_string),
            max_query_rows,
            static_auth: None,
            store: Some(store),
            workspace: workspace.to_string(),
            snapshot: parking_lot::RwLock::new(InstanceSchema::default()),
            pending_usage: parking_lot::RwLock::new(BTreeMap::new()),
            schema_store: None,
            published: parking_lot::RwLock::new(None),
        })
    }

    /// Attach the persistent schema store.
    ///
    /// Separate from construction because the store is process-wide
    /// infrastructure while clients are built per agent: threading it through
    /// every constructor would push a Postgres-shaped parameter into paths
    /// that have no use for one.
    pub fn with_schema_store(mut self, store: Option<Arc<dyn MetabaseSchemaStore>>) -> Self {
        self.schema_store = store;
        self
    }

    /// The configured Metabase origin, which identifies the instance in the
    /// store; `None` when it is resolved per request from the vault.
    pub fn site_url(&self) -> Option<&str> {
        self.site_url.as_deref()
    }

    /// The text the native-query tool schema carries, as of the last publish.
    pub fn published_description(&self) -> Option<Arc<str>> {
        self.published.read().clone()
    }

    pub fn is_published(&self) -> bool {
        self.published.read().is_some()
    }

    /// Freeze the snapshot known now — engines included — into the text the
    /// tool schema carries.
    pub fn publish(&self) {
        let rendered = schema_prompt::database_id_description_for(&self.snapshot.read());
        *self.published.write() = Some(Arc::from(rendered));
    }

    /// Push buffered usage to the store, then absorb what the store knows.
    ///
    /// Flushing first means the reload already contains this process's own
    /// counts. Nothing to do without a store or a configured site.
    pub async fn reload_from_store(&self) {
        let (Some(store), Some(key)) = (self.schema_store.as_ref(), self.store_key()) else {
            return;
        };
        self.flush_usage().await;
        match store.load(&key, &key).await {
            Ok(loaded) => self.snapshot.write().absorb(loaded),
            Err(e) => {
                tracing::warn!(error = %e, instance = %self.workspace, "Metabase schema load failed")
            }
        }
    }

    /// Whether no database's names are known yet, so a reload may still
    /// bring the schema the tool should publish.
    pub fn knows_no_schema(&self) -> bool {
        self.snapshot.read().databases.is_empty()
    }

    pub async fn reload_and_publish(&self) {
        self.reload_from_store().await;
        self.publish();
    }

    /// The engine known for a database id, if one has been harvested or
    /// loaded from the store.
    pub fn cached_engine(&self, database_id: i64) -> Option<String> {
        self.snapshot
            .read()
            .databases
            .get(&database_id)
            .and_then(|db| db.engine.clone())
    }

    /// Cache hit, else `GET /database/{id}` and read `engine`; `Ok(None)`
    /// when the field is absent (fail-open), `Err` only on transport/auth
    /// errors.
    pub async fn engine_for(&self, database_id: i64) -> Result<Option<String>> {
        if let Some(engine) = self.cached_engine(database_id) {
            return Ok(Some(engine));
        }
        let data = self.get(&format!("/database/{database_id}"), &[]).await?;
        let engine = data
            .get("engine")
            .and_then(|e| e.as_str())
            .filter(|e| !e.is_empty())
            .map(str::to_string);
        if let Some(ref engine) = engine {
            self.note_engine(database_id, engine.clone());
        }
        Ok(engine)
    }

    fn note_engine(&self, database_id: i64, engine: String) {
        self.snapshot
            .write()
            .databases
            .entry(database_id)
            .or_default()
            .engine = Some(engine);
    }

    /// Harvest `(id, engine)` pairs from database-list rows; rows without
    /// both fields are ignored.
    pub fn record_engines(&self, rows: &[Value]) {
        for (id, engine) in engine_pairs(rows) {
            self.note_engine(id, engine);
        }
    }

    /// All known `(id, engine)` pairs, in id order — for schema hints.
    pub fn known_engines(&self) -> Vec<(i64, String)> {
        self.snapshot.read().known_engines()
    }

    /// Harvest table names from a `/database/{id}/metadata` response, and any
    /// columns the instance inlined with them.
    ///
    /// Replaces the table list wholesale (the response is authoritative) and
    /// merges columns, because a later per-table describe carries columns this
    /// response did not. Columns of tables the response no longer lists are
    /// dropped first: they would otherwise outlive the table and keep being
    /// spelled out in the tool schema, pointing the model at something that is
    /// gone.
    pub fn record_schema(&self, database_id: i64, metadata: &Value) {
        let harvested: Vec<(String, Vec<String>)> = metadata
            .get("tables")
            .and_then(|v| v.as_array())
            .map(|tables| {
                tables
                    .iter()
                    .filter_map(|entry| Some((qualified_table_name(entry)?, field_names(entry))))
                    .collect()
            })
            .unwrap_or_default();
        if harvested.is_empty() {
            return;
        }
        let names: std::collections::HashSet<&str> =
            harvested.iter().map(|(name, _)| name.as_str()).collect();
        let mut snapshot = self.snapshot.write();
        let db = snapshot.databases.entry(database_id).or_default();
        db.columns.retain(|table, _| names.contains(table.as_str()));
        db.tables = harvested.iter().map(|(name, _)| name.clone()).collect();
        db.columns.extend(
            harvested
                .into_iter()
                .filter(|(_, columns)| !columns.is_empty()),
        );
    }

    /// Record the columns of one table, as spelled in [`DbSchema::tables`].
    pub fn record_columns(&self, database_id: i64, table: &str, columns: Vec<String>) {
        if columns.is_empty() {
            return;
        }
        self.snapshot
            .write()
            .databases
            .entry(database_id)
            .or_default()
            .columns
            .insert(table.to_string(), columns);
    }

    /// Every database of this instance known to hold a table of this name:
    /// its id, the table as that database spells it, and its engine.
    pub fn locate_table(&self, name: &str) -> Vec<(i64, String, Option<String>)> {
        let snapshot = self.snapshot.read();
        snapshot
            .locate_table(name)
            .into_iter()
            .map(|(id, table)| {
                let engine = snapshot.databases.get(&id).and_then(|db| db.engine.clone());
                (id, table.to_string(), engine)
            })
            .collect()
    }

    /// What is known about a database's schema, if anything.
    pub fn known_schema(&self, database_id: i64) -> Option<DbSchema> {
        self.snapshot
            .read()
            .databases
            .get(&database_id)
            .filter(|schema| !schema.tables.is_empty())
            .cloned()
    }

    /// Everything recorded for one database, engine included; empty when
    /// unknown.
    pub fn database_schema(&self, database_id: i64) -> DbSchema {
        self.snapshot
            .read()
            .databases
            .get(&database_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Note that a query referenced this table (`fumbled` when the query then
    /// failed on a name the table does not have).
    ///
    /// Counted in memory and against the snapshot, so ranking reflects the
    /// current turn without waiting for a flush.
    pub fn note_table_use(&self, database_id: i64, table: &str, fumbled: bool) {
        let delta = match fumbled {
            true => TableUsage { hits: 0, misses: 1 },
            false => TableUsage { hits: 1, misses: 0 },
        };
        let key = (database_id, table.to_string());
        let mut pending = self.pending_usage.write();
        let entry = pending.entry(key.clone()).or_default();
        *entry = entry.merge(delta);
        drop(pending);
        let mut snapshot = self.snapshot.write();
        let total = snapshot.usage.entry(key).or_default();
        *total = total.merge(delta);
    }

    /// Tables worth spending prompt bytes on, most valuable first.
    pub fn hot_tables(&self, database_id: i64, limit: usize) -> Vec<String> {
        self.snapshot.read().hot_tables(database_id, limit)
    }

    /// Tables the sync should describe, most valuable first.
    pub fn tables_to_describe(&self, database_id: i64, limit: usize) -> Vec<String> {
        self.snapshot.read().tables_to_describe(database_id, limit)
    }

    /// Databases in the order a pass should spend its budget on them.
    pub fn databases_by_need(&self, candidates: &[i64]) -> Vec<i64> {
        self.snapshot.read().databases_by_need(candidates)
    }

    /// The whole snapshot, for the sync pass.
    pub fn snapshot(&self) -> InstanceSchema {
        self.snapshot.read().clone()
    }

    /// The configured instance name.
    pub fn instance(&self) -> &str {
        &self.workspace
    }

    /// What identifies this Metabase in the store: its site, normalised, so
    /// agents naming one server differently share its schema and agents with
    /// different servers under the same instance name never collide. `None`
    /// without a configured site.
    pub fn store_key(&self) -> Option<String> {
        self.site_url.as_deref().map(normalised_site)
    }

    /// Write buffered usage as deltas and clear the buffer.
    ///
    /// The buffer is taken before the write so a concurrent caller cannot
    /// double-count; a failed write puts the deltas back rather than losing a
    /// ranking signal that took real queries to earn.
    pub async fn flush_usage(&self) {
        let (Some(store), Some(key)) = (self.schema_store.as_ref(), self.store_key()) else {
            return;
        };
        let taken = std::mem::take(&mut *self.pending_usage.write());
        if taken.is_empty() {
            return;
        }
        let deltas: Vec<(i64, String, TableUsage)> = taken
            .iter()
            .map(|((db, table), usage)| (*db, table.clone(), *usage))
            .collect();
        if let Err(e) = store.add_usage(&key, &deltas).await {
            tracing::warn!(error = %e, instance = %self.workspace, "Metabase usage flush failed");
            let mut pending = self.pending_usage.write();
            for (key, usage) in taken {
                let entry = pending.entry(key).or_default();
                *entry = entry.merge(usage);
            }
        }
    }

    fn paths(&self, field: &str) -> (String, String) {
        field_paths("metabase", "instances", &self.workspace, field)
    }

    fn build_request(&self) -> Result<(HeaderMap, String)> {
        let (auth, base_url) = match (&self.static_auth, &self.store) {
            (Some(h), _) => (h.clone(), self.base_url.clone()),
            (None, Some(store)) => {
                let (workspace, flat) = self.paths("api_key");
                let api_key = resolve_credential(store.as_ref(), &workspace, &flat)
                    .ok_or_else(|| eyre!("Metabase api_key not configured at {workspace}"))?;
                let (site_ws, site_flat) = self.paths("site_url");
                let base_url =
                    resolve_companion(store.as_ref(), api_key.per_user, &site_ws, &site_flat)
                        .map(|site| api_base(&site))
                        .or_else(|| self.base_url.clone());
                let auth = HeaderValue::from_str(&api_key.value)
                    .map_err(|e| eyre!("Invalid Metabase API key: {e}"))?;
                (auth, base_url)
            }
            (None, None) => return Err(eyre!("Metabase client has no credential store")),
        };
        let base_url = base_url.ok_or_else(|| {
            eyre!(
                "Metabase site_url not configured at {}",
                self.paths("site_url").0
            )
        })?;
        let mut headers = HeaderMap::new();
        headers.insert(HeaderName::from_static(API_KEY_HEADER), auth);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok((headers, base_url))
    }

    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let (headers, base_url) = self.build_request()?;
        let mut req = self.client.get(url(&base_url, path)).headers(headers);
        if !query.is_empty() {
            req = req.query(query);
        }
        Self::handle_response(req.send().await?).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        let (headers, base_url) = self.build_request()?;
        Self::handle_response(
            self.client
                .post(url(&base_url, path))
                .headers(headers)
                .json(&body)
                .send()
                .await?,
        )
        .await
    }

    pub async fn put(&self, path: &str, body: Value) -> Result<Value> {
        let (headers, base_url) = self.build_request()?;
        Self::handle_response(
            self.client
                .put(url(&base_url, path))
                .headers(headers)
                .json(&body)
                .send()
                .await?,
        )
        .await
    }

    async fn handle_response(response: reqwest::Response) -> Result<Value> {
        let status = response.status();
        let retry_after = retry_after_secs(response.headers());
        let text = response.text().await.unwrap_or_default();

        if status.is_success() {
            if text.is_empty() {
                return Ok(Value::Null);
            }
            serde_json::from_str(&text)
                .map_err(|e| eyre!("Metabase returned non-JSON success body: {e}"))
        } else {
            // Try to extract a message from the JSON body.
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| {
                    v.get("message")
                        .and_then(|m| m.as_str().map(String::from))
                        .or_else(|| v.get("cause").and_then(|c| c.as_str().map(String::from)))
                        .or_else(|| v.get("error").and_then(|e| e.as_str().map(String::from)))
                })
                .unwrap_or_else(|| {
                    if text.is_empty() {
                        format!("HTTP {}", status.as_u16())
                    } else {
                        text.clone()
                    }
                });

            match status.as_u16() {
                401 => Err(eyre!(
                    "Authentication failed — check Metabase api_key: {message}"
                )),
                403 => Err(eyre!(
                    "Forbidden (check collection/database permissions): {message}"
                )),
                404 => Err(eyre!("Not found: {message}")),
                429 => Err(
                    RateLimitedError::new(format!("Rate limited: {message}"), retry_after).into(),
                ),
                s if (500..600).contains(&s) => {
                    Err(eyre!("Metabase server error ({s}): {message}"))
                }
                _ => Err(eyre!("Metabase API error ({status}): {message}")),
            }
        }
    }
}

fn api_base(site_url: &str) -> String {
    format!("{}/api", site_url.trim_end_matches('/'))
}

fn url(base_url: &str, path: &str) -> String {
    let sep = if path.starts_with('/') { "" } else { "/" };
    format!("{base_url}{sep}{path}")
}

#[cfg(test)]
mod tests {
    use super::super::test_support::FakeSchemaStore;
    use super::*;

    #[test]
    fn client_builds_and_appends_api_path() {
        let c = MetabaseClient::new("mb_test", "https://mb.example.com/", 2000).unwrap();
        assert_eq!(c.base_url.as_deref(), Some("https://mb.example.com/api"));
    }

    #[test]
    fn url_handles_leading_slash_and_bare() {
        let c = MetabaseClient::new("mb_test", "https://mb.example.com", 2000).unwrap();
        let base = c.base_url.as_deref().unwrap();
        assert_eq!(url(base, "/card"), "https://mb.example.com/api/card");
        assert_eq!(url(base, "card/1"), "https://mb.example.com/api/card/1");
    }

    #[test]
    fn record_schema_harvests_tables_and_any_inlined_columns() {
        let c = MetabaseClient::new("k", "https://mb.example.com", 2000).unwrap();
        c.record_schema(
            2,
            &serde_json::json!({"tables": [
                {"name": "people", "schema": "public",
                 "fields": [{"name": "id"}, {"name": "email_address"}]},
                {"name": "orders", "schema": ""},
                {"schema": "public"}
            ]}),
        );
        let s = c.known_schema(2).expect("schema recorded");
        assert_eq!(
            s.tables,
            vec!["public.people".to_string(), "orders".to_string()],
            "schema-qualified where reported, and a nameless row is dropped"
        );
        assert_eq!(
            s.columns.get("public.people").unwrap(),
            &vec!["id".to_string(), "email_address".to_string()]
        );
        assert!(
            !s.columns.contains_key("orders"),
            "no inlined fields means no column claim"
        );
        assert!(c.known_schema(99).is_none());
    }

    /// A later per-table describe must not be wiped by a subsequent
    /// database-wide refresh that happens not to inline fields.
    /// A table that disappears upstream must take its columns with it: ranking
    /// reads `columns`, so a leftover entry would keep being spelled out in the
    /// tool schema and point the model at a table that no longer exists.
    #[test]
    fn a_dropped_table_does_not_leave_its_columns_behind() {
        let c = MetabaseClient::new("k", "https://mb.example.com", 2000).unwrap();
        c.record_schema(
            2,
            &serde_json::json!({"tables": [{"name": "people"}, {"name": "legacy"}]}),
        );
        c.record_columns(2, "people", vec!["id".into()]);
        c.record_columns(2, "legacy", vec!["old".into()]);
        assert_eq!(c.hot_tables(2, 10).len(), 2);

        c.record_schema(2, &serde_json::json!({"tables": [{"name": "people"}]}));

        let s = c.known_schema(2).unwrap();
        assert_eq!(s.tables, vec!["people".to_string()]);
        assert!(
            !s.columns.contains_key("legacy"),
            "the departed table's columns go with it"
        );
        assert!(
            s.columns.contains_key("people"),
            "the survivor keeps its own"
        );
        assert_eq!(c.hot_tables(2, 10), vec!["people".to_string()]);
    }

    #[test]
    fn a_metadata_refresh_keeps_separately_described_columns() {
        let c = MetabaseClient::new("k", "https://mb.example.com", 2000).unwrap();
        c.record_schema(2, &serde_json::json!({"tables": [{"name": "people"}]}));
        c.record_columns(2, "people", vec!["id".into(), "email_address".into()]);
        c.record_schema(
            2,
            &serde_json::json!({"tables": [{"name": "people"}, {"name": "orders"}]}),
        );
        let s = c.known_schema(2).unwrap();
        assert_eq!(s.tables.len(), 2, "table list is refreshed wholesale");
        assert_eq!(
            s.columns.get("people").map(Vec::len),
            Some(2),
            "columns survive the refresh"
        );
    }

    #[test]
    fn hot_tables_rank_by_use_and_weight_misses_higher() {
        let c = MetabaseClient::new("k", "https://mb.example.com", 2000).unwrap();
        c.record_schema(
            2,
            &serde_json::json!({"tables": [
                {"name": "quiet"}, {"name": "busy"}, {"name": "fumbled"}, {"name": "bare"}
            ]}),
        );
        for t in ["quiet", "busy", "fumbled"] {
            c.record_columns(2, t, vec!["id".into()]);
        }
        for _ in 0..10 {
            c.note_table_use(2, "busy", false);
        }
        c.note_table_use(2, "quiet", false);
        for _ in 0..3 {
            c.note_table_use(2, "fumbled", true);
        }

        let hot = c.hot_tables(2, 10);
        assert_eq!(
            hot,
            vec![
                "fumbled".to_string(), // 3 misses × 5 = 15
                "busy".to_string(),    // 10 hits
                "quiet".to_string(),   // 1 hit
            ],
            "a fumbled table outranks a merely busy one, and a table with no \
             columns is never offered"
        );
        assert_eq!(c.hot_tables(2, 1), vec!["fumbled".to_string()]);
        assert!(c.hot_tables(99, 5).is_empty());
    }

    #[test]
    fn record_engines_harvests_only_complete_rows() {
        let c = MetabaseClient::new("mb_test", "https://mb.example.com", 2000).unwrap();
        c.record_engines(&[
            serde_json::json!({"id": 2, "engine": "mysql", "name": "app"}),
            serde_json::json!({"id": 5, "engine": "postgres"}),
            serde_json::json!({"id": 9}),
            serde_json::json!({"engine": "h2"}),
            serde_json::json!({"id": 11, "engine": ""}),
        ]);
        assert_eq!(c.cached_engine(2).as_deref(), Some("mysql"));
        assert_eq!(c.cached_engine(5).as_deref(), Some("postgres"));
        assert_eq!(c.cached_engine(9), None);
        assert_eq!(c.cached_engine(11), None, "empty engine not cached");
        assert_eq!(
            c.known_engines(),
            vec![(2, "mysql".to_string()), (5, "postgres".to_string())],
            "sorted by id"
        );
    }

    #[test]
    fn database_schema_carries_the_engine_alongside_names() {
        let c = MetabaseClient::new("k", "https://mb.example.com", 2000).unwrap();
        assert_eq!(c.database_schema(2), DbSchema::default());

        c.record_engines(&[serde_json::json!({"id": 2, "engine": "mysql"})]);
        assert_eq!(
            c.known_schema(2),
            None,
            "an engine alone is not a known schema"
        );
        assert_eq!(c.database_schema(2).engine.as_deref(), Some("mysql"));

        c.record_schema(2, &serde_json::json!({"tables": [{"name": "people"}]}));
        let schema = c.database_schema(2);
        assert_eq!(schema.tables, vec!["people".to_string()]);
        assert_eq!(schema.engine.as_deref(), Some("mysql"));
        assert_eq!(engine_pairs(&[serde_json::json!({"id": 2})]), vec![]);
    }

    #[test]
    fn nothing_is_published_until_publish_is_called() {
        let c = MetabaseClient::new("k", "https://mb.example.com", 2000).unwrap();
        c.record_schema(
            2,
            &serde_json::json!({"tables": [{"name": "people", "fields": [{"name": "id"}]}]}),
        );
        c.record_engines(&[serde_json::json!({"id": 2, "engine": "mysql"})]);
        assert!(c.published_description().is_none());
        assert!(!c.is_published());

        c.publish();
        let published = c.published_description().expect("published");
        assert!(c.is_published());
        assert!(published.contains("people("), "{published}");
        assert!(published.contains("Known engines: 2=mysql"), "{published}");
    }

    #[test]
    fn publish_freezes_the_description_against_later_learning() {
        let c = MetabaseClient::new("k", "https://mb.example.com", 2000).unwrap();
        c.record_schema(
            2,
            &serde_json::json!({"tables": [
                {"name": "people", "fields": [{"name": "id"}]},
                {"name": "orders"}
            ]}),
        );
        c.publish();
        let frozen = c.published_description().expect("published");

        for _ in 0..5 {
            c.note_table_use(2, "orders", false);
        }
        c.record_columns(2, "orders", vec!["starts_at".into()]);
        c.record_engines(&[serde_json::json!({"id": 9, "engine": "postgres"})]);
        assert_eq!(
            c.published_description().as_deref(),
            Some(&*frozen),
            "live learning must not move the published text"
        );

        c.publish();
        let republished = c.published_description().expect("published");
        assert_ne!(&*republished, &*frozen);
        assert!(republished.contains("orders(starts_at)"), "{republished}");
        assert!(republished.contains("9=postgres"), "{republished}");
    }

    #[tokio::test]
    async fn reload_from_store_absorbs_the_store_and_keeps_local_only_databases() {
        let fake = Arc::new(FakeSchemaStore::default());
        {
            let mut loaded = fake.loaded.lock().unwrap();
            loaded.databases.insert(
                1,
                DbSchema {
                    tables: vec!["stored".into()],
                    columns: BTreeMap::from([("stored".to_string(), vec!["id".to_string()])]),
                    engine: Some("postgres".into()),
                },
            );
            loaded.databases.insert(
                2,
                DbSchema {
                    tables: vec!["local".into()],
                    ..Default::default()
                },
            );
        }
        let store: Arc<dyn MetabaseSchemaStore> = fake.clone();
        let c = MetabaseClient::new("k", "https://mb.example.com", 2000)
            .unwrap()
            .with_schema_store(Some(store));
        c.record_schema(2, &serde_json::json!({"tables": [{"name": "local"}]}));
        c.record_engines(&[serde_json::json!({"id": 2, "engine": "mysql"})]);
        c.note_table_use(2, "local", true);

        c.reload_and_publish().await;

        assert_eq!(
            c.known_schema(1).map(|s| s.tables),
            Some(vec!["stored".to_string()]),
            "the store's database arrives"
        );
        assert_eq!(c.cached_engine(1).as_deref(), Some("postgres"));
        assert_eq!(
            c.known_schema(2).map(|s| s.tables),
            Some(vec!["local".to_string()]),
            "a database only this process knows survives"
        );
        assert_eq!(
            c.cached_engine(2).as_deref(),
            Some("mysql"),
            "a stored row without an engine keeps the one learned here"
        );
        assert_eq!(
            fake.usage.lock().unwrap().as_slice(),
            &[(2, "local".to_string(), TableUsage { hits: 0, misses: 1 })],
            "pending usage is flushed before the load"
        );
        assert!(c.is_published());
    }

    #[test]
    fn live_clients_tracks_skill_clients_until_they_drop() {
        use super::super::{MetabaseSkill, MetabaseSkillConfig};

        let skill = MetabaseSkill::new(MetabaseSkillConfig {
            api_key: Some("mb_x".to_string()),
            site_url: Some("https://mb.live.example.com".to_string()),
            ..MetabaseSkillConfig::default()
        })
        .unwrap();
        let tools = skill.tools();
        let client = Arc::clone(&skill.clients()["default"]);
        let tracked = |client: &Arc<MetabaseClient>| {
            live_clients().iter().any(|live| Arc::ptr_eq(live, client))
        };
        assert!(tracked(&client), "a skill whose tools exist is live");

        drop(tools);
        drop(skill);
        let weak = Arc::downgrade(&client);
        drop(client);
        // Another test listing the live clients holds this one for as long as
        // its list lives, so the last reference may go a moment later.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while weak.strong_count() > 0 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(
            weak.upgrade().is_none(),
            "the registry holds only a weak reference"
        );
        assert!(
            !live_clients()
                .iter()
                .any(|live| Weak::ptr_eq(&Arc::downgrade(live), &weak)),
            "a dropped client is pruned"
        );
    }

    #[test]
    fn a_site_is_normalised_to_its_host_and_path() {
        for spelling in [
            "http://Metabase.Example.com/",
            "https://metabase.example.com",
            "  https://metabase.example.com//  ",
            "metabase.example.com",
        ] {
            assert_eq!(
                normalised_site(spelling),
                "metabase.example.com",
                "{spelling}"
            );
        }
        assert_eq!(
            normalised_site("https://Example.com:3000/Metabase/"),
            "example.com:3000/Metabase"
        );
        assert_ne!(
            normalised_site("https://a.example.com"),
            normalised_site("https://b.example.com")
        );
    }

    #[tokio::test]
    async fn the_store_is_keyed_by_the_site_not_the_instance_name() {
        let fake = Arc::new(FakeSchemaStore::default());
        let store: Arc<dyn MetabaseSchemaStore> = fake.clone();
        let vault: Arc<dyn SecretStore> = crate::adapters::test_support::InMem::from(&[]);
        let one = MetabaseClient::with_store(
            Arc::clone(&vault),
            "default",
            Some("http://MB.example.com/"),
            2000,
        )
        .unwrap()
        .with_schema_store(Some(Arc::clone(&store)));
        let same = MetabaseClient::with_store(
            Arc::clone(&vault),
            "default",
            Some("https://mb.example.com"),
            2000,
        )
        .unwrap()
        .with_schema_store(Some(Arc::clone(&store)));
        let other =
            MetabaseClient::with_store(vault, "default", Some("https://other.example.com"), 2000)
                .unwrap()
                .with_schema_store(Some(store));
        one.reload_from_store().await;
        same.reload_from_store().await;
        other.reload_from_store().await;
        let keys: Vec<String> = fake
            .keys
            .lock()
            .unwrap()
            .iter()
            .map(|(k, _)| k.clone())
            .collect();
        assert_eq!(
            keys,
            ["mb.example.com", "mb.example.com", "other.example.com"],
            "one server spelled two ways shares a key; two servers named 'default' do not"
        );
    }

    #[tokio::test]
    async fn an_empty_published_schema_is_reloaded_until_the_store_has_one() {
        let fake = Arc::new(FakeSchemaStore::default());
        let store: Arc<dyn MetabaseSchemaStore> = fake.clone();
        let client = Arc::new(
            MetabaseClient::new("k", "https://empty-then-full.example.com", 2000)
                .unwrap()
                .with_schema_store(Some(store)),
        );
        track_live(&client);
        client.reload_and_publish().await;
        assert!(client.is_published() && client.knows_no_schema());
        fake.loaded.lock().unwrap().databases.insert(
            7,
            DbSchema {
                tables: vec!["public.orders".into()],
                ..Default::default()
            },
        );
        super::super::sync::publish_live_clients(false).await;
        assert!(!client.knows_no_schema());
        assert!(
            client
                .published_description()
                .unwrap()
                .contains("public.orders"),
            "the reconcile tick republishes once the store has the schema"
        );
    }

    #[test]
    fn tracking_a_client_twice_lists_it_once() {
        let client =
            Arc::new(MetabaseClient::new("k", "https://mb.twice.example.com", 2000).unwrap());
        track_live(&client);
        track_live(&client);
        let listed = live_clients()
            .iter()
            .filter(|live| Arc::ptr_eq(live, &client))
            .count();
        assert_eq!(
            listed, 1,
            "a client is published once per tick however often its tools are built"
        );
    }

    use crate::adapters::test_support::{InMem, LayeredInMem};

    #[test]
    fn build_headers_falls_back_to_flat_path() {
        let store = InMem::from(&[("skills/metabase/api_key", "mb_flat_key")]);
        let client =
            MetabaseClient::with_store(store, "default", Some("https://mb.example.com"), 2000)
                .unwrap();
        let (headers, base_url) = client.build_request().unwrap();
        assert_eq!(headers.get(API_KEY_HEADER).unwrap(), "mb_flat_key");
        assert_eq!(base_url, "https://mb.example.com/api");
    }

    #[test]
    fn build_headers_prefers_instance_path_over_flat() {
        let store = InMem::from(&[
            ("skills/metabase/api_key", "flat_key"),
            ("skills/metabase/instances/prod/api_key", "instance_key"),
        ]);
        let client =
            MetabaseClient::with_store(store, "prod", Some("https://mb.example.com"), 2000)
                .unwrap();
        let (headers, _) = client.build_request().unwrap();
        assert_eq!(headers.get(API_KEY_HEADER).unwrap(), "instance_key");
    }

    #[test]
    fn a_shared_key_ignores_a_per_user_site() {
        let store = LayeredInMem::new(
            &[("skills/metabase/api_key", "shared_key")],
            &[("skills/metabase/site_url", "https://attacker.example")],
        );
        let client =
            MetabaseClient::with_store(store, "prod", Some("https://mb.example.com/"), 2000)
                .unwrap();
        let (_, base_url) = client.build_request().unwrap();
        assert_eq!(base_url, "https://mb.example.com/api");
    }

    #[test]
    fn a_per_user_key_uses_its_own_site_and_errors_without_any() {
        let store = LayeredInMem::new(
            &[("skills/metabase/site_url", "https://mb.example.com")],
            &[
                ("skills/metabase/instances/prod/api_key", "my_key"),
                (
                    "skills/metabase/instances/prod/site_url",
                    "https://mb.me.example",
                ),
            ],
        );
        let client = MetabaseClient::with_store(store, "prod", None, 2000).unwrap();
        let (_, base_url) = client.build_request().unwrap();
        assert_eq!(base_url, "https://mb.me.example/api");

        let no_site = LayeredInMem::new(&[], &[("skills/metabase/api_key", "my_key")]);
        let client = MetabaseClient::with_store(no_site, "prod", None, 2000).unwrap();
        let err = client.build_request().unwrap_err().to_string();
        assert!(err.contains("site_url not configured"), "{err}");
    }

    #[tokio::test]
    async fn a_429_maps_to_a_typed_rate_limit_with_the_retry_after_hint() {
        let resp = reqwest::Response::from(
            http::Response::builder()
                .status(429)
                .header("retry-after", "15")
                .body(r#"{"message":"Too many requests"}"#)
                .unwrap(),
        );
        let err = MetabaseClient::handle_response(resp).await.unwrap_err();
        let limit = err
            .downcast_ref::<RateLimitedError>()
            .expect("429 should carry the typed rate-limit signal");
        assert_eq!(limit.message, "Rate limited: Too many requests");
        assert_eq!(limit.retry_after_secs, Some(15));
        assert_eq!(
            err.to_string(),
            "Rate limited: Too many requests (retry after 15s)"
        );
    }
}
