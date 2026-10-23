//! `store.*` — the document store as platform tools, for agent turns and
//! workflow tool steps.
//!
//! Every tool takes a `scope` (`user`, `user:<id>`, `agent`, `agent:<id>`,
//! `global`; default `agent`), resolves it against the caller, checks the
//! policy in [`crate::store::Caller::authorise`], and then hands the call to
//! the backend. Errors come back as failures with a short `code` so an
//! agent can branch on `version_conflict` or `claimed` without parsing
//! prose.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use eyre::Result;
use serde_json::{json, Value};
use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::{Tool, ToolResult};

use crate::authz::IsAdminFn;
use crate::hub_core::pipelines::{Access, Caller, Scope, StoreError};
use crate::store::{content, Descriptor, DocumentStore, Query, WriteOp, MAX_BATCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOp {
    Get,
    Set,
    Update,
    Delete,
    List,
    Query,
    Batch,
    Claim,
    Release,
    Describe,
    Described,
    Collections,
    ContentWrite,
    ContentCommit,
    ContentRead,
    ContentStat,
    ContentList,
    ContentDelete,
}

impl StoreOp {
    pub const ALL: [StoreOp; 18] = [
        StoreOp::Get,
        StoreOp::Set,
        StoreOp::Update,
        StoreOp::Delete,
        StoreOp::List,
        StoreOp::Query,
        StoreOp::Batch,
        StoreOp::Claim,
        StoreOp::Release,
        StoreOp::Describe,
        StoreOp::Described,
        StoreOp::Collections,
        StoreOp::ContentWrite,
        StoreOp::ContentCommit,
        StoreOp::ContentRead,
        StoreOp::ContentStat,
        StoreOp::ContentList,
        StoreOp::ContentDelete,
    ];

    fn name(self) -> &'static str {
        match self {
            StoreOp::Get => "store.get",
            StoreOp::Set => "store.set",
            StoreOp::Update => "store.update",
            StoreOp::Delete => "store.delete",
            StoreOp::List => "store.list",
            StoreOp::Query => "store.query",
            StoreOp::Batch => "store.batch",
            StoreOp::Claim => "store.claim",
            StoreOp::Release => "store.release",
            StoreOp::Describe => "store.describe",
            StoreOp::Described => "store.described",
            StoreOp::Collections => "store.collections",
            StoreOp::ContentWrite => "store.content_write",
            StoreOp::ContentCommit => "store.content_commit",
            StoreOp::ContentRead => "store.content_read",
            StoreOp::ContentStat => "store.content_stat",
            StoreOp::ContentList => "store.content_list",
            StoreOp::ContentDelete => "store.content_delete",
        }
    }

    /// The op a `wf_store_call` request names (`get`, `set`, …).
    pub fn parse(op: &str) -> Option<StoreOp> {
        Self::ALL
            .into_iter()
            .find(|o| o.name().strip_prefix("store.") == Some(op))
    }

    pub fn access(self) -> Access {
        match self {
            StoreOp::Get
            | StoreOp::List
            | StoreOp::Query
            | StoreOp::Described
            | StoreOp::Collections
            | StoreOp::ContentRead
            | StoreOp::ContentStat
            | StoreOp::ContentList => Access::Read,
            _ => Access::Write,
        }
    }

    fn description(self) -> &'static str {
        match self {
            StoreOp::Get => "Read one document from the document store by scope, collection and id. \
                             Returns the document with its version, or found=false.",
            StoreOp::Set => "Create or replace a document in the document store. The body is any JSON \
                             object (not secrets — those go in the vault). Pass if_version=0 to create \
                             only, or the version you last read to write only if nothing changed since \
                             (a version_conflict failure means re-read and retry). ttl_secs expires it.",
            StoreOp::Update => "Merge fields into an existing document: top-level keys in body replace \
                                the stored ones, a null value removes a key. Fails with not_found when \
                                the document does not exist; if_version works as in store.set.",
            StoreOp::Delete => "Delete one document. existed=false when there was nothing to delete. \
                                if_version deletes only if the document is still at that version.",
            StoreOp::List => "List a collection, oldest-updated first (direction=desc for newest). \
                              Pages of up to 1000; pass next_cursor back as cursor to continue.",
            StoreOp::Query => "Query a collection with where clauses [field, op, value] (ops: eq, ne, \
                               lt, lte, gt, gte, in, not-in, array-contains; field may be a dotted \
                               path), order_by, direction, limit and cursor. Comparisons only hold \
                               between values of the same JSON type.",
            StoreOp::Batch => "Apply up to 50 writes ({op: set|update|delete, collection, doc_id, body, \
                               if_version}) in one scope, all or nothing. Prefer it over separate \
                               calls whenever writing more than a couple of documents.",
            StoreOp::Claim => "Take a document for ttl_secs as owner so no other worker acts on it: \
                               a claimed failure names who holds it and until when. Claiming again as \
                               the same owner renews. Writes by anyone else are refused while held.",
            StoreOp::Release => "Give back a claim taken with store.claim. released=false when the \
                                 owner did not hold it.",
            StoreOp::Describe => "Declare what a collection needs: scale (small, the default, or large \
                                  to give it its own partition and indexes), indexes ([{field, type}] \
                                  where type is ordered or contains), text_search ({fields, language}) \
                                  to make it searchable, ttl_secs to expire its documents by default, \
                                  and max_doc_bytes. Applying the same descriptor twice changes nothing.",
            StoreOp::Described => "The descriptor a collection was given, or described=false when it \
                                   has none — which means it is small and declares nothing.",
            StoreOp::Collections => "Every described collection in a scope, with its descriptor.",
            StoreOp::ContentWrite => "Append bytes (base64 in `bytes`) to a document's named content \
                                      stream at `offset`, which must be where the last write ended. \
                                      Nothing is readable until store.content_commit.",
            StoreOp::ContentCommit => "Publish the pending content stream, replacing what was committed \
                                       before. Returns its size and BLAKE3 digest.",
            StoreOp::ContentRead => "Read a range of a committed content stream: offset and len, at most \
                                     8MB per call, returning base64 `bytes` and the stream's size so a \
                                     reader knows when it has reached the end.",
            StoreOp::ContentStat => "The size, type and digest of a committed content stream, without \
                                     reading any of it.",
            StoreOp::ContentList => "Every committed content stream on a document.",
            StoreOp::ContentDelete => "Remove a content stream and every version of its bytes.",
        }
    }
}

const SCOPE_DESCRIPTION: &str =
    "Who the document belongs to: `user` (private to you), `user:<id>`, \
    `agent` (shared by every run of this agent — the default), `agent:<id>`, or `global` \
    (readable by everyone, written by admins).";

pub struct StoreTool {
    store: Arc<dyn DocumentStore>,
    is_admin: IsAdminFn,
    op: StoreOp,
}

impl StoreTool {
    pub fn new(store: Arc<dyn DocumentStore>, is_admin: IsAdminFn, op: StoreOp) -> Self {
        Self {
            store,
            is_admin,
            op,
        }
    }
}

/// Register every `store.*` tool.
pub fn register(tools: &ToolRegistry, store: Arc<dyn DocumentStore>, is_admin: IsAdminFn) {
    for op in StoreOp::ALL {
        tools.register(Arc::new(StoreTool::new(
            Arc::clone(&store),
            Arc::clone(&is_admin),
            op,
        )));
    }
}

/// Who is calling, from the run's task-locals. The `system` identity is the
/// one the vault treats as a service account, and reads any user scope.
pub fn caller_from_context(is_admin: &IsAdminFn) -> Caller {
    let identity = crate::user_config::current_identity_id();
    let agent_id = workforce_agent::runner::CURRENT_AGENT_ID
        .try_with(|a| a.clone())
        .ok()
        .filter(|a| !a.is_empty());
    Caller {
        system: identity.as_deref() == Some("system"),
        admin: is_admin(),
        identity,
        agent_id,
    }
}

fn default_owner() -> Option<String> {
    workforce_agent::runner::CURRENT_SESSION_ID
        .try_with(|s| s.clone())
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            workforce_agent::runner::CURRENT_AGENT_ID
                .try_with(|a| a.clone())
                .ok()
                .filter(|a| !a.is_empty())
        })
}

fn failure(start: Instant, e: StoreError) -> ToolResult {
    let extra = match &e {
        StoreError::VersionConflict { expected, actual } => {
            json!({"expected": expected, "actual": actual})
        }
        StoreError::Claimed { owner, until } => json!({"owner": owner, "until": until}),
        _ => Value::Null,
    };
    let mut text = format!("{} ({})", e, e.code());
    if !extra.is_null() {
        text.push(' ');
        text.push_str(&extra.to_string());
    }
    ToolResult::failure(text, start.elapsed().as_millis() as u64)
}

fn str_field<'a>(input: &'a Value, key: &str) -> Result<&'a str, StoreError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| StoreError::Invalid(format!("`{key}` is required")))
}

/// The three fields every content op addresses a stream by.
fn content_target(input: &Value) -> Result<(&str, &str, &str), StoreError> {
    Ok((
        str_field(input, "collection")?,
        str_field(input, "doc_id")?,
        str_field(input, "name")?,
    ))
}

/// Content crosses these surfaces as base64: a tool result and a WASM reply
/// are JSON, which has no bytes of its own.
fn decode_bytes(input: &Value) -> Result<Vec<u8>, StoreError> {
    BASE64
        .decode(str_field(input, "bytes")?)
        .map_err(|e| StoreError::Invalid(format!("`bytes` must be base64: {e}")))
}

fn i64_field(input: &Value, key: &str) -> Result<Option<i64>, StoreError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_i64()
            .map(Some)
            .ok_or_else(|| StoreError::Invalid(format!("`{key}` must be an integer"))),
    }
}

/// Run one store op for `caller`: resolve the scope (default `agent`),
/// check policy, dispatch. Shared by the platform tools and the WASM
/// bridge so both surfaces answer identically.
pub async fn perform(
    store: &dyn DocumentStore,
    caller: &Caller,
    op: StoreOp,
    input: &Value,
) -> Result<Value, StoreError> {
    {
        let scope_spec = input
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("agent");
        let scope = Scope::parse(scope_spec, caller)?;
        caller.authorise(&scope, op.access())?;
        let actor = caller.identity.clone().or_else(|| caller.agent_id.clone());
        let input = input.clone();
        match op {
            StoreOp::Get => {
                let (c, d) = (
                    str_field(&input, "collection")?,
                    str_field(&input, "doc_id")?,
                );
                Ok(match store.get(&scope, c, d).await? {
                    Some(doc) => json!({"found": true, "document": doc}),
                    None => {
                        json!({"found": false, "scope": scope.to_string(), "collection": c, "doc_id": d})
                    }
                })
            }
            StoreOp::Set => {
                let (c, d) = (
                    str_field(&input, "collection")?,
                    str_field(&input, "doc_id")?,
                );
                let body = input
                    .get("body")
                    .cloned()
                    .ok_or_else(|| StoreError::Invalid("`body` is required".into()))?;
                let doc = store
                    .set(
                        &scope,
                        c,
                        d,
                        body,
                        i64_field(&input, "if_version")?,
                        i64_field(&input, "ttl_secs")?,
                        actor.as_deref(),
                    )
                    .await?;
                Ok(json!({"written": true, "version": doc.version, "document": doc}))
            }
            StoreOp::Update => {
                let (c, d) = (
                    str_field(&input, "collection")?,
                    str_field(&input, "doc_id")?,
                );
                let body = input
                    .get("body")
                    .cloned()
                    .ok_or_else(|| StoreError::Invalid("`body` is required".into()))?;
                let doc = store
                    .update(
                        &scope,
                        c,
                        d,
                        body,
                        i64_field(&input, "if_version")?,
                        actor.as_deref(),
                    )
                    .await?;
                Ok(json!({"written": true, "version": doc.version, "document": doc}))
            }
            StoreOp::Delete => {
                let (c, d) = (
                    str_field(&input, "collection")?,
                    str_field(&input, "doc_id")?,
                );
                let existed = store
                    .delete(
                        &scope,
                        c,
                        d,
                        i64_field(&input, "if_version")?,
                        actor.as_deref(),
                    )
                    .await?;
                Ok(json!({"deleted": true, "existed": existed}))
            }
            StoreOp::List | StoreOp::Query => {
                let c = str_field(&input, "collection")?;
                let query = Query::from_json(&input)?;
                let page = if op == StoreOp::List {
                    store.list(&scope, c, &query).await?
                } else {
                    store.query(&scope, c, &query).await?
                };
                Ok(json!({
                    "count": page.documents.len(),
                    "documents": page.documents,
                    "next_cursor": page.next_cursor,
                }))
            }
            StoreOp::Batch => {
                let writes: Vec<WriteOp> = serde_json::from_value(
                    input
                        .get("writes")
                        .cloned()
                        .ok_or_else(|| StoreError::Invalid("`writes` is required".into()))?,
                )
                .map_err(|e| StoreError::Invalid(format!("writes: {e}")))?;
                if writes.len() > MAX_BATCH {
                    return Err(StoreError::Invalid(format!(
                        "batch has {} writes; the limit is {MAX_BATCH}",
                        writes.len()
                    )));
                }
                let outcomes = store.batch(&scope, &writes, actor.as_deref()).await?;
                Ok(json!({"applied": outcomes.len(), "outcomes": outcomes}))
            }
            StoreOp::Claim => {
                let (c, d) = (
                    str_field(&input, "collection")?,
                    str_field(&input, "doc_id")?,
                );
                let owner = input
                    .get("owner")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(default_owner)
                    .ok_or_else(|| StoreError::Invalid("`owner` is required".into()))?;
                let ttl = i64_field(&input, "ttl_secs")?.unwrap_or(300);
                let doc = store.claim(&scope, c, d, &owner, ttl).await?;
                Ok(json!({
                    "claimed": true,
                    "owner": owner,
                    "until": doc.lease_expires_at,
                    "document": doc,
                }))
            }
            StoreOp::Release => {
                let (c, d) = (
                    str_field(&input, "collection")?,
                    str_field(&input, "doc_id")?,
                );
                let owner = input
                    .get("owner")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(default_owner)
                    .ok_or_else(|| StoreError::Invalid("`owner` is required".into()))?;
                let released = store.release(&scope, c, d, &owner).await?;
                Ok(json!({"released": released, "owner": owner}))
            }
            StoreOp::Describe => {
                let c = str_field(&input, "collection")?;
                let descriptor = Descriptor::from_json(
                    input
                        .get("descriptor")
                        .ok_or_else(|| StoreError::Invalid("`descriptor` is required".into()))?,
                )?;
                let info = store.describe(&scope, c, &descriptor).await?;
                Ok(json!({"described": true, "collection": info.collection,
                          "revision": info.revision, "descriptor": info.descriptor}))
            }
            StoreOp::Described => {
                let c = str_field(&input, "collection")?;
                Ok(match store.described(&scope, c).await? {
                    Some(info) => json!({"described": true, "collection": info.collection,
                                         "revision": info.revision, "descriptor": info.descriptor}),
                    None => json!({"described": false, "collection": c}),
                })
            }
            StoreOp::Collections => Ok(json!({"collections": store.collections(&scope).await?})),
            StoreOp::ContentWrite => {
                let (c, d, name) = content_target(&input)?;
                let bytes = decode_bytes(&input)?;
                let offset = i64_field(&input, "offset")?.unwrap_or(0);
                let write = store
                    .content_write(
                        &scope,
                        c,
                        d,
                        name,
                        offset,
                        &bytes,
                        input.get("content_type").and_then(Value::as_str),
                    )
                    .await?;
                Ok(json!({"written": bytes.len(), "version": write.version,
                          "next_offset": write.next_offset}))
            }
            StoreOp::ContentCommit => {
                let (c, d, name) = content_target(&input)?;
                Ok(json!({"committed": store.content_commit(&scope, c, d, name).await?}))
            }
            StoreOp::ContentRead => {
                let (c, d, name) = content_target(&input)?;
                let offset = i64_field(&input, "offset")?.unwrap_or(0);
                let len = i64_field(&input, "len")?.unwrap_or(content::MAX_READ_BYTES);
                Ok(
                    match store.content_read(&scope, c, d, name, offset, len).await? {
                        Some(slice) => json!({
                            "found": true,
                            "offset": slice.offset,
                            "size_bytes": slice.size_bytes,
                            "eof": slice.offset + slice.bytes.len() as i64 >= slice.size_bytes,
                            "bytes": BASE64.encode(&slice.bytes),
                        }),
                        None => json!({"found": false, "name": name}),
                    },
                )
            }
            StoreOp::ContentStat => {
                let (c, d, name) = content_target(&input)?;
                Ok(match store.content_stat(&scope, c, d, name).await? {
                    Some(info) => json!({"found": true, "content": info}),
                    None => json!({"found": false, "name": name}),
                })
            }
            StoreOp::ContentList => {
                let (c, d) = (
                    str_field(&input, "collection")?,
                    str_field(&input, "doc_id")?,
                );
                Ok(json!({"content": store.content_list(&scope, c, d).await?}))
            }
            StoreOp::ContentDelete => {
                let (c, d, name) = content_target(&input)?;
                Ok(json!({"deleted": store.content_delete(&scope, c, d, name).await?}))
            }
        }
    }
}

#[async_trait]
impl Tool for StoreTool {
    fn name(&self) -> &str {
        self.op.name()
    }

    fn description(&self) -> &str {
        self.op.description()
    }

    fn input_schema(&self) -> Value {
        let scope = json!({"type": "string", "description": SCOPE_DESCRIPTION});
        let collection = json!({"type": "string", "description": "Collection name: letters, digits and _ - . ~ : @ +"});
        let doc_id = json!({"type": "string", "description": "Document id within the collection"});
        let if_version = json!({"type": "integer", "description": "Write only if the document is at this version; 0 = only if it does not exist"});
        let content_name =
            json!({"type": "string", "description": "Which content stream on the document"});
        match self.op {
            StoreOp::Get => json!({
                "type": "object",
                "properties": {"scope": scope, "collection": collection, "doc_id": doc_id},
                "required": ["collection", "doc_id"]
            }),
            StoreOp::Set => json!({
                "type": "object",
                "properties": {
                    "scope": scope, "collection": collection, "doc_id": doc_id,
                    "body": {"type": "object", "description": "The document, as a JSON object (no secrets)"},
                    "if_version": if_version,
                    "ttl_secs": {"type": "integer", "description": "Expire the document after this many seconds"}
                },
                "required": ["collection", "doc_id", "body"]
            }),
            StoreOp::Update => json!({
                "type": "object",
                "properties": {
                    "scope": scope, "collection": collection, "doc_id": doc_id,
                    "body": {"type": "object", "description": "Fields to merge; null removes a field"},
                    "if_version": if_version
                },
                "required": ["collection", "doc_id", "body"]
            }),
            StoreOp::Delete => json!({
                "type": "object",
                "properties": {"scope": scope, "collection": collection, "doc_id": doc_id, "if_version": if_version},
                "required": ["collection", "doc_id"]
            }),
            StoreOp::List => json!({
                "type": "object",
                "properties": {
                    "scope": scope, "collection": collection,
                    "direction": {"type": "string", "enum": ["asc", "desc"]},
                    "limit": {"type": "integer"},
                    "cursor": {"type": "string"}
                },
                "required": ["collection"]
            }),
            StoreOp::Query => json!({
                "type": "object",
                "properties": {
                    "scope": scope, "collection": collection,
                    "where": {"type": "array", "description": "[[field, op, value], …]", "items": {"type": "array"}},
                    "order_by": {"type": "string", "description": "Body field to order by (dotted path)"},
                    "direction": {"type": "string", "enum": ["asc", "desc"]},
                    "limit": {"type": "integer"},
                    "cursor": {"type": "string"}
                },
                "required": ["collection"]
            }),
            StoreOp::Batch => json!({
                "type": "object",
                "properties": {
                    "scope": scope,
                    "writes": {
                        "type": "array",
                        "description": "Up to 50 of {op: set|update|delete, collection, doc_id, body?, if_version?, ttl_secs?}",
                        "items": {"type": "object"}
                    }
                },
                "required": ["writes"]
            }),
            StoreOp::Claim => json!({
                "type": "object",
                "properties": {
                    "scope": scope, "collection": collection, "doc_id": doc_id,
                    "owner": {"type": "string", "description": "Who holds the claim; defaults to this session"},
                    "ttl_secs": {"type": "integer", "description": "How long to hold it (default 300, max 86400)"}
                },
                "required": ["collection", "doc_id"]
            }),
            StoreOp::Release => json!({
                "type": "object",
                "properties": {
                    "scope": scope, "collection": collection, "doc_id": doc_id,
                    "owner": {"type": "string", "description": "The holder; defaults to this session"}
                },
                "required": ["collection", "doc_id"]
            }),
            StoreOp::Describe => json!({
                "type": "object",
                "properties": {
                    "scope": scope, "collection": collection,
                    "descriptor": {
                        "type": "object",
                        "description": "What the collection needs: scale, indexes, text_search, \
                                        ttl_secs, max_doc_bytes. An empty object declares nothing."
                    }
                },
                "required": ["collection", "descriptor"]
            }),
            StoreOp::Described => json!({
                "type": "object",
                "properties": {"scope": scope, "collection": collection},
                "required": ["collection"]
            }),
            StoreOp::Collections => json!({
                "type": "object",
                "properties": {"scope": scope}
            }),
            StoreOp::ContentWrite => json!({
                "type": "object",
                "properties": {
                    "scope": scope, "collection": collection, "doc_id": doc_id,
                    "name": content_name.clone(),
                    "offset": {"type": "integer", "description": "Where this write starts; must be where the last one ended (0 to begin)"},
                    "bytes": {"type": "string", "description": "The bytes to append, base64"},
                    "content_type": {"type": "string", "description": "Media type of the whole stream"}
                },
                "required": ["collection", "doc_id", "name", "bytes"]
            }),
            StoreOp::ContentCommit | StoreOp::ContentStat | StoreOp::ContentDelete => json!({
                "type": "object",
                "properties": {
                    "scope": scope, "collection": collection, "doc_id": doc_id,
                    "name": content_name.clone()
                },
                "required": ["collection", "doc_id", "name"]
            }),
            StoreOp::ContentRead => json!({
                "type": "object",
                "properties": {
                    "scope": scope, "collection": collection, "doc_id": doc_id,
                    "name": content_name,
                    "offset": {"type": "integer", "description": "Where to start reading (default 0)"},
                    "len": {"type": "integer", "description": "How many bytes, at most 8MB (the default)"}
                },
                "required": ["collection", "doc_id", "name"]
            }),
            StoreOp::ContentList => json!({
                "type": "object",
                "properties": {"scope": scope, "collection": collection, "doc_id": doc_id},
                "required": ["collection", "doc_id"]
            }),
        }
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let caller = caller_from_context(&self.is_admin);
        match perform(self.store.as_ref(), &caller, self.op, &input).await {
            Ok(out) => Ok(ToolResult::success(out, start.elapsed().as_millis() as u64)),
            Err(e) => Ok(failure(start, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryDocumentStore;
    use workforce_agent::runner::{CURRENT_AGENT_ID, CURRENT_USER_ID};

    fn tools(admin: bool) -> (Arc<MemoryDocumentStore>, Vec<StoreTool>) {
        let store = Arc::new(MemoryDocumentStore::new());
        let is_admin: IsAdminFn = Arc::new(move || admin);
        let tools = StoreOp::ALL
            .iter()
            .map(|op| StoreTool::new(store.clone(), Arc::clone(&is_admin), *op))
            .collect();
        (store, tools)
    }

    fn tool(tools: &[StoreTool], op: StoreOp) -> &StoreTool {
        tools.iter().find(|t| t.op == op).unwrap()
    }

    async fn as_user<F: std::future::Future>(uid: &str, agent: &str, fut: F) -> F::Output {
        CURRENT_USER_ID
            .scope(
                uid.to_string(),
                CURRENT_AGENT_ID.scope(agent.to_string(), fut),
            )
            .await
    }

    #[tokio::test]
    async fn a_member_writes_their_own_and_their_agents_scope_but_not_global() {
        let (_store, tools) = tools(false);
        let set = tool(&tools, StoreOp::Set);
        let get = tool(&tools, StoreOp::Get);
        as_user("alice", "atlas", async {
            let r = set
                .execute(json!({"scope": "user", "collection": "todo", "doc_id": "1", "body": {"t": "milk"}}))
                .await
                .unwrap();
            assert!(r.success, "{:?}", r.output);
            let r = set
                .execute(json!({"collection": "pipeline", "doc_id": "ENG-1", "body": {"stage": "pick"}}))
                .await
                .unwrap();
            assert!(r.success, "default scope is the agent's");
            let r = set
                .execute(json!({"scope": "global", "collection": "x", "doc_id": "1", "body": {}}))
                .await
                .unwrap();
            assert!(!r.success);
            assert!(r.error.as_deref().unwrap_or("").contains("denied"), "{:?}", r.error);
            let r = get
                .execute(json!({"scope": "user:bob", "collection": "todo", "doc_id": "1"}))
                .await
                .unwrap();
            assert!(!r.success, "no reading another user's scope");
        })
        .await;

        // Bob sees his own empty scope, not Alice's document.
        as_user("bob", "atlas", async {
            let r = get
                .execute(json!({"scope": "user", "collection": "todo", "doc_id": "1"}))
                .await
                .unwrap();
            assert!(r.success);
            assert_eq!(r.output["found"], false);
            // but the agent scope is shared
            let r = get
                .execute(json!({"collection": "pipeline", "doc_id": "ENG-1"}))
                .await
                .unwrap();
            assert_eq!(r.output["found"], true);
            assert_eq!(r.output["document"]["body"]["stage"], "pick");
        })
        .await;
    }

    #[tokio::test]
    async fn conflicts_and_claims_come_back_as_codes() {
        let (_store, tools) = tools(true);
        as_user("root", "atlas", async {
            let set = tool(&tools, StoreOp::Set);
            set.execute(json!({"collection": "c", "doc_id": "d", "body": {"v": 1}}))
                .await
                .unwrap();
            let r = set
                .execute(json!({"collection": "c", "doc_id": "d", "body": {"v": 2}, "if_version": 5}))
                .await
                .unwrap();
            assert!(!r.success);
            let err = r.error.unwrap();
            assert!(err.contains("version_conflict") && err.contains("\"actual\":1"), "{err}");

            let claim = tool(&tools, StoreOp::Claim);
            let r = claim
                .execute(json!({"collection": "c", "doc_id": "d", "owner": "w1", "ttl_secs": 60}))
                .await
                .unwrap();
            assert!(r.success, "{:?}", r.error);
            let r = claim
                .execute(json!({"collection": "c", "doc_id": "d", "owner": "w2"}))
                .await
                .unwrap();
            assert!(r.error.unwrap().contains("(claimed)"));

            let r = set
                .execute(json!({"scope": "global", "collection": "cfg", "doc_id": "auto_merge", "body": {"core-api": "dry-run"}}))
                .await
                .unwrap();
            assert!(r.success, "admins write global");
        })
        .await;
    }

    #[tokio::test]
    async fn a_user_scope_without_an_identity_is_refused_not_shared() {
        let (_store, tools) = tools(false);
        let r = CURRENT_AGENT_ID
            .scope(
                "atlas".to_string(),
                tool(&tools, StoreOp::Set).execute(json!({
                    "scope": "user", "collection": "todo", "doc_id": "1", "body": {}
                })),
            )
            .await
            .unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("no_identity"));
    }

    #[tokio::test]
    async fn query_and_batch_pass_through() {
        let (_store, tools) = tools(false);
        as_user("alice", "atlas", async {
            let r = tool(&tools, StoreOp::Batch)
                .execute(json!({"writes": [
                    {"op": "set", "collection": "p", "doc_id": "a", "body": {"n": 1}},
                    {"op": "set", "collection": "p", "doc_id": "b", "body": {"n": 2}}
                ]}))
                .await
                .unwrap();
            assert!(r.success, "{:?}", r.error);
            assert_eq!(r.output["applied"], 2);
            let r = tool(&tools, StoreOp::Query)
                .execute(json!({"collection": "p", "where": [["n", "gt", 1]]}))
                .await
                .unwrap();
            assert_eq!(r.output["count"], 1);
            assert_eq!(r.output["documents"][0]["doc_id"], "b");
        })
        .await;
    }
}
