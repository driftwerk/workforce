//! Generic auto-spill of large tool outputs (data-plane P4).
//!
//! Hooked at the runner's two tool-result join sites, where the in-flight
//! string (`output_str`) and the invocation record value (`output`) coexist —
//! replacing both there makes the digest envelope canonical by construction
//! across context, session persistence, replay, and traces. Array-of-objects
//! results stage as a queryable table ([`crate::artifacts::tabular`]); any
//! other oversized shape stages as an opaque blob ([`crate::artifacts::blob`])
//! recoverable via `tool.retrieve`. Either way no single result floods context.

use serde_json::Value;
use tracing::warn;
use workforce_common::session::SpillConfig;

use crate::artifacts::blob::stage_blob;
use crate::artifacts::store::{global_store, ArtifactStore, DEFAULT_INLINE_THRESHOLD};
use crate::artifacts::tabular::{stage_table, wrap_envelope_preview, StagedTable};
use crate::artifacts::{CLIPPED_ARTIFACT_KIND, TABULAR_ARTIFACT_KIND};
use crate::data::estimate_json_bytes;
use crate::runner::CURRENT_AGENT_ID;
use crate::tools::image_gen::GENERATED_IMAGES_KEY;

const VIEWED_IMAGES_KEY: &str = "viewed_images";

/// Where the row table sits inside a tool output.
#[derive(Debug)]
enum RowsAt {
    Whole,
    /// Object path to the rows array, outermost key first. A GraphQL result
    /// nests its rows under the operation (`{shiftsList: {list: [...]}}`), so a
    /// single key is not enough to reach them.
    Path(Vec<String>),
}

/// How deep to hunt for the rows array.
///
/// Depth is not what makes this safe — the majority-bytes rule is. Only a
/// branch holding more than half the payload is descended, and only an array
/// holding more than half is accepted, so **at most one array can ever qualify
/// at a given level** (two arrays cannot each be more than half). An incidental
/// inner array is excluded by size, at any depth.
///
/// So this is a cheap bound on work, not a correctness guard, and it is set
/// well past the shapes seen in practice: a GraphQL envelope reaches its rows in
/// two or three hops (`data` → operation → collection), a federated or
/// otherwise re-wrapped response in a few more. Past this an output simply
/// stages as a blob, which is honest rather than wrong — and now says so
/// clearly when queried.
const MAX_ROWS_DEPTH: usize = 8;

/// Name of the blob-retrieval tool. Declared here, beside the code that decides
/// whether staging a blob is worthwhile, so a rename cannot silently switch blob
/// spilling off across the hub — the compiler follows it.
pub const RETRIEVE_TOOL_NAME: &str = "tool.retrieve";

/// What the caller can actually DO with a staged artifact on this surface.
///
/// Derived from the tools registered for the run, never assumed. `data.query`
/// and `data.inspect` read tabular artifacts; `tool.retrieve` reads blobs. The
/// customer-facing edge registers the first pair and not the second, so a blob
/// staged there is unreachable by anything — the bytes sit in the store and no
/// available tool can name them. Staging a kind the surface cannot read is data
/// loss with extra steps, so the decision has to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetrievalSurface {
    /// `data.query` / `data.inspect` are registered.
    pub tables: bool,
    /// `tool.retrieve` is registered.
    pub blobs: bool,
}

impl RetrievalSurface {
    /// Every retrieval tool available — the hub's shape.
    pub const FULL: Self = Self {
        tables: true,
        blobs: true,
    };

    /// No retrieval at all: nothing may be staged, everything stays inline.
    pub const NONE: Self = Self {
        tables: false,
        blobs: false,
    };

    pub fn from_registry(tools: &crate::tools::registry::ToolRegistry) -> Self {
        Self {
            tables: tools
                .get(crate::tools::data::DATA_QUERY_TOOL_NAME)
                .is_some()
                || tools
                    .get(crate::tools::data::DATA_INSPECT_TOOL_NAME)
                    .is_some(),
            blobs: tools.get(RETRIEVE_TOOL_NAME).is_some(),
        }
    }
}

/// Why an output was left inline. Logged for operators; never shown to a model.
#[derive(Debug, Clone, Copy)]
enum InlineReason {
    ContractUnmet,
    NoRetrieval,
}

impl InlineReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::ContractUnmet => "no staged form was both readable here and smaller",
            Self::NoRetrieval => "surface registers no retrieval tool",
        }
    }
}

/// Replace a large tool output with a staged handle, when — and only when —
/// that handle would be both smaller than what it replaces and readable by a
/// tool this surface registers. Returns the (possibly replaced)
/// `(output_str, output)` pair; on any miss — too small, error, content tool,
/// already staged, no store/agent, or the contract unmet — both values pass
/// through unchanged, so data is never lost.
pub fn spill_tool_output(
    tool_name: &str,
    output_str: String,
    output: Value,
    is_error: bool,
    config: &SpillConfig,
    surface: RetrievalSurface,
) -> (String, Value) {
    if !config.enabled || is_error || output_str.len() < config.threshold_bytes {
        return (output_str, output);
    }
    if is_content_tool(tool_name, &config.skip_tools) || manages_own_staging(&output) {
        return (output_str, output);
    }
    let Some(store) = global_store() else {
        return (output_str, output);
    };
    let Some(agent_id) = CURRENT_AGENT_ID
        .try_with(|a| a.clone())
        .ok()
        .filter(|a| !a.is_empty())
    else {
        return (output_str, output);
    };
    // Unwrap an MCP transport envelope so staging sees the real tool payload
    // rather than the `{content:[{type,text}]}` wrapper (which would otherwise
    // stage as a bogus one-row `{type,text}` table). Only spilling results are
    // touched, so sub-threshold outputs keep a byte-identical shape.
    let (output_str, output) = match normalize_mcp_envelope(&output) {
        Some(inner) => (inner.to_string(), inner),
        None => (output_str, output),
    };
    // THE CONTRACT: a spill replaces a large output with a handle that is both
    // SMALLER than what it replaced and READABLE by a tool this surface
    // registers. Candidates are built in preference order and each is judged
    // against that rule in one place; if none satisfies it the output stays
    // inline, because data the model can still read beats a handle it cannot.
    let original_len = output_str.len();

    if surface.tables {
        if let Some(rows_at) = locate_rows(&output) {
            let candidate = stage_tabular_candidate(
                store.as_ref(),
                &agent_id,
                tool_name,
                output.clone(),
                rows_at,
            );
            if let Some((serialised, staged)) = accept_if_smaller(candidate, original_len) {
                record_spill(tool_name, original_len);
                tracing::debug!(
                    tool = tool_name,
                    before = original_len,
                    after = serialised.len(),
                    "spilled tool output as a queryable table"
                );
                return (serialised, staged);
            }
        }
    }

    if surface.blobs {
        let candidate = stage_blob_candidate(store.as_ref(), &agent_id, tool_name, output.clone());
        if let Some((serialised, staged)) = accept_if_smaller(candidate, original_len) {
            record_spill(tool_name, original_len);
            tracing::debug!(
                tool = tool_name,
                before = original_len,
                after = serialised.len(),
                "spilled tool output as an opaque blob"
            );
            return (serialised, staged);
        }
    }

    let reason = if surface.tables || surface.blobs {
        InlineReason::ContractUnmet
    } else {
        InlineReason::NoRetrieval
    };
    // Counted, not just logged: leaving a large output inline is the one
    // outcome that costs context instead of saving it, and it is invisible at
    // production log levels. Without this the change's main risk — a surface
    // that quietly stops spilling — could only be seen in a token bill.
    metrics::counter!(
        "workforce.tool_output.inline",
        "tool" => tool_name.to_string(),
        "reason" => reason.as_str(),
    )
    .increment(1);
    tracing::debug!(
        tool = tool_name,
        bytes = original_len,
        reason = reason.as_str(),
        "tool output left inline rather than staged"
    );
    (output_str, output)
}

/// The contract's size clause, applied in exactly one place. A candidate that
/// did not shrink is rejected; the artifact it already wrote expires on its own
/// TTL rather than complicating the accepted path with a rollback the store
/// trait does not offer.
fn accept_if_smaller(
    candidate: Option<(String, Value)>,
    original_len: usize,
) -> Option<(String, Value)> {
    candidate.filter(|(serialised, _)| serialised.len() < original_len)
}

/// Mutable reference to the value at `path`, for swapping rows out for the
/// staged envelope in place.
fn path_slot<'v>(root: &'v mut Value, path: &[String]) -> Option<&'v mut Value> {
    path.iter()
        .try_fold(root, |cur, key| cur.get_mut(key.as_str()))
}

fn record_spill(tool_name: &str, bytes: usize) {
    metrics::counter!("workforce.tool_output.spilled", "tool" => tool_name.to_string())
        .increment(1);
    metrics::histogram!("workforce.tool_output.spilled_bytes", "tool" => tool_name.to_string())
        .record(bytes as f64);
}

/// Build the tabular candidate: rows staged as a queryable table, with the
/// envelope substituted where the rows were. `None` when nothing was staged
/// (rows small enough to keep inline, or a store failure). It does NOT judge
/// whether the result is acceptable — `spill_tool_output` owns the contract.
fn stage_tabular_candidate(
    store: &dyn ArtifactStore,
    agent_id: &str,
    tool_name: &str,
    output: Value,
    rows_at: RowsAt,
) -> Option<(String, Value)> {
    let rows = match &rows_at {
        RowsAt::Whole => output.as_array().cloned().unwrap_or_default(),
        RowsAt::Path(path) => value_at(&output, path)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    };
    match stage_table(
        store,
        agent_id,
        tool_name,
        None,
        rows,
        DEFAULT_INLINE_THRESHOLD,
    ) {
        Ok(StagedTable::Staged { mut envelope, .. }) => {
            wrap_envelope_preview(&mut envelope, tool_name);
            let new_output = match rows_at {
                RowsAt::Whole => envelope,
                RowsAt::Path(path) => {
                    let mut out = output;
                    // Unreachable: the path was just walked to collect `rows`.
                    *path_slot(&mut out, &path)? = envelope;
                    out
                }
            };
            Some((new_output.to_string(), new_output))
        }
        Ok(StagedTable::Inline { .. }) => None,
        Err(e) => {
            warn!(tool = tool_name, error = %e, "tabular staging failed; leaving inline");
            None
        }
    }
}

/// Build the blob candidate: the whole output staged opaquely behind a capped
/// preview. Readable only where `tool.retrieve` is registered.
fn stage_blob_candidate(
    store: &dyn ArtifactStore,
    agent_id: &str,
    tool_name: &str,
    output: Value,
) -> Option<(String, Value)> {
    match stage_blob(store, agent_id, tool_name, output) {
        Ok(envelope) => Some((envelope.to_string(), envelope)),
        Err(e) => {
            warn!(tool = tool_name, error = %e, "blob staging failed; leaving inline");
            None
        }
    }
}

/// Unwrap an MCP `CallToolResult` transport envelope to the actual tool data.
///
/// MCP results arrive as `{content:[{type,text,…}], structuredContent?, isError?}`;
/// staging the envelope directly would trap the payload inside a one-row
/// `{type,text}` table. Returns `None` for anything that is not an MCP envelope,
/// so native tool outputs pass through untouched. `structuredContent` (the
/// MCP-blessed structured result) wins when present; otherwise the text blocks
/// decode — a lone JSON text block becomes its parsed value (surfacing the rows),
/// while prose or multiple/non-text blocks stay a string so they bound as a blob
/// rather than a bogus table.
fn normalize_mcp_envelope(output: &Value) -> Option<Value> {
    let obj = output.as_object()?;
    let content = obj.get("content").and_then(Value::as_array);
    // Require the MCP result signature — a typed `content` array or a
    // `structuredContent` — so a native tool that merely carries a `content`
    // field is never mistaken for an envelope.
    let is_envelope = obj.contains_key("structuredContent")
        || content.is_some_and(|blocks| {
            !blocks.is_empty()
                && blocks
                    .iter()
                    .all(|b| b.get("type").and_then(Value::as_str).is_some())
        });
    if !is_envelope {
        return None;
    }
    if let Some(structured) = obj.get("structuredContent").filter(|v| !v.is_null()) {
        return Some(structured.clone());
    }
    let texts: Vec<&str> = content?
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect();
    match texts.as_slice() {
        [only] => Some(
            serde_json::from_str::<Value>(only)
                .unwrap_or_else(|_| Value::String((*only).to_string())),
        ),
        [] => None,
        many => Some(Value::String(many.join("\n"))),
    }
}

/// Content tools' outputs are answers the model must read, not datasets to
/// transform — `data.query` is the wrong retrieval primitive for them.
fn is_content_tool(tool_name: &str, skip: &[String]) -> bool {
    skip.iter().any(|prefix| {
        tool_name == prefix
            || tool_name
                .strip_prefix(prefix.as_str())
                .is_some_and(|rest| rest.starts_with('.') || rest.starts_with('_'))
    })
}

fn manages_own_staging(output: &Value) -> bool {
    let Some(obj) = output.as_object() else {
        return false;
    };
    obj.contains_key(GENERATED_IMAGES_KEY)
        || obj.contains_key(VIEWED_IMAGES_KEY)
        || matches!(
            obj.get("kind").and_then(Value::as_str),
            Some(TABULAR_ARTIFACT_KIND | CLIPPED_ARTIFACT_KIND)
        )
}

fn is_object_rows(value: &Value) -> bool {
    value
        .as_array()
        .is_some_and(|rows| !rows.is_empty() && rows.iter().all(Value::is_object))
}

/// A spillable table is either the whole output, or the array-of-objects
/// nested somewhere inside it that carries the majority of the bytes.
///
/// The majority rule is what keeps this honest: an incidental inner array
/// (a handful of tags on each record) never dominates, so it is never mistaken
/// for the content. Searching deeper than one level is what makes a GraphQL
/// result usable at all — its rows live under the operation name, two levels
/// down, and stopping at the top level staged the whole envelope as an opaque
/// blob that `data.query` then refused to read.
fn locate_rows(output: &Value) -> Option<RowsAt> {
    if is_object_rows(output) {
        return Some(RowsAt::Whole);
    }
    let total = estimate_json_bytes(output).max(1);
    let mut path = Vec::new();
    find_rows_path(output, total, MAX_ROWS_DEPTH, &mut path).then_some(RowsAt::Path(path))
}

/// Depth-first hunt for the majority array-of-objects, recording its path.
/// Returns whether one was found; `path` holds it when so.
fn find_rows_path(value: &Value, total: usize, depth: usize, path: &mut Vec<String>) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    if depth == 0 {
        return false;
    }
    // Prefer a rows array at this level before descending, so the outermost
    // qualifying array wins and the path stays as short as possible.
    for (key, v) in obj {
        if is_object_rows(v) && estimate_json_bytes(v) * 2 >= total {
            path.push(key.clone());
            return true;
        }
    }
    for (key, v) in obj {
        if !v.is_object() {
            continue;
        }
        // Only descend through a branch big enough to still hold the majority.
        if estimate_json_bytes(v) * 2 < total {
            continue;
        }
        path.push(key.clone());
        if find_rows_path(v, total, depth - 1, path) {
            return true;
        }
        path.pop();
    }
    false
}

/// Follow an object path, returning the value at its end.
fn value_at<'v>(root: &'v Value, path: &[String]) -> Option<&'v Value> {
    path.iter().try_fold(root, |cur, key| cur.get(key.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::store::install_test_global_store;
    use crate::artifacts::types::{ArtifactBody, ArtifactId};
    use crate::runner::{CURRENT_SESSION_ID, CURRENT_USER_ID};
    use serde_json::json;

    pub(super) fn big_rows(n: usize) -> Vec<Value> {
        (0..n)
            .map(|i| json!({"id": i, "name": format!("row-{i}-padding-padding-padding")}))
            .collect()
    }

    pub(super) fn spill_scoped(
        agent: &str,
        output: Value,
        is_error: bool,
        config: &SpillConfig,
    ) -> (String, Value) {
        spill_scoped_on(RetrievalSurface::FULL, agent, output, is_error, config)
    }

    /// Same, on a named retrieval surface — what the contract table varies.
    pub(super) fn spill_scoped_on(
        surface: RetrievalSurface,
        agent: &str,
        output: Value,
        is_error: bool,
        config: &SpillConfig,
    ) -> (String, Value) {
        let output_str = output.to_string();
        CURRENT_AGENT_ID.sync_scope(agent.to_string(), || {
            CURRENT_SESSION_ID.sync_scope("spill-session".to_string(), || {
                spill_tool_output(
                    "mcp.fetch_rows",
                    output_str,
                    output,
                    is_error,
                    config,
                    surface,
                )
            })
        })
    }

    #[test]
    fn under_threshold_disabled_and_errors_pass_through() {
        install_test_global_store();
        let small = json!([{"a": 1}]);
        let cfg = SpillConfig::default();
        let (s, v) = spill_scoped("spill-a", small.clone(), false, &cfg);
        assert_eq!(v, small);
        assert_eq!(s, small.to_string());

        let big = Value::Array(big_rows(2000));
        let disabled = SpillConfig {
            enabled: false,
            ..SpillConfig::default()
        };
        let (_, v) = spill_scoped("spill-a", big.clone(), false, &disabled);
        assert_eq!(v, big);

        let (_, v) = spill_scoped("spill-a", big.clone(), true, &SpillConfig::default());
        assert_eq!(v, big, "errors never spill");
    }

    #[test]
    fn whole_array_output_spills_both_representations() {
        let store = install_test_global_store();
        let agent = "spill-whole";
        let rows = Value::Array(big_rows(2000));
        let (s, v) = spill_scoped(agent, rows, false, &SpillConfig::default());

        assert_eq!(v["kind"], TABULAR_ARTIFACT_KIND);
        assert_eq!(v["rows"], 2000);
        assert!(v["preview"].is_string(), "preview wrapped");
        assert_eq!(
            s,
            v.to_string(),
            "in-flight string and invocation value must be the same digest"
        );
        assert!(
            s.find("artifact_id").unwrap() < 20,
            "artifact_id serialises in the first bytes (prune-clip safety)"
        );

        let id = ArtifactId::parse(v["artifact_id"].as_str().unwrap()).unwrap();
        let descriptor = store.metadata(agent, &id).unwrap().unwrap();
        assert_eq!(descriptor.session_id.as_deref(), Some("spill-session"));
        assert_eq!(descriptor.producer, "mcp.fetch_rows");
        let ArtifactBody::Json(Value::Array(stored)) = store.read(agent, &id).unwrap() else {
            panic!("expected rows");
        };
        assert_eq!(stored.len(), 2000);
    }

    #[test]
    fn dominant_field_spills_in_place_preserving_siblings() {
        let store = install_test_global_store();
        let agent = "spill-field";
        let output = json!({
            "count": 2000,
            "cursor": "next-page-token",
            "logs": big_rows(2000),
        });
        let (s, v) = spill_scoped(agent, output, false, &SpillConfig::default());
        assert_eq!(v["cursor"], "next-page-token", "siblings survive");
        assert_eq!(v["count"], 2000);
        assert_eq!(v["logs"]["kind"], TABULAR_ARTIFACT_KIND);
        assert_eq!(s, v.to_string());
        let id = ArtifactId::parse(v["logs"]["artifact_id"].as_str().unwrap()).unwrap();
        assert!(store.read(agent, &id).is_ok());
    }

    #[test]
    fn ambiguous_and_non_tabular_outputs_stage_as_blob() {
        let store = install_test_global_store();
        let cfg = SpillConfig::default();

        // Two co-dominant arrays: no majority field, so the whole object bounds
        // as one blob rather than flooding context.
        let two_arrays = json!({ "a": big_rows(1000), "b": big_rows(1000) });
        let (s, v) = spill_scoped("spill-blob-a", two_arrays, false, &cfg);
        assert_eq!(v["kind"], CLIPPED_ARTIFACT_KIND);
        assert_eq!(s, v.to_string());
        let id = ArtifactId::parse(v["artifact_id"].as_str().unwrap()).unwrap();
        assert!(store.read("spill-blob-a", &id).is_ok());

        // A large scalar array is not a table; it still bounds as a blob.
        let scalars = json!((0..20_000).map(Value::from).collect::<Vec<_>>());
        let (_, v) = spill_scoped("spill-blob-b", scalars, false, &cfg);
        assert_eq!(v["kind"], CLIPPED_ARTIFACT_KIND);

        // A big string stays off the tabular path but is bounded all the same.
        let text = Value::String("x".repeat(40_000));
        let (_, v) = spill_scoped("spill-blob-c", text, false, &cfg);
        assert_eq!(v["kind"], CLIPPED_ARTIFACT_KIND);
    }

    #[test]
    fn nested_rows_stage_as_a_queryable_table_in_place() {
        let store = install_test_global_store();
        // A nested aggregation shape: the rows live two levels deep. These USED
        // to bound as an opaque blob, which `data.query` then refused to read —
        // the model spilled a result it could no longer query. They are now
        // staged as a table, with the surrounding envelope left intact.
        let output = json!({ "data": { "buckets": big_rows(2000) }, "meta": { "status": "ok" } });
        let (s, v) = spill_scoped("spill-agg", output.clone(), false, &SpillConfig::default());
        assert_eq!(s, v.to_string());
        assert_eq!(v["meta"]["status"], "ok", "siblings survive");
        let envelope = &v["data"]["buckets"];
        let id = ArtifactId::parse(envelope["artifact_id"].as_str().unwrap()).unwrap();
        let ArtifactBody::Json(stored) = store.read("spill-agg", &id).unwrap() else {
            panic!("expected staged json");
        };
        assert_eq!(
            stored,
            json!(big_rows(2000)),
            "the ROWS are stored as a top-level array, which is what data.query reads"
        );
    }

    #[test]
    fn big_string_output_stages_as_blob_preserving_text() {
        let store = install_test_global_store();
        let text = format!("error trace\n{}", "frame line\n".repeat(8000));
        let (_, v) = spill_scoped(
            "spill-text",
            Value::String(text.clone()),
            false,
            &SpillConfig::default(),
        );
        assert_eq!(v["kind"], CLIPPED_ARTIFACT_KIND);
        assert_eq!(v["shape"], "string");
        let id = ArtifactId::parse(v["artifact_id"].as_str().unwrap()).unwrap();
        let ArtifactBody::Json(Value::String(stored)) = store.read("spill-text", &id).unwrap()
        else {
            panic!("expected json string blob");
        };
        assert_eq!(stored, text);
    }

    #[test]
    fn content_tools_never_spill() {
        install_test_global_store();
        let cfg = SpillConfig::default();
        let big = json!({ "results": big_rows(2000), "count": 2000 });
        for tool in [
            "memory.recall",
            "knowledge_ask",
            "sessions",
            "session.search",
        ] {
            let output_str = big.to_string();
            let (_, v) = CURRENT_AGENT_ID.sync_scope("spill-content".to_string(), || {
                spill_tool_output(
                    tool,
                    output_str.clone(),
                    big.clone(),
                    false,
                    &cfg,
                    RetrievalSurface::FULL,
                )
            });
            assert_eq!(v, big, "{tool} output is content, must pass through");
        }
        assert!(!is_content_tool("sessionx.search", &cfg.skip_tools));
        assert!(!is_content_tool("metabase.card_query", &cfg.skip_tools));
    }

    #[test]
    fn tool_retrieve_output_never_spills() {
        install_test_global_store();
        // The recovery primitive's own result is a bounded answer; re-staging it
        // would hand the agent a fresh handle instead of the content it asked for.
        let cfg = SpillConfig::default();
        assert!(is_content_tool("tool.retrieve", &cfg.skip_tools));
        let big = json!({ "tool": "datadog.api", "content": "x".repeat(40_000) });
        let (_, v) = CURRENT_AGENT_ID.sync_scope("spill-retrieve".to_string(), || {
            spill_tool_output(
                "tool.retrieve",
                big.to_string(),
                big.clone(),
                false,
                &cfg,
                RetrievalSurface::FULL,
            )
        });
        assert_eq!(v, big, "tool.retrieve output must pass through unchanged");
    }

    /// Data adapters are NOT content tools: a large bulk list/search result
    /// (e.g. `linear.issue_list` returning {count, issues:[...]}) must stage
    /// its dominant array to an artifact so the agent inspects it via
    /// data.query instead of flooding context and forcing compaction.
    #[test]
    fn data_adapter_lists_spill() {
        let store = install_test_global_store();
        let cfg = SpillConfig::default();
        for tool in ["linear.issue_list", "jira.search", "github.list_issues"] {
            assert!(
                !is_content_tool(tool, &cfg.skip_tools),
                "{tool} is bulk data and must be spillable"
            );
        }
        let output = json!({ "count": 2000, "issues": big_rows(2000) });
        let (s, v) = CURRENT_AGENT_ID.sync_scope("spill-linear".to_string(), || {
            CURRENT_SESSION_ID.sync_scope("spill-session".to_string(), || {
                spill_tool_output(
                    "linear.issue_list",
                    output.to_string(),
                    output.clone(),
                    false,
                    &cfg,
                    RetrievalSurface::FULL,
                )
            })
        });
        assert_eq!(v["count"], 2000, "scalar sibling survives");
        assert_eq!(
            v["issues"]["kind"], TABULAR_ARTIFACT_KIND,
            "issues array staged to an artifact"
        );
        let id = ArtifactId::parse(v["issues"]["artifact_id"].as_str().unwrap()).unwrap();
        assert!(store.read("spill-linear", &id).is_ok());
        assert_eq!(
            s,
            v.to_string(),
            "in-flight string matches invocation value"
        );
    }

    #[test]
    fn already_enveloped_outputs_are_skipped() {
        install_test_global_store();
        let cfg = SpillConfig {
            threshold_bytes: 64,
            ..SpillConfig::default()
        };
        for output in [
            json!({ "kind": TABULAR_ARTIFACT_KIND, "artifact_id": "art_x", "preview": big_rows(50) }),
            json!({ "kind": CLIPPED_ARTIFACT_KIND, "artifact_id": "art_x", "preview": "x".repeat(40_000) }),
            json!({ GENERATED_IMAGES_KEY: [{"artifact_id": "art_x"}], "rows": big_rows(50) }),
        ] {
            let (_, v) = spill_scoped("spill-skip", output.clone(), false, &cfg);
            assert_eq!(v, output);
        }
    }

    #[test]
    fn big_output_with_minority_rows_stages_as_blob() {
        let store = install_test_global_store();
        let output = json!({
            "rows": big_rows(3),
            "narrative": "x".repeat(40_000),
        });
        let (_, v) = spill_scoped("spill-blob-min", output, false, &SpillConfig::default());
        assert_eq!(
            v["kind"], CLIPPED_ARTIFACT_KIND,
            "rows are a byte-minority, so the whole output bounds as a blob"
        );
        let id = ArtifactId::parse(v["artifact_id"].as_str().unwrap()).unwrap();
        assert!(store.read("spill-blob-min", &id).is_ok());
    }

    #[test]
    fn missing_agent_scope_passes_through() {
        install_test_global_store();
        let rows = Value::Array(big_rows(2000));
        let (_, v) = spill_tool_output(
            "t",
            rows.to_string(),
            rows.clone(),
            false,
            &SpillConfig::default(),
            RetrievalSurface::FULL,
        );
        assert_eq!(v, rows);
    }

    fn mcp_text_envelope(text: &str) -> Value {
        json!({ "content": [{ "type": "text", "text": text }] })
    }

    #[test]
    fn normalize_mcp_envelope_decodes_the_shapes() {
        // A lone JSON text block surfaces as its parsed value.
        assert_eq!(
            normalize_mcp_envelope(&mcp_text_envelope("{\"a\":1}")),
            Some(json!({ "a": 1 }))
        );
        // Prose stays a string, so it later bounds as a blob, not a table.
        assert_eq!(
            normalize_mcp_envelope(&mcp_text_envelope("hello world")),
            Some(json!("hello world"))
        );
        // structuredContent (the MCP-blessed structured result) wins.
        let structured = json!({
            "content": [{ "type": "text", "text": "ignored summary" }],
            "structuredContent": [{ "x": 1 }],
        });
        assert_eq!(
            normalize_mcp_envelope(&structured),
            Some(json!([{ "x": 1 }]))
        );
        // Multiple text blocks concatenate as prose.
        let multi = json!({ "content": [
            { "type": "text", "text": "a" },
            { "type": "text", "text": "b" },
        ]});
        assert_eq!(normalize_mcp_envelope(&multi), Some(json!("a\nb")));
        // Native (non-MCP) outputs pass through untouched.
        assert_eq!(normalize_mcp_envelope(&json!([{ "a": 1 }])), None);
        assert_eq!(
            normalize_mcp_envelope(&json!({ "content": "just a string" })),
            None
        );
        assert_eq!(
            normalize_mcp_envelope(&json!({ "content": [{ "note": "no type field" }] })),
            None
        );
        // A content array with no text blocks (image/resource) is left as-is.
        assert_eq!(
            normalize_mcp_envelope(&json!({ "content": [{ "type": "image", "data": "x" }] })),
            None
        );
    }

    #[test]
    fn mcp_json_text_envelope_spills_the_real_rows_as_a_table() {
        let store = install_test_global_store();
        let agent = "mcp-rows";
        // The exact shape mcp-service emits: reports-service JSON serialised into
        // a single text content block, wrapped `{row_count, truncated, rows}`.
        let inner = json!({ "row_count": 2000, "truncated": false, "rows": big_rows(2000) });
        let envelope = mcp_text_envelope(&inner.to_string());
        let (s, v) = spill_scoped(agent, envelope, false, &SpillConfig::default());

        // Scalar siblings survive; the dominant `rows` array becomes a table.
        assert_eq!(v["row_count"], 2000);
        assert_eq!(v["truncated"], false);
        assert_eq!(v["rows"]["kind"], TABULAR_ARTIFACT_KIND);
        assert_eq!(v["rows"]["rows"], 2000);
        assert_eq!(
            s,
            v.to_string(),
            "in-flight string matches invocation value"
        );

        // The staged artifact holds the REAL row objects, not `{type,text}`.
        let id = ArtifactId::parse(v["rows"]["artifact_id"].as_str().unwrap()).unwrap();
        let ArtifactBody::Json(Value::Array(stored)) = store.read(agent, &id).unwrap() else {
            panic!("expected the real rows");
        };
        assert_eq!(stored.len(), 2000);
        assert_eq!(stored[0]["id"], 0);
        assert!(stored[0].get("name").is_some());
    }

    #[test]
    fn mcp_prose_text_envelope_bounds_as_blob_not_a_bogus_table() {
        let store = install_test_global_store();
        // A large non-JSON MCP result must bound as a blob — historically it
        // staged as a one-row `{type,text}` table, which this asserts against.
        let envelope = mcp_text_envelope(&"error trace line\n".repeat(4000));
        let (_, v) = spill_scoped("mcp-prose", envelope, false, &SpillConfig::default());
        assert_eq!(v["kind"], CLIPPED_ARTIFACT_KIND);
        assert_eq!(v["shape"], "string");
        let id = ArtifactId::parse(v["artifact_id"].as_str().unwrap()).unwrap();
        assert!(store.read("mcp-prose", &id).is_ok());
    }

    #[test]
    fn mcp_structured_content_spills_as_a_table() {
        let store = install_test_global_store();
        let agent = "mcp-structured";
        let envelope = json!({
            "content": [{ "type": "text", "text": "2000 rows returned" }],
            "structuredContent": big_rows(2000),
        });
        let (_, v) = spill_scoped(agent, envelope, false, &SpillConfig::default());
        assert_eq!(v["kind"], TABULAR_ARTIFACT_KIND);
        assert_eq!(v["rows"], 2000);
        let id = ArtifactId::parse(v["artifact_id"].as_str().unwrap()).unwrap();
        let ArtifactBody::Json(Value::Array(stored)) = store.read(agent, &id).unwrap() else {
            panic!("expected the real rows");
        };
        assert_eq!(stored.len(), 2000);
    }

    /// End-to-end seam: an MCP `run_report_query` result spills, and
    /// `export_xlsx` reads the staged `artifact_id` back into a real table —
    /// under the same agent/session/user identity the edge host establishes.
    /// Guards the showstopper (real rows, not a `{type,text}` wrapper) and the
    /// scoping consistency (agent-partitioned store + session/user match).
    #[test]
    fn mcp_report_result_spills_then_export_xlsx_reads_the_real_table() {
        use crate::tools::export_xlsx::build_xlsx_from_input;
        let store = install_test_global_store();
        let agent = "fabrikam-assistant";
        let (bytes, filename, total_rows, sheet_count) =
            CURRENT_AGENT_ID.sync_scope(agent.to_string(), || {
                CURRENT_SESSION_ID.sync_scope("sess-1".to_string(), || {
                    CURRENT_USER_ID.sync_scope("ext:issuer\u{1f}sub".to_string(), || {
                        let inner =
                            json!({ "row_count": 2000, "truncated": false, "rows": big_rows(2000) });
                        let envelope = mcp_text_envelope(&inner.to_string());
                        let (_, v) = spill_tool_output(
                            "run_report_query",
                            envelope.to_string(),
                            envelope,
                            false,
                            &SpillConfig::default(),
                            RetrievalSurface::FULL,
                        );
                        let artifact_id = v["rows"]["artifact_id"].as_str().unwrap().to_string();
                        let input = json!({ "artifact_id": artifact_id, "filename": "report.xlsx" });
                        build_xlsx_from_input(&input, store.as_ref(), agent).unwrap()
                    })
                })
            });
        assert_eq!(
            &bytes[0..2],
            b"PK",
            "a real xlsx built from the staged rows"
        );
        assert_eq!(filename, "report.xlsx");
        assert_eq!(
            total_rows, 2000,
            "all real rows exported, not a 1-row wrapper"
        );
        assert_eq!(sheet_count, 1);
    }
}

#[cfg(test)]
mod nested_rows_tests {
    use super::*;

    fn rows(n: usize) -> Value {
        Value::Array(
            (0..n)
                .map(|i| serde_json::json!({"id": i, "startTime": "2026-07-20T08:00:00Z"}))
                .collect(),
        )
    }

    // The shape every Harbor GraphQL read returns: rows under the operation
    // name, beside its pagination block. Stopping at the top level staged this
    // whole envelope as an opaque blob, and `data.query` then refused to read
    // it — the model spilled a result it could no longer query, and burned
    // rounds retrying.
    #[test]
    fn finds_rows_nested_under_an_operation() {
        let output = serde_json::json!({
            "shiftsList": {
                "list": rows(500),
                "pagination": {"count": 4312, "page": 1, "items": 500},
            }
        });
        let at = locate_rows(&output).expect("rows located");
        match at {
            RowsAt::Path(path) => assert_eq!(path, vec!["shiftsList", "list"]),
            other => panic!("expected a nested path, got {other:?}"),
        }
    }

    #[test]
    fn still_finds_rows_at_the_top_level_and_one_deep() {
        assert!(matches!(locate_rows(&rows(50)), Some(RowsAt::Whole)));
        let one_deep = serde_json::json!({"list": rows(200)});
        match locate_rows(&one_deep).expect("rows located") {
            RowsAt::Path(path) => assert_eq!(path, vec!["list"]),
            other => panic!("expected a one-key path, got {other:?}"),
        }
    }

    // An incidental inner array must never be mistaken for the content.
    #[test]
    fn ignores_a_minority_array_buried_in_a_blob() {
        let output = serde_json::json!({
            "meta": {"tags": [{"t": 1}, {"t": 2}]},
            "narrative": "x".repeat(20_000),
        });
        assert!(locate_rows(&output).is_none());
    }

    #[test]
    fn finds_rows_several_levels_down() {
        // Deeper than a plain GraphQL envelope — a re-wrapped or federated
        // response. The majority rule, not the depth, is what keeps this honest.
        let deep = serde_json::json!({"a": {"b": {"c": {"d": {"e": rows(300)}}}}});
        match locate_rows(&deep).expect("rows located") {
            RowsAt::Path(path) => assert_eq!(path, vec!["a", "b", "c", "d", "e"]),
            other => panic!("expected a deep path, got {other:?}"),
        }
    }

    #[test]
    fn gives_up_past_the_depth_limit() {
        let mut v = rows(300);
        for key in ["i", "h", "g", "f", "e", "d", "c", "b", "a"] {
            v = serde_json::json!({ key: v });
        }
        assert!(locate_rows(&v).is_none(), "9 levels exceeds the bound");
    }
}

#[cfg(test)]
mod majority_rule_tests {
    use super::tests::big_rows;
    use super::*;
    use serde_json::json;

    /// The invariant the recursive search rests on, tested at its own level:
    /// an array is the content only when it carries the byte majority. The
    /// end-to-end consequences live in `contract_tests`.
    /// The skip-list that stops `tool.retrieve`'s own output being re-spilled
    /// lives in `workforce-common`, which sits below this crate and so cannot
    /// import the constant. This pins the two together from the side that can.
    #[test]
    fn the_retrieve_tool_is_in_the_default_skip_list() {
        assert!(
            is_content_tool(RETRIEVE_TOOL_NAME, &SpillConfig::default().skip_tools),
            "renaming RETRIEVE_TOOL_NAME without updating the skip list in \
             workforce-common would make tool.retrieve re-spill its own output"
        );
    }

    #[test]
    fn an_array_is_the_content_only_when_it_is_the_majority() {
        // Dominant array ⇒ it is the table.
        let dominant = json!({"trace": big_rows(3000), "answer": {"total": 4312}});
        assert!(matches!(locate_rows(&dominant), Some(RowsAt::Path(_))));
        // Neither side dominates ⇒ nothing qualifies.
        let split = json!({"a": big_rows(1000), "b": "z".repeat(200_000)});
        assert!(locate_rows(&split).is_none(), "no majority ⇒ no table");
        // A minority array buried in prose is never mistaken for content.
        let buried = json!({"meta": {"tags": [{"t": 1}]}, "narrative": "x".repeat(20_000)});
        assert!(locate_rows(&buried).is_none());
    }
}

#[cfg(test)]
mod contract_tests {
    use super::tests::spill_scoped_on;
    use super::*;
    use crate::artifacts::store::install_test_global_store;
    use serde_json::json;

    const EDGE: RetrievalSurface = RetrievalSurface {
        tables: true,
        blobs: false,
    };
    const BLOBS_ONLY: RetrievalSurface = RetrievalSurface {
        tables: false,
        blobs: true,
    };

    fn rows(n: usize, width: usize) -> Value {
        Value::Array(
            (0..n)
                .map(|i| json!({"id": i, "note": "n".repeat(width)}))
                .collect(),
        )
    }

    /// What the contract says must happen, per shape per surface. Asserting the
    /// negatives alone lets a regression that stages NOTHING pass every cell, so
    /// each cell states its expected outcome too.
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    enum Expect {
        Table,
        Blob,
        Inline,
    }

    /// Named shapes, each one a real payload that has exercised this code.
    fn corpus() -> Vec<(&'static str, Value)> {
        vec![
            ("flat rows", rows(2000, 20)),
            (
                "graphql nesting",
                json!({"shiftsList": {"list": rows(800, 30), "pagination": {"count": 4312}}}),
            ),
            (
                "five fat rows",
                json!({"shiftsList": {"list": rows(5, 2_500), "pagination": {"count": 16}}}),
            ),
            (
                "one huge row",
                json!({"shiftsList": {"list": rows(1, 60_000), "pagination": {"count": 1}}}),
            ),
            (
                "fat sibling beside rows",
                json!({"data": {"rows": rows(400, 20), "audit": "y".repeat(200_000)}}),
            ),
            (
                "partial-error rows",
                json!({"list": [json!({"id": 1}), Value::Null, json!({"id": 3})],
                       "filler": "f".repeat(40_000)}),
            ),
            ("prose blob", Value::String("p".repeat(60_000))),
            (
                "deeply nested rows",
                json!({"a": {"b": {"c": {"d": rows(500, 20)}}}}),
            ),
            // An MCP transport envelope wrapping prose: normalisation unwraps it
            // before the decision, so a pass-through is NOT byte-identical here.
            (
                "mcp text envelope",
                json!({"content": [{"type": "text", "text": "p".repeat(60_000)}]}),
            ),
        ]
    }

    /// `kind` of a staged envelope, wherever it ended up in the output.
    fn staged_kind(v: &Value) -> Option<String> {
        if let Some(kind) = v.get("kind").and_then(Value::as_str) {
            if kind == TABULAR_ARTIFACT_KIND || kind == CLIPPED_ARTIFACT_KIND {
                return Some(kind.to_string());
            }
        }
        match v {
            Value::Object(map) => map.values().find_map(staged_kind),
            _ => None,
        }
    }

    /// Golden outcome per (shape, surface): `[full, edge, blobs_only, none]`.
    fn expected(shape: &str) -> [Expect; 4] {
        use Expect::*;
        match shape {
            // Rows are the payload: staged wherever tables can be read, blob
            // where only blobs can, inline where neither.
            "flat rows" | "graphql nesting" | "deeply nested rows" | "five fat rows" => {
                [Table, Table, Blob, Inline]
            }
            // Rows exist but cannot be previewed AND shrink, so the table arm
            // is refused and the blob arm takes over where it is readable.
            "one huge row" => [Blob, Inline, Blob, Inline],
            // Not tabular: no rows array carries the byte majority.
            "fat sibling beside rows"
            | "partial-error rows"
            | "prose blob"
            | "mcp text envelope" => [Blob, Inline, Blob, Inline],
            other => panic!("no golden outcome recorded for shape {other}"),
        }
    }

    #[test]
    fn every_shape_on_every_surface_honours_the_contract() {
        let config = SpillConfig {
            threshold_bytes: 8 * 1024,
            ..SpillConfig::default()
        };
        let surfaces = [
            ("hub (tables+blobs)", RetrievalSurface::FULL),
            ("edge (tables only)", EDGE),
            ("blobs only", BLOBS_ONLY),
            ("no retrieval", RetrievalSurface::NONE),
        ];

        for (shape_name, shape) in corpus() {
            let golden = expected(shape_name);
            for (idx, (surface_name, surface)) in surfaces.into_iter().enumerate() {
                let _store = install_test_global_store();
                let original = shape.to_string();
                let (out, v) =
                    spill_scoped_on(surface, "contract-agent", shape.clone(), false, &config);
                let where_ = format!("{shape_name} on {surface_name}");

                // Clause 1: never bigger than what it replaced.
                assert!(
                    out.len() <= original.len(),
                    "{where_}: spilling grew the output ({} → {})",
                    original.len(),
                    out.len()
                );

                let outcome = match staged_kind(&v) {
                    // Clause 2: whatever was staged must be readable here.
                    Some(kind) if kind == TABULAR_ARTIFACT_KIND => {
                        assert!(surface.tables, "{where_}: staged a table it cannot read");
                        assert!(
                            out.len() < original.len(),
                            "{where_}: staged without shrinking"
                        );
                        Expect::Table
                    }
                    Some(kind) if kind == CLIPPED_ARTIFACT_KIND => {
                        assert!(surface.blobs, "{where_}: staged a blob it cannot read");
                        assert!(
                            out.len() < original.len(),
                            "{where_}: staged without shrinking"
                        );
                        Expect::Blob
                    }
                    Some(other) => panic!("{where_}: unknown staged kind {other}"),
                    None => {
                        // Pass-through returns the payload untouched, except for
                        // an MCP envelope, which normalisation unwraps before the
                        // decision is reached.
                        if shape_name != "mcp text envelope" {
                            assert_eq!(out, original, "{where_}: passed through but was altered");
                        }
                        Expect::Inline
                    }
                };

                // Liveness: the contract must also STAGE when it can. Without
                // this a regression that spills nothing satisfies every negative.
                assert_eq!(
                    outcome, golden[idx],
                    "{where_}: expected {:?}, got {outcome:?}",
                    golden[idx]
                );
            }
        }
    }

    /// The live 2026-07-21 failure, stated as an expectation rather than a
    /// property: on the edge these rows must become queryable, not a blob.
    #[test]
    fn the_edge_gets_a_queryable_table_for_a_graphql_page() {
        let _store = install_test_global_store();
        let config = SpillConfig {
            threshold_bytes: 8 * 1024,
            ..SpillConfig::default()
        };
        let shape = json!({"shiftsList": {"list": rows(5, 2_500), "pagination": {"count": 16}}});
        let (_, v) = spill_scoped_on(EDGE, "edge-agent", shape, false, &config);
        assert_eq!(
            v["shiftsList"]["list"]["kind"], TABULAR_ARTIFACT_KIND,
            "the rows must be queryable on a surface with no blob retrieval"
        );
        assert_eq!(
            v["shiftsList"]["pagination"]["count"], 16,
            "siblings survive"
        );
    }

    /// A blob envelope keeps its handle in the first bytes so a later
    /// prune-clip cannot sever it. Previously asserted only by a test that was
    /// repurposed into a tabular case.
    #[test]
    fn a_blob_envelope_keeps_its_handle_in_the_first_bytes() {
        let _store = install_test_global_store();
        let (s, v) = spill_scoped_on(
            RetrievalSurface::FULL,
            "blob-agent",
            json!({"narrative": "x".repeat(40_000), "meta": {"status": "ok"}}),
            false,
            &SpillConfig::default(),
        );
        assert_eq!(v["kind"], CLIPPED_ARTIFACT_KIND);
        assert!(
            s.find("artifact_id").unwrap() < 20,
            "artifact_id must serialise first so a prune-clip preserves it"
        );
    }
}
