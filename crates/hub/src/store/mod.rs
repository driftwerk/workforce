//! The document store: scoped JSON persistence for workflows, skills and
//! agents.
//!
//! Every consumer of the hub that needed to remember something between runs
//! improvised its own store — pipeline state as Jira labels, a register in a
//! Google Sheet, a noise list in a ticket description, big results in the
//! artifact store, everything else in `instance.variables`. None of those
//! can be queried, none can be claimed, and two runs writing the same thing
//! at once silently clobber each other. This module is the one place for
//! that state.
//!
//! A document lives in a **scope** that mirrors the vault's tiers:
//! `user` (private to one identity), `agent` (every run of one agent) or
//! `global` (readable by everyone, written by admins). Inside a scope it is
//! addressed by `collection/doc_id`. Writes carry a `version` for
//! compare-and-swap, and a document can be **claimed** for a while by an
//! owner so two workers never act on it together.
//!
//! Secrets do not belong here: a body that looks like one is refused, with
//! the vault named as the place for it.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use eyre::Result;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use workforce_common::settings::WorkforceConfig;

pub mod content;
pub mod descriptor;
#[cfg(feature = "postgres")]
pub mod postgres;

pub use crate::hub_core::pipelines::{Access, Caller, Scope, StoreError};
pub use content::{ContentInfo, ContentSlice, ContentWrite};
pub use descriptor::{CollectionInfo, Descriptor, IndexKind, IndexSpec, Scale, TextSearch};

/// Largest body a single document may carry.
pub const MAX_BODY_BYTES: usize = 256 * 1024;
/// Most writes one `batch` call may carry.
pub const MAX_BATCH: usize = 50;
/// Default and ceiling for a page of `list`/`query` results.
pub const DEFAULT_PAGE: usize = 100;
pub const MAX_PAGE: usize = 1000;
/// Longest a claim may be held before it lapses on its own.
pub const MAX_CLAIM_SECS: i64 = 24 * 60 * 60;

// ---------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Caller and policy
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Documents, queries, writes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub scope: Scope,
    pub collection: String,
    pub doc_id: String,
    pub body: Value,
    pub version: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at: Option<DateTime<Utc>>,
}

impl Document {
    /// Whether an unexpired claim other than `owner`'s is on the document.
    pub fn claimed_by_other(
        &self,
        owner: &str,
        now: DateTime<Utc>,
    ) -> Option<(&str, DateTime<Utc>)> {
        match (&self.lease_owner, self.lease_expires_at) {
            (Some(holder), Some(until)) if holder != owner && until > now => {
                Some((holder.as_str(), until))
            }
            _ => None,
        }
    }
}

/// A page of results. `next_cursor` is opaque; pass it back to continue.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Page {
    pub documents: Vec<Document>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
    In,
    NotIn,
    ArrayContains,
}

impl Op {
    pub fn parse(s: &str) -> Option<Op> {
        Some(match s {
            "eq" | "==" | "=" => Op::Eq,
            "ne" | "!=" => Op::Ne,
            "lt" | "<" => Op::Lt,
            "lte" | "<=" => Op::Lte,
            "gt" | ">" => Op::Gt,
            "gte" | ">=" => Op::Gte,
            "in" => Op::In,
            "not-in" | "not_in" => Op::NotIn,
            "array-contains" | "array_contains" | "contains" => Op::ArrayContains,
            _ => return None,
        })
    }
}

/// One `[field, op, value]` filter on a body. `field` is a top-level key or
/// a dotted path into nested objects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Where {
    pub field: String,
    pub op: Op,
    pub value: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    #[default]
    Asc,
    Desc,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Query {
    #[serde(default, rename = "where")]
    pub filters: Vec<Where>,
    /// Body field to order by; `updated_at` is the default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_by: Option<String>,
    #[serde(default)]
    pub direction: Direction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Full-text search over the fields the collection's descriptor declares
    /// searchable. Accepts the phrasing a person expects — quoted phrases,
    /// `-exclusions` — because it is `websearch_to_tsquery` underneath.
    ///
    /// A collection that declares no `text_search` refuses it rather than
    /// falling back to a scan: what is searchable is declared, the same way
    /// what is indexed is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
}

impl Query {
    pub fn page_size(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE)
    }

    /// Where this query resumes; a cursor that will not decode starts over
    /// rather than failing the read.
    pub fn resume(&self) -> Option<Resume> {
        self.cursor.as_deref().and_then(decode_cursor)
    }

    /// Parse the JSON shape tools and routes accept:
    /// `{"where": [["stage","eq","review"]], "order_by": "…", "direction": "desc", "limit": 50, "cursor": "…"}`.
    /// `where` entries may be triples or `{field, op, value}` objects.
    pub fn from_json(v: &Value) -> Result<Query, StoreError> {
        let mut q = Query::default();
        if let Some(items) = v.get("where").and_then(Value::as_array) {
            for item in items {
                q.filters.push(parse_where(item)?);
            }
            if q.filters.len() > 10 {
                return Err(StoreError::Invalid("at most 10 where clauses".into()));
            }
        }
        if let Some(o) = v.get("order_by") {
            match o {
                Value::String(field) => q.order_by = Some(field.clone()),
                Value::Object(m) => {
                    q.order_by = m.get("field").and_then(Value::as_str).map(str::to_string);
                    if let Some(d) = m.get("direction").and_then(Value::as_str) {
                        q.direction = parse_direction(d)?;
                    }
                }
                _ => return Err(StoreError::Invalid("order_by must be a field name".into())),
            }
        }
        if let Some(d) = v.get("direction").and_then(Value::as_str) {
            q.direction = parse_direction(d)?;
        }
        if let Some(field) = &q.order_by {
            validate_field(field)?;
        }
        if let Some(l) = v.get("limit") {
            q.limit = Some(
                l.as_u64()
                    .ok_or_else(|| StoreError::Invalid("limit must be a positive integer".into()))?
                    as usize,
            );
        }
        if let Some(term) = v.get("search") {
            let term = term
                .as_str()
                .ok_or_else(|| StoreError::Invalid("search must be a string".into()))?;
            if term.trim().is_empty() {
                return Err(StoreError::Invalid("search needs a term".into()));
            }
            q.search = Some(term.to_string());
        }
        if let Some(c) = v.get("cursor").and_then(Value::as_str) {
            if let Some(Resume::After(anchor)) = decode_cursor(c) {
                if anchor.order_by.as_deref() != q.order_by.as_deref()
                    || anchor.direction != q.direction
                {
                    return Err(StoreError::Invalid(
                        "cursor is from a differently ordered query — page with the order_by and \
                         direction it was issued under, or start again without it"
                            .into(),
                    ));
                }
            }
            q.cursor = Some(c.to_string());
        }
        Ok(q)
    }
}

fn parse_direction(d: &str) -> Result<Direction, StoreError> {
    match d {
        "asc" => Ok(Direction::Asc),
        "desc" => Ok(Direction::Desc),
        other => Err(StoreError::Invalid(format!(
            "direction `{other}` — expected asc or desc"
        ))),
    }
}

fn parse_where(item: &Value) -> Result<Where, StoreError> {
    let (field, op, value) = match item {
        Value::Array(triple) if triple.len() == 3 => (
            triple[0].as_str().map(str::to_string),
            triple[1].as_str().map(str::to_string),
            triple[2].clone(),
        ),
        Value::Object(m) => (
            m.get("field").and_then(Value::as_str).map(str::to_string),
            m.get("op").and_then(Value::as_str).map(str::to_string),
            m.get("value").cloned().unwrap_or(Value::Null),
        ),
        _ => {
            return Err(StoreError::Invalid(
                "each where clause is [field, op, value]".into(),
            ))
        }
    };
    let field = field.ok_or_else(|| StoreError::Invalid("where clause needs a field".into()))?;
    validate_field(&field)?;
    let op = op.as_deref().and_then(Op::parse).ok_or_else(|| {
        StoreError::Invalid(
            "where op must be one of eq, ne, lt, lte, gt, gte, in, not-in, array-contains".into(),
        )
    })?;
    if matches!(op, Op::In | Op::NotIn) && !value.is_array() {
        return Err(StoreError::Invalid(format!(
            "`{}` needs an array value",
            op_name(op)
        )));
    }
    Ok(Where { field, op, value })
}

fn op_name(op: Op) -> &'static str {
    match op {
        Op::Eq => "eq",
        Op::Ne => "ne",
        Op::Lt => "lt",
        Op::Lte => "lte",
        Op::Gt => "gt",
        Op::Gte => "gte",
        Op::In => "in",
        Op::NotIn => "not-in",
        Op::ArrayContains => "array-contains",
    }
}

/// Where a page ended: the value its last row sorted on, and that row's id as
/// the tiebreaker. `(key, doc_id)` is a total order — `doc_id` is unique
/// within a collection — so the next page resumes immediately after the anchor
/// with no row skipped or repeated, however much has been written in between.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    #[serde(rename = "k")]
    pub key: Value,
    #[serde(rename = "d")]
    pub doc_id: String,
    /// The ordering this anchor was taken under. A cursor means nothing except
    /// against the same ordering, so it carries it and [`Query::from_json`]
    /// refuses a mismatch rather than returning a page from the wrong place.
    #[serde(rename = "o", default, skip_serializing_if = "Option::is_none")]
    pub order_by: Option<String>,
    #[serde(rename = "r", default)]
    pub direction: Direction,
}

/// Where a cursor says to resume. `Offset` is the pre-keyset encoding, still
/// honoured so a cursor held across the upgrade keeps paging instead of
/// silently restarting.
#[derive(Debug, Clone, PartialEq)]
pub enum Resume {
    After(Anchor),
    Offset(usize),
}

/// The value a document sorts on, which is also what its anchor carries.
///
/// A missing field reads as JSON null rather than as absent, so every document
/// has a place in the order and none can fall out of a paged walk. The default
/// ordering renders `updated_at` with fixed-width nanoseconds, which makes the
/// string's lexical order its chronological order — the variable-width form
/// `to_rfc3339` produces does not, and would misplace rows whose fractional
/// second has fewer digits.
pub fn sort_key(doc: &Document, order_by: Option<&str>) -> Value {
    match order_by {
        Some(field) => field_value(&doc.body, field)
            .cloned()
            .unwrap_or(Value::Null),
        None => Value::String(doc.updated_at.to_rfc3339_opts(SecondsFormat::Nanos, true)),
    }
}

pub fn anchor_of(doc: &Document, query: &Query) -> Anchor {
    let order_by = query.order_by.as_deref();
    Anchor {
        key: sort_key(doc, order_by),
        doc_id: doc.doc_id.clone(),
        order_by: order_by.map(str::to_string),
        direction: query.direction,
    }
}

pub fn encode_cursor(anchor: &Anchor) -> String {
    format!(
        "k{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(anchor).unwrap_or_default())
    )
}

pub fn decode_cursor(cursor: &str) -> Option<Resume> {
    match cursor.chars().next()? {
        'k' => serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&cursor[1..]).ok()?)
            .ok()
            .map(Resume::After),
        'o' => cursor[1..].parse().ok().map(Resume::Offset),
        _ => None,
    }
}

/// Whether `doc` falls after `anchor` in `direction`, i.e. belongs to a page
/// resuming there. The comparison is the ordering itself, so a row equal on
/// the sort key is still placed by its id rather than dropped.
pub fn after_anchor(
    doc: &Document,
    anchor: &Anchor,
    order_by: Option<&str>,
    direction: Direction,
) -> bool {
    let ord = compare_values(&sort_key(doc, order_by), &anchor.key)
        .then_with(|| doc.doc_id.as_str().cmp(anchor.doc_id.as_str()));
    match direction {
        Direction::Asc => ord.is_gt(),
        Direction::Desc => ord.is_lt(),
    }
}

/// One write inside a `batch`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum WriteOp {
    Set {
        collection: String,
        doc_id: String,
        body: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        if_version: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ttl_secs: Option<i64>,
    },
    Update {
        collection: String,
        doc_id: String,
        body: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        if_version: Option<i64>,
    },
    Delete {
        collection: String,
        doc_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        if_version: Option<i64>,
    },
}

impl WriteOp {
    pub fn collection(&self) -> &str {
        match self {
            WriteOp::Set { collection, .. }
            | WriteOp::Update { collection, .. }
            | WriteOp::Delete { collection, .. } => collection,
        }
    }
    pub fn doc_id(&self) -> &str {
        match self {
            WriteOp::Set { doc_id, .. }
            | WriteOp::Update { doc_id, .. }
            | WriteOp::Delete { doc_id, .. } => doc_id,
        }
    }
}

/// What one write in a batch produced.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "lowercase")]
pub enum WriteOutcome {
    Written { version: i64 },
    Deleted { existed: bool },
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Collection names and document ids are one path segment: no slashes, no
/// dots-only names, nothing that would surprise a URL.
pub fn validate_segment(what: &str, s: &str) -> Result<(), StoreError> {
    if s.is_empty() || s.len() > 200 {
        return Err(StoreError::Invalid(format!(
            "{what} must be 1–200 characters"
        )));
    }
    if s == "." || s == ".." {
        return Err(StoreError::Invalid(format!("{what} may not be `{s}`")));
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_-.~:@+".contains(&b))
    {
        return Err(StoreError::Invalid(format!(
            "{what} `{s}` may only contain letters, digits and _ - . ~ : @ +"
        )));
    }
    Ok(())
}

/// A body field named in a query: dotted path of plain keys.
pub fn validate_field(field: &str) -> Result<(), StoreError> {
    if field.is_empty() || field.len() > 200 {
        return Err(StoreError::Invalid("field must be 1–200 characters".into()));
    }
    let ok = field.split('.').all(|seg| {
        !seg.is_empty()
            && seg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    });
    if !ok {
        return Err(StoreError::Invalid(format!(
            "field `{field}` must be a dotted path of letters, digits, _ and -"
        )));
    }
    Ok(())
}

/// A body must be a JSON object, small enough, and hold no secret.
pub fn validate_body(body: &Value) -> Result<(), StoreError> {
    if !body.is_object() {
        return Err(StoreError::Invalid("body must be a JSON object".into()));
    }
    let size = serde_json::to_vec(body)
        .map(|v| v.len())
        .unwrap_or(usize::MAX);
    if size > MAX_BODY_BYTES {
        return Err(StoreError::Invalid(format!(
            "body is {size} bytes; the limit is {MAX_BODY_BYTES}"
        )));
    }
    if let Some(path) = find_secret_like(body, "") {
        return Err(StoreError::Invalid(format!(
            "`{path}` looks like a secret — the store is not the vault; keep credentials in the vault and store a reference"
        )));
    }
    Ok(())
}

/// Depth-first look for a string value that reads as a credential. The
/// patterns are the well-known prefixes; a false positive costs a rename,
/// a false negative leaks a key into a queryable table.
fn find_secret_like(v: &Value, path: &str) -> Option<String> {
    match v {
        Value::String(s) => looks_like_secret(s).then(|| path.to_string()),
        Value::Object(m) => m.iter().find_map(|(k, v)| {
            let p = if path.is_empty() {
                k.clone()
            } else {
                format!("{path}.{k}")
            };
            find_secret_like(v, &p)
        }),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(i, v)| find_secret_like(v, &format!("{path}[{i}]"))),
        _ => None,
    }
}

fn looks_like_secret(s: &str) -> bool {
    let s = s.trim();
    if s.starts_with("[[vault:") {
        return true;
    }
    const PREFIXES: [&str; 9] = [
        "sk-ant-",
        "sk-",
        "AKIA",
        "ghp_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "glpat-",
        "pat-",
    ];
    if s.len() >= 20 && PREFIXES.iter().any(|p| s.starts_with(p)) {
        return true;
    }
    s.starts_with("-----BEGIN") && s.contains("PRIVATE KEY")
}

// ---------------------------------------------------------------------------
// Merge and filter helpers (shared by backends)
// ---------------------------------------------------------------------------

/// Shallow merge: top-level keys of `patch` replace those of `base`; a
/// `null` in `patch` removes the key.
pub fn merge_shallow(base: &Value, patch: &Value) -> Value {
    let mut out = base.as_object().cloned().unwrap_or_default();
    if let Some(p) = patch.as_object() {
        for (k, v) in p {
            if v.is_null() {
                out.remove(k);
            } else {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// Read a dotted path out of a body.
/// The one column `documents` is partitioned by: a scope and collection
/// joined into a single value, because LIST partitioning takes exactly one
/// column.
///
/// `/` is the separator because [`validate_segment`] forbids it inside a
/// segment while permitting `:`. Joining on `:` would let scope `a:b` with
/// collection `c` and scope `a` with collection `b:c` produce the same key,
/// putting two unrelated tenants in one partition sharing one primary key
/// namespace.
pub fn partition_key(scope: &Scope, collection: &str) -> String {
    format!("{}/{}/{}", scope.kind(), scope.id(), collection)
}

pub fn field_value<'a>(body: &'a Value, field: &str) -> Option<&'a Value> {
    field.split('.').try_fold(body, |v, seg| v.get(seg))
}

/// Order two JSON scalars the way the Postgres backend does: numbers
/// numerically, strings lexically, booleans false < true; mixed kinds by
/// kind.
///
/// The kind order is jsonb's own — `Null < String < Number < Bool < Array <
/// Object` — because the Postgres backend orders by the jsonb value and this
/// is the reference the in-memory backend has to match. Ranking booleans
/// below strings, as this did before, made the two backends return mixed-type
/// pages in different orders, which a keyset cursor cannot survive: the anchor
/// from one ordering means something else under the other.
pub fn compare_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Null => 0,
            Value::String(_) => 1,
            Value::Number(_) => 2,
            Value::Bool(_) => 3,
            Value::Array(_) => 4,
            Value::Object(_) => 5,
        }
    }
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x
            .as_f64()
            .partial_cmp(&y.as_f64())
            .unwrap_or(Ordering::Equal),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => rank(a).cmp(&rank(b)),
    }
}

/// Whether a body satisfies one filter (the in-memory backend's evaluator,
/// and the reference semantics the SQL must match).
pub fn matches(body: &Value, w: &Where) -> bool {
    use std::cmp::Ordering::*;
    let actual = field_value(body, &w.field);
    match w.op {
        Op::Eq => actual == Some(&w.value),
        Op::Ne => actual != Some(&w.value),
        Op::Lt | Op::Lte | Op::Gt | Op::Gte => {
            let Some(actual) = actual else { return false };
            if std::mem::discriminant(actual) != std::mem::discriminant(&w.value) {
                return false;
            }
            let ord = compare_values(actual, &w.value);
            match w.op {
                Op::Lt => ord == Less,
                Op::Lte => ord != Greater,
                Op::Gt => ord == Greater,
                _ => ord != Less,
            }
        }
        Op::In => w
            .value
            .as_array()
            .is_some_and(|set| actual.is_some_and(|a| set.contains(a))),
        Op::NotIn => w
            .value
            .as_array()
            .is_some_and(|set| !actual.is_some_and(|a| set.contains(a))),
        Op::ArrayContains => actual
            .and_then(Value::as_array)
            .is_some_and(|items| items.contains(&w.value)),
    }
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// Everything a surface needs. Backends enforce nothing about *who* asks —
/// that is [`Caller::authorise`], applied by the surface before the call —
/// but they do enforce versions, claims and expiry.
#[async_trait]
pub trait DocumentStore: Send + Sync {
    async fn get(&self, scope: &Scope, collection: &str, doc_id: &str) -> Result<Option<Document>>;

    /// Replace (or create) a document. `if_version` makes it a
    /// compare-and-swap: `Some(0)` means "only create"; `Some(n)` means
    /// "only if still at n". An unexpired claim by someone other than
    /// `actor` refuses the write.
    #[allow(clippy::too_many_arguments)]
    async fn set(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        body: Value,
        if_version: Option<i64>,
        ttl_secs: Option<i64>,
        actor: Option<&str>,
    ) -> Result<Document, StoreError>;

    /// Shallow-merge `patch` into an existing document (see
    /// [`merge_shallow`]). Missing documents are not created.
    async fn update(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        patch: Value,
        if_version: Option<i64>,
        actor: Option<&str>,
    ) -> Result<Document, StoreError>;

    /// Delete; `Ok(false)` when there was nothing to delete.
    async fn delete(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        if_version: Option<i64>,
        actor: Option<&str>,
    ) -> Result<bool, StoreError>;

    async fn list(&self, scope: &Scope, collection: &str, query: &Query) -> Result<Page>;

    async fn query(&self, scope: &Scope, collection: &str, query: &Query) -> Result<Page>;

    /// Apply up to [`MAX_BATCH`] writes in one scope. Atomic on backends
    /// that can be; the outcomes come back in order.
    async fn batch(
        &self,
        scope: &Scope,
        writes: &[WriteOp],
        actor: Option<&str>,
    ) -> Result<Vec<WriteOutcome>, StoreError>;

    /// Take the document for `ttl_secs` as `owner`. Succeeds when nobody
    /// holds it, the holder's claim lapsed, or `owner` already holds it
    /// (renewal). Fails with [`StoreError::Claimed`] otherwise.
    async fn claim(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        owner: &str,
        ttl_secs: i64,
    ) -> Result<Document, StoreError>;

    /// Give a claim back; a no-op when `owner` does not hold it.
    async fn release(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        owner: &str,
    ) -> Result<bool, StoreError>;

    /// Give a collection a descriptor, and make the store match it: a
    /// collection declared `large` is moved to a partition of its own and the
    /// indexes it names are built there, and indexes it no longer names are
    /// dropped. Applying the same descriptor twice changes nothing.
    ///
    /// Declaring an existing collection `large` moves its rows and builds its
    /// indexes in one transaction, which holds a lock on that collection for
    /// the duration — seconds on a large one. It is an administrative act on a
    /// named collection, not something a write path does.
    async fn describe(
        &self,
        scope: &Scope,
        collection: &str,
        descriptor: &Descriptor,
    ) -> Result<CollectionInfo, StoreError>;

    /// A collection's descriptor, or `None` where it has never been given one
    /// — which is every collection by default, and means `small`.
    async fn described(
        &self,
        scope: &Scope,
        collection: &str,
    ) -> Result<Option<CollectionInfo>, StoreError>;

    /// Every described collection in a scope.
    async fn collections(&self, scope: &Scope) -> Result<Vec<CollectionInfo>, StoreError>;

    /// Append `bytes` to a document's pending content stream, starting the
    /// stream on the first write. `offset` must be where the last write ended,
    /// so a stream is written in order and no chunk is ever rewritten.
    /// Nothing written here is readable until [`Self::content_commit`].
    #[allow(clippy::too_many_arguments)]
    async fn content_write(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        name: &str,
        offset: i64,
        bytes: &[u8],
        content_type: Option<&str>,
    ) -> Result<ContentWrite, StoreError>;

    /// Publish the pending stream, replacing whatever was committed before.
    /// The digest is computed over the bytes as they are read back, so it
    /// describes what was actually stored.
    async fn content_commit(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        name: &str,
    ) -> Result<ContentInfo, StoreError>;

    /// Read a range of the committed stream. `None` where nothing is
    /// committed under that name; an empty slice where the range starts past
    /// the end.
    #[allow(clippy::too_many_arguments)]
    async fn content_read(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        name: &str,
        offset: i64,
        len: i64,
    ) -> Result<Option<ContentSlice>, StoreError>;

    /// What is committed under `name`, without reading any of it.
    async fn content_stat(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        name: &str,
    ) -> Result<Option<ContentInfo>, StoreError>;

    /// Every committed stream on a document.
    async fn content_list(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
    ) -> Result<Vec<ContentInfo>, StoreError>;

    /// Remove a stream and every version of its bytes.
    async fn content_delete(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        name: &str,
    ) -> Result<bool, StoreError>;

    /// How many documents a scope holds, across every collection.
    async fn scope_documents(&self, scope: &Scope) -> Result<usize, StoreError>;

    /// How many documents each person's scope holds, by the id it is kept
    /// under.
    async fn user_scopes(&self) -> Result<BTreeMap<String, usize>, StoreError>;

    /// Move everything in `from` into `to`: documents with their content
    /// streams. A document `to` already holds under the same collection and
    /// id keeps `to`'s, and `from`'s is dropped and named in the outcome.
    /// Collection descriptors stay with their scope: `to`'s own apply.
    async fn rescope(&self, from: &Scope, to: &Scope) -> Result<Rescoped, StoreError>;

    /// Delete everything a scope holds: documents, content streams and
    /// collection descriptors. Returns how many documents went.
    async fn drop_scope(&self, scope: &Scope) -> Result<usize, StoreError>;

    /// Drop every document past its `expires_at`. Returns how many went.
    async fn purge_expired(&self) -> Result<u64>;

    /// Whether other hub instances can write this store too.
    fn is_shared(&self) -> bool;
}

/// The limits a collection's descriptor sets on a single document. Mirrors the
/// `enforce_collection_limits` trigger, which is where the Postgres backend
/// applies the same rules.
fn check_limits(
    collection: &str,
    body: &Value,
    limits: Option<&Descriptor>,
) -> Result<(), StoreError> {
    let Some(max) = limits.and_then(|d| d.max_doc_bytes) else {
        return Ok(());
    };
    let size = body.to_string().len();
    if size > max {
        return Err(StoreError::Invalid(format!(
            "document is {size} bytes; `{collection}` allows {max}"
        )));
    }
    Ok(())
}

/// When a document expires: the write's own TTL where it gives one, else the
/// collection's default, else never.
fn expiry(
    ttl_secs: Option<i64>,
    limits: Option<&Descriptor>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    ttl_secs
        .or_else(|| limits.and_then(|d| d.ttl_secs))
        .map(|t| now + Duration::seconds(t))
}

/// Checks every backend runs before a write.
fn check_write(
    collection: &str,
    doc_id: &str,
    body: Option<&Value>,
    ttl: Option<i64>,
) -> Result<(), StoreError> {
    validate_segment("collection", collection)?;
    validate_segment("doc_id", doc_id)?;
    if let Some(b) = body {
        validate_body(b)?;
    }
    if let Some(t) = ttl {
        if t <= 0 {
            return Err(StoreError::Invalid("ttl_secs must be positive".into()));
        }
    }
    Ok(())
}

fn check_claim(ttl_secs: i64, owner: &str) -> Result<(), StoreError> {
    if owner.trim().is_empty() {
        return Err(StoreError::Invalid("claim needs an owner".into()));
    }
    if ttl_secs <= 0 || ttl_secs > MAX_CLAIM_SECS {
        return Err(StoreError::Invalid(format!(
            "claim ttl_secs must be between 1 and {MAX_CLAIM_SECS}"
        )));
    }
    Ok(())
}

/// What [`DocumentStore::rescope`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Rescoped {
    pub moved: usize,
    /// `collection/doc_id` of each document left to the destination's own.
    pub kept: Vec<String>,
}

// ---------------------------------------------------------------------------
// In-memory backend
// ---------------------------------------------------------------------------

type Key = (String, String, String, String);

/// Single-process backend: the reference semantics, used by tests and by a
/// hub running without Postgres (state then lives as long as the process).
#[derive(Default)]
pub struct MemoryDocumentStore {
    docs: Mutex<BTreeMap<Key, Document>>,
    /// Descriptors by `(scope kind, scope id, collection)`. Held so a
    /// descriptor round-trips exactly as it does on Postgres; the indexes one
    /// declares have no counterpart here, because every query is already a
    /// scan of a collection small enough to live in a process.
    described: Mutex<BTreeMap<(String, String, String), CollectionInfo>>,
    /// Content streams by `(scope kind, scope id, collection, doc_id, name)`.
    content: Mutex<BTreeMap<ContentKey, ContentEntry>>,
}

type ContentKey = (String, String, String, String, String);

/// A stream's committed bytes and the pending ones a writer is building, kept
/// apart for the same reason the Postgres backend versions them: a reader must
/// never see half a write.
#[derive(Default)]
struct ContentEntry {
    committed: Option<(Vec<u8>, ContentInfo)>,
    pending: Vec<u8>,
    pending_type: Option<String>,
    version: i64,
}

impl MemoryDocumentStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// The descriptor a write is measured against, if the collection has one.
    fn limits_for(&self, scope: &Scope, collection: &str) -> Option<Descriptor> {
        self.described
            .lock()
            .get(&Self::described_key(scope, collection))
            .map(|info| info.descriptor.clone())
    }

    fn content_key(scope: &Scope, collection: &str, doc_id: &str, name: &str) -> ContentKey {
        (
            scope.kind().to_string(),
            scope.id().to_string(),
            collection.to_string(),
            doc_id.to_string(),
            name.to_string(),
        )
    }

    fn described_key(scope: &Scope, collection: &str) -> (String, String, String) {
        (
            scope.kind().to_string(),
            scope.id().to_string(),
            collection.to_string(),
        )
    }

    fn key(scope: &Scope, collection: &str, doc_id: &str) -> Key {
        (
            scope.kind().to_string(),
            scope.id().to_string(),
            collection.to_string(),
            doc_id.to_string(),
        )
    }

    fn live(doc: &Document, now: DateTime<Utc>) -> bool {
        doc.expires_at.is_none_or(|e| e > now)
    }

    /// The write core shared by `set`, `update`, `delete` and `batch`.
    /// Runs under the lock so a batch is atomic.
    fn apply(
        docs: &mut BTreeMap<Key, Document>,
        scope: &Scope,
        write: &WriteOp,
        actor: Option<&str>,
        now: DateTime<Utc>,
        limits: Option<&Descriptor>,
    ) -> Result<WriteOutcome, StoreError> {
        let key = Self::key(scope, write.collection(), write.doc_id());
        let existing = docs.get(&key).filter(|d| Self::live(d, now)).cloned();
        if let Some(doc) = &existing {
            if let Some((owner, until)) = doc.claimed_by_other(actor.unwrap_or(""), now) {
                return Err(StoreError::Claimed {
                    owner: owner.to_string(),
                    until,
                });
            }
        }
        let expect = |want: Option<i64>| -> Result<(), StoreError> {
            match (want, &existing) {
                (None, _) => Ok(()),
                (Some(0), None) => Ok(()),
                (Some(0), Some(d)) => Err(StoreError::VersionConflict {
                    expected: 0,
                    actual: d.version,
                }),
                (Some(n), Some(d)) if d.version == n => Ok(()),
                (Some(n), Some(d)) => Err(StoreError::VersionConflict {
                    expected: n,
                    actual: d.version,
                }),
                (Some(n), None) => Err(StoreError::VersionConflict {
                    expected: n,
                    actual: 0,
                }),
            }
        };
        match write {
            WriteOp::Set {
                collection,
                doc_id,
                body,
                if_version,
                ttl_secs,
            } => {
                check_write(collection, doc_id, Some(body), *ttl_secs)?;
                check_limits(collection, body, limits)?;
                expect(*if_version)?;
                let version = existing.as_ref().map_or(1, |d| d.version + 1);
                let doc = Document {
                    scope: scope.clone(),
                    collection: collection.clone(),
                    doc_id: doc_id.clone(),
                    body: body.clone(),
                    version,
                    created_by: existing
                        .as_ref()
                        .and_then(|d| d.created_by.clone())
                        .or_else(|| actor.map(str::to_string)),
                    created_at: existing.as_ref().map_or(now, |d| d.created_at),
                    updated_at: now,
                    expires_at: expiry(*ttl_secs, limits, now),
                    lease_owner: existing.as_ref().and_then(|d| d.lease_owner.clone()),
                    lease_expires_at: existing.as_ref().and_then(|d| d.lease_expires_at),
                };
                docs.insert(key, doc);
                Ok(WriteOutcome::Written { version })
            }
            WriteOp::Update {
                collection,
                doc_id,
                body,
                if_version,
            } => {
                check_write(collection, doc_id, Some(body), None)?;
                if existing.is_none() {
                    return Err(StoreError::NotFound);
                }
                expect(*if_version)?;
                let current = existing.expect("checked above");
                let merged = merge_shallow(&current.body, body);
                validate_body(&merged)?;
                let version = current.version + 1;
                docs.insert(
                    key,
                    Document {
                        body: merged,
                        version,
                        updated_at: now,
                        ..current
                    },
                );
                Ok(WriteOutcome::Written { version })
            }
            WriteOp::Delete {
                collection,
                doc_id,
                if_version,
            } => {
                check_write(collection, doc_id, None, None)?;
                if existing.is_none() {
                    if let Some(n) = if_version {
                        if *n != 0 {
                            return Err(StoreError::VersionConflict {
                                expected: *n,
                                actual: 0,
                            });
                        }
                    }
                    docs.remove(&key);
                    return Ok(WriteOutcome::Deleted { existed: false });
                }
                expect(*if_version)?;
                docs.remove(&key);
                Ok(WriteOutcome::Deleted { existed: true })
            }
        }
    }

    fn page(mut items: Vec<Document>, query: &Query) -> Page {
        let field = query.order_by.as_deref();
        items.sort_by(|a, b| {
            let ord = compare_values(&sort_key(a, field), &sort_key(b, field))
                .then_with(|| a.doc_id.cmp(&b.doc_id));
            match query.direction {
                Direction::Asc => ord,
                Direction::Desc => ord.reverse(),
            }
        });
        let size = query.page_size();
        let remaining: Vec<Document> = match query.resume() {
            Some(Resume::After(anchor)) => items
                .into_iter()
                .filter(|d| after_anchor(d, &anchor, field, query.direction))
                .collect(),
            Some(Resume::Offset(offset)) => items.into_iter().skip(offset).collect(),
            None => items,
        };
        let more = remaining.len() > size;
        let documents: Vec<Document> = remaining.into_iter().take(size).collect();
        Page {
            next_cursor: more
                .then(|| {
                    documents
                        .last()
                        .map(|d| encode_cursor(&anchor_of(d, query)))
                })
                .flatten(),
            documents,
        }
    }
}

#[async_trait]
impl DocumentStore for MemoryDocumentStore {
    async fn get(&self, scope: &Scope, collection: &str, doc_id: &str) -> Result<Option<Document>> {
        let now = Utc::now();
        Ok(self
            .docs
            .lock()
            .get(&Self::key(scope, collection, doc_id))
            .filter(|d| Self::live(d, now))
            .cloned())
    }

    async fn set(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        body: Value,
        if_version: Option<i64>,
        ttl_secs: Option<i64>,
        actor: Option<&str>,
    ) -> Result<Document, StoreError> {
        let write = WriteOp::Set {
            collection: collection.into(),
            doc_id: doc_id.into(),
            body,
            if_version,
            ttl_secs,
        };
        let limits = self.limits_for(scope, write.collection());
        let mut docs = self.docs.lock();
        Self::apply(&mut docs, scope, &write, actor, Utc::now(), limits.as_ref())?;
        Ok(docs[&Self::key(scope, collection, doc_id)].clone())
    }

    async fn update(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        patch: Value,
        if_version: Option<i64>,
        actor: Option<&str>,
    ) -> Result<Document, StoreError> {
        let write = WriteOp::Update {
            collection: collection.into(),
            doc_id: doc_id.into(),
            body: patch,
            if_version,
        };
        let limits = self.limits_for(scope, write.collection());
        let mut docs = self.docs.lock();
        Self::apply(&mut docs, scope, &write, actor, Utc::now(), limits.as_ref())?;
        Ok(docs[&Self::key(scope, collection, doc_id)].clone())
    }

    async fn delete(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        if_version: Option<i64>,
        actor: Option<&str>,
    ) -> Result<bool, StoreError> {
        let write = WriteOp::Delete {
            collection: collection.into(),
            doc_id: doc_id.into(),
            if_version,
        };
        let limits = self.limits_for(scope, write.collection());
        let mut docs = self.docs.lock();
        match Self::apply(&mut docs, scope, &write, actor, Utc::now(), limits.as_ref())? {
            WriteOutcome::Deleted { existed } => Ok(existed),
            WriteOutcome::Written { .. } => unreachable!("delete never writes"),
        }
    }

    async fn list(&self, scope: &Scope, collection: &str, query: &Query) -> Result<Page> {
        let now = Utc::now();
        let items: Vec<Document> = self
            .docs
            .lock()
            .values()
            .filter(|d| &d.scope == scope && d.collection == collection && Self::live(d, now))
            .cloned()
            .collect();
        Ok(Self::page(items, query))
    }

    async fn query(&self, scope: &Scope, collection: &str, query: &Query) -> Result<Page> {
        if query.search.is_some() {
            // Stemming, stop words, phrase handling and ranking are Postgres
            // text-search semantics. A substring match here would agree with
            // them just often enough to be trusted and then differ in
            // production, so this says so instead.
            return Err(StoreError::Unsupported(
                "full-text search needs the Postgres backend; this store cannot reproduce its \
                 text-search semantics and will not approximate them"
                    .into(),
            )
            .into());
        }
        let now = Utc::now();
        let items: Vec<Document> = self
            .docs
            .lock()
            .values()
            .filter(|d| &d.scope == scope && d.collection == collection && Self::live(d, now))
            .filter(|d| query.filters.iter().all(|w| matches(&d.body, w)))
            .cloned()
            .collect();
        Ok(Self::page(items, query))
    }

    async fn batch(
        &self,
        scope: &Scope,
        writes: &[WriteOp],
        actor: Option<&str>,
    ) -> Result<Vec<WriteOutcome>, StoreError> {
        check_batch(writes)?;
        // A batch may span collections, so each write is measured against its
        // own. Resolved before the documents lock is taken, in the same order
        // every other write path takes them.
        let limits: Vec<Option<Descriptor>> = writes
            .iter()
            .map(|w| self.limits_for(scope, w.collection()))
            .collect();
        let mut docs = self.docs.lock();
        // Apply to a copy so a failure part-way leaves nothing behind.
        let mut staged = docs.clone();
        let now = Utc::now();
        let mut outcomes = Vec::with_capacity(writes.len());
        for (w, limits) in writes.iter().zip(&limits) {
            outcomes.push(Self::apply(
                &mut staged,
                scope,
                w,
                actor,
                now,
                limits.as_ref(),
            )?);
        }
        *docs = staged;
        Ok(outcomes)
    }

    async fn claim(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        owner: &str,
        ttl_secs: i64,
    ) -> Result<Document, StoreError> {
        check_claim(ttl_secs, owner)?;
        let now = Utc::now();
        let mut docs = self.docs.lock();
        let key = Self::key(scope, collection, doc_id);
        let doc = docs
            .get_mut(&key)
            .filter(|d| Self::live(d, now))
            .ok_or(StoreError::NotFound)?;
        if let Some((holder, until)) = doc.claimed_by_other(owner, now) {
            return Err(StoreError::Claimed {
                owner: holder.to_string(),
                until,
            });
        }
        doc.lease_owner = Some(owner.to_string());
        doc.lease_expires_at = Some(now + Duration::seconds(ttl_secs));
        Ok(doc.clone())
    }

    async fn release(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        owner: &str,
    ) -> Result<bool, StoreError> {
        let mut docs = self.docs.lock();
        let Some(doc) = docs.get_mut(&Self::key(scope, collection, doc_id)) else {
            return Ok(false);
        };
        if doc.lease_owner.as_deref() != Some(owner) {
            return Ok(false);
        }
        doc.lease_owner = None;
        doc.lease_expires_at = None;
        Ok(true)
    }

    async fn describe(
        &self,
        scope: &Scope,
        collection: &str,
        descriptor: &Descriptor,
    ) -> Result<CollectionInfo, StoreError> {
        validate_segment("collection", collection)?;
        let mut described = self.described.lock();
        let entry = described
            .entry(Self::described_key(scope, collection))
            .and_modify(|existing| {
                if existing.descriptor != *descriptor {
                    existing.descriptor = descriptor.clone();
                    existing.revision += 1;
                }
            })
            .or_insert_with(|| CollectionInfo {
                collection: collection.to_string(),
                descriptor: descriptor.clone(),
                revision: 1,
            });
        Ok(entry.clone())
    }

    async fn described(
        &self,
        scope: &Scope,
        collection: &str,
    ) -> Result<Option<CollectionInfo>, StoreError> {
        Ok(self
            .described
            .lock()
            .get(&Self::described_key(scope, collection))
            .cloned())
    }

    async fn collections(&self, scope: &Scope) -> Result<Vec<CollectionInfo>, StoreError> {
        let (kind, id) = (scope.kind().to_string(), scope.id().to_string());
        Ok(self
            .described
            .lock()
            .iter()
            .filter(|((k, i, _), _)| *k == kind && *i == id)
            .map(|(_, info)| info.clone())
            .collect())
    }

    async fn content_write(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        name: &str,
        offset: i64,
        bytes: &[u8],
        content_type: Option<&str>,
    ) -> Result<ContentWrite, StoreError> {
        validate_segment("name", name)?;
        let mut content = self.content.lock();
        let entry = content
            .entry(Self::content_key(scope, collection, doc_id, name))
            .or_default();
        content::check_write(offset, bytes, entry.pending.len() as i64)?;
        if entry.pending.is_empty() {
            entry.version += 1;
            entry.pending_type = content_type.map(str::to_string);
        }
        entry.pending.extend_from_slice(bytes);
        Ok(ContentWrite {
            version: entry.version,
            next_offset: entry.pending.len() as i64,
        })
    }

    async fn content_commit(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        name: &str,
    ) -> Result<ContentInfo, StoreError> {
        let mut content = self.content.lock();
        let entry = content
            .get_mut(&Self::content_key(scope, collection, doc_id, name))
            .filter(|e| !e.pending.is_empty())
            .ok_or_else(|| StoreError::Invalid("nothing is pending under that name".into()))?;
        let bytes = std::mem::take(&mut entry.pending);
        let info = ContentInfo {
            name: name.to_string(),
            size_bytes: bytes.len() as i64,
            content_type: entry.pending_type.clone(),
            digest: Some(content::digest_of(&bytes)),
            version: entry.version,
        };
        entry.committed = Some((bytes, info.clone()));
        Ok(info)
    }

    async fn content_read(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        name: &str,
        offset: i64,
        len: i64,
    ) -> Result<Option<ContentSlice>, StoreError> {
        content::check_read(offset, len)?;
        Ok(self
            .content
            .lock()
            .get(&Self::content_key(scope, collection, doc_id, name))
            .and_then(|e| e.committed.as_ref())
            .map(|(bytes, info)| {
                let start = (offset as usize).min(bytes.len());
                let end = start.saturating_add(len as usize).min(bytes.len());
                ContentSlice {
                    offset,
                    bytes: bytes[start..end].to_vec(),
                    size_bytes: info.size_bytes,
                }
            }))
    }

    async fn content_stat(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        name: &str,
    ) -> Result<Option<ContentInfo>, StoreError> {
        Ok(self
            .content
            .lock()
            .get(&Self::content_key(scope, collection, doc_id, name))
            .and_then(|e| e.committed.as_ref())
            .map(|(_, info)| info.clone()))
    }

    async fn content_list(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
    ) -> Result<Vec<ContentInfo>, StoreError> {
        let (kind, id) = (scope.kind().to_string(), scope.id().to_string());
        Ok(self
            .content
            .lock()
            .iter()
            .filter(|((k, i, c, d, _), _)| *k == kind && *i == id && c == collection && d == doc_id)
            .filter_map(|(_, e)| e.committed.as_ref().map(|(_, info)| info.clone()))
            .collect())
    }

    async fn content_delete(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        name: &str,
    ) -> Result<bool, StoreError> {
        Ok(self
            .content
            .lock()
            .remove(&Self::content_key(scope, collection, doc_id, name))
            .is_some_and(|e| e.committed.is_some()))
    }

    async fn scope_documents(&self, scope: &Scope) -> Result<usize, StoreError> {
        let docs = self.docs.lock();
        Ok(docs
            .keys()
            .filter(|(kind, id, _, _)| kind == scope.kind() && id == scope.id())
            .count())
    }

    async fn user_scopes(&self) -> Result<BTreeMap<String, usize>, StoreError> {
        let user = Scope::User(String::new());
        Ok(self
            .docs
            .lock()
            .keys()
            .filter(|(kind, _, _, _)| kind == user.kind())
            .fold(BTreeMap::new(), |mut held, (_, id, _, _)| {
                *held.entry(id.clone()).or_default() += 1;
                held
            }))
    }

    async fn rescope(&self, from: &Scope, to: &Scope) -> Result<Rescoped, StoreError> {
        let in_scope =
            |scope: &Scope, kind: &str, id: &str| kind == scope.kind() && id == scope.id();
        let mut docs = self.docs.lock();
        let mut content = self.content.lock();
        let mut outcome = Rescoped::default();
        let theirs: Vec<Key> = docs
            .keys()
            .filter(|(kind, id, _, _)| in_scope(from, kind, id))
            .cloned()
            .collect();
        for key in theirs {
            let mut doc = docs.remove(&key).expect("listed above");
            let (_, _, collection, doc_id) = key;
            let streams: Vec<ContentKey> = content
                .keys()
                .filter(|(kind, id, c, d, _)| {
                    in_scope(from, kind, id) && *c == collection && *d == doc_id
                })
                .cloned()
                .collect();
            let dest = (
                to.kind().to_string(),
                to.id().to_string(),
                collection.clone(),
                doc_id.clone(),
            );
            if docs.contains_key(&dest) {
                streams.iter().for_each(|k| {
                    content.remove(k);
                });
                outcome.kept.push(format!("{collection}/{doc_id}"));
                continue;
            }
            for stream in streams {
                let entry = content.remove(&stream).expect("listed above");
                let (_, _, c, d, name) = stream;
                content.insert(
                    (to.kind().to_string(), to.id().to_string(), c, d, name),
                    entry,
                );
            }
            doc.scope = to.clone();
            doc.lease_owner = None;
            doc.lease_expires_at = None;
            docs.insert(dest, doc);
            outcome.moved += 1;
        }
        self.described
            .lock()
            .retain(|(kind, id, _), _| !in_scope(from, kind, id));
        Ok(outcome)
    }

    async fn drop_scope(&self, scope: &Scope) -> Result<usize, StoreError> {
        let in_scope = |kind: &str, id: &str| kind == scope.kind() && id == scope.id();
        let mut docs = self.docs.lock();
        let before = docs.len();
        docs.retain(|(kind, id, _, _), _| !in_scope(kind, id));
        self.content
            .lock()
            .retain(|(kind, id, _, _, _), _| !in_scope(kind, id));
        self.described
            .lock()
            .retain(|(kind, id, _), _| !in_scope(kind, id));
        Ok(before - docs.len())
    }

    async fn purge_expired(&self) -> Result<u64> {
        let now = Utc::now();
        let mut docs = self.docs.lock();
        let before = docs.len();
        docs.retain(|_, d| Self::live(d, now));
        Ok((before - docs.len()) as u64)
    }

    fn is_shared(&self) -> bool {
        false
    }
}

fn check_batch(writes: &[WriteOp]) -> Result<(), StoreError> {
    if writes.is_empty() {
        return Err(StoreError::Invalid("batch has no writes".into()));
    }
    if writes.len() > MAX_BATCH {
        return Err(StoreError::Invalid(format!(
            "batch has {} writes; the limit is {MAX_BATCH}",
            writes.len()
        )));
    }
    let mut seen = std::collections::HashSet::new();
    for w in writes {
        if !seen.insert((w.collection(), w.doc_id())) {
            return Err(StoreError::Invalid(format!(
                "batch addresses {}/{} twice",
                w.collection(),
                w.doc_id()
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

static SHARED_DOCUMENT_STORE: tokio::sync::OnceCell<Arc<dyn DocumentStore>> =
    tokio::sync::OnceCell::const_new();

/// The process's document store, built once: Postgres when `[postgres]` is
/// enabled (shared across hub instances, durable); otherwise in-memory,
/// which is honest about what it is — a hub without Postgres keeps this
/// state only while it runs. Every agent service and the gateway share it.
pub async fn build_document_store(config: &WorkforceConfig) -> Result<Arc<dyn DocumentStore>> {
    SHARED_DOCUMENT_STORE
        .get_or_try_init(|| async {
            match config.postgres.as_ref().filter(|p| p.enabled) {
                Some(pg) => build_postgres_document_store(pg).await,
                None => {
                    tracing::warn!(
                        "[postgres] is not enabled — the document store is in-memory and lost on restart"
                    );
                    Ok(Arc::new(MemoryDocumentStore::new()) as Arc<dyn DocumentStore>)
                }
            }
        })
        .await
        .cloned()
}

/// The store built by [`build_document_store`], for surfaces that come up
/// after the agent services (the gateway). `None` until the first build.
pub fn global_document_store() -> Option<Arc<dyn DocumentStore>> {
    SHARED_DOCUMENT_STORE.get().cloned()
}

#[cfg(feature = "postgres")]
async fn build_postgres_document_store(
    pg: &workforce_postgres::PostgresConfig,
) -> Result<Arc<dyn DocumentStore>> {
    let pool = postgres::shared_pool(pg).await?;
    Ok(Arc::new(postgres::PostgresDocumentStore::new(pool)))
}

#[cfg(not(feature = "postgres"))]
async fn build_postgres_document_store(
    _pg: &workforce_postgres::PostgresConfig,
) -> Result<Arc<dyn DocumentStore>> {
    eyre::bail!("[postgres] is enabled but this build lacks the `postgres` feature")
}

/// A store whose reads and writes reach `inner` only after `interceptor` has
/// acted, as another run would act between two steps of the code under test.
#[cfg(test)]
pub(crate) mod intercepted {
    use super::*;

    #[async_trait]
    pub trait Interceptor: Send + Sync {
        async fn before_get(&self, _inner: &dyn DocumentStore) {}

        async fn before_set(
            &self,
            _inner: &dyn DocumentStore,
            _scope: &Scope,
            _collection: &str,
            _doc_id: &str,
        ) -> Result<(), StoreError> {
            Ok(())
        }

        async fn before_list(
            &self,
            _inner: &dyn DocumentStore,
            _scope: &Scope,
            _collection: &str,
        ) -> Result<()> {
            Ok(())
        }
    }

    pub struct Intercepted<S, I> {
        pub inner: S,
        pub interceptor: I,
    }

    #[async_trait]
    impl<S: DocumentStore, I: Interceptor> DocumentStore for Intercepted<S, I> {
        async fn get(
            &self,
            scope: &Scope,
            collection: &str,
            doc_id: &str,
        ) -> Result<Option<Document>> {
            self.interceptor.before_get(&self.inner).await;
            self.inner.get(scope, collection, doc_id).await
        }

        async fn set(
            &self,
            scope: &Scope,
            collection: &str,
            doc_id: &str,
            body: Value,
            if_version: Option<i64>,
            ttl_secs: Option<i64>,
            actor: Option<&str>,
        ) -> Result<Document, StoreError> {
            self.interceptor
                .before_set(&self.inner, scope, collection, doc_id)
                .await?;
            self.inner
                .set(scope, collection, doc_id, body, if_version, ttl_secs, actor)
                .await
        }

        async fn update(
            &self,
            scope: &Scope,
            collection: &str,
            doc_id: &str,
            patch: Value,
            if_version: Option<i64>,
            actor: Option<&str>,
        ) -> Result<Document, StoreError> {
            self.inner
                .update(scope, collection, doc_id, patch, if_version, actor)
                .await
        }

        async fn delete(
            &self,
            scope: &Scope,
            collection: &str,
            doc_id: &str,
            if_version: Option<i64>,
            actor: Option<&str>,
        ) -> Result<bool, StoreError> {
            self.inner
                .delete(scope, collection, doc_id, if_version, actor)
                .await
        }

        async fn list(&self, scope: &Scope, collection: &str, query: &Query) -> Result<Page> {
            self.interceptor
                .before_list(&self.inner, scope, collection)
                .await?;
            self.inner.list(scope, collection, query).await
        }

        async fn query(&self, scope: &Scope, collection: &str, query: &Query) -> Result<Page> {
            self.inner.query(scope, collection, query).await
        }

        async fn batch(
            &self,
            scope: &Scope,
            writes: &[WriteOp],
            actor: Option<&str>,
        ) -> Result<Vec<WriteOutcome>, StoreError> {
            self.inner.batch(scope, writes, actor).await
        }

        async fn claim(
            &self,
            scope: &Scope,
            collection: &str,
            doc_id: &str,
            owner: &str,
            ttl_secs: i64,
        ) -> Result<Document, StoreError> {
            self.inner
                .claim(scope, collection, doc_id, owner, ttl_secs)
                .await
        }

        async fn release(
            &self,
            scope: &Scope,
            collection: &str,
            doc_id: &str,
            owner: &str,
        ) -> Result<bool, StoreError> {
            self.inner.release(scope, collection, doc_id, owner).await
        }

        async fn describe(
            &self,
            scope: &Scope,
            collection: &str,
            descriptor: &Descriptor,
        ) -> Result<CollectionInfo, StoreError> {
            self.inner.describe(scope, collection, descriptor).await
        }

        async fn described(
            &self,
            scope: &Scope,
            collection: &str,
        ) -> Result<Option<CollectionInfo>, StoreError> {
            self.inner.described(scope, collection).await
        }

        async fn collections(&self, scope: &Scope) -> Result<Vec<CollectionInfo>, StoreError> {
            self.inner.collections(scope).await
        }

        async fn content_write(
            &self,
            scope: &Scope,
            collection: &str,
            doc_id: &str,
            name: &str,
            offset: i64,
            bytes: &[u8],
            content_type: Option<&str>,
        ) -> Result<ContentWrite, StoreError> {
            self.inner
                .content_write(scope, collection, doc_id, name, offset, bytes, content_type)
                .await
        }

        async fn content_commit(
            &self,
            scope: &Scope,
            collection: &str,
            doc_id: &str,
            name: &str,
        ) -> Result<ContentInfo, StoreError> {
            self.inner
                .content_commit(scope, collection, doc_id, name)
                .await
        }

        async fn content_read(
            &self,
            scope: &Scope,
            collection: &str,
            doc_id: &str,
            name: &str,
            offset: i64,
            len: i64,
        ) -> Result<Option<ContentSlice>, StoreError> {
            self.inner
                .content_read(scope, collection, doc_id, name, offset, len)
                .await
        }

        async fn content_stat(
            &self,
            scope: &Scope,
            collection: &str,
            doc_id: &str,
            name: &str,
        ) -> Result<Option<ContentInfo>, StoreError> {
            self.inner
                .content_stat(scope, collection, doc_id, name)
                .await
        }

        async fn content_list(
            &self,
            scope: &Scope,
            collection: &str,
            doc_id: &str,
        ) -> Result<Vec<ContentInfo>, StoreError> {
            self.inner.content_list(scope, collection, doc_id).await
        }

        async fn content_delete(
            &self,
            scope: &Scope,
            collection: &str,
            doc_id: &str,
            name: &str,
        ) -> Result<bool, StoreError> {
            self.inner
                .content_delete(scope, collection, doc_id, name)
                .await
        }

        async fn scope_documents(&self, scope: &Scope) -> Result<usize, StoreError> {
            self.inner.scope_documents(scope).await
        }

        async fn user_scopes(&self) -> Result<BTreeMap<String, usize>, StoreError> {
            self.inner.user_scopes().await
        }

        async fn rescope(&self, from: &Scope, to: &Scope) -> Result<Rescoped, StoreError> {
            self.inner.rescope(from, to).await
        }

        async fn drop_scope(&self, scope: &Scope) -> Result<usize, StoreError> {
            self.inner.drop_scope(scope).await
        }

        async fn purge_expired(&self) -> Result<u64> {
            self.inner.purge_expired().await
        }

        fn is_shared(&self) -> bool {
            self.inner.is_shared()
        }
    }
}

// ---------------------------------------------------------------------------
// Tests: the reference semantics every backend must match
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod conformance {
    //! Shared by the in-memory tests here and the Postgres tests in
    //! `postgres.rs`, so the two backends cannot drift.
    use super::*;
    use serde_json::json;

    pub async fn run_all(store: &dyn DocumentStore) {
        set_get_update_delete(store).await;
        versions_gate_writes(store).await;
        update_needs_a_document(store).await;
        scopes_are_separate(store).await;
        query_filters_orders_and_pages(store).await;
        a_keyset_walk_sees_every_row_exactly_once(store).await;
        a_keyset_walk_survives_writes_underneath_it(store).await;
        mixed_types_and_missing_fields_still_page(store).await;
        batch_is_all_or_nothing(store).await;
        claims_exclude_other_owners_and_lapse(store).await;
        ttl_hides_then_purges(store).await;
        bodies_are_checked(store).await;
        describing_a_collection_is_idempotent(store).await;
        an_undescribed_collection_declares_nothing(store).await;
        content_streams_in_and_reads_back_in_ranges(store).await;
        a_reader_never_sees_a_stream_mid_write(store).await;
        content_writes_out_of_order_are_refused(store).await;
        a_collection_can_limit_document_size_and_lifetime(store).await;
        a_scope_moves_keeping_the_destinations_own_and_is_dropped(store).await;
    }

    pub async fn a_scope_moves_keeping_the_destinations_own_and_is_dropped(
        store: &dyn DocumentStore,
    ) {
        let alice = Scope::User("lifecycle-alice".into());
        let bob = Scope::User("lifecycle-bob".into());
        for (scope, doc, body) in [
            (&alice, "mine", json!({"who": "alice"})),
            (&alice, "both", json!({"who": "alice"})),
            (&bob, "both", json!({"who": "bob"})),
        ] {
            store
                .set(scope, "notes", doc, body, None, None, Some("t"))
                .await
                .unwrap();
        }
        store
            .content_write(&alice, "notes", "mine", "file", 0, b"bytes", None)
            .await
            .unwrap();
        store
            .content_commit(&alice, "notes", "mine", "file")
            .await
            .unwrap();
        store
            .content_write(&alice, "notes", "both", "file", 0, b"alices", None)
            .await
            .unwrap();
        store
            .content_commit(&alice, "notes", "both", "file")
            .await
            .unwrap();
        assert_eq!(store.scope_documents(&alice).await.unwrap(), 2);
        let held = store.user_scopes().await.unwrap();
        assert_eq!(
            (held.get("lifecycle-alice"), held.get("lifecycle-bob")),
            (Some(&2), Some(&1))
        );
        assert!(!held.contains_key("limits"), "only people's scopes");

        let outcome = store.rescope(&alice, &bob).await.unwrap();
        assert_eq!(
            outcome,
            Rescoped {
                moved: 1,
                kept: vec!["notes/both".into()],
            }
        );
        assert_eq!(store.scope_documents(&alice).await.unwrap(), 0);
        let both = store.get(&bob, "notes", "both").await.unwrap().unwrap();
        assert_eq!(
            both.body,
            json!({"who": "bob"}),
            "the destination's own wins"
        );
        let moved = store.get(&bob, "notes", "mine").await.unwrap().unwrap();
        assert_eq!(moved.scope, bob);
        let read = store
            .content_read(&bob, "notes", "mine", "file", 0, 100)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read.bytes, b"bytes");
        assert!(store
            .content_read(&bob, "notes", "both", "file", 0, 100)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .content_read(&alice, "notes", "mine", "file", 0, 100)
            .await
            .unwrap()
            .is_none());

        assert_eq!(store.drop_scope(&bob).await.unwrap(), 2);
        assert_eq!(store.scope_documents(&bob).await.unwrap(), 0);
        assert!(store
            .content_read(&bob, "notes", "mine", "file", 0, 100)
            .await
            .unwrap()
            .is_none());
    }

    pub async fn a_collection_can_limit_document_size_and_lifetime(store: &dyn DocumentStore) {
        let s = Scope::Agent("limits".into());
        store
            .describe(
                &s,
                "capped",
                &Descriptor::from_json(&json!({"max_doc_bytes": 120, "ttl_secs": 600})).unwrap(),
            )
            .await
            .unwrap();

        let small = store
            .set(&s, "capped", "ok", json!({"n": 1}), None, None, None)
            .await
            .unwrap();
        assert!(
            small.expires_at.is_some(),
            "a collection TTL applies to a write that names none"
        );

        let own_ttl = store
            .set(&s, "capped", "own", json!({"n": 1}), None, Some(30), None)
            .await
            .unwrap();
        assert!(
            own_ttl.expires_at.unwrap() < small.expires_at.unwrap(),
            "a write's own TTL wins over the collection's default"
        );

        let oversized = json!({"padding": "x".repeat(200)});
        assert!(
            store
                .set(&s, "capped", "too-big", oversized, None, None, None)
                .await
                .is_err(),
            "a document past the collection's limit is refused"
        );

        // An undescribed collection is unlimited beyond the store's own cap.
        assert!(store
            .set(
                &s,
                "free",
                "big",
                json!({"padding": "x".repeat(200)}),
                None,
                None,
                None
            )
            .await
            .is_ok());
    }

    pub async fn content_streams_in_and_reads_back_in_ranges(store: &dyn DocumentStore) {
        let s = Scope::Agent("content".into());
        store
            .set(
                &s,
                "packs",
                "p1",
                json!({"evidence": {"$content": "dump"}}),
                None,
                None,
                None,
            )
            .await
            .unwrap();

        let pieces: Vec<Vec<u8>> = (0..4u8).map(|i| vec![b'a' + i; 1000]).collect();
        let mut offset = 0i64;
        for piece in &pieces {
            let write = store
                .content_write(&s, "packs", "p1", "dump", offset, piece, Some("text/plain"))
                .await
                .unwrap();
            offset = write.next_offset;
        }
        assert_eq!(offset, 4000);

        let info = store
            .content_commit(&s, "packs", "p1", "dump")
            .await
            .unwrap();
        assert_eq!(info.size_bytes, 4000);
        assert_eq!(info.content_type.as_deref(), Some("text/plain"));

        let whole: Vec<u8> = pieces.concat();
        assert_eq!(
            info.digest.as_deref(),
            Some(content::digest_of(&whole).as_str()),
            "the digest describes the bytes that were actually stored"
        );

        let all = store
            .content_read(&s, "packs", "p1", "dump", 0, 4000)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(all.bytes, whole);
        assert_eq!(all.size_bytes, 4000);

        // Ranges that start mid-chunk, span chunks, and run past the end.
        for (offset, len) in [(0i64, 10i64), (999, 2), (1500, 1200), (3990, 50)] {
            let slice = store
                .content_read(&s, "packs", "p1", "dump", offset, len)
                .await
                .unwrap()
                .unwrap();
            let end = ((offset + len) as usize).min(whole.len());
            assert_eq!(
                slice.bytes,
                whole[offset as usize..end],
                "range {offset}..{len} came back wrong"
            );
        }

        assert_eq!(
            store.content_list(&s, "packs", "p1").await.unwrap().len(),
            1
        );
        assert_eq!(
            store
                .content_stat(&s, "packs", "p1", "dump")
                .await
                .unwrap()
                .unwrap()
                .size_bytes,
            4000
        );
        assert!(store
            .content_delete(&s, "packs", "p1", "dump")
            .await
            .unwrap());
        assert!(store
            .content_read(&s, "packs", "p1", "dump", 0, 10)
            .await
            .unwrap()
            .is_none());
        assert!(!store
            .content_delete(&s, "packs", "p1", "dump")
            .await
            .unwrap());
    }

    /// The property the versioning exists for: bytes are invisible until they
    /// are committed, so a writer that dies mid-upload cannot be read as a
    /// truncated stream.
    pub async fn a_reader_never_sees_a_stream_mid_write(store: &dyn DocumentStore) {
        let s = Scope::Agent("torn".into());
        store
            .content_write(&s, "c", "d", "s", 0, b"first", None)
            .await
            .unwrap();
        store.content_commit(&s, "c", "d", "s").await.unwrap();

        store
            .content_write(&s, "c", "d", "s", 0, b"second-and-longer", None)
            .await
            .unwrap();
        let during = store
            .content_read(&s, "c", "d", "s", 0, 100)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            during.bytes, b"first",
            "an uncommitted write must not be visible"
        );

        store.content_commit(&s, "c", "d", "s").await.unwrap();
        let after = store
            .content_read(&s, "c", "d", "s", 0, 100)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.bytes, b"second-and-longer");
        assert_eq!(after.size_bytes, 17, "the size is the new version's");
    }

    pub async fn content_writes_out_of_order_are_refused(store: &dyn DocumentStore) {
        let s = Scope::Agent("order".into());
        store
            .content_write(&s, "c", "d", "s", 0, b"abc", None)
            .await
            .unwrap();
        assert!(
            store
                .content_write(&s, "c", "d", "s", 0, b"x", None)
                .await
                .is_err(),
            "rewriting an offset already written is refused"
        );
        assert!(
            store
                .content_write(&s, "c", "d", "s", 99, b"x", None)
                .await
                .is_err(),
            "a gap is refused"
        );
        assert!(
            store.content_commit(&s, "c", "d", "nothing").await.is_err(),
            "committing a stream nobody wrote is refused"
        );
    }

    pub async fn describing_a_collection_is_idempotent(store: &dyn DocumentStore) {
        let s = Scope::Agent("desc".into());
        let descriptor = Descriptor::from_json(&json!({
            "scale": "large",
            "indexes": [{"field": "status"}, {"field": "tags", "type": "contains"}]
        }))
        .unwrap();

        let first = store.describe(&s, "events", &descriptor).await.unwrap();
        assert_eq!(first.revision, 1);
        assert_eq!(first.descriptor, descriptor);

        let again = store.describe(&s, "events", &descriptor).await.unwrap();
        assert_eq!(
            again.revision, 1,
            "re-applying the same descriptor changes nothing"
        );

        let narrowed = Descriptor::from_json(&json!({
            "scale": "large",
            "indexes": [{"field": "status"}]
        }))
        .unwrap();
        let third = store.describe(&s, "events", &narrowed).await.unwrap();
        assert_eq!(third.revision, 2, "a changed descriptor is a new revision");
        assert_eq!(third.descriptor, narrowed);

        assert_eq!(
            store
                .described(&s, "events")
                .await
                .unwrap()
                .unwrap()
                .descriptor,
            narrowed
        );
        assert_eq!(
            store.collections(&s).await.unwrap().len(),
            1,
            "one described collection in this scope"
        );

        // Documents written before and after a descriptor both read back.
        store
            .set(
                &s,
                "events",
                "e1",
                json!({"status": "open"}),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .query(
                    &s,
                    "events",
                    &Query::from_json(&json!({"where": [["status", "eq", "open"]]})).unwrap()
                )
                .await
                .unwrap()
                .documents
                .len(),
            1
        );
    }

    pub async fn an_undescribed_collection_declares_nothing(store: &dyn DocumentStore) {
        let s = Scope::Agent("plain".into());
        store
            .set(&s, "todo", "t1", json!({"done": false}), None, None, None)
            .await
            .unwrap();
        assert!(
            store.described(&s, "todo").await.unwrap().is_none(),
            "a collection nobody described has no descriptor, and works anyway"
        );
        assert_eq!(
            store.get(&s, "todo", "t1").await.unwrap().unwrap().version,
            1
        );
    }

    /// Walk a collection a page at a time, following `next_cursor` to the end.
    async fn walk(
        store: &dyn DocumentStore,
        scope: &Scope,
        collection: &str,
        q: &Query,
    ) -> Vec<String> {
        let mut seen = Vec::new();
        let mut cursor = q.cursor.clone();
        loop {
            let page = store
                .query(
                    scope,
                    collection,
                    &Query {
                        cursor: cursor.clone(),
                        ..q.clone()
                    },
                )
                .await
                .unwrap();
            seen.extend(page.documents.iter().map(|d| d.doc_id.clone()));
            match page.next_cursor {
                Some(next) => {
                    assert_ne!(Some(&next), cursor.as_ref(), "cursor must advance");
                    cursor = Some(next);
                }
                None => return seen,
            }
        }
    }

    pub async fn a_keyset_walk_sees_every_row_exactly_once(store: &dyn DocumentStore) {
        let s = Scope::Agent("walk".into());
        let expected: Vec<String> = (0..25).map(|i| format!("d{i:03}")).collect();
        for (i, id) in expected.iter().enumerate() {
            store
                .set(&s, "c", id, json!({"n": i}), None, None, None)
                .await
                .unwrap();
        }
        let q = Query {
            order_by: Some("n".into()),
            limit: Some(4),
            ..Default::default()
        };
        assert_eq!(walk(store, &s, "c", &q).await, expected);

        let mut backwards = expected.clone();
        backwards.reverse();
        assert_eq!(
            walk(
                store,
                &s,
                "c",
                &Query {
                    direction: Direction::Desc,
                    ..q.clone()
                }
            )
            .await,
            backwards
        );
    }

    /// The property offset paging cannot hold: rows inserted behind the walk
    /// must not move the ones ahead of it into being skipped or repeated.
    pub async fn a_keyset_walk_survives_writes_underneath_it(store: &dyn DocumentStore) {
        let s = Scope::Agent("order".into());
        for i in 0..10u32 {
            store
                .set(
                    &s,
                    "c",
                    &format!("b{i:02}"),
                    json!({"n": i * 10}),
                    None,
                    None,
                    None,
                )
                .await
                .unwrap();
        }
        let q = Query {
            order_by: Some("n".into()),
            limit: Some(3),
            ..Default::default()
        };

        let first = store.query(&s, "c", &q).await.unwrap();
        let cursor = first.next_cursor.clone().expect("more to come");

        // Three rows sort before the cursor: under offset paging these would
        // push three unseen rows past the offset and out of the walk.
        for i in 0..3u32 {
            store
                .set(&s, "c", &format!("a{i}"), json!({"n": 0}), None, None, None)
                .await
                .unwrap();
        }

        let mut seen: Vec<String> = first.documents.iter().map(|d| d.doc_id.clone()).collect();
        seen.extend(
            walk(
                store,
                &s,
                "c",
                &Query {
                    cursor: Some(cursor),
                    ..q
                },
            )
            .await,
        );

        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            seen.len(),
            "a row was returned twice: {seen:?}"
        );
        for i in 0..10u32 {
            let id = format!("b{i:02}");
            assert!(seen.contains(&id), "row {id} was skipped: {seen:?}");
        }
    }

    /// Sort keys of different JSON types, and documents missing the field
    /// entirely, all have a definite place — otherwise a walk loses them.
    pub async fn mixed_types_and_missing_fields_still_page(store: &dyn DocumentStore) {
        let s = Scope::Agent("mixed".into());
        for (id, body) in [
            ("str", json!({"v": "text"})),
            ("num", json!({"v": 42})),
            ("boo", json!({"v": true})),
            ("nul", json!({"v": null})),
            ("absent", json!({"other": 1})),
        ] {
            store
                .set(&s, "c", id, body, None, None, None)
                .await
                .unwrap();
        }
        let q = Query {
            order_by: Some("v".into()),
            limit: Some(2),
            ..Default::default()
        };
        let seen = walk(store, &s, "c", &q).await;
        assert_eq!(seen.len(), 5, "every row must be paged: {seen:?}");
        assert_eq!(
            &seen[..2],
            ["absent", "nul"],
            "a missing field sorts as JSON null, and ties break by id: {seen:?}"
        );
        assert_eq!(
            &seen[2..],
            ["str", "num", "boo"],
            "jsonb kind order is null < string < number < boolean: {seen:?}"
        );
    }

    fn agent() -> Scope {
        Scope::Agent("atlas".into())
    }

    pub async fn set_get_update_delete(store: &dyn DocumentStore) {
        let s = agent();
        let doc = store
            .set(
                &s,
                "pipeline",
                "ENG-1",
                json!({"stage": "pick", "n": 1}),
                None,
                None,
                Some("run-a"),
            )
            .await
            .unwrap();
        assert_eq!(doc.version, 1);
        assert_eq!(doc.created_by.as_deref(), Some("run-a"));
        let got = store.get(&s, "pipeline", "ENG-1").await.unwrap().unwrap();
        assert_eq!(got.body["stage"], "pick");

        let doc = store
            .update(
                &s,
                "pipeline",
                "ENG-1",
                json!({"stage": "review", "n": null}),
                None,
                Some("run-b"),
            )
            .await
            .unwrap();
        assert_eq!(doc.version, 2);
        assert_eq!(doc.body, json!({"stage": "review"}), "null removes a key");
        assert_eq!(doc.created_by.as_deref(), Some("run-a"), "creator is kept");

        assert!(store
            .delete(&s, "pipeline", "ENG-1", None, None)
            .await
            .unwrap());
        assert!(!store
            .delete(&s, "pipeline", "ENG-1", None, None)
            .await
            .unwrap());
        assert!(store.get(&s, "pipeline", "ENG-1").await.unwrap().is_none());
    }

    pub async fn versions_gate_writes(store: &dyn DocumentStore) {
        let s = agent();
        // create-only
        store
            .set(&s, "cas", "d", json!({"v": "a"}), Some(0), None, None)
            .await
            .unwrap();
        let err = store
            .set(&s, "cas", "d", json!({"v": "b"}), Some(0), None, None)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::VersionConflict {
                    expected: 0,
                    actual: 1
                }
            ),
            "{err}"
        );
        // right version wins, stale loses
        store
            .set(&s, "cas", "d", json!({"v": "b"}), Some(1), None, None)
            .await
            .unwrap();
        let err = store
            .update(&s, "cas", "d", json!({"v": "c"}), Some(1), None)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::VersionConflict {
                    expected: 1,
                    actual: 2
                }
            ),
            "{err}"
        );
        let err = store
            .delete(&s, "cas", "d", Some(9), None)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::VersionConflict { .. }));
        assert!(store.delete(&s, "cas", "d", Some(2), None).await.unwrap());
    }

    pub async fn update_needs_a_document(store: &dyn DocumentStore) {
        let err = store
            .update(&agent(), "cas", "missing", json!({"x": 1}), None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::NotFound));
    }

    pub async fn scopes_are_separate(store: &dyn DocumentStore) {
        let a = Scope::User("alice".into());
        let b = Scope::User("bob".into());
        store
            .set(&a, "todo", "1", json!({"t": "a"}), None, None, None)
            .await
            .unwrap();
        store
            .set(&b, "todo", "1", json!({"t": "b"}), None, None, None)
            .await
            .unwrap();
        store
            .set(
                &Scope::Global,
                "todo",
                "1",
                json!({"t": "g"}),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            store.get(&a, "todo", "1").await.unwrap().unwrap().body["t"],
            "a"
        );
        assert_eq!(
            store.get(&b, "todo", "1").await.unwrap().unwrap().body["t"],
            "b"
        );
        assert_eq!(
            store
                .get(&Scope::Global, "todo", "1")
                .await
                .unwrap()
                .unwrap()
                .body["t"],
            "g"
        );
        assert_eq!(
            store
                .list(&a, "todo", &Query::default())
                .await
                .unwrap()
                .documents
                .len(),
            1
        );
    }

    pub async fn query_filters_orders_and_pages(store: &dyn DocumentStore) {
        let s = Scope::Agent("q".into());
        for (id, stage, n, tags) in [
            ("a", "pick", 3, json!(["x"])),
            ("b", "review", 1, json!(["x", "y"])),
            ("c", "review", 2, json!([])),
            ("d", "done", 5, json!(["y"])),
        ] {
            store
                .set(
                    &s,
                    "p",
                    id,
                    json!({"stage": stage, "n": n, "tags": tags, "meta": {"k": id}}),
                    None,
                    None,
                    None,
                )
                .await
                .unwrap();
        }
        let q = |w: Value| Query::from_json(&json!({"where": w})).unwrap();
        let ids = |p: Page| {
            p.documents
                .into_iter()
                .map(|d| d.doc_id)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            ids(store
                .query(&s, "p", &q(json!([["stage", "eq", "review"]])))
                .await
                .unwrap()),
            ["b", "c"]
        );
        assert_eq!(
            ids(store
                .query(&s, "p", &q(json!([["stage", "ne", "review"]])))
                .await
                .unwrap()),
            ["a", "d"]
        );
        assert_eq!(
            ids(store
                .query(&s, "p", &q(json!([["n", "gte", 3]])))
                .await
                .unwrap()),
            ["a", "d"]
        );
        assert_eq!(
            ids(store
                .query(&s, "p", &q(json!([["n", "lt", 2]])))
                .await
                .unwrap()),
            ["b"]
        );
        assert_eq!(
            ids(store
                .query(&s, "p", &q(json!([["stage", "in", ["pick", "done"]]])))
                .await
                .unwrap()),
            ["a", "d"]
        );
        assert_eq!(
            ids(store
                .query(&s, "p", &q(json!([["stage", "not-in", ["pick", "done"]]])))
                .await
                .unwrap()),
            ["b", "c"]
        );
        assert_eq!(
            ids(store
                .query(&s, "p", &q(json!([["tags", "array-contains", "y"]])))
                .await
                .unwrap()),
            ["b", "d"]
        );
        assert_eq!(
            ids(store
                .query(&s, "p", &q(json!([["meta.k", "eq", "c"]])))
                .await
                .unwrap()),
            ["c"]
        );
        assert_eq!(
            ids(store
                .query(
                    &s,
                    "p",
                    &q(json!([["stage", "eq", "review"], ["n", "gt", 1]]))
                )
                .await
                .unwrap()),
            ["c"]
        );
        // a string filter never matches a number field
        assert!(ids(store
            .query(&s, "p", &q(json!([["n", "gt", "1"]])))
            .await
            .unwrap())
        .is_empty());

        let ordered =
            Query::from_json(&json!({"order_by": "n", "direction": "desc", "limit": 2})).unwrap();
        let page = store.query(&s, "p", &ordered).await.unwrap();
        assert_eq!(
            page.documents
                .iter()
                .map(|d| d.doc_id.as_str())
                .collect::<Vec<_>>(),
            ["d", "a"]
        );
        let cursor = page.next_cursor.expect("more to come");
        let next = Query {
            cursor: Some(cursor),
            ..ordered.clone()
        };
        let page = store.query(&s, "p", &next).await.unwrap();
        assert_eq!(
            page.documents
                .iter()
                .map(|d| d.doc_id.as_str())
                .collect::<Vec<_>>(),
            ["c", "b"]
        );
        assert!(page.next_cursor.is_none());

        // list = query with no filters; default order is updated_at ascending
        assert_eq!(
            store
                .list(&s, "p", &Query::default())
                .await
                .unwrap()
                .documents
                .len(),
            4
        );
    }

    pub async fn batch_is_all_or_nothing(store: &dyn DocumentStore) {
        let s = Scope::Agent("batch".into());
        store
            .set(&s, "b", "keep", json!({"v": 1}), None, None, None)
            .await
            .unwrap();
        let writes = vec![
            WriteOp::Set {
                collection: "b".into(),
                doc_id: "new".into(),
                body: json!({"v": 2}),
                if_version: None,
                ttl_secs: None,
            },
            WriteOp::Update {
                collection: "b".into(),
                doc_id: "keep".into(),
                body: json!({"v": 3}),
                if_version: Some(7),
            },
        ];
        let err = store.batch(&s, &writes, None).await.unwrap_err();
        assert!(matches!(err, StoreError::VersionConflict { .. }));
        assert!(
            store.get(&s, "b", "new").await.unwrap().is_none(),
            "first write rolled back"
        );

        let writes = vec![
            WriteOp::Set {
                collection: "b".into(),
                doc_id: "new".into(),
                body: json!({"v": 2}),
                if_version: None,
                ttl_secs: None,
            },
            WriteOp::Update {
                collection: "b".into(),
                doc_id: "keep".into(),
                body: json!({"v": 3}),
                if_version: Some(1),
            },
            WriteOp::Delete {
                collection: "b".into(),
                doc_id: "absent".into(),
                if_version: None,
            },
        ];
        let out = store.batch(&s, &writes, None).await.unwrap();
        assert!(matches!(out[0], WriteOutcome::Written { version: 1 }));
        assert!(matches!(out[1], WriteOutcome::Written { version: 2 }));
        assert!(matches!(out[2], WriteOutcome::Deleted { existed: false }));

        let dup = vec![
            WriteOp::Delete {
                collection: "b".into(),
                doc_id: "new".into(),
                if_version: None,
            },
            WriteOp::Delete {
                collection: "b".into(),
                doc_id: "new".into(),
                if_version: None,
            },
        ];
        assert!(matches!(
            store.batch(&s, &dup, None).await.unwrap_err(),
            StoreError::Invalid(_)
        ));
    }

    pub async fn claims_exclude_other_owners_and_lapse(store: &dyn DocumentStore) {
        let s = Scope::Agent("claims".into());
        assert!(matches!(
            store.claim(&s, "c", "missing", "w1", 60).await.unwrap_err(),
            StoreError::NotFound
        ));
        store
            .set(&s, "c", "job", json!({"state": "queued"}), None, None, None)
            .await
            .unwrap();
        let held = store.claim(&s, "c", "job", "w1", 60).await.unwrap();
        assert_eq!(held.lease_owner.as_deref(), Some("w1"));
        let err = store.claim(&s, "c", "job", "w2", 60).await.unwrap_err();
        assert!(
            matches!(err, StoreError::Claimed { ref owner, .. } if owner == "w1"),
            "{err}"
        );
        // the holder may write and renew; another may not write
        store.claim(&s, "c", "job", "w1", 120).await.unwrap();
        store
            .update(
                &s,
                "c",
                "job",
                json!({"state": "running"}),
                None,
                Some("w1"),
            )
            .await
            .unwrap();
        let err = store
            .update(&s, "c", "job", json!({"state": "stolen"}), None, Some("w2"))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Claimed { .. }));
        // release, then the other may take it
        assert!(!store.release(&s, "c", "job", "w2").await.unwrap());
        assert!(store.release(&s, "c", "job", "w1").await.unwrap());
        store.claim(&s, "c", "job", "w2", 60).await.unwrap();
        assert!(matches!(
            store.claim(&s, "c", "job", "w1", 0).await.unwrap_err(),
            StoreError::Invalid(_)
        ));
    }

    pub async fn ttl_hides_then_purges(store: &dyn DocumentStore) {
        let s = Scope::Agent("ttl".into());
        store
            .set(&s, "t", "short", json!({"v": 1}), None, Some(1), None)
            .await
            .unwrap();
        store
            .set(&s, "t", "long", json!({"v": 1}), None, Some(3600), None)
            .await
            .unwrap();
        assert!(store.get(&s, "t", "short").await.unwrap().is_some());
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        assert!(
            store.get(&s, "t", "short").await.unwrap().is_none(),
            "expired reads as absent"
        );
        assert_eq!(
            store
                .list(&s, "t", &Query::default())
                .await
                .unwrap()
                .documents
                .len(),
            1
        );
        // an expired document may be re-created as new
        let doc = store
            .set(&s, "t", "short", json!({"v": 2}), Some(0), None, None)
            .await
            .unwrap();
        assert_eq!(doc.version, 1);
        assert!(store.purge_expired().await.unwrap() <= 1);
    }

    pub async fn bodies_are_checked(store: &dyn DocumentStore) {
        let s = Scope::Agent("checks".into());
        for (body, why) in [
            (json!("not an object"), "object"),
            (
                json!({"token": concat!("ghp_ab", "cdefghijklmnopqrstuvwxyz0123")}),
                "secret",
            ),
            (json!({"nested": {"key": "[[vault:global:x]]"}}), "secret"),
        ] {
            let err = store
                .set(&s, "k", "d", body, None, None, None)
                .await
                .unwrap_err();
            assert!(
                matches!(err, StoreError::Invalid(ref m) if m.contains(why)),
                "{err}"
            );
        }
        for bad in ["", "a/b", "..", "x y"] {
            let err = store
                .set(&s, bad, "d", json!({}), None, None, None)
                .await
                .unwrap_err();
            assert!(matches!(err, StoreError::Invalid(_)), "collection `{bad}`");
            let err = store
                .set(&s, "k", bad, json!({}), None, None, None)
                .await
                .unwrap_err();
            assert!(matches!(err, StoreError::Invalid(_)), "doc_id `{bad}`");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn memory_backend_meets_the_reference_semantics() {
        conformance::run_all(&MemoryDocumentStore::new()).await;
    }

    fn caller(identity: Option<&str>, agent: Option<&str>, admin: bool) -> Caller {
        Caller {
            identity: identity.map(str::to_string),
            agent_id: agent.map(str::to_string),
            admin,
            system: false,
        }
    }

    #[test]
    fn scopes_parse_against_the_caller() {
        let c = caller(Some("alice"), Some("atlas"), false);
        assert_eq!(
            Scope::parse("user", &c).unwrap(),
            Scope::User("alice".into())
        );
        assert_eq!(
            Scope::parse("user:bob", &c).unwrap(),
            Scope::User("bob".into())
        );
        assert_eq!(
            Scope::parse("agent", &c).unwrap(),
            Scope::Agent("atlas".into())
        );
        assert_eq!(
            Scope::parse("agent:other", &c).unwrap(),
            Scope::Agent("other".into())
        );
        assert_eq!(Scope::parse("global", &c).unwrap(), Scope::Global);
        assert!(matches!(
            Scope::parse("user", &caller(None, None, false)),
            Err(StoreError::NoIdentity)
        ));
        assert!(matches!(
            Scope::parse("team", &c),
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            Scope::parse("global:x", &c),
            Err(StoreError::Invalid(_))
        ));
        assert_eq!(Scope::Global.to_string(), "global");
        assert_eq!(Scope::Agent("a".into()).to_string(), "agent:a");
    }

    #[test]
    fn policy_mirrors_the_vault() {
        let member = caller(Some("alice"), Some("atlas"), false);
        let admin = caller(Some("root"), Some("atlas"), true);
        let system = Caller {
            system: true,
            ..Default::default()
        };
        let mine = Scope::User("alice".into());
        let theirs = Scope::User("bob".into());
        let own_agent = Scope::Agent("atlas".into());
        let other_agent = Scope::Agent("edge".into());

        assert!(member.authorise(&mine, Access::Write).is_ok());
        assert!(
            member.authorise(&theirs, Access::Read).is_err(),
            "no cross-user reads"
        );
        assert!(
            admin.authorise(&theirs, Access::Read).is_err(),
            "not even for admins"
        );
        assert!(
            system.authorise(&theirs, Access::Read).is_ok(),
            "system runs may"
        );
        assert!(member.authorise(&own_agent, Access::Write).is_ok());
        assert!(member.authorise(&other_agent, Access::Read).is_err());
        assert!(admin.authorise(&other_agent, Access::Write).is_ok());
        assert!(member.authorise(&Scope::Global, Access::Read).is_ok());
        assert!(member.authorise(&Scope::Global, Access::Write).is_err());
        assert!(admin.authorise(&Scope::Global, Access::Write).is_ok());
        assert!(system.authorise(&Scope::Global, Access::Write).is_ok());
    }

    #[test]
    fn queries_parse_triples_and_objects_and_reject_nonsense() {
        let q = Query::from_json(&json!({
            "where": [["stage", "==", "review"], {"field": "n", "op": ">=", "value": 2}],
            "order_by": {"field": "n", "direction": "desc"},
            "limit": 5,
            "cursor": "o10"
        }))
        .unwrap();
        assert_eq!(q.filters.len(), 2);
        assert_eq!(q.filters[0].op, Op::Eq);
        assert_eq!(q.filters[1].op, Op::Gte);
        assert_eq!(q.order_by.as_deref(), Some("n"));
        assert_eq!(q.direction, Direction::Desc);
        assert_eq!(q.page_size(), 5);
        assert_eq!(
            q.resume(),
            Some(Resume::Offset(10)),
            "pre-keyset cursors still page"
        );
        assert!(Query::from_json(&json!({"where": [["a", "like", 1]]})).is_err());
        assert!(Query::from_json(&json!({"where": [["a", "in", 1]]})).is_err());
        assert!(Query::from_json(&json!({"where": [["a b", "eq", 1]]})).is_err());
        assert!(Query::from_json(&json!({"order_by": "x;drop"})).is_err());
        assert!(Query::from_json(&json!({"direction": "sideways"})).is_err());
        assert_eq!(
            Query {
                limit: Some(0),
                ..Default::default()
            }
            .page_size(),
            1
        );
        assert_eq!(
            Query {
                limit: Some(10_000),
                ..Default::default()
            }
            .page_size(),
            MAX_PAGE
        );
        assert_eq!(
            Query {
                cursor: Some("junk".into()),
                ..Default::default()
            }
            .resume(),
            None,
            "an unreadable cursor starts over rather than failing the read"
        );

        let anchor = Anchor {
            key: json!(7),
            doc_id: "d".into(),
            order_by: Some("n".into()),
            direction: Direction::Desc,
        };
        let cursor = encode_cursor(&anchor);
        assert_eq!(
            Query::from_json(&json!({
                "order_by": {"field": "n", "direction": "desc"},
                "cursor": cursor,
            }))
            .unwrap()
            .resume(),
            Some(Resume::After(anchor)),
            "a cursor round-trips under the ordering it was issued for"
        );
        for mismatch in [
            json!({"cursor": cursor}),
            json!({"order_by": "other", "direction": "desc", "cursor": cursor}),
            json!({"order_by": {"field": "n", "direction": "asc"}, "cursor": cursor}),
        ] {
            assert!(
                Query::from_json(&mismatch).is_err(),
                "a cursor from another ordering must be refused, not silently resumed: {mismatch}"
            );
        }
    }

    #[test]
    fn merge_is_shallow_and_null_deletes() {
        let base = json!({"a": 1, "b": {"x": 1}, "c": 3});
        let out = merge_shallow(&base, &json!({"b": {"y": 2}, "c": null, "d": 4}));
        assert_eq!(out, json!({"a": 1, "b": {"y": 2}, "d": 4}));
    }

    #[test]
    fn secret_detection_names_the_path() {
        let body = json!({"ok": "sk-", "deep": [{"k": "AKIAABCDEFGHIJKLMNOPQR"}]});
        assert_eq!(find_secret_like(&body, "").as_deref(), Some("deep[0].k"));
        assert!(find_secret_like(&json!({"s": "short sk- text"}), "").is_none());
        assert!(looks_like_secret("-----BEGIN RSA PRIVATE KEY-----\nabc"));
    }
}
